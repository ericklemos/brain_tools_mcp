use brain_kernel::protocol::Tool;

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
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto aproximado de tokens para a resposta; o excedente fica recuperável via brain_peek."
                }
            },
            "required": ["natural_query", "keywords"]
        }),
    }
}
