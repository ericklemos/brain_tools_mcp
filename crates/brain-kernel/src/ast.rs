use crate::error::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tree_sitter::{Node, Point, Tree};

use crate::parse_cache::{ParseCache, ParseWorker};

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

    /// The tree-sitter grammar this language is parsed with.
    pub fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Kotlin => "Kotlin",
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

#[derive(Default, Clone)]
pub struct AstChunker {
    cache: Option<Arc<ParseCache>>,
}

impl AstChunker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cache(cache: Arc<ParseCache>) -> Self {
        Self { cache: Some(cache) }
    }

    pub fn extract_snippets(
        &self,
        file_path: &Path,
        content: &str,
        matched_lines: &[usize],
    ) -> Result<Vec<AstSnippet>> {
        self.extract_snippets_limited(file_path, content, matched_lines, None)
    }

    /// Same as [`Self::extract_snippets`], with an optional per-symbol line cap.
    ///
    /// `brain_read` passes `None`: the full span is the product. `brain_search`
    /// passes a cap because a result set is a summary, and the marker points the
    /// agent at the tool that has the whole body.
    pub fn extract_snippets_limited(
        &self,
        file_path: &Path,
        content: &str,
        matched_lines: &[usize],
        max_lines: Option<usize>,
    ) -> Result<Vec<AstSnippet>> {
        let mut worker = ParseWorker::new();
        self.extract_snippets_limited_with(
            &mut worker,
            file_path,
            content,
            matched_lines,
            max_lines,
        )
    }

    /// Same as [`Self::extract_snippets`], but reuses the parser owned by
    /// `worker` instead of building one per file. One worker per thread:
    /// tree-sitter parsers are not `Sync`.
    pub fn extract_snippets_with(
        &self,
        worker: &mut ParseWorker,
        file_path: &Path,
        content: &str,
        matched_lines: &[usize],
    ) -> Result<Vec<AstSnippet>> {
        self.extract_snippets_limited_with(worker, file_path, content, matched_lines, None)
    }

    /// Shared implementation; `max_lines: None` returns the whole symbol.
    pub fn extract_snippets_limited_with(
        &self,
        worker: &mut ParseWorker,
        file_path: &Path,
        content: &str,
        matched_lines: &[usize],
        max_lines: Option<usize>,
    ) -> Result<Vec<AstSnippet>> {
        let lang = match SupportedLanguage::from_path(file_path) {
            Some(l) => l,
            None => return Ok(Vec::new()),
        };

        if matched_lines.is_empty() {
            return Ok(Vec::new());
        }

        let tree = self.tree_for(worker, file_path, content, lang)?;
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
                    let code = match max_lines {
                        Some(limit) if lines.len() > limit => format!(
                            "{}\n// ... (+{} linhas; peça o corpo inteiro com brain_read)",
                            lines[..limit].join("\n"),
                            lines.len() - limit
                        ),
                        _ => raw_code.to_string(),
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

    /// Cache lookup keyed by content digest. A changed file is a miss, never
    /// a stale answer; a miss parses and stores the canonical tree.
    fn tree_for(
        &self,
        worker: &mut ParseWorker,
        file_path: &Path,
        content: &str,
        lang: SupportedLanguage,
    ) -> Result<Arc<Tree>> {
        let digest = self
            .cache
            .as_ref()
            .map(|_| blake3::hash(content.as_bytes()));
        let cached = match (&self.cache, digest) {
            (Some(cache), Some(digest)) => cache.get(file_path, digest),
            _ => None,
        };
        if let Some(tree) = cached {
            return Ok(tree);
        }

        let tree = Arc::new(worker.parse(lang, content)?);
        if let (Some(cache), Some(digest)) = (&self.cache, digest) {
            cache.insert(file_path, digest, tree.clone());
        }
        Ok(tree)
    }

    fn find_enclosing_scope<'a>(
        &self,
        mut node: Node<'a>,
        lang: SupportedLanguage,
    ) -> Option<Node<'a>> {
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

            node = node.parent()?;
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
