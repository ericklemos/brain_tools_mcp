//! Measurement report, baseline comparison and the human table.

use serde::{Deserialize, Serialize};

/// Bumped when the JSON shape changes so an old baseline is rejected with a
/// clear message instead of being compared field by field.
pub const REPORT_VERSION: u32 = 1;

/// Growth below this many tokens is noise, not regression: the graph's own
/// confidence line and result counters move with the repository.
const REGRESSION_NOISE_FLOOR: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRun {
    pub tool: String,
    /// Tokens the agent pays for the call arguments.
    pub request_tokens: usize,
    /// Tokens the agent pays for the response text.
    pub response_tokens: usize,
    pub chars: usize,
    pub is_error: bool,
    pub millis: u64,
}

impl CallRun {
    pub fn tokens(&self) -> usize {
        self.request_tokens + self.response_tokens
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub question: String,
    pub calls: Vec<CallRun>,
    pub brain_tokens: usize,
    pub brain_chars: usize,
    pub naive_tokens: usize,
    pub naive_bytes: usize,
    pub naive_files: Vec<String>,
    pub millis: u64,
    /// Set when the task could not be completed: the ratio is not printed for
    /// a task that never got its answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TaskRun {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    pub fn factor(&self) -> Option<f64> {
        if !self.ok() || self.brain_tokens == 0 || self.naive_tokens == 0 {
            return None;
        }
        Some(self.naive_tokens as f64 / self.brain_tokens as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaRun {
    pub tools: usize,
    pub chars: usize,
    pub tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    pub version: u32,
    pub server: String,
    pub root: String,
    /// `tools/list` cost. Many clients resend it on every request, so it is
    /// reported separately: it is paid once per turn, not once per question.
    pub schema: SchemaRun,
    pub tasks: Vec<TaskRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub brain_tokens: usize,
    pub naive_tokens: usize,
    pub brain_chars: usize,
    pub naive_bytes: usize,
    pub millis: u64,
}

impl RunReport {
    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for task in self.tasks.iter().filter(|t| t.ok()) {
            totals.brain_tokens += task.brain_tokens;
            totals.naive_tokens += task.naive_tokens;
            totals.brain_chars += task.brain_chars;
            totals.naive_bytes += task.naive_bytes;
            totals.millis += task.millis;
        }
        totals
    }

    /// Share of the naive cost that the server saved, in percent. `None` when
    /// nothing was measured, so the report never prints a fake 0% or 100%.
    pub fn savings_percent(&self) -> Option<f64> {
        let totals = self.totals();
        if totals.naive_tokens == 0 {
            return None;
        }
        Some((1.0 - totals.brain_tokens as f64 / totals.naive_tokens as f64) * 100.0)
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Regression {
    pub task: String,
    /// `task` or `schema`.
    pub scope: String,
    pub baseline_tokens: usize,
    pub current_tokens: usize,
    /// Growth over baseline, in percent.
    pub percent: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comparison {
    pub regressions: Vec<Regression>,
    /// Tasks in the current run with no baseline entry: not a regression.
    pub new_tasks: Vec<String>,
    /// Tasks in the baseline that the current run no longer declares.
    pub missing_tasks: Vec<String>,
}

/// Flags tasks that now pay more than `max_growth_percent` above the baseline.
pub fn compare(baseline: &RunReport, current: &RunReport, max_growth_percent: f64) -> Comparison {
    let mut comparison = Comparison::default();

    for task in &current.tasks {
        let Some(base) = baseline.tasks.iter().find(|b| b.id == task.id) else {
            comparison.new_tasks.push(task.id.clone());
            continue;
        };
        if grew(base.brain_tokens, task.brain_tokens, max_growth_percent) {
            comparison.regressions.push(Regression {
                task: task.id.clone(),
                scope: "task".to_string(),
                baseline_tokens: base.brain_tokens,
                current_tokens: task.brain_tokens,
                percent: growth_percent(base.brain_tokens, task.brain_tokens),
            });
        }
    }

    for base in &baseline.tasks {
        if !current.tasks.iter().any(|t| t.id == base.id) {
            comparison.missing_tasks.push(base.id.clone());
        }
    }

    if grew(
        baseline.schema.tokens,
        current.schema.tokens,
        max_growth_percent,
    ) {
        comparison.regressions.push(Regression {
            task: "tools/list".to_string(),
            scope: "schema".to_string(),
            baseline_tokens: baseline.schema.tokens,
            current_tokens: current.schema.tokens,
            percent: growth_percent(baseline.schema.tokens, current.schema.tokens),
        });
    }

    comparison
}

fn grew(base: usize, now: usize, max_growth_percent: f64) -> bool {
    if now <= base || now - base < REGRESSION_NOISE_FLOOR {
        return false;
    }
    growth_percent(base, now) > max_growth_percent
}

fn growth_percent(base: usize, now: usize) -> f64 {
    if base == 0 {
        return 100.0;
    }
    (now as f64 - base as f64) / base as f64 * 100.0
}

pub fn render(report: &RunReport) -> String {
    let width = report
        .tasks
        .iter()
        .map(|t| t.id.chars().count())
        .max()
        .unwrap_or(6)
        .max("tarefa".len());

    let mut out = String::new();
    out.push_str(&format!(
        "{:<width$}  {:>8}  {:>8}  {:>7}  {:>7}  {}\n",
        "tarefa",
        "brain",
        "naïve",
        "fator",
        "tempo",
        "resultado",
        width = width
    ));

    for task in &report.tasks {
        let result = match &task.error {
            Some(err) => format!("ERRO: {err}"),
            None => "ok".to_string(),
        };
        let factor = task
            .factor()
            .map(|f| format!("{:.1}x", f).replace('.', ","))
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "{:<width$}  {:>8}  {:>8}  {:>7}  {:>7}  {}\n",
            task.id,
            integer(task.brain_tokens),
            integer(task.naive_tokens),
            factor,
            seconds(task.millis),
            result,
            width = width
        ));
    }

    let totals = report.totals();
    let factor = if totals.brain_tokens == 0 {
        "—".to_string()
    } else {
        format!(
            "{:.1}x",
            totals.naive_tokens as f64 / totals.brain_tokens as f64
        )
        .replace('.', ",")
    };
    out.push_str(&format!(
        "{:<width$}  {:>8}  {:>8}  {:>7}  {:>7}\n",
        "TOTAL",
        integer(totals.brain_tokens),
        integer(totals.naive_tokens),
        factor,
        seconds(totals.millis),
        width = width
    ));

    out.push_str(&format!(
        "\nschema tools/list: {} tokens em {} ferramentas (pago a cada requisição, fora das tarefas)\n",
        integer(report.schema.tokens),
        report.schema.tools
    ));
    match report.savings_percent() {
        Some(saved) => out.push_str(&format!(
            "economia: {}% dos tokens que reler os arquivos declarados custaria\n",
            format!("{saved:.1}").replace('.', ",")
        )),
        None => out.push_str("economia: não medida (nenhuma tarefa concluída)\n"),
    }

    out
}

fn integer(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

fn seconds(millis: u64) -> String {
    format!("{:.1}s", millis as f64 / 1000.0).replace('.', ",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, brain: usize, naive: usize) -> TaskRun {
        TaskRun {
            id: id.to_string(),
            question: "q".to_string(),
            calls: vec![CallRun {
                tool: "brain_read".to_string(),
                request_tokens: brain,
                response_tokens: 0,
                chars: brain * 4,
                is_error: false,
                millis: 10,
            }],
            brain_tokens: brain,
            brain_chars: brain * 4,
            naive_tokens: naive,
            naive_bytes: naive * 4,
            naive_files: vec!["a.rs".to_string()],
            millis: 10,
            error: None,
        }
    }

    fn report(tasks: Vec<TaskRun>, schema: usize) -> RunReport {
        RunReport {
            version: REPORT_VERSION,
            server: "target/release/brain-tools".to_string(),
            root: ".".to_string(),
            schema: SchemaRun {
                tools: 11,
                chars: schema * 4,
                tokens: schema,
            },
            tasks,
        }
    }

    #[test]
    fn growth_above_the_threshold_is_a_regression() {
        let base = report(vec![task("a", 100, 1000)], 500);
        let now = report(vec![task("a", 200, 1000)], 500);
        let comparison = compare(&base, &now, 10.0);
        assert_eq!(comparison.regressions.len(), 1);
        assert_eq!(comparison.regressions[0].task, "a");
        assert!((comparison.regressions[0].percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn small_absolute_growth_is_noise() {
        let base = report(vec![task("a", 300, 1000)], 500);
        let now = report(vec![task("a", 340, 1000)], 500);
        assert!(compare(&base, &now, 10.0).regressions.is_empty());
    }

    #[test]
    fn a_shrinking_task_is_never_a_regression() {
        let base = report(vec![task("a", 300, 1000)], 500);
        let now = report(vec![task("a", 100, 1000)], 500);
        assert!(compare(&base, &now, 10.0).regressions.is_empty());
    }

    #[test]
    fn schema_growth_is_reported_separately() {
        let base = report(vec![task("a", 100, 1000)], 500);
        let now = report(vec![task("a", 100, 1000)], 2000);
        let comparison = compare(&base, &now, 10.0);
        assert_eq!(comparison.regressions.len(), 1);
        assert_eq!(comparison.regressions[0].scope, "schema");
    }

    #[test]
    fn new_and_missing_tasks_are_listed_instead_of_hidden() {
        let base = report(vec![task("a", 100, 1000), task("b", 100, 1000)], 500);
        let now = report(vec![task("a", 100, 1000), task("c", 100, 1000)], 500);
        let comparison = compare(&base, &now, 10.0);
        assert_eq!(comparison.new_tasks, vec!["c"]);
        assert_eq!(comparison.missing_tasks, vec!["b"]);
        assert!(comparison.regressions.is_empty());
    }

    #[test]
    fn a_failed_task_is_not_counted_in_the_totals() {
        let mut failed = task("b", 0, 5000);
        failed.error = Some("timeout".to_string());
        let report = report(vec![task("a", 100, 1000), failed], 500);
        let totals = report.totals();
        assert_eq!(totals.brain_tokens, 100);
        assert_eq!(totals.naive_tokens, 1000);
    }

    #[test]
    fn the_table_names_every_task_and_the_schema_cost() {
        let report = report(vec![task("a", 1234, 14000)], 500);
        let table = render(&report);
        assert!(table.contains("a "));
        assert!(table.contains("1.234"));
        assert!(table.contains("14.000"));
        assert!(table.contains("TOTAL"));
        assert!(table.contains("schema tools/list: 500 tokens"));
        assert!(table.contains("economia:"));
    }
}
