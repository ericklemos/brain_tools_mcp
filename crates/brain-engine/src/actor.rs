use crate::llama_validator::LlamaValidator;
use brain_kernel::{BrainError, Result};
use brain_kernel::ast::AstSnippet;
use brain_kernel::ranking::{LexicalRanker, ScoredSnippet};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

pub struct ValidationJob {
    pub query: String,
    pub keywords: Vec<String>,
    pub candidates: Vec<AstSnippet>,
    pub min_confidence: f32,
    pub max_results: usize,
    pub response_tx: oneshot::Sender<Vec<ScoredSnippet>>,
}

#[derive(Clone)]
pub struct ValidationClient {
    tx: mpsc::Sender<ValidationJob>,
}

impl ValidationClient {
    pub fn new(model_path: Option<PathBuf>, gpu_layers: u32) -> Self {
        let (tx, mut rx) = mpsc::channel::<ValidationJob>(64);

        std::thread::spawn(move || {
            let validator = if let Some(ref path) = model_path {
                match LlamaValidator::new(path, gpu_layers) {
                    Ok(v) => {
                        tracing::info!(path = %path.display(), "LlamaValidator successfully initialized");
                        Some(v)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to initialize LlamaValidator; falling back to LexicalRanker");
                        None
                    }
                }
            } else {
                tracing::info!("No model path provided; running in LexicalRanker mode");
                None
            };

            let ranker = LexicalRanker::new();

            while let Some(job) = rx.blocking_recv() {
                let ValidationJob {
                    query,
                    keywords,
                    candidates,
                    min_confidence,
                    max_results,
                    response_tx,
                } = job;

                let keyword_refs: Vec<&str> = keywords.iter().map(|s| s.as_str()).collect();

                let results = if let Some(ref val) = validator {
                    match val.validate_batch(&query, candidates.clone(), min_confidence) {
                        Ok(mut scored) => {
                            if scored.is_empty() {
                                let mut fallback = ranker.rank(&query, &keyword_refs, candidates);
                                fallback.truncate(max_results);
                                fallback
                            } else {
                                scored.truncate(max_results);
                                scored
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Batch validation failed, falling back to lexical ranker");
                            let mut scored = ranker.rank(&query, &keyword_refs, candidates);
                            scored.truncate(max_results);
                            scored
                        }
                    }
                } else {
                    // No threshold here on purpose: the ranker normalises by the
                    // best candidate, so its score is a relative rank, not a
                    // probability. Filtering on it used to drop good matches and
                    // made the response look complete while it wasn't.
                    let mut scored = ranker.rank(&query, &keyword_refs, candidates);
                    scored.truncate(max_results);
                    scored
                };

                let _ = response_tx.send(results);
            }

            tracing::info!("Validation actor thread stopped");
        });

        Self { tx }
    }

    pub async fn validate(
        &self,
        query: String,
        keywords: Vec<String>,
        candidates: Vec<AstSnippet>,
        min_confidence: f32,
        max_results: usize,
    ) -> Result<Vec<ScoredSnippet>> {
        let (response_tx, response_rx) = oneshot::channel();
        let job = ValidationJob {
            query,
            keywords,
            candidates,
            min_confidence,
            max_results,
            response_tx,
        };

        self.tx
            .send(job)
            .await
            .map_err(|e| BrainError::Inference(format!("Actor channel error: {e}")))?;

        response_rx
            .await
            .map_err(|e| BrainError::Inference(format!("Actor response error: {e}")))
    }
}
