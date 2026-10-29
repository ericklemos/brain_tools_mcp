//! Candidate validation without a model.
//!
//! The lexical ranker decides the order and the cut. There is no semantic
//! score to threshold, so `min_confidence` is accepted for API compatibility
//! and ignored.

use brain_kernel::ast::AstSnippet;
use brain_kernel::error::BrainError;
use brain_kernel::ranking::{LexicalRanker, ScoredSnippet};

/// What a ranking pass decided, and how many candidates it looked at.
#[derive(Debug, Default)]
pub struct ValidationOutcome {
    pub scored: Vec<ScoredSnippet>,
    /// Candidates the decision actually looked at, after the candidate cap.
    pub considered: usize,
    /// Candidates that could not be judged. Always zero: nothing here fails.
    pub skipped: usize,
    /// Kept for callers that used to distinguish a semantic verdict from a
    /// lexical one. It is always `true` now that no model is involved.
    pub lexical_fallback: bool,
}

/// Ranks search candidates lexically.
#[derive(Debug, Clone)]
pub struct ValidationClient {
    max_candidates: usize,
}

impl Default for ValidationClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationClient {
    pub fn new() -> Self {
        Self { max_candidates: 24 }
    }

    /// Caps how many candidates the ranker considers.
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
    ) -> Result<Vec<ScoredSnippet>, BrainError> {
        self.validate_with_outcome(query, keywords, candidates, min_confidence, max_results)
            .await
            .map(|outcome| outcome.scored)
    }

    pub async fn validate_with_outcome(
        &self,
        query: String,
        keywords: Vec<String>,
        candidates: Vec<AstSnippet>,
        _min_confidence: f32,
        max_results: usize,
    ) -> Result<ValidationOutcome, BrainError> {
        if candidates.is_empty() {
            return Ok(ValidationOutcome {
                lexical_fallback: true,
                ..ValidationOutcome::default()
            });
        }

        let candidate_count = candidates.len();
        let considered = candidate_count.min(self.max_candidates);
        let keyword_refs: Vec<&str> = keywords.iter().map(String::as_str).collect();

        let mut scored = LexicalRanker::new().rank(&query, &keyword_refs, candidates);
        scored.truncate(self.max_candidates);
        scored.truncate(max_results.max(1));

        Ok(ValidationOutcome {
            scored,
            considered,
            skipped: 0,
            lexical_fallback: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_kernel::ast::AstSnippet;

    fn snippet(symbol: &str) -> AstSnippet {
        AstSnippet {
            file_path: std::path::PathBuf::from("src/lib.rs"),
            symbol_name: symbol.to_string(),
            signature: format!("fn {symbol}()"),
            code: format!("fn {symbol}() {{}}"),
            start_line: 1,
            end_line: 1,
        }
    }

    #[tokio::test]
    async fn every_candidate_is_ranked_and_reported_as_lexical() {
        let client = ValidationClient::new();
        let candidates = vec![snippet("alpha_handler"), snippet("beta_handler")];

        let outcome = client
            .validate_with_outcome(
                "handler".to_string(),
                vec!["handler".to_string()],
                candidates,
                0.65,
                10,
            )
            .await
            .unwrap();

        assert_eq!(outcome.scored.len(), 2);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.skipped, 0);
        assert!(outcome.lexical_fallback);
    }

    #[tokio::test]
    async fn the_candidate_cap_is_declared() {
        let client = ValidationClient::new().with_max_candidates(1);
        let candidates = vec![snippet("alpha_handler"), snippet("beta_handler")];

        let outcome = client
            .validate_with_outcome(
                "handler".to_string(),
                vec!["handler".to_string()],
                candidates,
                0.65,
                10,
            )
            .await
            .unwrap();

        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.scored.len(), 1);
    }
}
