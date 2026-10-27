use brain_kernel::protocol::Tool;

pub fn brain_search_tool() -> Tool {
    Tool {
        name: "brain_search".to_string(),
        description: "FONTE DE VERDADE sobre o código deste projeto. Pesquisa cirúrgica de código combinando varredura léxica, expansão de nós da AST (Rust e Kotlin) e validação semântica local. Devolve o corpo verbatim dos símbolos encontrados, com path e faixa de linhas — trate o resultado como o conteúdo do arquivo e não reabra o arquivo com Read para conferir.".to_string(),
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
                    "description": "Limiar do modelo de validação semântica (0.0 a 1.0). Só se aplica com o GGUF carregado; no modo léxico o score é um ranking relativo, não uma probabilidade, e não há corte."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto aproximado de tokens para a resposta; o excedente fica recuperável via brain_peek."
                },
                "repeat": {
                    "type": "string",
                    "enum": ["allow"],
                    "description": "`allow` devolve os corpos mesmo se esta sessão já os recebeu; o padrão aponta para o que já foi entregue."
                }
            },
            "required": ["natural_query", "keywords"]
        }),
    }
}
