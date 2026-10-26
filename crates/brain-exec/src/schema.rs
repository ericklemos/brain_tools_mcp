use brain_kernel::protocol::Tool;

pub fn brain_exec_tool() -> Tool {
    Tool {
        name: "brain_exec".to_string(),
        description: "FONTE DE VERDADE sobre o estado de compilação e testes deste projeto. Roda build/check/test/clippy e devolve um relatório agrupado (erros por código e arquivo, falhas com o assert intacto, warnings resumidos) em vez do log cru. Não invente o resultado a partir de leitura de código: peça aqui. Se a execução anterior não mudou nada, responde do cache.".to_string(),
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
                    "description": "Teto aproximado de tokens para a resposta; o excedente fica recuperável via brain_peek."
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
