use brain_kernel::ast::AstChunker;
use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Reads a file as an outline, or one symbol's body, instead of the whole file.
///
/// This is the tool that replaces wholesale `Read` calls: the agent asks for
/// "what is in this file" and gets signatures, or "show me this function" and
/// gets just that function.
pub struct ReadTool {
    root_dir: PathBuf,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct ReadParams {
    pub path: String,
    pub symbol: Option<String>,
    pub budget_tokens: Option<usize>,
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
        }
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

        let sections = match &params.symbol {
            Some(symbol) => match self.symbol_sections(&path, &content, &relative, symbol) {
                Ok(sections) if !sections.is_empty() => sections,
                Ok(_) => {
                    return CallToolResult::text(format!(
                        "Símbolo `{symbol}` não encontrado em `{relative}`."
                    ))
                }
                Err(e) => return CallToolResult::error(e.to_string()),
            },
            None => self.outline_sections(&path, &content, &relative),
        };

        let budget = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
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
                heading: format!("### `{relative}` — {} linhas", content.lines().count()),
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
    ) -> brain_kernel::Result<Vec<Section>> {
        let chunker = AstChunker::new();
        let all_lines: Vec<usize> = (1..=content.lines().count()).collect();
        let snippets = chunker.extract_snippets(path, content, &all_lines)?;

        let lang = if path.extension().and_then(|e| e.to_str()) == Some("kt") {
            "kotlin"
        } else {
            "rust"
        };

        Ok(snippets
            .into_iter()
            .filter(|snippet| snippet.symbol_name == symbol)
            .map(|snippet| Section {
                heading: format!(
                    "### `{}` (`{relative}:{}-{}`)",
                    snippet.symbol_name, snippet.start_line, snippet.end_line
                ),
                body: format!("```{lang}\n{}\n```\n", snippet.code),
                priority: 255,
            })
            .collect())
    }
}

fn whole_file_section(content: &str, relative: &str) -> Section {
    let lang = if relative.ends_with(".kt") { "kotlin" } else { "rust" };
    Section {
        heading: format!("### `{relative}` — arquivo inteiro"),
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
        });

        assert_eq!(out.is_error, Some(true));
    }
}
