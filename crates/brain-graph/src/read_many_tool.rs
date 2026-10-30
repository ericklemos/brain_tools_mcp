//! Batch reading: one call validates every item before returning, and the
//! whole batch shares a single token budget.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;

use crate::read_tool::{ReadParams, ReadTool};

#[derive(Debug, Deserialize)]
pub struct ReadManyItem {
    pub path: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReadManyParams {
    pub reads: Vec<ReadManyItem>,
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    #[serde(default)]
    pub repeat: Option<String>,
}

pub struct ReadManyTool {
    inner: Arc<ReadTool>,
}

impl ReadManyTool {
    pub fn new(inner: Arc<ReadTool>) -> Self {
        Self { inner }
    }

    pub fn read_many(&self, params: ReadManyParams) -> CallToolResult {
        if params.reads.is_empty() {
            return CallToolResult::error("`reads` está vazio: nada a ler.");
        }

        let mut rendered = Vec::new();
        let mut errors = Vec::new();
        let mut seen = HashSet::new();

        for (index, item) in params.reads.into_iter().enumerate() {
            let request = ReadParams {
                path: item.path.clone(),
                symbol: item.symbol.clone(),
                budget_tokens: None,
                repeat: params.repeat.clone(),
                rev: item.rev.clone(),
            };

            let (path, content, relative) = match self.inner.prepare_read(&request) {
                Ok(read) => read,
                Err(message) => {
                    errors.push(format!("- item {} (`{}`): {message}", index + 1, item.path));
                    continue;
                }
            };

            if !seen.insert((path.clone(), item.rev.clone(), item.symbol.clone())) {
                errors.push(format!(
                    "- item {} (`{}`): leitura repetida no lote.",
                    index + 1,
                    item.path
                ));
                continue;
            }

            match self.inner.render_read(&request, &path, &content, &relative) {
                Ok(sections) if !sections.is_empty() => rendered.extend(sections),
                Ok(_) => errors.push(format!(
                    "- item {} (`{}`): símbolo `{}` não encontrado em `{relative}`.",
                    index + 1,
                    item.path,
                    item.symbol.as_deref().unwrap_or_default()
                )),
                Err(e) => errors.push(format!("- item {} (`{}`): {e}", index + 1, item.path)),
            }
        }

        if !errors.is_empty() {
            return CallToolResult::error(format!(
                "### `brain_read_many` — nada foi lido\n\n{}\n",
                errors.join("\n")
            ));
        }

        self.inner.finish_read(rendered, params.budget_tokens)
    }
}

impl ToolPlugin for ReadManyTool {
    fn name(&self) -> &'static str {
        "brain_read_many"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_read_many".to_string(),
            description: "Lê vários arquivos ou símbolos como um lote: valida todas as leituras antes de responder, recusa o lote inteiro se uma for inválida e divide um único orçamento de tokens entre os itens.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reads": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Leituras a executar, na ordem do relatório. A mesma combinação de caminho, revisão e símbolo é recusada duas vezes.",
                        "items": {
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
                                "rev": {
                                    "type": "string",
                                    "description": "Opcional: lê `rev:path` (commit, tag ou ref) em vez da árvore de trabalho."
                                }
                            },
                            "required": ["path"]
                        }
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens compartilhado por todas as leituras do lote (excedente em brain_peek)."
                    },
                    "repeat": {
                        "type": "string",
                        "enum": ["allow"],
                        "description": "`allow` devolve os corpos mesmo se esta sessão já os recebeu; o padrão aponta para o corpo anterior."
                    }
                },
                "required": ["reads"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: ReadManyParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.read_many(params) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use brain_kernel::store::ResponseStore;
    use tempfile::{tempdir, TempDir};

    fn tool(dir: &Path) -> ReadManyTool {
        let inner = Arc::new(ReadTool::new(
            dir,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
        ));
        ReadManyTool::new(inner)
    }

    fn item(path: &str, symbol: &str) -> ReadManyItem {
        ReadManyItem {
            path: path.into(),
            symbol: Some(symbol.into()),
            rev: None,
        }
    }

    fn batch(reads: Vec<ReadManyItem>) -> ReadManyParams {
        ReadManyParams {
            reads,
            budget_tokens: None,
            repeat: None,
        }
    }

    fn text(out: &CallToolResult) -> &str {
        &out.content[0].text
    }

    fn fixture() -> TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auth.rs"),
            "pub fn verify_token(raw: &str) -> bool {\n    !raw.is_empty()\n}\n\npub fn logout() {\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("session.rs"),
            "pub fn start_session() -> u8 {\n    1\n}\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_batch_reads_every_symbol() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(vec![
            item("auth.rs", "verify_token"),
            item("session.rs", "start_session"),
        ]));

        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).contains("!raw.is_empty()"), "{}", text(&out));
        assert!(text(&out).contains("start_session"), "{}", text(&out));
    }

    #[test]
    fn the_same_file_may_appear_with_different_symbols() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(vec![
            item("auth.rs", "verify_token"),
            item("auth.rs", "logout"),
        ]));

        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).contains("!raw.is_empty()"), "{}", text(&out));
        assert!(text(&out).contains("pub fn logout"), "{}", text(&out));
    }

    #[test]
    fn a_repeated_read_is_rejected_before_returning_anything() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(vec![
            item("auth.rs", "verify_token"),
            item("./auth.rs", "verify_token"),
        ]));

        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("repetida"), "{}", text(&out));
        assert!(!text(&out).contains("!raw.is_empty()"), "{}", text(&out));
    }

    #[test]
    fn a_missing_file_aborts_the_whole_batch() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(vec![
            item("auth.rs", "verify_token"),
            item("missing.rs", "nope"),
        ]));

        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("nada foi lido"), "{}", text(&out));
        assert!(text(&out).contains("missing.rs"), "{}", text(&out));
        assert!(!text(&out).contains("!raw.is_empty()"), "{}", text(&out));
    }

    #[test]
    fn an_unknown_symbol_aborts_the_whole_batch() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(vec![
            item("auth.rs", "verify_token"),
            item("session.rs", "does_not_exist"),
        ]));

        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("não encontrado"), "{}", text(&out));
        assert!(!text(&out).contains("!raw.is_empty()"), "{}", text(&out));
    }

    #[test]
    fn an_empty_batch_is_an_explicit_error() {
        let dir = fixture();
        let out = tool(dir.path()).read_many(batch(Vec::new()));

        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("vazio"), "{}", text(&out));
    }

    #[test]
    fn the_batch_shares_one_budget_and_the_tail_is_recoverable() {
        let dir = tempdir().unwrap();
        let body = |name: &str| {
            format!(
                "pub fn {name}() -> u8 {{\n{}\n    1\n}}\n",
                "    let _ = 1;\n".repeat(80)
            )
        };
        fs::write(dir.path().join("one.rs"), body("first_long")).unwrap();
        fs::write(dir.path().join("two.rs"), body("second_long")).unwrap();

        let mut params = batch(vec![
            item("one.rs", "first_long"),
            item("two.rs", "second_long"),
        ]);
        params.budget_tokens = Some(80);

        let out = tool(dir.path()).read_many(params);

        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).contains("Resposta truncada"), "{}", text(&out));
        assert!(text(&out).contains("brain_peek"), "{}", text(&out));
    }
}
