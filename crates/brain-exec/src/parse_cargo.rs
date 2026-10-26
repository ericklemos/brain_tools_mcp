//! Parses `cargo --message-format=json` output into grouped diagnostics.
//!
//! Nothing here reads the source tree: the JSON already carries the source line
//! and the highlight columns in `spans[].text[]`, so the report is built from
//! the compiler's own view of the file — it cannot drift from what was compiled.

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Error,
    Warning,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
        }
    }
}

/// One place a diagnostic fired.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub line: usize,
    pub column: usize,
    pub label: String,
    pub source_line: Option<String>,
    /// 1-based, half-open columns into `source_line`.
    pub highlight: Option<(usize, usize)>,
    pub in_macro: bool,
}

/// Every occurrence of one `(code, file)` pair.
///
/// Grouping is what makes a 40-error build readable: the same `E0308` in twelve
/// call sites is one decision, not twelve.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub level: Level,
    pub code: String,
    pub message: String,
    pub file: String,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub groups: Vec<Group>,
    /// `build-finished.success`, when the run got that far.
    pub build_success: Option<bool>,
    /// Lines that were not JSON — on `cargo test` this is the libtest text.
    pub other_lines: Vec<String>,
    pub error_count: usize,
    pub warning_count: usize,
}

impl Report {
    pub fn errors(&self) -> impl Iterator<Item = &Group> {
        self.groups.iter().filter(|g| g.level == Level::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Group> {
        self.groups.iter().filter(|g| g.level == Level::Warning)
    }
}

/// Splits a cargo stdout stream into diagnostics and non-JSON text.
pub fn parse_stdout(text: &str) -> Report {
    let mut report = Report::default();
    // (level, code, file) -> index into report.groups, preserving first-seen order.
    let mut index: HashMap<(Level, String, String), usize> = HashMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                report.other_lines.push(line.to_string());
                continue;
            }
        };

        match value.get("reason").and_then(Value::as_str) {
            Some("build-finished") => {
                report.build_success = value.get("success").and_then(Value::as_bool);
            }
            Some("compiler-message") => {
                if let Some(diagnostic) = value.get("message") {
                    collect(&mut report, &mut index, diagnostic);
                }
            }
            _ => {}
        }
    }

    report
}

fn collect(
    report: &mut Report,
    index: &mut HashMap<(Level, String, String), usize>,
    diagnostic: &Value,
) {
    let level = match diagnostic.get("level").and_then(Value::as_str) {
        Some("error") => Level::Error,
        Some("warning") => Level::Warning,
        // `failure-note`, `note`, `help` are commentary about a diagnostic that
        // was already recorded; counting them would double the error count.
        _ => return,
    };

    let (occurrence, file) = match locate(diagnostic) {
        Some(found) => found,
        // No span at all (e.g. a crate-level error): keep it, with no location.
        None => (
            Occurrence {
                line: 0,
                column: 0,
                label: String::new(),
                source_line: None,
                highlight: None,
                in_macro: false,
            },
            String::from("<sem localização>"),
        ),
    };

    let code = diagnostic
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(level.as_str())
        .to_string();

    let message = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    match level {
        Level::Error => report.error_count += 1,
        Level::Warning => report.warning_count += 1,
    }

    let key = (level, code.clone(), file.clone());
    match index.get(&key) {
        Some(&at) => report.groups[at].occurrences.push(occurrence),
        None => {
            index.insert(key, report.groups.len());
            report.groups.push(Group {
                level,
                code,
                message,
                file,
                occurrences: vec![occurrence],
            });
        }
    }
}

/// Picks the span worth showing, and whether it sits inside a macro expansion.
///
/// A span inside a macro points at the macro body, where the agent cannot fix
/// anything; the call site can be found on a secondary span with no expansion.
fn locate(diagnostic: &Value) -> Option<(Occurrence, String)> {
    let spans = diagnostic.get("spans").and_then(Value::as_array)?;
    if spans.is_empty() {
        return None;
    }

    let primary = spans
        .iter()
        .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| spans.first())?;

    let in_macro = primary.get("expansion").is_some_and(|e| e.is_object());

    let display = if in_macro {
        spans
            .iter()
            .find(|s| s.get("expansion").is_some_and(Value::is_null))
            .unwrap_or(primary)
    } else {
        primary
    };

    let file = display
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or("<desconhecido>")
        .to_string();

    let line = display.get("line_start").and_then(Value::as_u64).unwrap_or(0) as usize;
    let column = display
        .get("column_start")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    // The label and the highlighted text come from the original span: they are
    // what the compiler said about the failure, even when the location moved.
    let label = primary
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let text_entry = primary
        .get("text")
        .and_then(Value::as_array)
        .and_then(|texts| texts.first());

    let source_line = text_entry
        .and_then(|t| t.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let highlight = text_entry.and_then(|t| {
        let start = t.get("highlight_start")?.as_u64()? as usize;
        let end = t.get("highlight_end")?.as_u64()? as usize;
        (end > start).then_some((start, end))
    });

    Some((
        Occurrence {
            line,
            column,
            label,
            source_line,
            highlight,
            in_macro,
        },
        file,
    ))
}

/// Strips the repository root so the report shows repo-relative paths.
pub fn relative(path: &str, root: &Path) -> String {
    let as_path = Path::new(path);
    match as_path.strip_prefix(root) {
        Ok(stripped) => stripped.display().to_string(),
        Err(_) => path.to_string(),
    }
}

/// Occurrences per section. A group with forty hits is one decision, but it is
/// also forty code excerpts — past this size it is split into consecutive
/// chunks so the budget can keep the first ones and leave the rest recoverable.
const OCCURRENCES_PER_BLOCK: usize = 5;

/// Renders error groups as budget-ready sections.
///
/// Each section is semantically whole: a heading that names its `(code, file)`
/// and range, then complete occurrence blocks. The budget can drop a section
/// without ever cutting an occurrence in half.
pub fn error_sections(groups: &[Group], root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();

    for group in groups.iter().filter(|g| g.level == Level::Error) {
        let file = relative(&group.file, root);
        let total = group.occurrences.len();
        let chunks: Vec<&[Occurrence]> = group
            .occurrences
            .chunks(OCCURRENCES_PER_BLOCK)
            .collect();

        for (index, chunk) in chunks.iter().enumerate() {
            let heading = if chunks.len() == 1 {
                format!("### `{file}` · {} · {total} ocorrência(s)\n", group.code)
            } else {
                let from = index * OCCURRENCES_PER_BLOCK + 1;
                let to = from + chunk.len() - 1;
                format!(
                    "### `{file}` · {} · ocorrências {from}–{to} de {total}\n",
                    group.code
                )
            };

            let mut body = format!("`{}`\n\n", group.message);
            for occurrence in chunk.iter() {
                body.push_str(&render_occurrence(occurrence));
            }

            out.push((heading, body));
        }
    }

    out
}

/// Renders errors as one string, for callers that do not budget.
pub fn render_errors(groups: &[Group], root: &Path) -> String {
    error_sections(groups, root)
        .into_iter()
        .map(|(heading, body)| format!("{heading}{body}"))
        .collect()
}

fn render_occurrence(occurrence: &Occurrence) -> String {
    let mut out = String::new();

    let location = if occurrence.line == 0 {
        "—".to_string()
    } else {
        format!("{}:{}", occurrence.line, occurrence.column)
    };
    let macro_note = if occurrence.in_macro { " `(em macro)`" } else { "" };

    if occurrence.label.is_empty() {
        out.push_str(&format!("- `{location}`{macro_note}\n"));
    } else {
        out.push_str(&format!("- `{location}` {}{macro_note}\n", occurrence.label));
    }

    if let Some(source) = &occurrence.source_line {
        out.push_str(&format!("```\n{source}\n"));
        if let Some((start, end)) = occurrence.highlight {
            let width = end.saturating_sub(start).max(1);
            out.push_str(&format!(
                "{}{}\n",
                " ".repeat(start.saturating_sub(1)),
                "^".repeat(width)
            ));
        }
        out.push_str("```\n");
    }

    out
}

/// Warning digest: the count and the three codes that fire most, each with one
/// example location. Bodies are not shown — warnings are rarely the task, but
/// silently dropping them hides why a build is noisy.
pub fn render_warning_digest(groups: &[Group], root: &Path) -> String {
    let warnings: Vec<&Group> = groups.iter().filter(|g| g.level == Level::Warning).collect();
    if warnings.is_empty() {
        return String::new();
    }

    let total: usize = warnings.iter().map(|g| g.occurrences.len()).sum();

    let mut by_code: Vec<(&str, usize, &Group, &Occurrence)> = Vec::new();
    for group in &warnings {
        match by_code.iter_mut().find(|(code, _, _, _)| *code == group.code) {
            Some(entry) => entry.1 += group.occurrences.len(),
            None => {
                if let Some(first) = group.occurrences.first() {
                    by_code.push((group.code.as_str(), group.occurrences.len(), group, first));
                }
            }
        }
    }
    by_code.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    let mut out = format!("### Warnings — {total}\n\n");
    for (code, count, group, occurrence) in by_code.iter().take(3) {
        let file = relative(&group.file, root);
        out.push_str(&format!(
            "- `{code}` × {count} — ex.: `{file}:{}`\n",
            occurrence.line
        ));
    }
    if by_code.len() > 3 {
        out.push_str(&format!(
            "\n_+{} outros códigos de warning._\n",
            by_code.len() - 3
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERRORS: &str = include_str!("../tests/fixtures/check_errors.jsonl");
    const WARNINGS: &str = include_str!("../tests/fixtures/check_warnings.jsonl");
    const MACRO: &str = include_str!("../tests/fixtures/check_macro.jsonl");

    #[test]
    fn same_code_in_one_file_becomes_one_group_with_all_occurrences() {
        let report = parse_stdout(ERRORS);

        let errors: Vec<&Group> = report.errors().collect();
        assert_eq!(errors.len(), 1, "expected one group, got {errors:#?}");
        assert_eq!(errors[0].code, "E0308");
        assert_eq!(errors[0].file, "src/main.rs");
        assert_eq!(errors[0].occurrences.len(), 2);
        assert_eq!(errors[0].occurrences[0].line, 6);
        assert_eq!(errors[0].occurrences[1].line, 7);
    }

    #[test]
    fn failure_notes_do_not_inflate_the_error_count() {
        let report = parse_stdout(ERRORS);
        // The fixture carries a `failure-note` record; only the two real errors count.
        assert_eq!(report.error_count, 2);
        assert_eq!(report.build_success, Some(false));
    }

    #[test]
    fn source_line_and_highlight_come_from_the_compiler_record() {
        let report = parse_stdout(ERRORS);
        let occurrence = &report.errors().next().unwrap().occurrences[0];

        assert_eq!(
            occurrence.source_line.as_deref(),
            Some(r#"    let a: u32 = "not a number";"#)
        );
        let (start, end) = occurrence.highlight.expect("highlight columns");
        assert_eq!(start, 18);
        assert!(end > start);
        assert_eq!(occurrence.label, "expected `u32`, found `&str`");
    }

    #[test]
    fn warnings_are_counted_by_code_not_listed_one_by_one() {
        let report = parse_stdout(WARNINGS);
        assert_eq!(report.error_count, 0);
        assert_eq!(report.warning_count, 4);

        let digest = render_warning_digest(&report.groups, Path::new(""));
        assert!(digest.contains("Warnings — 4"), "{digest}");
        assert!(digest.contains("`dead_code` × 2"), "{digest}");
        assert!(digest.contains("`unused_variables` × 2"), "{digest}");
        // One example per code: the `unused_variables` group has occurrences on
        // lines 7 and 10, and only the first is shown.
        assert_eq!(digest.matches("ex.:").count(), 2, "{digest}");
        assert!(!digest.contains(":10"), "second occurrence leaked: {digest}");
    }

    #[test]
    fn a_span_inside_a_macro_reports_the_call_site_and_says_so() {
        let report = parse_stdout(MACRO);
        let group = report
            .errors()
            .find(|g| g.code == "E0277")
            .expect("macro error group");

        let occurrence = &group.occurrences[0];
        assert!(occurrence.in_macro);
        assert_eq!(
            occurrence.line, 6,
            "must point at the call site, not the macro body"
        );

        let rendered = render_errors(&report.groups, Path::new(""));
        assert!(rendered.contains("(em macro)"), "{rendered}");
    }

    #[test]
    fn rendered_errors_keep_the_caret_line() {
        let report = parse_stdout(ERRORS);
        let rendered = render_errors(&report.groups, Path::new(""));

        assert!(
            rendered.contains("### `src/main.rs` · E0308 · 2 ocorrência(s)"),
            "{rendered}"
        );
        assert!(rendered.contains("^^^^^^^^^^^^^^"), "{rendered}");
    }

    #[test]
    fn a_large_group_is_chunked_so_it_stays_budgetable() {
        // 40 occurrences of one code in one file: one group, but not one section
        // — otherwise the budget could only take it whole or drop it whole.
        let report = parse_stdout(&synthetic_errors(40));
        let group = report.errors().next().expect("one group");
        assert_eq!(group.occurrences.len(), 40);

        let sections = error_sections(&report.groups, Path::new(""));
        assert_eq!(sections.len(), 8, "40 occurrences / 5 per block");
        assert!(sections[0].0.contains("ocorrências 1–5 de 40"), "{}", sections[0].0);
        assert!(sections[7].0.contains("ocorrências 36–40 de 40"), "{}", sections[7].0);
    }

    #[test]
    fn chunking_never_splits_an_occurrence() {
        let report = parse_stdout(&synthetic_errors(12));
        let sections = error_sections(&report.groups, Path::new(""));

        // Every chunk carries whole occurrence blocks: a source line plus carets.
        for (_, body) in &sections {
            assert_eq!(
                body.matches("```").count() % 2,
                0,
                "unbalanced code fence in a chunk: {body}"
            );
        }
        let total_blocks: usize = sections.iter().map(|(_, b)| b.matches("- `").count()).sum();
        assert_eq!(total_blocks, 12);
    }

    /// Builds cargo-shaped JSON with `count` `E0308`s in one file.
    fn synthetic_errors(count: usize) -> String {
        (1..=count)
            .map(|line| {
                serde_json::json!({
                    "reason": "compiler-message",
                    "message": {
                        "level": "error",
                        "message": "mismatched types",
                        "code": { "code": "E0308" },
                        "spans": [{
                            "file_name": "src/main.rs",
                            "line_start": line,
                            "column_start": 18,
                            "is_primary": true,
                            "label": "expected `u32`, found `&str`",
                            "expansion": null,
                            "text": [{
                                "text": format!("    let v{line}: u32 = \"x\";"),
                                "highlight_start": 18,
                                "highlight_end": 21
                            }]
                        }]
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn non_json_lines_are_preserved_for_the_test_parser() {
        let report = parse_stdout("{\"reason\":\"build-finished\",\"success\":true}\ntest result: ok. 1 passed\n");
        assert_eq!(report.other_lines.len(), 1);
        assert!(report.other_lines[0].contains("test result"));
    }

    #[test]
    fn greedy_extra_json_lines_are_ignored() {
        // compiler-artifact and build-script-executed carry no diagnostics.
        let report = parse_stdout("{\"reason\":\"compiler-artifact\",\"target\":{}}\n");
        assert!(report.groups.is_empty());
        assert!(report.other_lines.is_empty());
    }
}
