pub struct PromptBuilder;

impl PromptBuilder {
    pub fn build_prefix(query: &str) -> String {
        format!(
            "<|im_start|>system\nVocê é um filtro estrito de código. Avalie se o trecho de código responde ou implementa o que o usuário busca. Responda APENAS 'S' ou 'N'.<|im_end|>\n<|im_start|>user\nBusca: {}\n",
            query.trim()
        )
    }

    pub fn build_candidate_suffix(file_path: &str, symbol_name: &str, code: &str) -> String {
        format!(
            "Arquivo: {}\nSímbolo: {}\nCódigo:\n```\n{}\n```\nRelevante (S/N)?<|im_end|>\n<|im_start|>assistant\n",
            file_path, symbol_name, code.trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_builder() {
        let prefix = PromptBuilder::build_prefix("auth token");
        assert!(prefix.contains("Busca: auth token"));
        assert!(prefix.contains("<|im_start|>system"));

        let suffix = PromptBuilder::build_candidate_suffix("auth.rs", "verify", "fn verify() {}");
        assert!(suffix.contains("Arquivo: auth.rs"));
        assert!(suffix.contains("Símbolo: verify"));
        assert!(suffix.contains("Relevante (S/N)?"));
    }
}
