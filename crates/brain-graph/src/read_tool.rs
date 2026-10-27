use brain_kernel::ast::AstChunker;
use brain_kernel::budget::{self, Section};
use brain_kernel::deliveries::{self, Delivery, DeliveryLedger, Seen, Verdict};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::edit_tool::hash_prefix;
use crate::span::SpanResolver;

/// Reads a file as an outline, or one symbol's body, instead of the whole file.
///
/// This is the tool that replaces wholesale `Read` calls: the agent asks for
/// "what is in this file" and gets signatures, or "show me this function" and
/// gets just that function.
pub struct ReadTool {
    root_dir: PathBuf,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
    ledger: Option<Arc<DeliveryLedger>>,
}

#[derive(Debug, Deserialize)]
pub struct ReadParams {
    pub path: String,
    pub symbol: Option<String>,
    pub budget_tokens: Option<usize>,
    /// `"allow"` asks for the body even if this session already received it.
    pub repeat: Option<String>,
}

/// A section plus what to tell the ledger once it is decided whether the
/// section entered the response.
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
    /// `true` when the section is a pointer and the omission should be counted.
    omitted: bool,
}

impl ReadTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            store,
            default_budget_tokens,
            ledger: None,
        }
    }

    /// Opts into the delivery ledger: the same span is pointed at instead of
    /// re-sent while the file has not changed.
    pub fn with_deliveries(mut self, ledger: Option<Arc<DeliveryLedger>>) -> Self {
        self.ledger = ledger;
        self
    }

    pub fn read(&self, params: ReadParams) -> CallToolResult {
        let path = match self.resolve(&params.path) {
            Some(path) => path,
            None => {
                return CallToolResult::error(format!(
                    "Arquivo não encontrado ou fora do diretório raiz: {}",
                    params.path
                ))
            }
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => return CallToolResult::error(format!("Falha ao ler {}: {e}", path.display())),
        };

        let relative = path
            .strip_prefix(&self.root_dir)
            .unwrap_or(&path)
            .display()
            .to_string();

        // The outline is signatures, not bodies: it is never a repeat, so it
        // never consults the ledger either.
        let repeat_allowed = params.repeat.as_deref() == Some("allow");
        let rendered = match &params.symbol {
            Some(symbol) => match self.symbol_sections(
                &path,
                &content,
                &relative,
                symbol,
                !repeat_allowed,
            ) {
                Ok(rendered) if !rendered.is_empty() => rendered,
                Ok(_) => {
                    return CallToolResult::text(format!(
                        "Símbolo `{symbol}` não encontrado em `{relative}`."
                    ))
                }
                Err(e) => return CallToolResult::error(e.to_string()),
            },
            None => self
                .outline_sections(&path, &content, &relative)
                .into_iter()
                .map(|section| Rendered {
                    section,
                    pending: None,
                })
                .collect(),
        };

        let sections: Vec<Section> = rendered
            .iter()
            .map(|rendered| rendered.section.clone())
            .collect();
        let budget = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };

        self.settle(&rendered, &selection);

        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }

    /// Records only what actually entered the response: a section that was cut
    /// by the budget was not seen, so it must not be remembered as delivered.
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
                    tool: "brain_read",
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

    /// A pointer is only useful while its handle is alive. When the old one has
    /// expired the stored text is handed back to the store under a new handle,
    /// so the omission never becomes a dead end.
    fn live_handle(&self, ledger: &DeliveryLedger, seen: &Seen, path: &Path, id: &str) -> String {
        deliveries::live_handle(&self.store, ledger, seen, path, id)
    }

    /// Rejects paths escaping the root: the tool reads files for the agent, it
    /// must not become a way to read the machine.
    fn resolve(&self, requested: &str) -> Option<PathBuf> {
        let candidate = Path::new(requested);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root_dir.join(candidate)
        };
        let canonical = joined.canonicalize().ok()?;
        let root = self.root_dir.canonicalize().ok()?;
        canonical.starts_with(&root).then_some(canonical)
    }

    fn outline_sections(&self, path: &Path, content: &str, relative: &str) -> Vec<Section> {
        let chunker = AstChunker::new();
        // Every line, so the chunker returns every scope instead of only those
        // surrounding a match.
        let all_lines: Vec<usize> = (1..=content.lines().count()).collect();

        let snippets = match chunker.extract_snippets(path, content, &all_lines) {
            Ok(snippets) if !snippets.is_empty() => snippets,
            _ => return vec![whole_file_section(content, relative)],
        };

        let mut sections: Vec<Section> = snippets
            .into_iter()
            .map(|snippet| {
                let heading = format!(
                    "#### `{}` (`{relative}:{}-{}`)",
                    snippet.symbol_name, snippet.start_line, snippet.end_line
                );
                let body = if snippet.signature.is_empty() {
                    String::new()
                } else {
                    format!("`{}`\n", snippet.signature)
                };
                Section {
                    heading,
                    body,
                    priority: 200,
                }
            })
            .collect();

        // The outline leads: it tells the agent what else is in the file.
        sections.insert(
            0,
            Section {
                heading: format!(
                    "### `{relative}` — {} linhas · blake3 `{}`",
                    content.lines().count(),
                    hash_prefix(content.as_bytes())
                ),
                body: String::new(),
                priority: 255,
            },
        );
        sections
    }

    fn symbol_sections(
        &self,
        path: &Path,
        content: &str,
        relative: &str,
        symbol: &str,
        consult_ledger: bool,
    ) -> brain_kernel::Result<Vec<Rendered>> {
        let chunker = AstChunker::new();
        let all_lines: Vec<usize> = (1..=content.lines().count()).collect();
        let snippets = chunker.extract_snippets(path, content, &all_lines)?;

        let lang = if path.extension().and_then(|e| e.to_str()) == Some("kt") {
            "kotlin"
        } else {
            "rust"
        };

        // The chunker reports `end_position().row + 1`, which can point one line
        // past the closing brace. The resolver derives lines from the body bytes.
        let spans = SpanResolver::new()
            .symbols(path, content)
            .unwrap_or_default();
        let hash = hash_prefix(content.as_bytes());
        let file_hash = blake3::hash(content.as_bytes());

        Ok(snippets
            .into_iter()
            .filter(|snippet| snippet.symbol_name == symbol)
            .map(|snippet| {
                // Same name twice in one file is possible; the closest start
                // line is the one this chunk came from.
                let span = spans
                    .iter()
                    .filter(|span| span.name == snippet.symbol_name)
                    .min_by_key(|span| span.start_line.abs_diff(snippet.start_line));
                let start_line = span.map_or(snippet.start_line, |span| span.start_line);
                let end_line = span.map_or(snippet.end_line, |span| span.end_line);
                let heading = format!(
                    "### `{}` (`{relative}:{start_line}-{end_line}`) · blake3 `{hash}`",
                    snippet.symbol_name
                );
                let fence = format!("```{lang}\n{}\n```\n", snippet.code);
                let mut body = fence.clone();
                let mut pending = None;

                if let (Some(ledger), Some(span)) = (&self.ledger, span) {
                    let range = span.head_range.start..span.body_range.end;
                    let span_text = content.get(range).unwrap_or("");
                    let id = match &span.container {
                        Some(container) => format!("{container}::{}", span.name),
                        None => span.name.clone(),
                    };
                    let pending_now = Pending {
                        path: path.to_path_buf(),
                        id: id.clone(),
                        file_hash,
                        // The full span, not the chunk: a diff has to compare
                        // against everything the span holds today.
                        text: span_text.to_string(),
                        complete: span_text.trim_end() == snippet.code.trim_end(),
                        omitted: false,
                    };

                    if !consult_ledger {
                        pending = Some(pending_now);
                    } else {
                        match ledger.check(path, &id, &file_hash, span_text, start_line) {
                            Verdict::Seen(seen) => {
                                let handle = self.live_handle(ledger, &seen, path, &id);
                                body = deliveries::pointer_body(&seen, &handle);
                                pending = Some(Pending {
                                    omitted: true,
                                    ..pending_now
                                });
                            }
                            Verdict::Changed(changed) => {
                                body = deliveries::changed_body(&changed);
                                pending = Some(pending_now);
                            }
                            Verdict::Full { reason, .. } => {
                                body = format!("{}\n{}", deliveries::full_note(reason), fence);
                                pending = Some(pending_now);
                            }
                            Verdict::Fresh => pending = Some(pending_now),
                        }
                    }
                }

                Rendered {
                    section: Section {
                        heading,
                        body,
                        priority: 255,
                    },
                    pending,
                }
            })
            .collect())
    }
}

fn whole_file_section(content: &str, relative: &str) -> Section {
    let lang = if relative.ends_with(".kt") { "kotlin" } else { "rust" };
    Section {
        heading: format!(
            "### `{relative}` — arquivo inteiro · blake3 `{}`",
            hash_prefix(content.as_bytes())
        ),
        body: format!("```{lang}\n{content}\n```\n"),
        priority: 200,
    }
}

impl ToolPlugin for ReadTool {
    fn name(&self) -> &'static str {
        "brain_read"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_read".to_string(),
            description: "Lê um arquivo como esqueleto (assinaturas) ou o corpo de um símbolo específico, dentro de um orçamento de tokens.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Caminho relativo ao diretório raiz do projeto."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Opcional: nome exato da função/struct cujo corpo será devolvido."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto aproximado de tokens; o excedente fica recuperável via brain_peek."
                    },
                    "repeat": {
                        "type": "string",
                        "enum": ["allow"],
                        "description": "`allow` devolve o corpo mesmo se esta sessão já o recebeu; o padrão aponta para o corpo anterior."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: ReadParams = match serde_json::from_value(args) {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { CallToolResult::error(format!("Invalid arguments: {e}")) }),
        };
        Box::pin(async move { self.read(params) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn tool_for(dir: &Path) -> ReadTool {
        ReadTool::new(
            dir,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
        )
    }

    fn tool_with_ledger(
        dir: &Path,
        ledger: Arc<DeliveryLedger>,
    ) -> (ReadTool, Arc<ResponseStore>) {
        let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024));
        let tool = ReadTool::new(dir, store.clone(), 2000).with_deliveries(Some(ledger));
        (tool, store)
    }

    fn read_symbol(tool: &ReadTool, symbol: &str, repeat: Option<&str>) -> String {
        tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: Some(symbol.to_string()),
            budget_tokens: None,
            repeat: repeat.map(str::to_string),
        })
        .content[0]
            .text
            .clone()
    }

    #[test]
    fn the_second_read_of_an_unchanged_symbol_is_a_pointer_with_a_live_handle() {
        let dir = fixture();
        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (tool, store) = tool_with_ledger(dir.path(), ledger);

        let first = read_symbol(&tool, "verify_token", None);
        assert!(first.contains("!raw.is_empty()"), "{first}");

        let second = read_symbol(&tool, "verify_token", None);
        assert!(second.contains("Já entregue nesta sessão"), "{second}");
        assert!(
            !second.contains("!raw.is_empty()"),
            "the body must not come back: {second}"
        );

        let handle = second
            .split("`brain_peek` em `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .expect("pointer cites a handle");
        assert!(
            store.get(handle).is_some_and(|body| body.contains("!raw.is_empty()")),
            "the cited handle must return the body"
        );
    }

    #[test]
    fn a_changed_symbol_returns_only_the_diff() {
        let dir = fixture();
        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (tool, _) = tool_with_ledger(dir.path(), ledger);

        read_symbol(&tool, "verify_token", None);
        std::fs::write(
            dir.path().join("auth.rs"),
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.trim().is_empty()\n}\n\npub fn logout() {\n}\n",
        )
        .unwrap();

        let second = read_symbol(&tool, "verify_token", None);
        assert!(second.contains("Mudou desde a entrega"), "{second}");
        assert!(second.contains("+    !raw.trim().is_empty()"), "{second}");
        assert!(second.contains("-    !raw.is_empty()"), "{second}");
        assert!(
            second.contains(" pub fn verify_token"),
            "one line of context, marked as unchanged: {second}"
        );
    }

    #[test]
    fn repeat_allow_returns_the_body_and_registers_it_again() {
        let dir = fixture();
        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (tool, _) = tool_with_ledger(dir.path(), ledger);

        read_symbol(&tool, "verify_token", None);
        let again = read_symbol(&tool, "verify_token", Some("allow"));
        assert!(again.contains("!raw.is_empty()"), "{again}");
        assert!(!again.contains("Já entregue"), "{again}");

        // Back to the default: the body was registered again, so this is a pointer.
        let third = read_symbol(&tool, "verify_token", None);
        assert!(third.contains("Já entregue"), "{third}");
    }

    #[test]
    fn the_outline_is_never_a_repeat() {
        let dir = fixture();
        let ledger = Arc::new(DeliveryLedger::new(64, 1 << 20));
        let (tool, _) = tool_with_ledger(dir.path(), ledger.clone());

        for _ in 0..2 {
            let text = tool
                .read(ReadParams {
                    path: "auth.rs".to_string(),
                    symbol: None,
                    budget_tokens: None,
                    repeat: None,
                })
                .content[0]
                .text
                .clone();
            assert!(text.contains("pub fn verify_token"), "{text}");
            assert!(!text.contains("Já entregue"), "{text}");
        }
        assert!(ledger.is_empty(), "signatures are not deliveries");
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auth.rs"),
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n\npub fn logout() {\n}\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn outline_lists_every_symbol_without_bodies() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: None,
            budget_tokens: None,
            repeat: None,
        });
        let text = &out.content[0].text;

        assert!(text.contains("verify_token"));
        assert!(text.contains("logout"));
        assert!(text.contains("pub fn verify_token"));
        assert!(!text.contains("!raw.is_empty()"), "outline must not include bodies: {text}");
    }

    #[test]
    fn asking_for_a_symbol_returns_only_that_body() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: Some("verify_token".to_string()),
            budget_tokens: None,
            repeat: None,
        });
        let text = &out.content[0].text;

        assert!(text.contains("!raw.is_empty()"));
        assert!(!text.contains("logout"), "must not include sibling symbols: {text}");
    }

    #[test]
    fn unknown_symbol_is_reported_not_silently_empty() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: Some("nope".to_string()),
            budget_tokens: None,
            repeat: None,
        });

        assert!(out.content[0].text.contains("não encontrado"));
    }

    #[test]
    fn path_outside_the_root_is_rejected() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "../../etc/passwd".to_string(),
            symbol: None,
            budget_tokens: None,
            repeat: None,
        });

        assert_eq!(out.is_error, Some(true));
    }

    #[test]
    fn missing_file_is_an_explicit_error() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "absent.rs".to_string(),
            symbol: None,
            budget_tokens: None,
            repeat: None,
        });

        assert_eq!(out.is_error, Some(true));
    }

    #[test]
    fn the_outline_header_carries_the_file_hash() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let content = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        let expected = hash_prefix(content.as_bytes());

        let out = tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: None,
            budget_tokens: None,
            repeat: None,
        });
        assert!(out.content[0].text.contains(&expected), "{}", out.content[0].text);
    }

    #[test]
    fn a_symbol_heading_uses_the_resolved_body_end_line() {
        let dir = fixture();
        let tool = tool_for(dir.path());
        let out = tool.read(ReadParams {
            path: "auth.rs".to_string(),
            symbol: Some("verify_token".to_string()),
            budget_tokens: None,
            repeat: None,
        });
        assert!(out.content[0].text.contains(":1-3`"), "{}", out.content[0].text);
    }
}
