use crate::error::{BrainError, Result};
use crate::graph::model::*;
use crate::graph::queries::GraphQueries;
use crate::search::ast_chunker::SupportedLanguage;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

pub struct GraphBuilder {
    queries: GraphQueries,
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

        // Pass 1: Extract Definitions (Nodes)
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
                    self.extract_definitions_from_file(
                        &mut code_graph,
                        path,
                        root_dir,
                        &content,
                        &t.root_node(),
                        lang,
                    )?;
                    file_trees.insert(path.clone(), t);
                }
            }

            file_contents.insert(path.clone(), content);
        }

        // Pass 2: Extract Calls & Relations (Edges)
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

    /// Re-index a single file incrementally when it changes
    pub fn update_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &PathBuf,
        root_dir: &Path,
    ) -> Result<bool> {
        if !path.exists() {
            code_graph.remove_file_nodes(path);
            return Ok(true);
        }

        let content = fs::read_to_string(path).map_err(|e| BrainError::Io(e))?;
        let current_hash = blake3::hash(content.as_bytes());

        if let Some(existing_hash) = code_graph.file_hashes.get(path) {
            if *existing_hash == current_hash {
                return Ok(false); // Hash unchanged
            }
        }

        // Remove old nodes from graph
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
            self.extract_definitions_from_file(
                code_graph,
                path,
                root_dir,
                &content,
                &t.root_node(),
                lang,
            )?;
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
            if entry.file_type().map_or(false, |ft| ft.is_file()) {
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
        path: &PathBuf,
        root_dir: &Path,
        content: &str,
        root_node: &Node,
        lang: SupportedLanguage,
    ) -> Result<()> {
        let query = self.queries.defs_query(lang);
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root_node, content.as_bytes());

        let name_idx = query.capture_index_for_name("name").unwrap_or(0);
        let def_idx = query.capture_index_for_name("def").unwrap_or(1);

        let relative_path = path
            .strip_prefix(root_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

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

            if let Some(node) = def_node {
                if symbol_name.is_empty() {
                    continue;
                }

                let kind = match node.kind() {
                    "function_item" | "function_declaration" => SymbolKind::Function,
                    "struct_item" | "class_declaration" => SymbolKind::StructOrClass,
                    "enum_item" => SymbolKind::Enum,
                    "trait_item" => SymbolKind::TraitOrInterface,
                    _ => SymbolKind::Method,
                };

                let signature = self.extract_signature(node, content.as_bytes(), lang);
                let qualified_name = format!("{}::{}", relative_path, symbol_name);

                let symbol_node = SymbolNode {
                    qualified_name: qualified_name.clone(),
                    name: symbol_name.clone(),
                    kind,
                    file_path: path.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    signature,
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
                    .entry(path.clone())
                    .or_default()
                    .push(node_idx);
            }
        }

        Ok(())
    }

    fn extract_relations_from_file(
        &self,
        code_graph: &mut ProjectCodeGraph,
        path: &PathBuf,
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

        let call_idx = query.capture_index_for_name("call.name").unwrap_or(0);

        while let Some(m) = matches.next() {
            for capture in m.captures {
                if capture.index == call_idx {
                    let called_name = capture
                        .node
                        .utf8_text(content.as_bytes())
                        .unwrap_or("")
                        .to_string();
                    let call_line = capture.node.start_position().row + 1;

                    // Find which definition contains this call
                    let caller_idx = file_node_indices.iter().find(|&&n_idx| {
                        if let Some(caller) = code_graph.graph.node_weight(n_idx) {
                            call_line >= caller.start_line && call_line <= caller.end_line
                        } else {
                            false
                        }
                    });

                    if let Some(&caller) = caller_idx {
                        if let Some(target_indices) = code_graph.symbol_lookup.get(&called_name) {
                            for &callee in target_indices {
                                if caller != callee {
                                    code_graph.graph.add_edge(
                                        caller,
                                        callee,
                                        RelationEdge {
                                            kind: RelationKind::Calls,
                                            call_site_line: call_line,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
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
