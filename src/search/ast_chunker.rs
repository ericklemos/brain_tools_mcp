use crate::error::{BrainError, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Point};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Kotlin,
}

impl SupportedLanguage {
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs") => Some(Self::Rust),
            Some("kt") | Some("kts") => Some(Self::Kotlin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstSnippet {
    pub file_path: PathBuf,
    pub symbol_name: String,
    pub signature: String,
    pub code: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Default)]
pub struct AstChunker;

impl AstChunker {
    pub fn new() -> Self {
        Self
    }

    pub fn extract_snippets(
        &self,
        file_path: &Path,
        content: &str,
        matched_lines: &[usize],
    ) -> Result<Vec<AstSnippet>> {
        let lang = match SupportedLanguage::from_path(file_path) {
            Some(l) => l,
            None => return Ok(Vec::new()),
        };

        let mut parser = Parser::new();
        match lang {
            SupportedLanguage::Rust => {
                parser
                    .set_language(&tree_sitter_rust::LANGUAGE.into())
                    .map_err(|e| BrainError::Ast(format!("Rust grammar load error: {e}")))?;
            }
            SupportedLanguage::Kotlin => {
                parser
                    .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                    .map_err(|e| BrainError::Ast(format!("Kotlin grammar load error: {e}")))?;
            }
        }

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| BrainError::Ast("Failed to parse AST".to_string()))?;
        let root = tree.root_node();
        let content_bytes = content.as_bytes();

        let mut snippets = Vec::new();
        let mut visited_ranges = HashSet::new();

        for &line in matched_lines {
            if line == 0 {
                continue;
            }
            let row = line - 1;
            let point = Point { row, column: 0 };
            let target_node = root.descendant_for_point_range(point, point);

            if let Some(leaf) = target_node {
                if let Some(scope_node) = self.find_enclosing_scope(leaf, lang) {
                    let range_key = (scope_node.start_byte(), scope_node.end_byte());
                    if !visited_ranges.insert(range_key) {
                        continue;
                    }

                    let symbol_name = self.extract_symbol_name(scope_node, content_bytes, lang);
                    let signature = self.extract_signature(scope_node, content_bytes, lang);
                    let raw_code = &content[scope_node.start_byte()..scope_node.end_byte()];

                    let lines: Vec<&str> = raw_code.lines().collect();
                    let code = if lines.len() > 80 {
                        let truncated = lines[..80].join("\n");
                        format!("{}\n// ... (truncated {} lines)", truncated, lines.len() - 80)
                    } else {
                        raw_code.to_string()
                    };

                    snippets.push(AstSnippet {
                        file_path: file_path.to_path_buf(),
                        symbol_name,
                        signature,
                        code,
                        start_line: scope_node.start_position().row + 1,
                        end_line: scope_node.end_position().row + 1,
                    });
                }
            }
        }

        Ok(snippets)
    }

    fn find_enclosing_scope<'a>(&self, mut node: Node<'a>, lang: SupportedLanguage) -> Option<Node<'a>> {
        loop {
            let kind = node.kind();
            let is_scope = match lang {
                SupportedLanguage::Rust => matches!(
                    kind,
                    "function_item"
                        | "impl_item"
                        | "struct_item"
                        | "enum_item"
                        | "trait_item"
                        | "mod_item"
                ),
                SupportedLanguage::Kotlin => matches!(
                    kind,
                    "function_declaration"
                        | "class_declaration"
                        | "object_declaration"
                        | "property_declaration"
                        | "secondary_constructor"
                ),
            };

            if is_scope {
                return Some(node);
            }

            match node.parent() {
                Some(parent) => node = parent,
                None => return None,
            }
        }
    }

    fn extract_symbol_name(&self, node: Node, source: &[u8], _lang: SupportedLanguage) -> String {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = name_node.utf8_text(source).unwrap_or("");
            if !name.is_empty() {
                return name.to_string();
            }
        }
        if node.kind() == "impl_item" {
            if let Some(type_node) = node.child_by_field_name("type") {
                return format!("impl {}", type_node.utf8_text(source).unwrap_or(""));
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == "identifier"
                    || child.kind() == "simple_identifier"
                    || child.kind() == "type_identifier"
                {
                    let name = child.utf8_text(source).unwrap_or("");
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
        node.kind().to_string()
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
                } else if let Some(idx) = text.find(';') {
                    text[..=idx].trim().to_string()
                } else {
                    text.lines().next().unwrap_or("").trim().to_string()
                }
            }
            SupportedLanguage::Kotlin => {
                if let Some(idx) = text.find('{') {
                    text[..idx].trim().to_string()
                } else if let Some(idx) = text.find('=') {
                    text[..idx].trim().to_string()
                } else {
                    text.lines().next().unwrap_or("").trim().to_string()
                }
            }
        }
    }
}
