use brain_kernel::protocol::Tool;

pub fn brain_exec_tool() -> Tool {
    Tool {
        name: "brain_exec".to_string(),
        description: "Build/check/test/clippy com erros agrupados, assert intacto e cache.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["build", "check", "test", "clippy"],
                    "description": "Operação a executar: `check` é o mais rápido para erros de tipo; `build` também liga artefatos; `test` roda a suíte; `clippy` adiciona os lints."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                },
                "refresh": {
                    "type": "boolean",
                    "default": false,
                    "description": "Ignora o cache e reexecuta o cargo mesmo que nada tenha mudado."
                }
            },
            "required": ["kind"]
        }),
    }
}
