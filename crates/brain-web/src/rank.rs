use crate::model::Block;

const STOPWORDS: &[&str] = &[
    "a", "o", "as", "os", "de", "da", "do", "das", "dos", "e", "em", "na", "no", "nas", "nos",
    "para", "por", "que", "um", "uma", "com", "como", "the", "an", "and", "or", "of", "to", "in",
    "on", "for", "is", "are", "was", "with", "how", "what", "why",
];

/// Rank blocks against the question, best first.
///
/// BM25 over the page's own blocks: cheap, deterministic, and enough to turn a
/// 30k-token document into the 800 tokens that answer. With no question the
/// document order is the ranking, which is what a reader expects.
pub fn rank_blocks(blocks: &[Block], question: &str) -> Vec<usize> {
    let terms = tokenize(question);
    if terms.is_empty() {
        return (0..blocks.len()).collect();
    }

    let documents: Vec<String> = blocks
        .iter()
        .map(|block| format!("{} {}", block.heading, block.text).to_lowercase())
        .collect();
    let total = documents.len() as f32;
    let average = documents
        .iter()
        .map(|document| document.chars().count() as f32)
        .sum::<f32>()
        / total.max(1.0);

    let mut scored: Vec<(usize, f32)> = documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let length = document.chars().count() as f32;
            let mut score = 0.0f32;
            for term in &terms {
                let frequency = document.matches(term.as_str()).count() as f32;
                if frequency == 0.0 {
                    continue;
                }
                let containing = documents
                    .iter()
                    .filter(|other| other.contains(term.as_str()))
                    .count() as f32;
                let idf = ((total + 1.0) / (containing + 1.0)).ln() + 1.0;
                let k1 = 1.2f32;
                let b = 0.75f32;
                score += idf * (frequency * (k1 + 1.0))
                    / (frequency + k1 * (1.0 - b + b * length / average.max(1.0)));
            }
            (index, score)
        })
        .collect();

    // Ties keep document order so two runs of the same query are byte-stable.
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(index, _)| index).collect()
}

fn tokenize(question: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in question.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() < 3 || STOPWORDS.contains(&word.as_str()) {
            continue;
        }
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out
}
