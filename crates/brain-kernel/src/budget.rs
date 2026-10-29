/// One atomic piece of a tool response.
///
/// The unit of truncation is the section, never the character: a response that
/// loses half a caller list looks complete and misleads the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub heading: String,
    pub body: String,
    /// 0 is disposable, 255 is never dropped.
    pub priority: u8,
}

/// Characters per estimated token.
///
/// 3.3 is deliberately conservative for source code: Rust and JSON tokenize
/// near 3.0-3.3 chars/token, while prose sits near 4.0. A ceiling that
/// undershoots the budget is worse than one that trims a little early: the
/// client pays for the overflow on every call.
pub const CHARS_PER_TOKEN: f64 = 3.3;

pub fn estimate_tokens(text: &str) -> usize {
    ((text.chars().count() as f64) / CHARS_PER_TOKEN).ceil() as usize
}

fn section_tokens(section: &Section) -> usize {
    estimate_tokens(&section.heading) + estimate_tokens(&section.body)
}

/// Result of fitting sections into a budget.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Sections that fit, in the order the tool produced them.
    pub included: Vec<Section>,
    /// Sections that did not fit, in the order the tool produced them.
    pub omitted: Vec<Section>,
    /// Estimated cost of what was left out.
    pub omitted_tokens: usize,
}

impl Selection {
    fn needs_handle(&self) -> bool {
        !self.omitted.is_empty()
    }

    /// The omitted sections as one paginable body, each preceded by its heading
    /// so a retrieved excerpt always says what it belongs to.
    pub fn omitted_body(&self) -> String {
        let mut out = String::new();
        for section in &self.omitted {
            out.push_str(&section.heading);
            out.push('\n');
            out.push_str(&section.body);
            out.push('\n');
        }
        out
    }
}

/// Picks whole sections by descending priority until the budget is spent.
///
/// Ties keep tool order. The highest-priority section enters even if it alone
/// exceeds the budget: losing the main answer to honour the envelope would
/// defeat the purpose.
pub fn select(sections: &[Section], budget_tokens: usize) -> Selection {
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by(|&a, &b| sections[b].priority.cmp(&sections[a].priority));

    let mut keep = vec![false; sections.len()];
    let mut used = 0usize;
    let mut first = true;

    for idx in order {
        let cost = section_tokens(&sections[idx]);
        if first || used + cost <= budget_tokens {
            keep[idx] = true;
            used += cost;
            first = false;
        }
    }

    let mut selection = Selection::default();
    for (idx, section) in sections.iter().enumerate() {
        if keep[idx] {
            selection.included.push(section.clone());
        } else {
            selection.omitted.push(section.clone());
            selection.omitted_tokens += section_tokens(section);
        }
    }
    selection
}

/// Splits a section that alone exceeds the budget into self-labelled parts.
///
/// A section can be bigger than the whole ceiling (a 300-line symbol, a whole
/// file). Emitting it whole blows the budget; dropping it hides the answer.
/// Each part keeps the original heading plus `· parte i/n`, and a Markdown
/// code fence is reopened per part so every fragment still renders as code.
/// The omitted parts go to the store through the normal envelope, so nothing
/// becomes unreachable.
pub fn split_section(section: &Section, budget_tokens: usize) -> Vec<Section> {
    if budget_tokens == 0 || section_tokens(section) <= budget_tokens {
        return vec![section.clone()];
    }

    // Leave room for the heading and the `· parte i/n` suffix.
    let heading_tokens = estimate_tokens(&section.heading) + 8;
    let body_budget = budget_tokens.saturating_sub(heading_tokens).max(1);
    let body_chars = (body_budget as f64 * CHARS_PER_TOKEN).floor() as usize;

    let (prefix, inner, suffix) = split_fence(&section.body);
    let chunks = chunk_lines(&inner, body_chars);
    let total = chunks.len();
    if total <= 1 {
        return vec![section.clone()];
    }

    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| Section {
            heading: format!("{} · parte {}/{}", section.heading, index + 1, total),
            body: format!("{prefix}{chunk}{suffix}"),
            priority: section.priority,
        })
        .collect()
}

/// Separates a body into (preamble + opening fence, inner text, closing fence).
/// A body without a fence is returned whole.
fn split_fence(body: &str) -> (String, String, String) {
    let trimmed = body.trim_end_matches('\n');
    let lines: Vec<&str> = trimmed.lines().collect();
    let Some(open) = lines.iter().position(|line| line.starts_with("```")) else {
        return (String::new(), body.to_string(), String::new());
    };
    let Some(close) = lines.iter().rposition(|line| line.trim_end() == "```") else {
        return (String::new(), body.to_string(), String::new());
    };
    if close <= open {
        return (String::new(), body.to_string(), String::new());
    }

    (
        format!("{}\n", lines[..=open].join("\n")),
        lines[open + 1..close].join("\n"),
        "```\n".to_string(),
    )
}

/// Splits text into line-aligned chunks of at most `max_chars` characters.
///
/// A single line longer than the ceiling is cut on a char boundary so the loop
/// always makes progress (minified files exist).
fn chunk_lines(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        let line_chars = line.chars().count();
        let separator = usize::from(!current.is_empty());
        if !current.is_empty() && current.chars().count() + separator + line_chars > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        if line_chars > max_chars {
            for piece in hard_split(line, max_chars) {
                if !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                chunks.push(piece);
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn hard_split(line: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        current.push(ch);
        if current.chars().count() >= max_chars {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Renders included sections in tool order, prefixed by a truncation envelope
/// only when something was left out.
///
/// `handle` is supplied by the caller because the store owns id generation; a
/// selection with omissions and no handle still renders, minus the recovery
/// line.
pub fn render(selection: &Selection, handle: Option<&str>) -> String {
    let mut out = String::new();

    if selection.needs_handle() {
        out.push_str(&format!(
            "> Resposta truncada: {} seções omitidas (~{} tokens estimados).\n",
            selection.omitted.len(),
            selection.omitted_tokens
        ));
        if let Some(handle) = handle {
            out.push_str(&format!(
                "> Recupere com `brain_peek` — handle: `{handle}`.\n"
            ));
        }
        out.push('\n');
    }

    for section in &selection.included {
        out.push_str(&section.heading);
        out.push('\n');
        out.push_str(&section.body);
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(heading: &str, body_chars: usize, priority: u8) -> Section {
        Section {
            heading: heading.to_string(),
            body: "x".repeat(body_chars),
            priority,
        }
    }

    fn section_of(heading: &str, fill: char, body_chars: usize, priority: u8) -> Section {
        Section {
            heading: heading.to_string(),
            body: fill.to_string().repeat(body_chars),
            priority,
        }
    }

    #[test]
    fn a_section_that_fits_is_not_split() {
        let sections = vec![section("small", 350, 100)];
        let parts = split_section(&sections[0], 1000);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].heading, "small");
    }

    #[test]
    fn an_oversized_section_is_split_into_labelled_parts() {
        let body: Vec<String> = (0..200).map(|i| format!("linha {i}")).collect();
        let section = Section {
            heading: "symbol".to_string(),
            body: body.join("\n"),
            priority: 255,
        };

        let parts = split_section(&section, 100);
        assert!(parts.len() > 1, "{parts:?}");
        assert!(parts[0].heading.contains("parte 1/"));
        assert!(parts.last().unwrap().heading.contains("parte 2/") || parts.len() > 2);

        let reassembled = parts
            .iter()
            .map(|part| part.body.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(reassembled, body.join("\n"));
    }

    #[test]
    fn splitting_a_code_fence_reopens_it_in_every_part() {
        let body: Vec<String> = (0..200).map(|i| format!("    let x{i} = {i};")).collect();
        let section = Section {
            heading: "fn".to_string(),
            body: format!("```rust\n{}\n```\n", body.join("\n")),
            priority: 255,
        };

        let parts = split_section(&section, 80);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.body.starts_with("```rust\n"), "{part:?}");
            assert!(part.body.ends_with("```\n"), "{part:?}");
        }
    }

    #[test]
    fn section_that_fits_is_not_cut() {
        // ~350 chars / 3.5 = 100 tokens, budget 200 leaves room.
        let sections = vec![section("a", 350, 100)];
        let selected = select(&sections, 200);

        assert_eq!(selected.included.len(), 1);
        assert!(selected.omitted.is_empty());
        assert_eq!(selected.omitted_tokens, 0);
    }

    #[test]
    fn priority_decides_what_is_omitted() {
        // Both alone fit (100 tokens each) but together exceed a 150 budget.
        let sections = vec![section("low", 350, 10), section("high", 350, 200)];
        let selected = select(&sections, 150);

        let included: Vec<&str> = selected
            .included
            .iter()
            .map(|s| s.heading.as_str())
            .collect();
        let omitted: Vec<&str> = selected
            .omitted
            .iter()
            .map(|s| s.heading.as_str())
            .collect();

        assert_eq!(included, vec!["high"]);
        assert_eq!(omitted, vec!["low"]);
    }

    #[test]
    fn highest_priority_section_enters_even_when_it_alone_exceeds_budget() {
        let sections = vec![section("huge", 7000, 255)]; // 2000 tokens
        let selected = select(&sections, 100);

        assert_eq!(selected.included.len(), 1);
        assert_eq!(selected.included[0].heading, "huge");
    }

    #[test]
    fn rendering_keeps_the_original_order_not_the_priority_order() {
        let sections = vec![section("first", 350, 10), section("second", 350, 200)];
        let selected = select(&sections, 150);
        let rendered = render(&selected, None);

        assert!(
            !rendered.contains("first"),
            "omitted section leaked: {rendered}"
        );
        // Truncated: the envelope leads, then the included body in tool order.
        assert!(rendered.contains("second"));

        // With room for both, the original order must survive.
        let both = select(&sections, 1000);
        let rendered = render(&both, None);
        assert!(rendered.find("first").unwrap() < rendered.find("second").unwrap());
    }

    #[test]
    fn no_section_is_ever_emitted_half_way() {
        let sections = vec![
            section_of("big", 'a', 3500, 255),
            section_of("other", 'b', 3500, 100),
        ];
        let selected = select(&sections, 1200);
        let rendered = render(&selected, Some("h_deadbeef"));

        assert_eq!(selected.included.len(), 1);
        // The included body appears whole; the omitted body does not appear at all.
        assert!(rendered.contains(&"a".repeat(3500)));
        assert!(!rendered.contains(&"b".repeat(3500)));
    }

    #[test]
    fn priority_ties_keep_tool_order() {
        let sections = vec![section("one", 350, 100), section("two", 350, 100)];
        let selected = select(&sections, 150);

        assert_eq!(selected.included[0].heading, "one");
        assert_eq!(selected.omitted[0].heading, "two");
    }

    #[test]
    fn envelope_announces_the_omitted_sections_and_handle() {
        let sections = vec![section("kept", 350, 255), section("dropped", 3500, 10)];
        let selected = select(&sections, 500);
        let rendered = render(&selected, Some("h_1234abcd"));

        assert!(rendered.starts_with("> Resposta truncada: 1 seções omitidas"));
        assert!(rendered.contains("`h_1234abcd`"));
    }

    #[test]
    fn no_omission_means_no_envelope() {
        let sections = vec![section("only", 350, 100)];
        let selected = select(&sections, 1000);
        let rendered = render(&selected, Some("h_1234abcd"));

        assert!(!rendered.contains("truncada"));
        assert!(rendered.starts_with("only"));
    }

    #[test]
    fn omitted_body_concatenates_omitted_sections_with_headings() {
        let sections = vec![
            section("kept", 350, 255),
            section("drop-a", 350, 10),
            section("drop-b", 350, 20),
        ];
        let selected = select(&sections, 200);
        let body = selected.omitted_body();

        assert!(body.starts_with("drop-a\n"));
        assert!(body.contains("drop-b\n"));
        assert!(!body.contains("kept"));
    }
}
