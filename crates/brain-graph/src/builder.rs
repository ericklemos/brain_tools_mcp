use crate::model::*;
use crate::queries::GraphQueries;
use crate::scope::{self, ImportTarget, Resolution, ScopeResolver};
use brain_kernel::ast::SupportedLanguage;
use brain_kernel::{BrainError, Result};
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

pub struct GraphBuilder {
    queries: GraphQueries,
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
        })
    }

    pub fn build_graph(&self, root_dir: &Path) -> Result<ProjectCodeGraph> {
        let mut code_graph = ProjectCodeGraph::new();
        let files = self.collect_code_files(root_dir)?;

        let mut file_contents = HashMap::new();
        let mut file_trees = HashMap::new();

        // Pass 0: parse, and teach the resolver where every file lives. Imports
        // cannot be resolved before the whole directory layout is known, which
        // is why this is a pass of its own.
        for path in &files {
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let hash = blake3::hash(content.as_bytes());
            code_graph.file_hashes.insert(path.clone(), hash);

            if let Some(lang) = SupportedLanguage::from_path(path) {
                let mut parser = Parser::new();
                let tree = match lang {
                    SupportedLanguage::Rust => {
                        let _ = parser.set_language(&tree_sitter_rust::LANGUAGE.into());
                        parser.parse(&content, None)
                    }
                    SupportedLanguage::Kotlin => {
                        let _ = parser.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into());
                        parser.parse(&content, None)
                    }
                };

                if let Some(t) = tree {
                    code_graph
                        .resolver
                        .register_file(path, &scope::crate_name_for(path));
                    file_trees.insert(path.clone(), t);
                }
            }

            file_contents.insert(path.clone(), content);
        }

        // Pass 1: definitions (nodes).
        for (path, tree) in &file_trees {
            if let Some(lang) = SupportedLanguage::from_path(path) {
                if let Some(content) = file_contents.get(path) {
                    self.extract_definitions_from_file(
                        &mut code_graph,
                        path,
                        content,
                        &tree.root_node(),
                        lang,
                    )?;
                }
            }
        }

        // Pass 2: imports (scopes). Must follow pass 1 only for ordering
        // clarity; it depends on the resolver, populated in pass 0.
        for (path, tree) in &file_trees {
            if let Some(lang) = SupportedLanguage::from_path(path) {
                if let Some(content) = file_contents.get(path) {
                    self.extract_imports_from_file(
                        &mut code_graph,
                        path,
                        content,
                        &tree.root_node(),
                        lang,
                    )?;
                }
            }
        }

        // Pass 3: calls (edges), now that every name has a scope.
        for (path, tree) in &file_trees {
            if let Some(lang) = SupportedLanguage::from_path(path) {
                if let Some(content) = file_contents.get(path) {
                    self.extract_relations_from_file(
                        &mut code_graph,
                        path,
                        content,
                        &tree.root_node(),
                        lang,
                    )?;
                }
            }
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
        if !path.exists() {
            code_graph.remove_file_nodes(path);
            return Ok(true);
        }

        let content = fs::read_to_string(path).map_err(BrainError::Io)?;
        let current_hash = blake3::hash(content.as_bytes());

        if let Some(existing_hash) = code_graph.file_hashes.get(path) {
            if *existing_hash == current_hash {
                return Ok(false); // Hash unchanged
            }
        }

        // Remove old nodes, scope and resolver entry before re-adding.
        code_graph.remove_file_nodes(path);
        code_graph.file_hashes.insert(path.clone(), current_hash);

        let lang = match SupportedLanguage::from_path(path) {
            Some(l) => l,
            None => return Ok(false),
        };

        let mut parser = Parser::new();
        let tree = match lang {
            SupportedLanguage::Rust => {
                let _ = parser.set_language(&tree_sitter_rust::LANGUAGE.into());
                parser.parse(&content, None)
            }
            SupportedLanguage::Kotlin => {
                let _ = parser.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into());
                parser.parse(&content, None)
            }
        };

        if let Some(t) = tree {
            code_graph
                .resolver
                .register_file(path, &scope::crate_name_for(path));
            self.extract_definitions_from_file(code_graph, path, &content, &t.root_node(), lang)?;
            self.extract_imports_from_file(code_graph, path, &content, &t.root_node(), lang)?;
            self.extract_relations_from_file(code_graph, path, &content, &t.root_node(), lang)?;
        }

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
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path();
                let path_str = path.to_string_lossy();
                if !path_str.contains("/target/")
                    && !path_str.contains("/build/")
                    && !path_str.contains("/.gradle/")
                    && !path_str.contains("/node_modules/")
                    && !path_str.contains("/.git/")
                {
                    files.push(path.to_path_buf());
                }
            }
        }
        Ok(files)
    }

    fn extract_definitions_from_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
    ) -> Result<()> {
        let query = self.queries.defs_query(lang);
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let name_idx = query.capture_index_for_name("name").unwrap_or(0);
        let def_idx = query.capture_index_for_name("def").unwrap_or(1);

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
                Some(container) => format!("{container}::{}", symbol_name),
                None => symbol_name.clone(),
            };
            let qualified_name = format!("{}::{}", path.display(), qualified_name);

            let symbol_node = SymbolNode {
                qualified_name: qualified_name.clone(),
                name: symbol_name.clone(),
                kind,
                file_path: path.to_path_buf(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                signature,
                container,
            };

            let node_idx = code_graph.graph.add_node(symbol_node);
            code_graph
                .symbol_lookup
                .entry(symbol_name)
                .or_default()
                .push(node_idx);
            code_graph.qualified_lookup.insert(qualified_name, node_idx);
            code_graph
                .file_nodes
                .entry(path.to_path_buf())
                .or_default()
                .push(node_idx);
        }

        Ok(())
    }

    fn extract_imports_from_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
    ) -> Result<()> {
        let Some(query) = self.queries.imports_query(lang) else {
            return Ok(());
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let path_idx = query.capture_index_for_name("import.path").unwrap_or(0);
        let mut scope = crate::scope::FileScope::default();

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
                let module =
                    module_context(capture.node, content.as_bytes(), path, &code_graph.resolver);

                for item in scope::expand_use(&raw) {
                    match &item {
                        scope::UseItem::Glob { path: import_path } => {
                            if let Some(ImportTarget::File(file)) = code_graph
                                .resolver
                                .resolve_import_in(path, &module, import_path)
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
                                code_graph
                                    .resolver
                                    .resolve_import_in(path, &module, import_path)
                            {
                                scope.imports.insert(name.clone(), target);
                            }
                        }
                    }
                }
            }
        }

        code_graph.scopes.insert(path.to_path_buf(), scope);
        Ok(())
    }

    fn extract_relations_from_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
    ) -> Result<()> {
        let file_node_indices = match code_graph.file_nodes.get(path) {
            Some(indices) => indices.clone(),
            None => return Ok(()),
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
            Vec::new()
        };

        let empty_scope = crate::scope::FileScope::default();
        let scope = code_graph.scopes.get(path).unwrap_or(&empty_scope).clone();

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

        for site in sites.values() {
            // The caller is the innermost definition containing the call, not
            // the first one: closures and nested functions live inside their
            // parent, and picking the parent would attribute the edge to the
            // wrong node.
            let Some(caller) = innermost_enclosing(&file_node_indices, code_graph, site.line)
            else {
                continue;
            };
            let caller_container = code_graph
                .graph
                .node_weight(caller)
                .and_then(|node| node.container.clone());

            let resolution = self.resolve_call(
                code_graph,
                &scope,
                path,
                &site.name,
                site.path.as_deref(),
                site.receiver.as_deref(),
                caller_container.as_deref(),
                &bindings,
                site.line,
            );
            code_graph.resolution.record(&site.name, &resolution);

            if let Resolution::Resolved(callee) = resolution {
                if caller != callee {
                    code_graph.graph.add_edge(
                        caller,
                        callee,
                        RelationEdge {
                            kind: RelationKind::Calls,
                            call_site_line: site.line,
                        },
                    );
                }
            }
        }

        Ok(())
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
        scope: &crate::scope::FileScope,
        caller_file: &Path,
        called_name: &str,
        call_path: Option<&str>,
        receiver: Option<&str>,
        caller_container: Option<&str>,
        bindings: &[(usize, String, String)],
        call_line: usize,
    ) -> Resolution {
        // `self.method()` belongs to the enclosing impl.
        let receiver_type = match receiver {
            Some("self") => caller_container.map(str::to_string),
            Some(name) if name.chars().next().is_some_and(|c| c.is_uppercase()) => {
                Some(name.to_string())
            }
            Some(name) => binding_type_at(bindings, name, call_line),
            None => None,
        };

        if let Some(container) = receiver_type {
            let matches = code_graph.methods_of(&container, called_name);
            match matches.len() {
                1 => return Resolution::Resolved(matches[0].1),
                0 => {}
                n => {
                    // Several impls of the same type: fall through to scope
                    // resolution, which can at least use the file.
                    let _ = n;
                }
            }
        }

        // `Type::method()` where the type is not modelled (trait impls, enum
        // variants) still gets a shot through the path's module.
        if let Some(path_segments) = call_path {
            if let Some(module_file) = scope::last_segment(path_segments) {
                let matches: Vec<_> = code_graph
                    .definitions_of(called_name)
                    .into_iter()
                    .filter(|(_, idx)| {
                        code_graph
                            .graph
                            .node_weight(*idx)
                            .is_some_and(|node| node.container.as_deref() == Some(module_file))
                    })
                    .collect();
                if let [(_, node)] = matches.as_slice() {
                    return Resolution::Resolved(*node);
                }
            }
        }

        let definitions = code_graph.definitions_of(called_name);
        scope::pick_target(
            called_name,
            caller_file,
            scope,
            &code_graph.resolver,
            call_path,
            &definitions,
        )
    }

    /// `let x = Type::new(…)`, `let x: Type = …`, `let x = Type { … }` per line.
    fn collect_bindings(&self, content: &str, root_node: &Node) -> Vec<(usize, String, String)> {
        let mut bindings = Vec::new();
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
                    bindings.push((line, binding, binding_type));
                }
            }
        }

        bindings.sort_by_key(|(line, _, _)| *line);
        bindings
    }

    fn extract_signature(&self, node: Node, source: &[u8], lang: SupportedLanguage) -> String {
        let text = node.utf8_text(source).unwrap_or("");
        match lang {
            SupportedLanguage::Rust => {
                if let Some(body_node) = node.child_by_field_name("body") {
                    let sig_end = body_node.start_byte().saturating_sub(node.start_byte());
                    return text[..sig_end].trim().to_string();
                }
                if let Some(idx) = text.find('{') {
                    text[..idx].trim().to_string()
                } else {
                    text.lines().next().unwrap_or("").trim().to_string()
                }
            }
            SupportedLanguage::Kotlin => {
                if let Some(idx) = text.find('{') {
                    text[..idx].trim().to_string()
                } else {
                    text.lines().next().unwrap_or("").trim().to_string()
                }
            }
        }
    }
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
fn binding_type_at(
    bindings: &[(usize, String, String)],
    receiver: &str,
    line: usize,
) -> Option<String> {
    bindings
        .iter()
        .filter(|(bind_line, name, _)| *bind_line <= line && name == receiver)
        .max_by_key(|(bind_line, _, _)| *bind_line)
        .map(|(_, _, ty)| ty.clone())
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
