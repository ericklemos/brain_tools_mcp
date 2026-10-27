use brain_kernel::protocol::Tool;

pub fn brain_context_graph_tool() -> Tool {
    Tool {
        name: "brain_context_graph".to_string(),
        description: "FONTE DE VERDADE das relações. Quem chama e o que o símbolo/arquivo consome, com assinaturas; não reabra arquivos com Read para conferir.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target_symbol_or_file": {
                    "type": "string",
                    "description": "Nome da função/struct (ex: 'verify_token') ou caminho relativo do arquivo."
                },
                "depth": {
                    "type": "integer",
                    "default": 1,
                    "minimum": 1,
                    "maximum": 3,
                    "description": "Número de saltos no grafo de chamadas."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                }
            },
            "required": ["target_symbol_or_file"]
        }),
    }
}

pub fn brain_impact_check_tool() -> Tool {
    Tool {
        name: "brain_impact_check".to_string(),
        description: "FONTE DE VERDADE do impacto. Chamadores reversos que quebram se a assinatura mudar; não reabra com Read para conferir.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "symbol_name": {
                    "type": "string",
                    "description": "Nome da função, método ou struct que será modificada."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                }
            },
            "required": ["symbol_name"]
        }),
    }
}
