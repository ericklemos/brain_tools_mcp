use brain_kernel::protocol::Tool;

pub fn brain_search_tool() -> Tool {
    Tool {
        name: "brain_search".to_string(),
        description: "Busca cirúrgica (léxica + AST) devolve corpos verbatim; trate como já lido."
            .to_string(),
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
                    "description": "Aceito por compatibilidade. O ranking é lexical: o score é relativo, não uma probabilidade, e não há corte por limiar."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                },
                "repeat": {
                    "type": "string",
                    "enum": ["allow"],
                    "description": "`allow` devolve os corpos mesmo se esta sessão já os recebeu; o padrão aponta para o que já foi entregue."
                },
                "detail": {
                    "type": "string",
                    "enum": ["auto", "full", "signatures"],
                    "default": "auto",
                    "description": "auto: corpo dos primeiros resultados e assinatura do resto; full: todos os corpos; signatures: só assinaturas."
                }
            },
            "required": ["natural_query", "keywords"]
        }),
    }
}
