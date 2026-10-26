use crate::engine::prompt::PromptBuilder;
use crate::error::{BrainError, Result};
use crate::search::ast_chunker::AstSnippet;
use crate::search::ranker::ScoredSnippet;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::Path;

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
    ) -> Result<Vec<ScoredSnippet>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(4096))
            .with_n_batch(2048);

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| BrainError::Inference(format!("Failed to create LlamaContext: {e}")))?;

        let prefix_str = PromptBuilder::build_prefix(user_query);
        let prefix_tokens = self
            .model
            .vocab()
            .tokenize(prefix_str.as_bytes(), true, true);
        let n_past_prefix = prefix_tokens.len() as i32;

        let mut batch = LlamaBatch::new(2048, 1);
        for (i, &token) in prefix_tokens.iter().enumerate() {
            batch
                .add(token, i as i32, &[0], false)
                .map_err(|e| BrainError::Inference(format!("Batch add error: {e}")))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| BrainError::Inference(format!("Prefix decode error: {e}")))?;

        let mut approved = Vec::new();

        for candidate in candidates {
            let _ = ctx.clear_kv_cache_seq(Some(0), Some(n_past_prefix as u32), None);
            batch.clear();

            let suffix_str = PromptBuilder::build_candidate_suffix(
                &candidate.file_path.to_string_lossy(),
                &candidate.symbol_name,
                &candidate.code,
            );
            let suffix_tokens = self
                .model
                .vocab()
                .tokenize(suffix_str.as_bytes(), false, true);

            for (j, &token) in suffix_tokens.iter().enumerate() {
                let pos = n_past_prefix + (j as i32);
                let is_last = j == suffix_tokens.len() - 1;
                let _ = batch.add(token, pos, &[0], is_last);
            }

            if let Err(e) = ctx.decode(&mut batch) {
                tracing::warn!(error = %e, "Decode failed for candidate, skipping");
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
                approved.push(ScoredSnippet {
                    snippet: candidate,
                    score: prob_yes,
                });
            }
        }

        approved.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(approved)
    }
}
