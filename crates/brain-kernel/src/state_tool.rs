//! `brain_state`: the explicit face of the local session trail. It resumes,
//! records notes and tasks, and shows the current session. Durable facts still
//! belong to `brain_ledger`; this is the working memory that expires.

use crate::plugin::ToolPlugin;
use crate::protocol::{CallToolResult, Tool};
use crate::state::StateStore;
use serde::Deserialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct StateTool {
    state: Arc<StateStore>,
    default_budget_tokens: usize,
}

#[derive(Deserialize)]
struct StateParams {
    action: String,
    text: Option<String>,
    tags: Option<Vec<String>>,
    path: Option<String>,
    symbol: Option<String>,
    task_id: Option<String>,
    status: Option<String>,
    budget_tokens: Option<usize>,
}

impl StateTool {
    pub fn new(state: Arc<StateStore>, default_budget_tokens: usize) -> Self {
        Self {
            state,
            default_budget_tokens,
        }
    }

    pub fn handle(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: StateParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };
        let budget = params
            .budget_tokens
            .unwrap_or(self.default_budget_tokens)
            .min(600);

        match params.action.as_str() {
            "resume" => {
                let mut body = self
                    .state
                    .resume()
                    .unwrap_or_else(|| "Sem sessão anterior registrada.".to_string());
                body.push_str("\n\n");
                body.push_str(&self.state.status());
                CallToolResult::text(cap(body, budget))
            }
            "status" => CallToolResult::text(cap(self.state.status(), budget)),
            "memory" => CallToolResult::text(cap(
                self.state.memory(params.text.as_deref()),
                budget,
            )),
            "note" => {
                let Some(text) = params.text.as_deref() else {
                    return CallToolResult::error("A ação `note` exige `text`.".to_string());
                };
                match self.state.record_note(
                    text,
                    params.tags.unwrap_or_default(),
                    params.path.as_deref(),
                    params.symbol.as_deref(),
                ) {
                    Ok(()) => CallToolResult::text(format!("Nota registrada — {text}")),
                    Err(e) => CallToolResult::error(format!("Falha ao registrar a nota: {e}")),
                }
            }
            "task" => {
                let Some(text) = params.text.as_deref() else {
                    return CallToolResult::error("A ação `task` exige `text`.".to_string());
                };
                let status = params.status.as_deref().unwrap_or("open");
                if !matches!(status, "open" | "done") {
                    return CallToolResult::error(
                        "status inválido: use `open` ou `done`.".to_string(),
                    );
                }
                match self
                    .state
                    .record_task(text, status, params.task_id.as_deref())
                {
                    Ok(()) => CallToolResult::text(format!("Tarefa {status} — {text}")),
                    Err(e) => CallToolResult::error(format!("Falha ao registrar a tarefa: {e}")),
                }
            }
            "end" => {
                self.state.finish();
                CallToolResult::text("Sessão encerrada e estado preservado.".to_string())
            }
            other => CallToolResult::error(format!(
                "Ação desconhecida: `{other}`. Use resume, status, memory, note, task ou end."
            )),
        }
    }
}

impl ToolPlugin for StateTool {
    fn name(&self) -> &'static str {
        "brain_state"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_state".to_string(),
            description: "Estado local da sessão: retoma o que foi feito, registra notas e tarefas e consulta a memória durável. Fatos duráveis ficam em brain_ledger.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["resume", "status", "memory", "note", "task", "end"],
                        "description": "`resume` retoma a sessão anterior, `status` mostra a atual, `memory` consulta o rollup durável, `note`/`task` gravam, `end` fecha."
                    },
                    "text": {
                        "type": "string",
                        "description": "Texto da nota/tarefa ou, em `memory`, os termos da busca."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Opcional: rótulos da nota."
                    },
                    "path": {
                        "type": "string",
                        "description": "Opcional: arquivo ao qual a nota se ancora."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Opcional: símbolo ao qual a nota se ancora."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Opcional: id estável para atualizar a mesma tarefa."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["open", "done"],
                        "description": "Status da tarefa; o padrão é `open`."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens da resposta (máximo 600)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(async move { self.handle(args) })
    }
}

fn cap(text: String, budget_tokens: usize) -> String {
    let max = budget_tokens.max(80) * 4;
    if text.chars().count() <= max {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    format!(
        "{}\n> (truncado; peça o restante com budget_tokens maior)",
        &text[..cut]
    )
}
