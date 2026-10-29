use crate::budget::{estimate_tokens, CHARS_PER_TOKEN};
use crate::plugin::ToolPlugin;
use crate::protocol::{CallToolResult, Tool};
use crate::store::ResponseStore;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Reads back the body a truncated response left behind.
///
/// The pair to the truncation envelope: an invalid handle is an explicit error,
/// never an empty answer, because "no content" would read as "nothing more".
pub struct PeekTool {
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

impl PeekTool {
    pub fn new(store: Arc<ResponseStore>, default_budget_tokens: usize) -> Self {
        Self {
            store,
            default_budget_tokens,
        }
    }

    pub fn peek(&self, handle: &str, offset_chars: usize, budget_tokens: usize) -> CallToolResult {
        let body = match self.store.get(handle) {
            Some(body) => body,
            None => {
                return CallToolResult::error(format!(
                    "Handle `{handle}` não encontrado ou expirado. Refaça a chamada original."
                ))
            }
        };

        let start = offset_chars.min(body.len());
        if start > 0 && !body.is_char_boundary(start) {
            return CallToolResult::error(format!(
                "offset_chars {offset_chars} não está em um limite de caractere."
            ));
        }

        let remaining = &body[start..];
        let take = chars_for_budget(budget_tokens).min(remaining.len());
        let mut end = take;
        while end < remaining.len() && !remaining.is_char_boundary(end) {
            end += 1;
        }

        let page = &remaining[..end];
        let rest = &remaining[end..];

        if page.is_empty() && rest.is_empty() {
            return CallToolResult::text("Fim do corpo.");
        }

        let mut out = page.to_string();
        if !rest.is_empty() {
            out.push_str(&format!(
                "\n\nRestante: {} caracteres (~{} tokens).",
                rest.len(),
                estimate_tokens(rest)
            ));
        }

        CallToolResult::text(out)
    }
}

/// Inverse of the token estimate, used to size a page.
fn chars_for_budget(budget_tokens: usize) -> usize {
    (budget_tokens as f64 * CHARS_PER_TOKEN) as usize
}

impl ToolPlugin for PeekTool {
    fn name(&self) -> &'static str {
        "brain_peek"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_peek".to_string(),
            description:
                "Recupera o trecho de uma resposta truncada anteriormente, paginando por offset."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handle": {
                        "type": "string",
                        "description": "Handle devolvido no envelope de truncamento (formato h_xxxxxxxx)."
                    },
                    "offset_chars": {
                        "type": "integer",
                        "default": 0,
                        "minimum": 0,
                        "description": "Posição em caracteres no corpo omitido."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "default": self.default_budget_tokens,
                        "description": "Tamanho da página em tokens estimados."
                    }
                },
                "required": ["handle"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let handle = args
            .get("handle")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let offset = args
            .get("offset_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let budget = args
            .get("budget_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(self.default_budget_tokens);

        Box::pin(async move {
            if handle.is_empty() {
                return CallToolResult::error("Parâmetro obrigatório ausente: handle.");
            }
            self.peek(&handle, offset, budget)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tool() -> PeekTool {
        PeekTool::new(
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
        )
    }

    fn body_of(tool: &PeekTool, body: String) -> String {
        tool.store.insert(body)
    }

    #[test]
    fn peek_returns_the_whole_body_when_it_fits() {
        let t = tool();
        let handle = body_of(&t, "abc".to_string());
        let out = t.peek(&handle, 0, 2000);

        assert_eq!(out.is_error, None);
        assert!(out.content[0].text.starts_with("abc"));
        assert!(!out.content[0].text.contains("Restante"));
    }

    #[test]
    fn unknown_handle_is_an_explicit_error_not_an_empty_answer() {
        let t = tool();
        let out = t.peek("h_missing", 0, 2000);

        assert_eq!(out.is_error, Some(true));
        assert!(out.content[0].text.contains("não encontrado"));
    }

    #[test]
    fn paginating_with_offset_covers_the_body_without_gaps_or_overlap() {
        let t = tool();
        // 165 chars fits a 50-token page (~165 chars at 3.3 chars/token).
        let body: String = (0..165)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let handle = body_of(&t, body.clone());

        let first = t.peek(&handle, 0, 50);
        let first_text = first.content[0].text.split("\n\nRestante:").next().unwrap();
        assert_eq!(first_text.len(), 165);

        let second = t.peek(&handle, first_text.len(), 50);
        assert!(second.content[0].text.contains("Fim do corpo."));
    }

    #[test]
    fn offset_past_the_end_returns_nothing_left() {
        let t = tool();
        let handle = body_of(&t, "abc".to_string());
        let out = t.peek(&handle, 999, 2000);

        assert_eq!(out.is_error, None);
        assert!(out.content[0].text.contains("Fim do corpo."));
    }

    #[tokio::test]
    async fn missing_handle_argument_is_rejected() {
        let t = tool();
        let out = t.call(serde_json::json!({})).await;

        assert_eq!(out.is_error, Some(true));
        assert!(out.content[0].text.contains("handle"));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let t = tool();
        assert_eq!(t.name(), "brain_peek");
        assert_eq!(t.schema().name, "brain_peek");
    }
}
