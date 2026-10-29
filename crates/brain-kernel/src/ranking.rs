use crate::ast::AstSnippet;

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredSnippet {
    pub snippet: AstSnippet,
    pub score: f32,
}

#[derive(Default)]
pub struct LexicalRanker;

impl LexicalRanker {
    pub fn new() -> Self {
        Self
    }

    pub fn rank(
        &self,
        query: &str,
        keywords: &[&str],
        snippets: Vec<AstSnippet>,
    ) -> Vec<ScoredSnippet> {
        let ranked = self.rank_indices(query, keywords, &snippets);
        let mut slots: Vec<Option<AstSnippet>> = snippets.into_iter().map(Some).collect();
        ranked
            .into_iter()
            .filter_map(|(index, score)| {
                slots[index]
                    .take()
                    .map(|snippet| ScoredSnippet { snippet, score })
            })
            .collect()
    }

    /// Same ranking as [`Self::rank`], but borrows the snippets and returns
    /// indices. Callers that only need the top K avoid cloning every candidate.
    pub fn rank_indices(
        &self,
        query: &str,
        keywords: &[&str],
        snippets: &[AstSnippet],
    ) -> Vec<(usize, f32)> {
        if snippets.is_empty() {
            return Vec::new();
        }

        let scores = self.score(query, keywords, snippets);
        let mut ranked: Vec<(usize, f32)> = scores.into_iter().enumerate().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked
    }

    fn score(&self, query: &str, keywords: &[&str], snippets: &[AstSnippet]) -> Vec<f32> {
        let mut terms: Vec<String> = keywords
            .iter()
            .map(|keyword| keyword.to_lowercase())
            .filter(|keyword| !keyword.trim().is_empty())
            .collect();

        for word in query.split_whitespace() {
            let clean = word
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_lowercase();
            if clean.len() >= 2 && !terms.contains(&clean) {
                terms.push(clean);
            }
        }

        let mut scores: Vec<f32> = snippets
            .iter()
            .map(|snippet| {
                let symbol_lower = snippet.symbol_name.to_lowercase();
                let sig_lower = snippet.signature.to_lowercase();

                let mut raw_score = 0.0f32;
                for term in &terms {
                    if symbol_lower == *term {
                        raw_score += 10.0;
                    } else if symbol_lower.contains(term) {
                        raw_score += 5.0;
                    }

                    if sig_lower.contains(term) {
                        raw_score += 3.0;
                    }

                    // Count in place: lowercasing the whole snippet per term
                    // used to allocate a copy of every candidate body.
                    let occurrences = count_occurrences(&snippet.code, term);
                    if occurrences > 0 {
                        raw_score += (occurrences as f32).min(5.0) * 0.8;
                    }
                }
                raw_score
            })
            .collect();

        let max_score = scores.iter().copied().fold(0.0f32, f32::max);
        if max_score > 0.0 {
            for score in &mut scores {
                *score = (*score / max_score).clamp(0.0, 1.0);
            }
        } else {
            scores.fill(0.5);
        }
        scores
    }
}

/// Non-overlapping, ASCII case-insensitive count. Terms with non-ASCII bytes
/// fall back to the allocating path so Unicode matching keeps working.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    if !needle.is_ascii() {
        return haystack
            .to_lowercase()
            .matches(&needle.to_lowercase())
            .count();
    }

    let bytes = haystack.as_bytes();
    let pattern = needle.as_bytes();
    let mut count = 0;
    let mut at = 0;
    while at + pattern.len() <= bytes.len() {
        if bytes[at..at + pattern.len()].eq_ignore_ascii_case(pattern) {
            count += 1;
            at += pattern.len();
        } else {
            at += 1;
        }
    }
    count
}
