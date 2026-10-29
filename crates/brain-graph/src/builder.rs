use crate::model::*;
use crate::queries::GraphQueries;
use crate::scope::{self, FileScope, ImportTarget, Resolution, ScopeResolver};
use brain_kernel::ast::SupportedLanguage;
use brain_kernel::parse_cache::{ParseCache, ParseWorker};
use brain_kernel::{BrainError, Result};
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use streaming_iterator::StreamingIterator;
use petgraph::stable_graph::NodeIndex;
use tree_sitter::{Node, QueryCursor};

pub struct GraphBuilder {
    queries: GraphQueries,
    cache: Option<Arc<ParseCache>>,
}

/// One call expression in a file, with the captures of every query pattern
/// that matched it merged together.
struct CallSite {
    name: String,
    line: usize,
    path: Option<String>,
    receiver: Option<String>,
}

impl GraphBuilder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queries: GraphQueries::new()?,
            cache: None,
        })
    }

    /// Shares parsed trees with search and read. The graph pays for the parse
    /// once; later requests are digest hits instead of reparsing.
    pub fn with_parse_cache(mut self, cache: Arc<ParseCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    pub fn build_graph(&self, root_dir: &Path) -> Result<ProjectCodeGraph> {
        let mut code_graph = ProjectCodeGraph::new();
        let files = self.collect_code_files(root_dir)?;

        // Pass 0: read + parse in parallel, files indexed to keep order. The
        // resolver layout is merged in that same order, so node indices are
        // stable between runs.
        let parsed: Vec<Option<ParsedFile>> =
            parallel_map(files.len(), ParseWorker::new, |index, worker| {
                let path = &files[index];
                let content = fs::read_to_string(path).ok()?;
                let hash = blake3::hash(content.as_bytes());
                let tree = SupportedLanguage::from_path(path).and_then(|lang| {
                    worker
                        .parse(lang, &content)
                        .ok()
                        .map(|tree| (lang, Arc::new(tree)))
                });
                if let (Some(cache), Some((_, tree))) = (&self.cache, &tree) {
                    cache.insert(path, hash, tree.clone());
                }
                Some(ParsedFile {
                    path: path.clone(),
                    content,
                    hash,
                    tree,
                })
            });
        let parsed: Vec<ParsedFile> = parsed.into_iter().flatten().collect();

        for file in &parsed {
            code_graph.file_hashes.insert(file.path.clone(), file.hash);
            if file.tree.is_some() {
                code_graph
                    .resolver
                    .register_file(&file.path, &scope::crate_name_for(&file.path));
            }
        }

        // Pass 1: definitions. Query execution is independent per file; only
        // the merge touches the graph, in file order.
        let definitions: Vec<Option<Result<Vec<SymbolNode>>>> =
            parallel_map(parsed.len(), || (), |index, ()| {
                Some(self.definitions_for(&parsed[index]))
            });
        for nodes in definitions.into_iter().flatten() {
            merge_definitions(&mut code_graph, nodes?);
        }

        // Pass 2: imports. The resolver is complete after pass 0.
        let imports: Vec<Option<Result<FileScope>>> =
            parallel_map(parsed.len(), || (), |index, ()| {
                Some(self.imports_for(&parsed[index], &code_graph.resolver))
            });
        for (index, scope) in imports.into_iter().enumerate() {
            if let Some(scope) = scope {
                code_graph.scopes.insert(parsed[index].path.clone(), scope?);
            }
        }

        // Pass 3: calls, now that every name has a node and a scope. Resolution
        // reads the graph; edges are collected per file and merged in order.
        let relations: Vec<Option<Result<RelationsDelta>>> =
            parallel_map(parsed.len(), || (), |index, ()| {
                Some(self.relations_for(&parsed[index], &code_graph))
            });
        for delta in relations.into_iter().flatten() {
            apply_relations(&mut code_graph, delta?);
        }

        Ok(code_graph)
    }

    /// Re-index a single file incrementally when it changes.
    ///
    /// Only this file's own edges are recomputed. Edges from other files into
    /// its symbols were already removed with those nodes and are not rebuilt
    /// here — a full rebuild restores them.
    pub fn update_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &PathBuf,
        _root_dir: &Path,
    ) -> Result<bool> {
        let known_hash = code_graph.file_hashes.get(path).copied();
        let prepared = self.prepare_update(known_hash, path)?;
        self.apply_update(code_graph, prepared)
    }

    /// Read, hash and parse outside the graph lock. The watcher calls this on
    /// the blocking pool so a save never freezes readers while tree-sitter runs.
    pub fn prepare_update(
        &self,
        known_hash: Option<blake3::Hash>,
        path: &Path,
    ) -> Result<PreparedUpdate> {
        if !path.exists() {
            return Ok(PreparedUpdate {
                path: path.to_path_buf(),
                content: None,
                hash: None,
                tree: None,
                changed: true,
                removed: true,
            });
        }

        let content = fs::read_to_string(path).map_err(BrainError::Io)?;
        let hash = blake3::hash(content.as_bytes());
        if known_hash == Some(hash) {
            return Ok(PreparedUpdate {
                path: path.to_path_buf(),
                content: None,
                hash: Some(hash),
                tree: None,
                changed: false,
                removed: false,
            });
        }

        let tree = SupportedLanguage::from_path(path).and_then(|lang| {
            ParseWorker::new()
                .parse(lang, &content)
                .ok()
                .map(|tree| (lang, Arc::new(tree)))
        });
        if let (Some(cache), Some((_, tree))) = (&self.cache, &tree) {
            cache.insert(path, hash, tree.clone());
        }

        Ok(PreparedUpdate {
            path: path.to_path_buf(),
            content: Some(content),
            hash: Some(hash),
            tree,
            changed: true,
            removed: false,
        })
    }

    /// Applies a prepared update under a short write lock.
    pub fn apply_update(
        &self,
        code_graph: &mut ProjectCodeGraph,
        prepared: PreparedUpdate,
    ) -> Result<bool> {
        if !prepared.changed {
            return Ok(false);
        }

        code_graph.remove_file_nodes(&prepared.path);
        if prepared.removed {
            if let Some(cache) = &self.cache {
                cache.invalidate(&prepared.path);
            }
            return Ok(true);
        }

        let (Some(content), Some(hash)) = (prepared.content, prepared.hash) else {
            return Ok(true);
        };
        code_graph.file_hashes.insert(prepared.path.clone(), hash);

        let Some((lang, tree)) = prepared.tree else {
            return Ok(true);
        };
        code_graph
            .resolver
            .register_file(&prepared.path, &scope::crate_name_for(&prepared.path));

        let nodes = self.collect_definitions(
            &prepared.path,
            &content,
            &tree.root_node(),
            lang,
        )?;
        merge_definitions(code_graph, nodes);

        let scope = self.collect_imports(
            &prepared.path,
            &content,
            &tree.root_node(),
            lang,
            &code_graph.resolver,
        )?;
        code_graph.scopes.insert(prepared.path.clone(), scope);

        let delta = self.collect_relations(
            &prepared.path,
            &content,
            &tree.root_node(),
            lang,
            code_graph,
        )?;
        apply_relations(code_graph, delta);

        Ok(true)
    }

    fn collect_code_files(&self, root_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut types_builder = TypesBuilder::new();
        let _ = types_builder.add("code", "*.rs");
        let _ = types_builder.add("code", "*.kt");
        types_builder.select("code");
        let types = types_builder
            .build()
            .map_err(|e| BrainError::Search(e.to_string()))?;

        let mut walker = WalkBuilder::new(root_dir);
        walker
            .types(types)
            .hidden(true)
            .git_ignore(true)
            .git_global(true);

        let mut files = Vec::new();
        for entry in walker.build().filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            // Components, not a formatted string: no allocation per entry.
            let excluded = path.components().any(|component| {
                matches!(
                    component.as_os_str().to_str(),
                    Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                )
            });
            if !excluded {
                files.push(path.to_path_buf());
            }
        }
        // Walk order is not guaranteed; sorting makes the parallel parse and
        // the node indices stable between runs.
        files.sort();
        Ok(files)
    }

    fn definitions_for(&self, file: &ParsedFile) -> Result<Vec<SymbolNode>> {
        let Some((lang, tree)) = &file.tree else {
            return Ok(Vec::new());
        };
        self.collect_definitions(&file.path, &file.content, &tree.root_node(), *lang)
    }

    fn imports_for(&self, file: &ParsedFile, resolver: &ScopeResolver) -> Result<FileScope> {
        let Some((lang, tree)) = &file.tree else {
            return Ok(FileScope::default());
        };
        self.collect_imports(
            &file.path,
            &file.content,
            &tree.root_node(),
            *lang,
            resolver,
        )
    }

    fn relations_for(&self, file: &ParsedFile, graph: &ProjectCodeGraph) -> Result<RelationsDelta> {
        let Some((lang, tree)) = &file.tree else {
            return Ok(RelationsDelta::default());
        };
        self.collect_relations(
            &file.path,
            &file.content,
            &tree.root_node(),
            *lang,
            graph,
        )
    }

    fn collect_definitions(
        &self,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
    ) -> Result<Vec<SymbolNode>> {
        let query = self.queries.defs_query(lang);
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let name_idx = query.capture_index_for_name("name").unwrap_or(0);
        let def_idx = query.capture_index_for_name("def").unwrap_or(1);
        let mut definitions = Vec::new();

        while let Some(m) = matches.next() {
            let mut symbol_name = String::new();
            let mut def_node = None;

            for capture in m.captures {
                if capture.index == name_idx {
                    symbol_name = capture
                        .node
                        .utf8_text(content.as_bytes())
                        .unwrap_or("")
                        .to_string();
                } else if capture.index == def_idx {
                    def_node = Some(capture.node);
                }
            }

            let Some(node) = def_node else { continue };
            if symbol_name.is_empty() {
                continue;
            }

            // A function inside `impl GraphBuilder` is a method of GraphBuilder.
            // Without the container, `x.build()` cannot be told apart from the
            // eleven other `build`s in the workspace.
            let container = enclosing_container(node, content.as_bytes());
            let kind = match (node.kind(), container.is_some()) {
                ("function_item" | "function_declaration", true) => SymbolKind::Method,
                ("function_item" | "function_declaration", false) => SymbolKind::Function,
                ("struct_item" | "class_declaration", _) => SymbolKind::StructOrClass,
                ("enum_item", _) => SymbolKind::Enum,
                ("trait_item", _) => SymbolKind::TraitOrInterface,
                _ => SymbolKind::Method,
            };

            let signature = self.extract_signature(node, content.as_bytes(), lang);
            let qualified_name = match &container {
                Some(container) => format!("{container}::{symbol_name}"),
                None => symbol_name.clone(),
            };
            let qualified_name = format!("{}::{}", path.display(), qualified_name);

            definitions.push(SymbolNode {
                qualified_name,
                name: symbol_name,
                kind,
                file_path: path.to_path_buf(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                signature,
                container,
                is_test: is_test_definition(node, content.as_bytes(), lang)
                    || is_test_file(path),
            });
        }

        Ok(definitions)
    }

    fn collect_imports(
        &self,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
        resolver: &ScopeResolver,
    ) -> Result<FileScope> {
        let Some(query) = self.queries.imports_query(lang) else {
            return Ok(FileScope::default());
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let path_idx = query.capture_index_for_name("import.path").unwrap_or(0);
        let mut scope = FileScope::default();

        while let Some(m) = matches.next() {
            for capture in m.captures {
                if capture.index != path_idx {
                    continue;
                }
                let raw = capture
                    .node
                    .utf8_text(content.as_bytes())
                    .unwrap_or("")
                    .to_string();
                // `use super::x` inside `mod tests` climbs from the inline
                // module, not from the file's top level.
                let module = module_context(capture.node, content.as_bytes(), path, resolver);

                for item in scope::expand_use(&raw) {
                    match &item {
                        scope::UseItem::Glob { path: import_path } => {
                            if let Some(ImportTarget::File(file)) =
                                resolver.resolve_import_in(path, &module, import_path)
                            {
                                if !scope.globs.contains(&file) {
                                    scope.globs.push(file);
                                }
                            }
                        }
                        scope::UseItem::Named {
                            name,
                            path: import_path,
                        } => {
                            if let Some(target) =
                                resolver.resolve_import_in(path, &module, import_path)
                            {
                                scope.imports.insert(name.clone(), target);
                            }
                        }
                    }
                }
            }
        }

        Ok(scope)
    }

    fn collect_relations(
        &self,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
        graph: &ProjectCodeGraph,
    ) -> Result<RelationsDelta> {
        let Some(file_node_indices) = graph.file_nodes.get(path) else {
            return Ok(RelationsDelta::default());
        };

        let query = self.queries.calls_query(lang);
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let name_idx = query.capture_index_for_name("call.name");
        let path_idx = query.capture_index_for_name("call.path");
        let receiver_idx = query.capture_index_for_name("call.receiver");

        let bindings = if lang == SupportedLanguage::Rust {
            self.collect_bindings(content, root_node)
        } else {
            Bindings::new()
        };

        let empty_scope = FileScope::default();
        let scope = graph.scopes.get(path).unwrap_or(&empty_scope);

        // The call query overlaps on purpose: a single `crate::a::f()` matches
        // both the scoped and the path-shaped pattern, each carrying a
        // different piece of evidence (`receiver` vs `call_path`). Feeding
        // every match to the resolver produced two identical edges and counted
        // the same call twice in the confidence line — one call site is one
        // decision, so matches are merged by the name node's byte span and the
        // captures are combined.
        let mut sites: BTreeMap<(usize, usize), CallSite> = BTreeMap::new();
        while let Some(m) = matches.next() {
            let mut called_name = None;
            let mut call_path = None;
            let mut receiver = None;

            for capture in m.captures {
                let text = capture
                    .node
                    .utf8_text(content.as_bytes())
                    .unwrap_or("")
                    .to_string();
                if Some(capture.index) == name_idx {
                    called_name = Some((text, capture.node));
                } else if Some(capture.index) == path_idx {
                    call_path = Some(text);
                } else if Some(capture.index) == receiver_idx {
                    receiver = Some(text);
                }
            }

            let Some((called_name, name_node)) = called_name else {
                continue;
            };
            if called_name.is_empty() {
                continue;
            }

            let site = sites
                .entry((name_node.start_byte(), name_node.end_byte()))
                .or_insert_with(|| CallSite {
                    name: called_name.clone(),
                    line: name_node.start_position().row + 1,
                    path: None,
                    receiver: None,
                });
            if site.path.is_none() {
                site.path = call_path;
            }
            if site.receiver.is_none() {
                site.receiver = receiver;
            }
        }

        let mut delta = RelationsDelta::default();
        for site in sites.values() {
            // The caller is the innermost definition containing the call, not
            // the first one: closures and nested functions live inside their
            // parent, and picking the parent would attribute the edge to the
            // wrong node.
            let Some(caller) = innermost_enclosing(file_node_indices, graph, site.line) else {
                continue;
            };
            let caller_container = graph
                .graph
                .node_weight(caller)
                .and_then(|node| node.container.as_deref());

            let resolution = self.resolve_call(
                graph,
                scope,
                path,
                &site.name,
                site.path.as_deref(),
                site.receiver.as_deref(),
                caller_container,
                &bindings,
                site.line,
            );

            if let Resolution::Resolved(callee) = &resolution {
                if caller != *callee {
                    delta.edges.push((caller, *callee, site.line));
                }
            }
            delta.decisions.push((site.name.clone(), resolution));
        }

        Ok(delta)
    }

    /// Decides which definition a call refers to.
    ///
    /// `x.method()`, `Type::method()` and `self.method()` are method calls: the
    /// container has to match, or the edge is not made. A bare call goes
    /// through scope resolution.
    #[allow(clippy::too_many_arguments)]
    fn resolve_call(
        &self,
        code_graph: &ProjectCodeGraph,
        scope: &FileScope,
        caller_file: &Path,
        called_name: &str,
        call_path: Option<&str>,
        receiver: Option<&str>,
        caller_container: Option<&str>,
        bindings: &Bindings,
        call_line: usize,
    ) -> Resolution {
        use std::borrow::Cow;

        // `self.method()` belongs to the enclosing impl. Borrow the container
        // and the receiver type instead of cloning them per call site.
        let receiver_type: Option<Cow<'_, str>> = match receiver {
            Some("self") => caller_container.map(Cow::Borrowed),
            Some(name) if name.chars().next().is_some_and(|c| c.is_uppercase()) => {
                Some(Cow::Borrowed(name))
            }
            Some(name) => binding_type_at(bindings, name, call_line).map(Cow::Owned),
            None => None,
        };

        if let Some(container) = receiver_type {
            let matches = code_graph
                .container_lookup
                .get(container.as_ref())
                .and_then(|methods| methods.get(called_name))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if let [only] = matches {
                return Resolution::Resolved(*only);
            }
        }

        let definitions = code_graph
            .symbol_lookup
            .get(called_name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        // `Type::method()` where the type is not modelled (trait impls, enum
        // variants) still gets a shot through the path's module.
        if let Some(path_segments) = call_path {
            if let Some(module_file) = scope::last_segment(path_segments) {
                let matches: Vec<NodeIndex> = definitions
                    .iter()
                    .copied()
                    .filter(|&index| {
                        code_graph
                            .graph
                            .node_weight(index)
                            .is_some_and(|node| node.container.as_deref() == Some(module_file))
                    })
                    .collect();
                if let [node] = matches.as_slice() {
                    return Resolution::Resolved(*node);
                }
            }
        }

        scope::pick_target(
            called_name,
            caller_file,
            scope,
            &code_graph.resolver,
            call_path,
            definitions,
            code_graph,
        )
    }

    /// `let x = Type::new(…)`, `let x: Type = …`, `let x = Type { … }` per line.
    fn collect_bindings(&self, content: &str, root_node: &Node) -> Bindings {
        let mut bindings = Bindings::new();
        let Some(query) = &self.queries.rust_bindings else {
            return bindings;
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let name_idx = query.capture_index_for_name("bind.name");
        let type_idx = query.capture_index_for_name("bind.type");

        while let Some(m) = matches.next() {
            let mut binding = None;
            let mut binding_type = None;
            let mut line = 0;

            for capture in m.captures {
                let text = capture
                    .node
                    .utf8_text(content.as_bytes())
                    .unwrap_or("")
                    .to_string();
                if Some(capture.index) == name_idx {
                    binding = Some(text);
                    line = capture.node.start_position().row + 1;
                } else if Some(capture.index) == type_idx {
                    binding_type = Some(last_type_segment(&text));
                }
            }

            if let (Some(binding), Some(binding_type)) = (binding, binding_type) {
                if !binding_type.is_empty() {
                    bindings
                        .entry(binding)
                        .or_default()
                        .push((line, binding_type));
                }
            }
        }

        for entries in bindings.values_mut() {
            entries.sort_by_key(|(line, _)| *line);
        }
        bindings
    }

/// Whether a definition belongs to a test: `#[test]`, `#[tokio::test]`,
/// `#[cfg(test)]`, or a `mod tests` ancestor.
///
/// The graph stores it so callers can decide to collapse test-only impacts
/// instead of spending the response budget on them.
/// Test files live under a `tests/` directory or are named `*_test.rs`.
/// A fixture helper has no `#[test]` attribute but is still test-only.
fn is_test_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "tests")
        || path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem == "tests" || stem.ends_with("_test"))
}

fn is_test_definition(node: Node, source: &[u8], lang: SupportedLanguage) -> bool {
    if lang != SupportedLanguage::Rust {
        return false;
    }

    let mut current = Some(node);
    while let Some(item) = current {
        // Attributes are contiguous siblings immediately before the item.
        let mut previous = item.prev_sibling();
        while let Some(attribute) = previous {
            if attribute.kind() != "attribute_item" {
                break;
            }
            let text = attribute.utf8_text(source).unwrap_or("");
            if text.contains("test") && !text.contains("not(test)") {
                return true;
            }
            previous = attribute.prev_sibling();
        }

        if item.kind() == "mod_item"
            && item
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some("tests")
        {
            return true;
        }
        current = item.parent();
    }
    false
}

/// The type or trait the node is written inside, if any.
fn enclosing_container(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(parent.kind(), "impl_item" | "trait_item") {
            return parent
                .child_by_field_name("type")
                .or_else(|| parent.child_by_field_name("name"))
                .and_then(|n| n.utf8_text(source).ok())
                .map(last_type_segment);
        }
        current = parent.parent();
    }
    None
}

/// The innermost definition of the file containing `line`.
fn innermost_enclosing(
    file_node_indices: &[petgraph::stable_graph::NodeIndex],
    graph: &ProjectCodeGraph,
    line: usize,
) -> Option<petgraph::stable_graph::NodeIndex> {
    file_node_indices
        .iter()
        .filter_map(|&idx| graph.graph.node_weight(idx).map(|node| (idx, node)))
        .filter(|(_, node)| line >= node.start_line && line <= node.end_line)
        .min_by_key(|(_, node)| node.end_line.saturating_sub(node.start_line))
        .map(|(idx, _)| idx)
}

/// `crate::graph::builder::GraphBuilder` → `GraphBuilder`; `&mut Foo` → `Foo`.
fn last_type_segment(text: &str) -> String {
    let cleaned = text
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim();
    cleaned
        .rsplit("::")
        .next()
        .unwrap_or(cleaned)
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
        .to_string()
}

/// The most recent binding of `receiver` at or before `line`.
fn binding_type_at(bindings: &Bindings, receiver: &str, line: usize) -> Option<String> {
    let entries = bindings.get(receiver)?;
    // Last binding at or before the call line.
    let position = entries.partition_point(|(bind_line, _)| *bind_line <= line);
    position.checked_sub(1).map(|index| entries[index].1.clone())
}

fn merge_definitions(graph: &mut ProjectCodeGraph, nodes: Vec<SymbolNode>) {
    for node in nodes {
        let name = node.name.clone();
        let qualified = node.qualified_name.clone();
        let path = node.file_path.clone();
        let container = node.container.clone();
        let index = graph.graph.add_node(node);
        graph
            .symbol_lookup
            .entry(name.clone())
            .or_default()
            .push(index);
        graph.qualified_lookup.insert(qualified, index);
        graph.file_nodes.entry(path).or_default().push(index);
        if let Some(container) = container {
            graph
                .container_lookup
                .entry(container)
                .or_default()
                .entry(name)
                .or_default()
                .push(index);
        }
    }
}

fn apply_relations(graph: &mut ProjectCodeGraph, delta: RelationsDelta) {
    for (name, resolution) in delta.decisions {
        graph.resolution.record(&name, &resolution);
    }
    for (caller, callee, line) in delta.edges {
        graph.graph.add_edge(
            caller,
            callee,
            RelationEdge {
                kind: RelationKind::Calls,
                call_site_line: line,
            },
        );
    }
}

/// Runs `map` over `0..len` on a bounded worker set. Each worker owns a
/// context (a parser, for the read/parse pass) and claims indices from a shared
/// queue, so one huge file cannot become the tail while others idle. Results
/// stay at their original index, which keeps merges deterministic.
fn parallel_map<C, T, F>(len: usize, make: impl Fn() -> C, map: F) -> Vec<Option<T>>
where
    C: Send,
    T: Send,
    F: Fn(usize, &mut C) -> Option<T> + Sync,
{
    if len == 0 {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(len);
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<T>>> = (0..len).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let slots = &slots;
            let map = &map;
            let make = &make;
            scope.spawn(move || {
                let mut context = make();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= len {
                        break;
                    }
                    let value = map(index, &mut context);
                    *slots[index]
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
                }
            });
        }
    });

    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        })
        .collect()
}

/// The module path a `use` at `node` resolves from: the file's module plus any
/// inline `mod x { … }` it sits inside.
fn module_context(node: Node, source: &[u8], file: &Path, resolver: &ScopeResolver) -> String {
    let file_module = resolver
        .file_module(file)
        .map(str::to_string)
        .unwrap_or_default();

    let mut inline: Vec<String> = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            if let Some(name) = parent.child_by_field_name("name") {
                if let Ok(text) = name.utf8_text(source) {
                    inline.push(text.to_string());
                }
            }
        }
        current = parent.parent();
    }
    inline.reverse();

    if inline.is_empty() {
        file_module
    } else {
        format!("{file_module}/{}", inline.join("/"))
    }
}
