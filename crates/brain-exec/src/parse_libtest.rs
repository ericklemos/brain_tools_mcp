//! Parses libtest's text output into per-test failure sections.
//!
//! `cargo test` has no stable JSON output for test results, but it always emits
//! text delimited by `---- <name> stdout ----`, which has been stable for years.
//! Within a block the useful part is the panic — that is where `left != right`
//! lives — plus the tail of the log. The middle is noise: a test that prints
//! five thousand lines of tracing buries the assert.

/// How many trailing lines of a failing test's output to keep.
const TAIL_LINES: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct TestFailure {
    pub name: String,
    /// The panic block, verbatim, plus the tail of the output.
    pub excerpt: String,
    pub omitted_lines: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestReport {
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<TestFailure>,
}

impl TestReport {
    pub fn is_empty(&self) -> bool {
        self.passed == 0 && self.failed == 0 && self.failures.is_empty()
    }
}

/// Parses the text half of a `cargo test` run.
///
/// Summaries are aggregated across every test binary: `cargo test` runs several,
/// and the agent needs one total, not one per target.
pub fn parse(text: &str) -> TestReport {
    let mut report = TestReport::default();
    let lines: Vec<&str> = text.lines().collect();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if let Some(rest) = line.strip_prefix("test result: ") {
            accumulate_summary(rest, &mut report);
            i += 1;
            continue;
        }

        if let Some(name) = block_header(line) {
            // The block runs until the next header or summary line.
            let mut end = i + 1;
            while end < lines.len()
                && block_header(lines[end]).is_none()
                && !lines[end].starts_with("test result: ")
            {
                end += 1;
            }
            report
                .failures
                .push(excerpt_for(name, &lines[i + 1..end]));
            i = end;
            continue;
        }

        i += 1;
    }

    report
}

/// `---- tests::failing_plain stdout ----` → `tests::failing_plain`
fn block_header(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("---- ")?;
    let name = rest.strip_suffix(" stdout ----")?;
    Some(name.trim())
}

/// `FAILED. 1 passed; 2 failed; 0 ignored; ...`
///
/// The first segment carries the verdict (`FAILED. 1 passed`), so the count is
/// the last word of the segment, not the whole of it.
fn accumulate_summary(rest: &str, report: &mut TestReport) {
    for part in rest.split(';') {
        let words: Vec<&str> = part.split_whitespace().collect();
        let Some(&unit) = words.last() else { continue };
        let Some(count) = words
            .len()
            .checked_sub(2)
            .and_then(|at| words.get(at))
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };
        match unit {
            "passed" => report.passed += count,
            "failed" => report.failed += count,
            _ => {}
        }
    }
}

/// Keeps the panic block intact and the last [`TAIL_LINES`] lines of output.
///
/// The panic is located rather than assumed to be first: with `--nocapture` off
/// the block is the test's stdout followed by the panic, and a snapshot test can
/// emit megabytes before failing.
fn excerpt_for(name: &str, block: &[&str]) -> TestFailure {
    // libtest puts a blank line after the header; counting it as an omitted line
    // would announce a gap that holds nothing.
    let block = match block.iter().position(|line| !line.trim().is_empty()) {
        Some(first) => &block[first..],
        None => block,
    };

    let panic_at = block
        .iter()
        .position(|line| line.contains("panicked at") || line.contains("assertion"));

    let keep: &[&str] = match panic_at {
        Some(start) => &block[start.min(block.len())..],
        None => {
            let start = block.len().saturating_sub(TAIL_LINES);
            &block[start..]
        }
    };

    let omitted = block.len().saturating_sub(keep.len());
    let mut excerpt = keep.join("\n").trim_end().to_string();

    if omitted > 0 {
        excerpt = format!("[…{omitted} linhas omitidas]\n{excerpt}");
    }

    TestFailure {
        name: name.to_string(),
        excerpt,
        omitted_lines: omitted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = include_str!("../tests/fixtures/test_mixed.jsonl");

    /// The fixture is cargo's raw stdout: JSON lines interleaved with libtest
    /// text. Only the text half reaches this parser.
    fn libtest_text() -> String {
        MIXED
            .lines()
            .filter(|line| !line.trim_start().starts_with('{'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn totals_are_aggregated_across_every_test_binary() {
        let report = parse(&libtest_text());

        // `1 passed; 2 failed` for the lib, then `0 passed; 0 failed` for doctests.
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 2);
        assert_eq!(report.failures.len(), 2);
    }

    #[test]
    fn the_assertion_block_survives_intact() {
        let report = parse(&libtest_text());
        let plain = report
            .failures
            .iter()
            .find(|f| f.name == "tests::failing_plain")
            .expect("failing_plain block");

        assert!(plain.excerpt.contains("panicked at src/lib.rs:18:9"));
        assert!(plain.excerpt.contains("assertion `left == right` failed"));
        assert!(plain.excerpt.contains("  left: 2"));
        assert!(plain.excerpt.contains(" right: 3"));
        assert_eq!(plain.omitted_lines, 0);
    }

    #[test]
    fn a_noisy_test_keeps_the_assert_and_drops_the_middle() {
        let report = parse(&libtest_text());
        let noisy = report
            .failures
            .iter()
            .find(|f| f.name == "tests::failing_with_noise")
            .expect("failing_with_noise block");

        assert!(noisy.excerpt.contains("panicked at src/lib.rs:13:9"));
        assert!(noisy.excerpt.contains("  left: 4"));
        assert!(noisy.excerpt.contains(" right: 5"));
        assert!(
            !noisy.excerpt.contains("noise line 0"),
            "middle must be dropped: {}",
            noisy.excerpt
        );
        assert!(
            noisy.omitted_lines > 0,
            "omission must be counted and announced"
        );
        assert!(noisy.excerpt.starts_with("[…"), "Gap is announced up front");
    }

    #[test]
    fn a_test_that_prints_without_panicking_keeps_its_tail() {
        let text = "---- tests::weird stdout ----\nline one\nline two\n";
        let report = parse(text);

        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].excerpt.contains("line two"));
    }

    #[test]
    fn a_green_run_has_no_failures() {
        let report = parse("running 3 tests\ntest result: ok. 3 passed; 0 failed; 0 ignored\n");
        assert_eq!(report.passed, 3);
        assert_eq!(report.failed, 0);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn text_without_a_summary_line_is_not_mistaken_for_results() {
        let report = parse("some stray output\n");
        assert!(report.is_empty());
    }
}
