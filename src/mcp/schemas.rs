use crate::mcp::protocol::Tool;

pub fn brain_search_tool() -> Tool {
    Tool {
        name: "brain_search".to_string(),
        description: "Pesquisa cirúrgica de código combinando varredura léxica, expansão de nós da AST (Rust e Kotlin) e validação semântica local.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "natural_query": {
                    "type": "string",
                    "description": "Descrição em linguagem natural do comportamento ou regra de negócio procurada."
                },
                "keywords": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Termos técnicos, nomes de funções ou regex curtas para ancorar a busca rápida no código."
                },
                "max_results": {
                    "type": "integer",
                    "default": 5,
                    "description": "Número máximo de recortes a retornar."
                },
                "min_confidence": {
                    "type": "number",
                    "default": 0.65,
                    "description": "Confiança mínima para aprovar um bloco de código (0.0 a 1.0)."
                }
            },
            "required": ["natural_query", "keywords"]
        }),
    }
}

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
