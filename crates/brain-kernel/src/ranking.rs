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
        if snippets.is_empty() {
            return Vec::new();
        }

        let mut terms: Vec<String> = keywords
            .iter()
            .map(|k| k.to_lowercase())
            .filter(|k| !k.trim().is_empty())
            .collect();

        for word in query.split_whitespace() {
            let clean = word
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_lowercase();
            if clean.len() >= 2 && !terms.contains(&clean) {
                terms.push(clean);
            }
        }

        // Test scaffolding matches the same words as the production code it
        // exercises, but it is rarely what a search for behavior wants. Unless
        // the query asks for tests, demote snippets that look like tests.
        let wants_tests = terms.iter().any(|term| {
            term.contains("test")
                || term.contains("teste")
                || term.contains("fixture")
                || term.contains("mock")
        });

        let mut scored: Vec<ScoredSnippet> = snippets
            .into_iter()
            .map(|snippet| {
                let symbol_lower = snippet.symbol_name.to_lowercase();
                let sig_lower = snippet.signature.to_lowercase();
                let code_lower = snippet.code.to_lowercase();

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

                    let occurrences = code_lower.matches(term).count();
                    if occurrences > 0 {
                        raw_score += (occurrences as f32).min(5.0) * 0.8;
                    }
                }

                let score = if !wants_tests && looks_like_test(&snippet) {
                    raw_score * 0.35
                } else {
                    raw_score
                };

                ScoredSnippet { snippet, score }
            })
            .collect();

        let max_score = scored
            .iter()
            .map(|s| s.score)
            .fold(0.0f32, |acc, val| acc.max(val));

        if max_score > 0.0 {
            for item in &mut scored {
                item.score = (item.score / max_score).clamp(0.0, 1.0);
            }
        } else {
            for item in &mut scored {
                item.score = 0.5;
            }
        }

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

/// Test files and test-only helpers use the same vocabulary as the code they
/// exercise, so lexical ranking alone tends to put them first.
fn looks_like_test(snippet: &AstSnippet) -> bool {
    let in_tests_dir = snippet
        .file_path
        .components()
        .any(|component| component.as_os_str() == std::ffi::OsStr::new("tests"));
    let path = snippet.file_path.to_string_lossy().to_lowercase();
    if in_tests_dir || path.ends_with("_test.rs") || path.ends_with("_tests.rs") {
        return true;
    }

    let code = &snippet.code;
    code.contains("tempdir()")
        || code.contains("#[test]")
        || code.contains("#[tokio::test]")
        || code.contains("#[async_std::test]")
}
