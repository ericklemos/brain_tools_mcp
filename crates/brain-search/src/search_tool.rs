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
use brain_kernel::deliveries::{self, Delivery, DeliveryLedger, Verdict};
use brain_kernel::store::ResponseStore;
use brain_kernel::text::compact_signature;
use std::collections::HashSet;
use std::sync::Arc;

use crate::schema::brain_search_tool;

#[derive(Debug, Deserialize)]
pub struct BrainSearchParams {
    pub natural_query: String,
    pub keywords: Vec<String>,
    pub max_results: Option<usize>,
    pub min_confidence: Option<f32>,
    pub budget_tokens: Option<usize>,
    /// `"allow"` asks for bodies even if this session already received them.
    pub repeat: Option<String>,
    /// `auto` sends the body of the first results and signatures for the rest;
    /// `full` and `signatures` force one behaviour for every result.
    pub detail: Option<SearchDetail>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchDetail {
    #[default]
    Auto,
    Full,
    Signatures,
}

/// How many full bodies `detail: auto` may send before switching to
/// signatures. Two covers "top hit plus its neighbour" without paying for a
/// whole result set the agent has not chosen yet.
const MAX_FULL_BODIES_IN_AUTO: usize = 2;

/// A search result is a summary, not the whole symbol. The cap keeps one huge
/// function from crowding out the rest of the result set; the marker tells the
/// agent to ask `brain_read` for the full body, which has no cap.
const MAX_SEARCH_BODY_LINES: usize = 80;

pub struct SearchHandler {
    root_dir: PathBuf,
    scanner: LexicalScanner,
    chunker: AstChunker,
    validator: ValidationClient,
    default_max_results: usize,
    default_min_confidence: f32,
    store: Option<Arc<ResponseStore>>,
    default_budget_tokens: usize,
    ledger: Option<Arc<DeliveryLedger>>,
}

/// A result section plus what to tell the ledger once it is decided whether
/// the section entered the response.
struct Rendered {
    section: Section,
    pending: Option<Pending>,
}

struct Pending {
    path: PathBuf,
    id: String,
    file_hash: blake3::Hash,
    text: String,
    complete: bool,
    omitted: bool,
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

    /// Opts into the delivery ledger: a snippet this session already received
    /// is pointed at instead of re-sent while its file has not changed.
    pub fn with_deliveries(mut self, ledger: Option<Arc<DeliveryLedger>>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Shares the graph's parse trees; repeated searches become digest hits.
    pub fn with_parse_cache(mut self, cache: Arc<brain_kernel::parse_cache::ParseCache>) -> Self {
        self.chunker = AstChunker::with_cache(cache);
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
            ledger: None,
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
            Err(e) => {
                return CallToolResult::error(format!("Invalid arguments for brain_search: {e}"))
            }
        };

        let max_results = params.max_results.unwrap_or(self.default_max_results);
        let min_confidence = params.min_confidence.unwrap_or(self.default_min_confidence);
        let detail = params.detail.unwrap_or_default();
        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);

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
            file_matches
                .entry(m.file_path)
                .or_default()
                .push(m.line_number);
        }

        // Reading and tree-sitter parsing are blocking and independent per
        // file: they run on a bounded worker set inside `spawn_blocking` so the
        // async reactor is never stalled. The shared parse cache turns files
        // the graph already parsed into digest hits instead of second parses.
        let mut ordered_files: Vec<(PathBuf, Vec<usize>)> = file_matches.into_iter().collect();
        ordered_files.sort_by(|a, b| a.0.cmp(&b.0));

        let chunker = self.chunker.clone();
        let candidates = match tokio::task::spawn_blocking(move || {
            if ordered_files.is_empty() {
                return Vec::new();
            }

            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(ordered_files.len());
            let next = std::sync::atomic::AtomicUsize::new(0);
            let mut buckets: Vec<Vec<brain_kernel::ast::AstSnippet>> =
                Vec::with_capacity(workers);

            // One queue, not fixed chunks: a single huge file cannot become
            // the tail while other workers idle.
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(workers);
                for _ in 0..workers {
                    let chunker = &chunker;
                    let ordered_files = &ordered_files;
                    let next = &next;
                    handles.push(scope.spawn(move || {
                        // One parser per worker: `set_language` is not free.
                        let mut worker = brain_kernel::parse_cache::ParseWorker::new();
                        let mut snippets = Vec::new();
                        loop {
                            let index =
                                next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((file_path, lines)) = ordered_files.get(index) else {
                                break;
                            };
                            let content = match std::fs::read_to_string(file_path) {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::debug!(path = %file_path.display(), error = %e, "Could not read file for AST parsing");
                                    continue;
                                }
                            };

                            match chunker.extract_snippets_limited_with(
                                &mut worker,
                                file_path,
                                &content,
                                lines,
                                Some(MAX_SEARCH_BODY_LINES),
                            ) {
                                Ok(mut found) => snippets.append(&mut found),
                                Err(e) => {
                                    tracing::debug!(path = %file_path.display(), error = %e, "AST snippet extraction error");
                                }
                            }
                        }
                        snippets
                    }));
                }
                for handle in handles {
                    buckets.push(handle.join().unwrap_or_default());
                }
            });

            let mut snippets: Vec<brain_kernel::ast::AstSnippet> =
                buckets.into_iter().flatten().collect();
            // Worker completion order is not deterministic; the response must
            // be, and ranking keeps ties in input order.
            snippets.sort_by(|a, b| {
                a.file_path
                    .cmp(&b.file_path)
                    .then(a.start_line.cmp(&b.start_line))
            });
            snippets
        })
        .await
        {
            Ok(snippets) => snippets,
            Err(e) => {
                return CallToolResult::error(format!("AST extraction failed: {e}"));
            }
        };

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

        let repeat_allowed = params.repeat.as_deref() == Some("allow");
        let mut rendered: Vec<Rendered> = Vec::with_capacity(scored.len() + 1);
        // One read per file, however many snippets it contributed.
        let mut contents: HashMap<PathBuf, String> = HashMap::new();
        let mut full_bodies = 0usize;
        let mut full_tokens = 0usize;
        let mut signature_only = 0usize;

        for (idx, item) in scored.iter().enumerate() {
            let snippet = &item.snippet;
            let relative_path = snippet
                .file_path
                .strip_prefix(&self.root_dir)
                .unwrap_or(&snippet.file_path)
                .display();

            let lang = if snippet.file_path.extension().and_then(|e| e.to_str()) == Some("kt") {
                "kotlin"
            } else {
                "rust"
            };

            let heading = format!(
                "#### {}. `{}` (`{}:{}-{}`)",
                idx + 1,
                snippet.symbol_name,
                relative_path,
                snippet.start_line,
                snippet.end_line,
            );

            let code_tokens = budget::estimate_tokens(&snippet.code);
            let wants_full = match detail {
                SearchDetail::Full => true,
                SearchDetail::Signatures => false,
                SearchDetail::Auto => {
                    full_bodies < MAX_FULL_BODIES_IN_AUTO
                        && (full_bodies == 0 || full_tokens + code_tokens <= budget_tokens * 2 / 3)
                }
            };

            if !wants_full {
                signature_only += 1;
                let mut body = String::new();
                let signature = compact_signature(&snippet.signature);
                if !signature.is_empty() {
                    body.push_str(&format!("`{signature}`\n\n"));
                }
                rendered.push(Rendered {
                    section: Section {
                        heading,
                        body,
                        priority: (item.score * 200.0) as u8,
                    },
                    pending: None,
                });
                continue;
            }

            full_bodies += 1;
            full_tokens += code_tokens + budget::estimate_tokens(&heading);

            let mut body = String::new();
            let signature = snippet.signature.trim();
            // The signature is the opening of the code block; repeating it
            // above the body pays twice for the same text.
            if !signature.is_empty() && !snippet.code.trim_start().starts_with(signature) {
                body.push_str(&format!("`{}`\n\n", compact_signature(signature)));
            }
            body.push_str(&format!("```{}\n{}\n```\n\n", lang, snippet.code));

            let pending = self.apply_ledger(snippet, &mut contents, &mut body, repeat_allowed);

            rendered.push(Rendered {
                section: Section {
                    heading,
                    body,
                    // Higher confidence survives truncation.
                    priority: (item.score * 200.0) as u8,
                },
                pending,
            });
        }

        let sections: Vec<Section> = rendered
            .iter()
            .map(|rendered| rendered.section.clone())
            .collect();

        let signature_note = if signature_only > 0 {
            format!("> {signature_only} resultado(s) só com assinatura.\n\n")
        } else {
            String::new()
        };

        if self.store.is_none() {
            // No store configured: render everything, as before this contract.
            let mut out = format!(
                "### Resultados da Busca\n{}\n\n{}",
                completeness, signature_note
            );
            for section in &sections {
                out.push_str(&section.heading);
                out.push('\n');
                out.push_str(&section.body);
            }
            return CallToolResult::text(out);
        }

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

        self.settle(&rendered, &selection);

        let mut out = format!(
            "### Resultados da Busca\n{}\n\n{}",
            completeness, signature_note
        );
        out.push_str(&budget::render(&selection, handle.as_deref()));
        CallToolResult::text(out)
    }

    /// Replaces the body with a pointer or a diff when the ledger already has
    /// this snippet, and reports what to record once the section is included.
    fn apply_ledger(
        &self,
        snippet: &brain_kernel::ast::AstSnippet,
        contents: &mut HashMap<PathBuf, String>,
        body: &mut String,
        repeat_allowed: bool,
    ) -> Option<Pending> {
        let ledger = self.ledger.as_ref()?;
        let content = contents
            .entry(snippet.file_path.clone())
            .or_insert_with(|| std::fs::read_to_string(&snippet.file_path).unwrap_or_default());
        if content.is_empty() {
            return None;
        }

        let range = deliveries::line_span(content, snippet.start_line, snippet.end_line)?;
        let span_text = content.get(range).unwrap_or("");
        let complete = span_text.trim() == snippet.code.trim();
        let file_hash = deliveries::content_hash(content);
        // The line is part of the identity here: a snippet has no container,
        // and two `new`s in one file would otherwise share a pointer.
        let id = format!("{}@{}", snippet.symbol_name, snippet.start_line);

        let verdict = if repeat_allowed {
            Verdict::Fresh
        } else {
            ledger.check(
                &snippet.file_path,
                &id,
                &file_hash,
                span_text,
                snippet.start_line,
            )
        };

        let draft = Pending {
            path: snippet.file_path.clone(),
            id: id.clone(),
            file_hash,
            text: span_text.to_string(),
            complete,
            omitted: false,
        };

        match verdict {
            Verdict::Seen(seen) => {
                let store = self.store.as_ref()?;
                let handle = deliveries::live_handle(store, ledger, &seen, &snippet.file_path, &id);
                *body = deliveries::pointer_body(&seen, &handle);
                Some(Pending {
                    omitted: true,
                    ..draft
                })
            }
            Verdict::Changed(changed) => {
                *body = deliveries::changed_body(&changed);
                Some(draft)
            }
            Verdict::Full { reason, .. } => {
                *body = format!("{}\n{}", deliveries::full_note(reason), body);
                Some(draft)
            }
            Verdict::Fresh => Some(draft),
        }
    }

    /// Records only what entered the response: a snippet cut by the budget was
    /// not seen, so it must not be remembered as delivered.
    fn settle(&self, rendered: &[Rendered], selection: &budget::Selection) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let included: HashSet<&str> = selection
            .included
            .iter()
            .map(|section| section.heading.as_str())
            .collect();

        for rendered in rendered {
            let Some(pending) = &rendered.pending else {
                continue;
            };
            if !included.contains(rendered.section.heading.as_str()) {
                continue;
            }
            if pending.omitted {
                ledger.note_omitted(&pending.path, &pending.id);
            } else {
                ledger.record(Delivery {
                    tool: "brain_search",
                    path: &pending.path,
                    id: &pending.id,
                    file_hash: pending.file_hash,
                    text: &pending.text,
                    complete: pending.complete,
                    handle: None,
                });
            }
        }
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
            "{} de {} {} — max_results={} cortou o resto.",
            shown, approved_total, noun, max_results
        )
    } else if shown < considered {
        format!(
            "{} de {} candidatas casaram com a query.",
            shown, considered
        )
    } else {
        format!("Todas as {} candidatas casaram com a query.", shown)
    };

    let mut line = format!("#### {verdict}");

    if skipped > 0 {
        line.push_str(&format!(" {skipped} de {considered} não julgadas."));
    }

    if considered < candidate_count {
        line.push_str(&format!(
            " {considered} de {candidate_count} avaliadas (teto)."
        ));
    }

    if lexical_fallback {
        line.push_str(" lexical.");
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
        let handler = SearchHandler::new(root, validator, 5, 0.0).with_budget(store.clone(), 200);

        let out = handler
            .handle_brain_search(serde_json::json!({
                "natural_query": "handler",
                "keywords": ["handler"]
            }))
            .await;

        let text = &out.content[0].text;
        assert!(
            text.contains("Resposta truncada"),
            "expected truncation: {text}"
        );
        assert!(
            text.contains("brain_peek"),
            "envelope must say how to recover: {text}"
        );

        let handle = text
            .split("handle: `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("handle in envelope");
        assert!(
            store.get(handle).is_some(),
            "handle must resolve in the store"
        );
    }

    #[tokio::test]
    async fn automatic_detail_switches_to_signatures_after_two_bodies() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for name in ["handler_a", "handler_b", "handler_c"] {
            fs::write(
                root.join(format!("{name}.rs")),
                format!("pub fn {name}() -> u8 {{\n    1\n}}\n"),
            )
            .unwrap();
        }

        let validator = ValidationClient::new(None, 0);
        let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
        let handler = SearchHandler::new(root, validator, 5, 0.0).with_budget(store, 100_000);

        let search = |args: serde_json::Value| {
            let handler = &handler;
            async move {
                handler.handle_brain_search(args).await.content[0]
                    .text
                    .clone()
            }
        };

        let auto = search(serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"]
        }))
        .await;
        assert_eq!(auto.matches("```rust").count(), 2, "{auto}");
        assert!(auto.contains("1 resultado(s) só com assinatura"), "{auto}");
        assert!(
            auto.contains("pub fn handler_"),
            "signatures remain: {auto}"
        );

        let signatures = search(serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"],
            "detail": "signatures"
        }))
        .await;
        assert!(!signatures.contains("```rust"), "{signatures}");
        assert!(
            signatures.contains("3 resultado(s) só com assinatura"),
            "{signatures}"
        );

        let full = search(serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"],
            "detail": "full",
            "repeat": "allow"
        }))
        .await;
        assert_eq!(full.matches("```rust").count(), 3, "{full}");
        assert!(!full.contains("só com assinatura"), "{full}");
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
        let handler = SearchHandler::new(root, validator, 5, 0.0).with_budget(store, 100_000);

        let out = handler
            .handle_brain_search(serde_json::json!({
                "natural_query": "jwt token",
                "keywords": ["verify_jwt_token"]
            }))
            .await;

        let text = &out.content[0].text;
        // The completeness claim is what licenses the agent to stop verifying.
        assert!(text.contains("candidatas casaram com a query"), "{text}");
        assert!(text.contains("```rust"), "the body is verbatim: {text}");
        // The signature is the opening of the code block; it must not be printed twice.
        assert_eq!(
            text.matches("pub fn verify_jwt_token").count(),
            1,
            "signature duplicated above the body: {text}"
        );
        assert!(!text.contains("Assinatura"), "{text}");
        // The number is a relative rank, not a probability: it must not read as one.
        assert!(
            !text.contains("Confiança:"),
            "relative rank leaked as confidence: {text}"
        );
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
            text.contains("max_results=2 cortou o resto"),
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
            text.contains("lexical"),
            "a lexical decision must not read as semantic validation: {text}"
        );
        assert!(text.contains("candidatas casaram com a query"));
    }

    #[test]
    fn partially_skipped_candidates_are_declared() {
        let text = completeness_line(5, 3, 3, 5, 5, 2, false);
        assert!(
            text.contains("2 de 5 não julgadas"),
            "skipped candidates must be visible: {text}"
        );
        assert!(text.contains("3 de 5 candidatas casaram com a query"));
    }

    #[test]
    fn the_validation_cap_is_declared_separately_from_max_results() {
        let text = completeness_line(100, 5, 5, 5, 24, 0, false);
        assert!(
            text.contains("24 de 100 avaliadas (teto)"),
            "the validation cap must not look like a model judgement: {text}"
        );
        assert!(text.contains("5 de 24 candidatas casaram com a query"));
    }

    fn ledger_handler(
        root: &std::path::Path,
        ledger: Arc<DeliveryLedger>,
    ) -> (SearchHandler, Arc<ResponseStore>) {
        let validator = ValidationClient::new(None, 0);
        let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
        let handler = SearchHandler::new(root, validator, 5, 0.0)
            .with_budget(store.clone(), 4000)
            .with_deliveries(Some(ledger));
        (handler, store)
    }

    #[tokio::test]
    async fn the_second_search_of_the_same_snippet_is_a_pointer() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("auth.rs"),
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n",
        )
        .unwrap();

        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (handler, store) = ledger_handler(root, ledger);

        let args =
            || serde_json::json!({ "natural_query": "verify token", "keywords": ["verify_token"] });
        let first = handler.handle_brain_search(args()).await.content[0]
            .text
            .clone();
        assert!(first.contains("!raw.is_empty()"), "{first}");

        let second = handler.handle_brain_search(args()).await.content[0]
            .text
            .clone();
        assert!(second.contains("Já entregue nesta sessão"), "{second}");
        assert!(!second.contains("!raw.is_empty()"), "{second}");

        let handle = second
            .split("`brain_peek` em `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .expect("pointer cites a handle");
        assert!(store.get(handle).is_some());
    }

    #[tokio::test]
    async fn a_changed_file_turns_the_search_hit_into_a_diff() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let file = root.join("auth.rs");
        fs::write(
            &file,
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n",
        )
        .unwrap();

        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (handler, _) = ledger_handler(root, ledger);
        let args =
            || serde_json::json!({ "natural_query": "verify token", "keywords": ["verify_token"] });

        handler.handle_brain_search(args()).await;
        fs::write(
            &file,
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.trim().is_empty()\n}\n",
        )
        .unwrap();

        let second = handler.handle_brain_search(args()).await.content[0]
            .text
            .clone();
        assert!(second.contains("Mudou desde a entrega"), "{second}");
        assert!(second.contains("+    !raw.trim().is_empty()"), "{second}");
    }

    #[tokio::test]
    async fn repeat_allow_returns_the_body_from_search() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("auth.rs"),
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n",
        )
        .unwrap();

        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (handler, _) = ledger_handler(root, ledger);

        handler
            .handle_brain_search(serde_json::json!({ "natural_query": "verify token", "keywords": ["verify_token"] }))
            .await;
        let again = handler
            .handle_brain_search(serde_json::json!({
                "natural_query": "verify token",
                "keywords": ["verify_token"],
                "repeat": "allow"
            }))
            .await
            .content[0]
            .text
            .clone();
        assert!(again.contains("!raw.is_empty()"), "{again}");
        assert!(!again.contains("Já entregue"), "{again}");
    }
}
