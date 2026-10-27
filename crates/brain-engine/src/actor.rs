use crate::llama_validator::{BatchValidation, LlamaValidator};
use brain_kernel::ast::AstSnippet;
use brain_kernel::ranking::{LexicalRanker, ScoredSnippet};
use brain_kernel::{BrainError, Result};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

/// Default ceiling on how many candidates reach the semantic model.
///
/// Each candidate is a serial forward pass (~0.2 s on CPU for this GGUF), so an
/// uncapped broad search would block the MCP call for tens of seconds. The
/// candidates are pre-ranked lexically, so the ceiling keeps the promising ones.
pub const DEFAULT_MAX_VALIDATE_CANDIDATES: usize = 24;

/// What actually decided the ranking.
///
/// The caller needs this to stay honest: a lexical fallback used to be
/// indistinguishable from model-approved results, and the response said
/// "N blocos aprovados" either way.
#[derive(Debug, Clone, Default)]
pub struct ValidationOutcome {
    pub scored: Vec<ScoredSnippet>,
    /// Candidates the decision actually looked at, after the validation cap.
    pub considered: usize,
    /// Candidates the semantic model could not judge (decode failure).
    pub skipped: usize,
    /// True when the lexical ranker decided because the model could not.
    pub lexical_fallback: bool,
}

pub struct ValidationJob {
    pub query: String,
    pub keywords: Vec<String>,
    pub candidates: Vec<AstSnippet>,
    pub min_confidence: f32,
    pub max_results: usize,
    pub max_candidates: usize,
    pub response_tx: oneshot::Sender<ValidationOutcome>,
}

#[derive(Clone)]
pub struct ValidationClient {
    tx: mpsc::Sender<ValidationJob>,
    max_candidates: usize,
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

            while let Some(job) = rx.blocking_recv() {
                let ValidationJob {
                    query,
                    keywords,
                    candidates,
                    min_confidence,
                    max_results,
                    max_candidates,
                    response_tx,
                } = job;

                let keyword_refs: Vec<&str> = keywords.iter().map(|s| s.as_str()).collect();

                let (verdict, considered) = match validator {
                    Some(ref val) => {
                        let shortlisted =
                            shortlist(&query, &keyword_refs, candidates.clone(), max_candidates);
                        let considered = shortlisted.len();
                        let verdict = match val.validate_batch(&query, shortlisted, min_confidence)
                        {
                            Ok(report) => ModelVerdict::Judged(report),
                            Err(e) => {
                                tracing::warn!(error = %e, "Batch validation failed");
                                ModelVerdict::Failed
                            }
                        };
                        (verdict, considered)
                    }
                    None => (ModelVerdict::NoModel, 0),
                };

                let outcome = decide(
                    verdict,
                    &query,
                    &keyword_refs,
                    candidates,
                    considered,
                    max_results,
                );

                let _ = response_tx.send(outcome);
            }

            tracing::info!("Validation actor thread stopped");
        });

        Self {
            tx,
            max_candidates: DEFAULT_MAX_VALIDATE_CANDIDATES,
        }
    }

    /// Caps how many candidates reach the semantic model.
    pub fn with_max_candidates(mut self, max_candidates: usize) -> Self {
        self.max_candidates = max_candidates.max(1);
        self
    }

    pub async fn validate(
        &self,
        query: String,
        keywords: Vec<String>,
        candidates: Vec<AstSnippet>,
        min_confidence: f32,
        max_results: usize,
    ) -> Result<Vec<ScoredSnippet>> {
        self.validate_with_outcome(query, keywords, candidates, min_confidence, max_results)
            .await
            .map(|outcome| outcome.scored)
    }

    pub async fn validate_with_outcome(
        &self,
        query: String,
        keywords: Vec<String>,
        candidates: Vec<AstSnippet>,
        min_confidence: f32,
        max_results: usize,
    ) -> Result<ValidationOutcome> {
        let (response_tx, response_rx) = oneshot::channel();
        let job = ValidationJob {
            query,
            keywords,
            candidates,
            min_confidence,
            max_results,
            max_candidates: self.max_candidates,
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

/// Pre-ranks candidates so the model only sees the most promising ones.
///
/// Ties keep the original order (`sort_by` is stable), so the shortlist is
/// deterministic for the same input.
fn shortlist(
    query: &str,
    keywords: &[&str],
    candidates: Vec<AstSnippet>,
    limit: usize,
) -> Vec<AstSnippet> {
    let mut ranked = LexicalRanker::new().rank(query, keywords, candidates);
    ranked.truncate(limit.max(1));
    ranked.into_iter().map(|scored| scored.snippet).collect()
}

/// What the semantic stage was able to say.
enum ModelVerdict {
    /// No GGUF configured: only lexical ranking exists.
    NoModel,
    /// The model judged (some of) the shortlist.
    Judged(BatchValidation),
    /// The model blew up before judging anything.
    Failed,
}

/// Turns the model's verdict into the outcome the response will declare.
///
/// The distinction that matters: "the model rejected every candidate" is a
/// verdict and must stand, while "the model could not judge" is an
/// unavailability and must be declared as a lexical fallback.
fn decide(
    verdict: ModelVerdict,
    query: &str,
    keywords: &[&str],
    candidates: Vec<AstSnippet>,
    considered: usize,
    max_results: usize,
) -> ValidationOutcome {
    let ranker = LexicalRanker::new();

    match verdict {
        ModelVerdict::Judged(report) if !report.scored.is_empty() => {
            let mut scored = report.scored;
            scored.truncate(max_results.max(1));
            ValidationOutcome {
                scored,
                considered,
                skipped: report.skipped,
                lexical_fallback: false,
            }
        }
        ModelVerdict::Judged(report) if report.skipped == 0 => ValidationOutcome {
            scored: Vec::new(),
            considered,
            skipped: 0,
            lexical_fallback: false,
        },
        ModelVerdict::Judged(report) => {
            let mut scored = ranker.rank(query, keywords, candidates);
            scored.truncate(max_results.max(1));
            ValidationOutcome {
                scored,
                considered,
                skipped: report.skipped,
                lexical_fallback: true,
            }
        }
        ModelVerdict::Failed => {
            let mut scored = ranker.rank(query, keywords, candidates);
            scored.truncate(max_results.max(1));
            ValidationOutcome {
                scored,
                considered,
                skipped: considered,
                lexical_fallback: true,
            }
        }
        // Without a model the ranker's score is a relative rank, not a
        // probability, so there is no threshold to apply.
        ModelVerdict::NoModel => {
            let considered = candidates.len();
            let mut scored = ranker.rank(query, keywords, candidates);
            scored.truncate(max_results.max(1));
            ValidationOutcome {
                scored,
                considered,
                skipped: 0,
                lexical_fallback: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(symbol: &str) -> AstSnippet {
        AstSnippet {
            file_path: PathBuf::from("src/lib.rs"),
            symbol_name: symbol.to_string(),
            signature: format!("pub fn {symbol}()"),
            code: format!("pub fn {symbol}() {{}}"),
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn shortlist_keeps_only_the_lexically_best_candidates() {
        let candidates = vec![
            snippet("draw_canvas"),
            snippet("verify_jwt_token"),
            snippet("parse_csv"),
            snippet("verify_jwt_signature"),
        ];

        let picked = shortlist("verify jwt token", &["verify_jwt_token"], candidates, 2);

        assert_eq!(picked.len(), 2, "the cap must bound the model's work");
        assert_eq!(picked[0].symbol_name, "verify_jwt_token");
    }

    #[test]
    fn shortlist_is_a_no_op_when_everything_fits() {
        let candidates = vec![snippet("alpha"), snippet("beta")];
        let picked = shortlist("alpha", &["alpha"], candidates, 24);
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn a_model_that_rejects_everything_is_not_overridden_by_the_lexical_ranker() {
        let candidates = vec![snippet("verify_jwt_token"), snippet("draw_canvas")];
        let outcome = decide(
            ModelVerdict::Judged(BatchValidation {
                scored: Vec::new(),
                skipped: 0,
            }),
            "verify jwt token",
            &["verify_jwt_token"],
            candidates,
            2,
            5,
        );

        assert!(
            outcome.scored.is_empty(),
            "the model's rejection must stand; a lexical rank would undo the filter"
        );
        assert!(!outcome.lexical_fallback);
    }

    #[test]
    fn decode_failures_are_declared_and_fall_back_to_lexical_ranking() {
        let candidates = vec![snippet("verify_jwt_token"), snippet("draw_canvas")];
        let outcome = decide(
            ModelVerdict::Judged(BatchValidation {
                scored: Vec::new(),
                skipped: 2,
            }),
            "verify jwt token",
            &["verify_jwt_token"],
            candidates,
            2,
            5,
        );

        assert_eq!(outcome.skipped, 2, "unjudged candidates must be visible");
        assert!(outcome.lexical_fallback);
        assert_eq!(outcome.scored.len(), 2);
    }

    #[test]
    fn a_model_error_is_declared_as_a_lexical_fallback() {
        let candidates = vec![snippet("alpha"), snippet("beta")];
        let outcome = decide(
            ModelVerdict::Failed,
            "alpha",
            &["alpha"],
            candidates,
            2,
            5,
        );

        assert!(outcome.lexical_fallback);
        assert_eq!(outcome.skipped, 2, "a failed batch judged nothing");
    }

    #[test]
    fn the_validation_cap_survives_into_the_outcome() {
        let candidates = vec![snippet("a"), snippet("b"), snippet("c")];
        let outcome = decide(
            ModelVerdict::Judged(BatchValidation {
                scored: Vec::new(),
                skipped: 0,
            }),
            "a",
            &["a"],
            candidates,
            2,
            5,
        );

        assert_eq!(outcome.considered, 2, "considered is the shortlist, not every candidate");
    }

    #[test]
    fn without_a_model_the_lexical_ranking_is_declared() {
        let candidates = vec![snippet("alpha"), snippet("beta")];
        let outcome = decide(ModelVerdict::NoModel, "alpha", &["alpha"], candidates, 0, 5);

        assert!(outcome.lexical_fallback);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.scored.len(), 2);
    }
}
