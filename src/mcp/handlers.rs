use crate::engine::ValidationClient;
use crate::mcp::protocol::CallToolResult;
use crate::search::ast_chunker::AstChunker;
use crate::search::scanner::LexicalScanner;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct BrainSearchParams {
    pub natural_query: String,
    pub keywords: Vec<String>,
    pub max_results: Option<usize>,
    pub min_confidence: Option<f32>,
}

pub struct SearchHandler {
    root_dir: PathBuf,
    scanner: LexicalScanner,
    chunker: AstChunker,
    validator: ValidationClient,
    default_max_results: usize,
    default_min_confidence: f32,
}

impl SearchHandler {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        validator: ValidationClient,
        default_max_results: usize,
        default_min_confidence: f32,
    ) -> Self {
        let root = root_dir.into();
        Self {
            scanner: LexicalScanner::new(&root),
            chunker: AstChunker::new(),
            root_dir: root,
            validator,
            default_max_results,
            default_min_confidence,
        }
    }

    pub async fn handle_brain_search(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: BrainSearchParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments for brain_search: {e}")),
        };

        let max_results = params.max_results.unwrap_or(self.default_max_results);
        let min_confidence = params.min_confidence.unwrap_or(self.default_min_confidence);

        let keyword_strs: Vec<&str> = params.keywords.iter().map(|s| s.as_str()).collect();

        let matches = match self.scanner.search_keywords(&keyword_strs) {
            Ok(m) => m,
            Err(e) => return CallToolResult::error(format!("Lexical search failed: {e}")),
        };

        if matches.is_empty() {
            return CallToolResult::text(format!(
                "Nenhum resultado léxico encontrado para os termos: {:?}.",
                params.keywords
            ));
        }

        let mut file_matches: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for m in matches {
            file_matches.entry(m.file_path).or_default().push(m.line_number);
        }

        let mut candidates = Vec::new();
        for (file_path, lines) in file_matches {
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(path = %file_path.display(), error = %e, "Could not read file for AST parsing");
                    continue;
                }
            };

            match self.chunker.extract_snippets(&file_path, &content, &lines) {
                Ok(snippets) => candidates.extend(snippets),
                Err(e) => {
                    tracing::debug!(path = %file_path.display(), error = %e, "AST snippet extraction error");
                }
            }
        }

        if candidates.is_empty() {
            return CallToolResult::text("Termos encontrados, porém nenhum bloco de código AST (Rust/Kotlin) pôde ser delimitado.");
        }

        let scored = match self
            .validator
            .validate(
                params.natural_query.clone(),
                params.keywords.clone(),
                candidates,
                min_confidence,
                max_results,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => return CallToolResult::error(format!("Validation failed: {e}")),
        };

        if scored.is_empty() {
            return CallToolResult::text(format!(
                "Nenhum bloco de código atingiu a confiança mínima exigida ({:.2}).",
                min_confidence
            ));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "### Resultados da Busca (`{}`)\n\n",
            params.natural_query
        ));

        for (idx, item) in scored.iter().enumerate() {
            let relative_path = item
                .snippet
                .file_path
                .strip_prefix(&self.root_dir)
                .unwrap_or(&item.snippet.file_path)
                .display();

            let lang = if item.snippet.file_path.extension().and_then(|e| e.to_str()) == Some("kt") {
                "kotlin"
            } else {
                "rust"
            };

            out.push_str(&format!(
                "#### {}. `{}` (`{}:{}-{}`) — Confiança: {:.0}%\n",
                idx + 1,
                item.snippet.symbol_name,
                relative_path,
                item.snippet.start_line,
                item.snippet.end_line,
                item.score * 100.0
            ));

            if !item.snippet.signature.is_empty() {
                out.push_str(&format!("**Assinatura:** `{}`\n\n", item.snippet.signature));
            }

            out.push_str(&format!("```{}\n{}\n```\n\n", lang, item.snippet.code));
        }

        CallToolResult::text(out)
    }
}

pub struct GraphHandler {
    root_dir: PathBuf,
    graph: std::sync::Arc<tokio::sync::RwLock<crate::graph::model::ProjectCodeGraph>>,
}

impl GraphHandler {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: std::sync::Arc<tokio::sync::RwLock<crate::graph::model::ProjectCodeGraph>>,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            graph,
        }
    }

    pub async fn handle_context_graph(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            target_symbol_or_file: String,
            depth: Option<usize>,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let depth = params.depth.unwrap_or(1);
        let lock = self.graph.read().await;
        let markdown = crate::graph::traversal::GraphTraversal::context_graph(
            &lock,
            &params.target_symbol_or_file,
            depth,
            &self.root_dir,
        );
        CallToolResult::text(markdown)
    }

    pub async fn handle_impact_check(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            symbol_name: String,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let markdown = crate::graph::traversal::GraphTraversal::impact_check(
            &lock,
            &params.symbol_name,
            &self.root_dir,
        );
        CallToolResult::text(markdown)
    }
}
