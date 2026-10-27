use crate::prompt::PromptBuilder;
use brain_kernel::{BrainError, Result};
use brain_kernel::ast::AstSnippet;
use brain_kernel::ranking::ScoredSnippet;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::Path;

/// Janela de contexto usada na validação.
const N_CTX: u32 = 4096;
/// Piso do batch: abaixo disso desperdiça a janela disponível.
const N_BATCH_FLOOR: u32 = 512;

/// Dimensiona o `n_batch`/`n_ubatch` para o maior par prefixo+candidato.
///
/// O GGUF vllm-d3-edge declara `attention.causal = false`, e para modelos
/// não-causais o llama.cpp exige `n_ubatch >= n_tokens_all`
/// (llama-context.cpp:1789). Quando o batch é menor, o processo aborta com
/// SIGABRT no meio de uma chamada MCP — o cliente só vê "Connection closed".
///
/// Como cada candidato é decodificado separadamente (o KV cache da sequência
/// é limpo a cada iteração), o batch precisa cobrir apenas o maior deles
/// somado ao prefixo, não a soma de todos.
fn required_n_batch(prefix_tokens: usize, candidate_tokens: &[usize]) -> u32 {
    let longest = candidate_tokens.iter().copied().max().unwrap_or(0);
    let needed = (prefix_tokens + longest).max(N_BATCH_FLOOR as usize);
    needed.min(N_CTX as usize) as u32
}

/// Result of judging a whole batch.
///
/// `skipped` is the part the caller cannot afford to lose: a candidate the
/// model failed to decode is *unjudged*, not rejected, and reporting it as
/// "did not match" silently degrades the search to a lexical one.
#[derive(Debug, Default, Clone)]
pub struct BatchValidation {
    pub scored: Vec<ScoredSnippet>,
    /// Candidates that could not be decoded and therefore never got a verdict.
    pub skipped: usize,
}

pub struct LlamaValidator {
    backend: LlamaBackend,
    model: LlamaModel,
    token_yes: LlamaToken,
    token_no: LlamaToken,
}

impl LlamaValidator {
    pub fn new(model_path: &Path, gpu_layers: u32) -> Result<Self> {
        let backend = LlamaBackend::init()
            .map_err(|e| BrainError::Inference(format!("Failed to init LlamaBackend: {e}")))?;

        let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
        let model = LlamaModel::load_from_file(&backend, model_path, &model_params)
            .map_err(|e| BrainError::Inference(format!("Failed to load GGUF model: {e}")))?;

        let vocab = model.vocab();

        let token_yes = vocab
            .tokenize("S".as_bytes(), false, false)
            .first()
            .copied()
            .or_else(|| vocab.tokenize("Y".as_bytes(), false, false).first().copied())
            .ok_or_else(|| BrainError::Inference("Token S/Y not found in model vocab".to_string()))?;

        let token_no = vocab
            .tokenize("N".as_bytes(), false, false)
            .first()
            .copied()
            .ok_or_else(|| BrainError::Inference("Token N not found in model vocab".to_string()))?;

        Ok(Self {
            backend,
            model,
            token_yes,
            token_no,
        })
    }

    pub fn validate_batch(
        &self,
        user_query: &str,
        candidates: Vec<AstSnippet>,
        threshold: f32,
    ) -> Result<BatchValidation> {
        if candidates.is_empty() {
            return Ok(BatchValidation::default());
        }

        let prefix_str = PromptBuilder::build_prefix(user_query);
        let prefix_tokens = self
            .model
            .vocab()
            .tokenize(prefix_str.as_bytes(), true, true);
        let n_past_prefix = prefix_tokens.len();

        // Tokeniza todos os candidatos antes de criar o contexto: o tamanho do
        // batch é fixado na criação e precisa cobrir o maior par prefixo+candidato.
        let mut tokenized: Vec<(AstSnippet, Vec<LlamaToken>)> = Vec::with_capacity(candidates.len());
        let mut lengths: Vec<usize> = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let suffix_str = PromptBuilder::build_candidate_suffix(
                &candidate.file_path.to_string_lossy(),
                &candidate.symbol_name,
                &candidate.code,
            );
            let tokens = self
                .model
                .vocab()
                .tokenize(suffix_str.as_bytes(), false, true);
            lengths.push(tokens.len());
            tokenized.push((candidate, tokens));
        }

        let n_batch = required_n_batch(n_past_prefix, &lengths);
        tracing::debug!(
            candidates = tokenized.len(),
            prefix_tokens = n_past_prefix,
            longest_candidate = lengths.iter().copied().max().unwrap_or(0),
            n_batch,
            "Validating candidate batch"
        );

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(n_batch)
            // Modelos não-causais (o GGUF declara attention.causal = false)
            // exigem n_ubatch >= n_tokens_all a cada decode.
            .with_n_ubatch(n_batch);

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| BrainError::Inference(format!("Failed to create LlamaContext: {e}")))?;

        let mut batch = LlamaBatch::new(n_batch as usize, 1);
        let mut scored = Vec::new();
        let mut skipped = 0usize;

        for (candidate, suffix_tokens) in tokenized {
            let total = n_past_prefix + suffix_tokens.len();
            if total > N_CTX as usize {
                tracing::warn!(
                    symbol = %candidate.symbol_name,
                    tokens = total,
                    n_ctx = N_CTX,
                    "Candidate does not fit the context window; leaving it unjudged"
                );
                skipped += 1;
                continue;
            }

            // O GGUF híbrido (qwen35) tem camadas recorrentes cuja memória
            // recusa rollback parcial: `clear_kv_cache_seq(Some(0), Some(w), None)`
            // devolve `false`, as posições antigas permanecem e o próximo batch
            // viola a asserção M-RoPE (`X < Y`) do llama.cpp. Limpar a sequência
            // inteira é a única remoção que sempre funciona (kv_cache.rs:70).
            let cleared = ctx.clear_kv_cache_seq(Some(0), None, None);
            if !matches!(cleared, Ok(true)) {
                ctx.clear_kv_cache();
            }

            // Um único batch com prefixo + candidato: sem reuso de KV entre
            // candidatos, as posições voltam a começar em zero.
            batch.clear();
            for (i, &token) in prefix_tokens.iter().enumerate() {
                batch
                    .add(token, i as i32, &[0], false)
                    .map_err(|e| BrainError::Inference(format!("Batch add error: {e}")))?;
            }
            for (j, &token) in suffix_tokens.iter().enumerate() {
                let pos = (n_past_prefix + j) as i32;
                let is_last = j == suffix_tokens.len() - 1;
                batch
                    .add(token, pos, &[0], is_last)
                    .map_err(|e| BrainError::Inference(format!("Batch add error: {e}")))?;
            }

            if let Err(e) = ctx.decode(&mut batch) {
                tracing::warn!(
                    error = %e,
                    symbol = %candidate.symbol_name,
                    "Decode failed for candidate, skipping"
                );
                skipped += 1;
                continue;
            }

            let logits = ctx.candidates_ith(batch.n_tokens() - 1);
            let mut logit_s = f32::NEG_INFINITY;
            let mut logit_n = f32::NEG_INFINITY;

            for token_data in logits {
                if token_data.id() == self.token_yes {
                    logit_s = token_data.logit();
                } else if token_data.id() == self.token_no {
                    logit_n = token_data.logit();
                }
            }

            let max_logit = logit_s.max(logit_n);
            let exp_s = (logit_s - max_logit).exp();
            let exp_n = (logit_n - max_logit).exp();
            let prob_yes = if (exp_s + exp_n) > 0.0 {
                exp_s / (exp_s + exp_n)
            } else {
                0.0
            };

            if prob_yes >= threshold {
                scored.push(ScoredSnippet {
                    snippet: candidate,
                    score: prob_yes,
                });
            }
        }

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(BatchValidation { scored, skipped })
    }
}

#[cfg(test)]
mod tests {
    /// O GGUF vllm-d3-edge declara `attention.causal = false`. Para modelos
    /// não-causais o llama.cpp exige `n_ubatch >= n_tokens_all`
    /// (llama-context.cpp:1789), senão aborta o processo com SIGABRT.
    /// O batch precisa ser dimensionado para caber todos os tokens de uma vez.
    #[test]
    fn batch_capacity_covers_prefix_plus_longest_candidate() {
        let prefix_tokens = 40;
        let candidate_tokens = vec![1500, 2048, 120];

        let n_batch = super::required_n_batch(prefix_tokens, &candidate_tokens);

        let longest = *candidate_tokens.iter().max().unwrap();
        assert!(
            n_batch >= (prefix_tokens + longest) as u32,
            "n_batch ({n_batch}) deve cobrir prefixo ({prefix_tokens}) + maior candidato ({longest})"
        );
    }

    #[test]
    fn batch_capacity_never_exceeds_context_window() {
        // Um candidato absurdo não pode pedir um batch maior que n_ctx=4096,
        // senão o próprio llama.cpp rejeita na criação do contexto.
        let n_batch = super::required_n_batch(10, &[100_000]);
        assert_eq!(n_batch, 4096);
    }

    #[test]
    fn batch_capacity_has_sane_floor() {
        let n_batch = super::required_n_batch(5, &[10]);
        assert!(n_batch >= 512, "batch muito pequeno desperdiça a janela de contexto");
    }
}
