use brain_engine::ValidationClient;
use brain_kernel::ast::AstChunker;
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::scanner::LexicalScanner;
use brain_kernel::budget::{self, Section};
use brain_kernel::store::ResponseStore;
use std::sync::Arc;

use crate::schema::brain_search_tool;

#[derive(Debug, Deserialize)]
pub struct BrainSearchParams {
    pub natural_query: String,
    pub keywords: Vec<String>,
    pub max_results: Option<usize>,
    pub min_confidence: Option<f32>,
    pub budget_tokens: Option<usize>,
}

pub struct SearchHandler {
    root_dir: PathBuf,
    scanner: LexicalScanner,
    chunker: AstChunker,
    validator: ValidationClient,
    default_max_results: usize,
    default_min_confidence: f32,
    store: Option<Arc<ResponseStore>>,
    default_budget_tokens: usize,
}

impl SearchHandler {
    /// Opts into the response contract: truncation with a recoverable handle.
    ///
    /// Kept as a builder rather than a constructor parameter so existing
    /// callers keep the old "render everything" behaviour until they opt in.
    pub fn with_budget(mut self, store: Arc<ResponseStore>, default_budget_tokens: usize) -> Self {
        self.store = Some(store);
        self.default_budget_tokens = default_budget_tokens;
        self
    }
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
            store: None,
            default_budget_tokens: 2000,
        }
    }

    /// Handler for unit tests: no GGUF, so `ValidationClient` falls back to
    /// lexical ranking and the test needs no GPU.
    pub fn for_test() -> Self {
        Self::new(".", ValidationClient::new(None, 0), 5, 0.65)
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

        // Ask the validator for every approved block and cut here instead. The
        // validator used to truncate to `max_results` itself, which made a
        // capped response indistinguishable from a complete one — the agent had
        // no way to tell, so it went to the file to find out.
        let candidate_count = candidates.len();
        let outcome = match self
            .validator
            .validate_with_outcome(
                params.natural_query.clone(),
                params.keywords.clone(),
                candidates,
                min_confidence,
                candidate_count.max(1),
            )
            .await
        {
            Ok(o) => o,
            Err(e) => return CallToolResult::error(format!("Validation failed: {e}")),
        };

        if outcome.scored.is_empty() {
            return CallToolResult::text(format!(
                "Nenhum bloco de código atingiu a confiança mínima exigida ({:.2}).",
                min_confidence
            ));
        }

        let approved_total = outcome.scored.len();
        let scored: Vec<_> = outcome
            .scored
            .into_iter()
            .take(max_results.max(1))
            .collect();

        // What the response can honestly promise the agent: every section is the
        // verbatim body of its symbol, and whether anything was left out — and
        // why. Both are stated up front so re-opening the file is a choice, not
        // a doubt.
        let completeness = completeness_line(
            candidate_count,
            approved_total,
            scored.len(),
            max_results,
            outcome.considered,
            outcome.skipped,
            outcome.lexical_fallback,
        );

        let mut sections = Vec::with_capacity(scored.len() + 1);

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

            let heading = format!(
                "#### {}. `{}` (`{}:{}-{}`)",
                idx + 1,
                item.snippet.symbol_name,
                relative_path,
                item.snippet.start_line,
                item.snippet.end_line,
            );

            let mut body = String::new();
            if !item.snippet.signature.is_empty() {
                body.push_str(&format!("**Assinatura:** `{}`\n\n", item.snippet.signature));
            }
            body.push_str(&format!("```{}\n{}\n```\n\n", lang, item.snippet.code));

            sections.push(Section {
                heading,
                body,
                // Higher confidence survives truncation.
                priority: (item.score * 200.0) as u8,
            });
        }

        if self.store.is_none() {
            // No store configured: render everything, as before this contract.
            let mut out = format!(
                "### Resultados da Busca (`{}`)\n{}\n\n> Trecho verbatim do arquivo, corpo completo do símbolo.\n\n",
                params.natural_query, completeness
            );
            for section in &sections {
                out.push_str(&section.heading);
                out.push('\n');
                out.push_str(&section.body);
            }
            return CallToolResult::text(out);
        }

        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(
                self.store
                    .as_ref()
                    .expect("checked above")
                    .insert(selection.omitted_body()),
            )
        };

        let mut out = format!(
            "### Resultados da Busca (`{}`)\n{}\n\n> Trecho verbatim do arquivo, corpo completo do símbolo.\n\n",
            params.natural_query, completeness
        );
        out.push_str(&budget::render(&selection, handle.as_deref()));
        CallToolResult::text(out)
    }
}

/// One line the agent can trust: what the response contains, what was left
/// out, and which of those decisions the semantic model actually made.
///
/// `candidate_count` is everything the lexical scan produced; `considered` is
/// what survived the validation cap; `skipped` is what the model failed to
/// decode. Collapsing those into "N blocos aprovados" is what let a lexical
/// fallback pass for a semantic verdict.
fn completeness_line(
    candidate_count: usize,
    approved_total: usize,
    shown: usize,
    max_results: usize,
    considered: usize,
    skipped: usize,
    lexical_fallback: bool,
) -> String {
    let verdict = if approved_total > shown {
        // "aprovados" would claim a semantic verdict that never happened when
        // the lexical ranker did the deciding.
        let noun = if lexical_fallback {
            "resultados"
        } else {
            "blocos aprovados"
        };
        format!(
            "{} de {} {} — o teto max_results={} cortou o resto; aumente max_results para ver mais.",
            shown, approved_total, noun, max_results
        )
    } else if shown < considered {
        format!(
            "{} de {} candidatas casaram com a query — as demais não são o código pedido.",
            shown, considered
        )
    } else {
        format!("Todas as {} candidatas casaram com a query.", shown)
    };

    let mut line = format!("#### {verdict}");

    if skipped > 0 {
        line.push_str(&format!(
            " {} de {} candidatas não puderam ser julgadas pelo modelo.",
            skipped, considered
        ));
    }

    if considered < candidate_count {
        line.push_str(&format!(
            " {} de {} candidatas avaliadas — o teto de validação cortou o resto.",
            considered, candidate_count
        ));
    }

    if lexical_fallback {
        line.push_str(" Validação semântica indisponível — ranking lexical.");
    }

    line
}

impl ToolPlugin for SearchHandler {
    fn name(&self) -> &'static str {
        "brain_search"
    }

    fn schema(&self) -> Tool {
        brain_search_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_brain_search(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_kernel::plugin::ToolPlugin;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn search_plugin_announces_its_name_and_schema() {
        let p = SearchHandler::for_test();
        assert_eq!(p.name(), "brain_search");
        assert_eq!(p.schema().name, "brain_search");
    }

#[tokio::test]
async fn search_honours_budget_and_leaves_the_rest_in_the_store() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Two large functions so a small budget cannot hold both.
    for name in ["alpha_handler", "beta_handler"] {
        let body = format!(
            "pub fn {name}(input: &str) -> String {{\n{}\n    input.to_string()\n}}\n",
            "    let _ = input.len();\n".repeat(40)
        );
        fs::write(src.join(format!("{name}.rs")), body).unwrap();
    }

    let validator = ValidationClient::new(None, 0);
    let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
    let handler = SearchHandler::new(root, validator, 5, 0.0)
        .with_budget(store.clone(), 200);

    let out = handler
        .handle_brain_search(serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"]
        }))
        .await;

    let text = &out.content[0].text;
    assert!(text.contains("Resposta truncada"), "expected truncation: {text}");
    assert!(
        text.contains("brain_peek"),
        "envelope must say how to recover: {text}"
    );

    let handle = text
        .split("handle: `")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .expect("handle in envelope");
    assert!(store.get(handle).is_some(), "handle must resolve in the store");
}

#[tokio::test]
async fn response_declares_completeness_and_carries_no_fake_confidence() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("auth.rs"),
        "pub fn verify_jwt_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n",
    )
    .unwrap();

    let validator = ValidationClient::new(None, 0);
    let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
    let handler =
        SearchHandler::new(root, validator, 5, 0.0).with_budget(store, 100_000);

    let out = handler
        .handle_brain_search(serde_json::json!({
            "natural_query": "jwt token",
            "keywords": ["verify_jwt_token"]
        }))
        .await;

    let text = &out.content[0].text;
    // The completeness claim is what licenses the agent to stop verifying.
    assert!(text.contains("candidatas casaram com a query"), "{text}");
    assert!(text.contains("Trecho verbatim"), "{text}");
    // The number is a relative rank, not a probability: it must not read as one.
    assert!(!text.contains("Confiança:"), "relative rank leaked as confidence: {text}");
}

#[tokio::test]
async fn a_max_results_cut_is_declared_instead_of_looking_complete() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    for name in ["handler_a", "handler_b", "handler_c", "handler_d"] {
        fs::write(
            src.join(format!("{name}.rs")),
            format!("pub fn {name}() -> u8 {{ 1 }}\n"),
        )
        .unwrap();
    }

    let validator = ValidationClient::new(None, 0);
    let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
    let handler = SearchHandler::new(root, validator, 2, 0.0).with_budget(store, 100_000);

    let out = handler
        .handle_brain_search(serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"]
        }))
        .await;

    let text = &out.content[0].text;
    assert!(
        text.contains("teto max_results=2 cortou o resto"),
        "a capped response must say so: {text}"
    );
}

#[tokio::test]
async fn search_with_a_generous_budget_is_unchanged() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("auth.rs"),
        "pub fn verify_jwt_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n",
    )
    .unwrap();

    let validator = ValidationClient::new(None, 0);
    let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
    let handler = SearchHandler::new(root, validator, 5, 0.0).with_budget(store, 100_000);

    let out = handler
        .handle_brain_search(serde_json::json!({
            "natural_query": "jwt token",
            "keywords": ["verify_jwt_token"]
        }))
        .await;

    let text = &out.content[0].text;
    assert!(!text.contains("Resposta truncada"));
    assert!(text.contains("verify_jwt_token"));
    assert!(text.contains("pub fn verify_jwt_token"));
}

#[test]
fn a_lexical_fallback_is_announced_instead_of_pretending_validation_happened() {
    let text = completeness_line(1, 1, 1, 5, 1, 0, true);
    assert!(
        text.contains("Validação semântica indisponível"),
        "a lexical decision must not read as semantic validation: {text}"
    );
    assert!(text.contains("candidatas casaram com a query"));
}

#[test]
fn partially_skipped_candidates_are_declared() {
    let text = completeness_line(5, 3, 3, 5, 5, 2, false);
    assert!(
        text.contains("2 de 5 candidatas não puderam ser julgadas"),
        "skipped candidates must be visible: {text}"
    );
    assert!(text.contains("3 de 5 candidatas casaram com a query"));
}

#[test]
fn the_validation_cap_is_declared_separately_from_max_results() {
    let text = completeness_line(100, 5, 5, 5, 24, 0, false);
    assert!(
        text.contains("24 de 100 candidatas avaliadas"),
        "the validation cap must not look like a model judgement: {text}"
    );
    assert!(text.contains("5 de 24 candidatas casaram com a query"));
}
}
