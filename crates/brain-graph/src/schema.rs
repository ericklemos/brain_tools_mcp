use brain_kernel::protocol::Tool;

pub fn brain_context_graph_tool() -> Tool {
    Tool {
        name: "brain_context_graph".to_string(),
        description: "Mapeia as conexões diretas e indiretas (quem chama e o que consome) de um símbolo ou arquivo, retornando assinaturas enxutas.".to_string(),
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
                }
            },
            "required": ["target_symbol_or_file"]
        }),
    }
}

pub fn brain_impact_check_tool() -> Tool {
    Tool {
        name: "brain_impact_check".to_string(),
        description: "Percorre as chamadas reversas no grafo para listar todos os locais afetados caso uma assinatura ou contrato de símbolo seja alterado.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "symbol_name": {
                    "type": "string",
                    "description": "Nome da função, método ou struct que será modificada."
                }
            },
            "required": ["symbol_name"]
        }),
    }
}
