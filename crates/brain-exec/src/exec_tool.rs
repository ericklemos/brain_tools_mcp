use crate::cache::{BuildCache, Entry, SourceHashes};
use crate::parse_cargo::{self, Report};
use crate::parse_libtest::{self, TestReport};
use crate::runner::{Kind, Outcome, Runner};
use crate::schema::brain_exec_tool;
use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Priorities for the shared budget. The summary leads; the first error block is
/// never dropped because it is the primary answer. Later blocks and the warning
/// digest are recoverable through `brain_peek`.
///
/// Two levels, not three, on purpose: giving the summary a lower priority than
/// errors would let a wall of errors push "40 erros em 1 arquivo" out of the
/// response, and that headline is the cheapest part of the answer.
const PRIORITY_HEADLINE: u8 = 255;
const PRIORITY_ERROR: u8 = 250;
const PRIORITY_RAW: u8 = 240;
const PRIORITY_WARNINGS: u8 = 100;

/// How much raw stderr to fall back to when nothing parsed.
const RAW_TAIL_LINES: usize = 40;

#[derive(Debug, Deserialize)]
pub struct ExecParams {
    pub kind: String,
    pub budget_tokens: Option<usize>,
    #[serde(default)]
    pub refresh: bool,
}

pub struct ExecTool {
    runner: Runner,
    store: Arc<ResponseStore>,
    cache: BuildCache,
    default_budget_tokens: usize,
    /// Serializes executions: cargo already locks `target/`, and two concurrent
    /// runs would make the second hang with no explanation.
    gate: tokio::sync::Mutex<()>,
    sources: Option<Arc<dyn SourceHashes>>,
}

impl ExecTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
        timeout: Duration,
    ) -> Self {
        let root: PathBuf = root_dir.into();
        Self {
            runner: Runner::new(&root, timeout),
            store,
            cache: BuildCache::new().with_path(root.join(".brain").join("cache").join("exec.json")),
            default_budget_tokens,
            gate: tokio::sync::Mutex::new(()),
            sources: None,
        }
    }

    /// Gives the cache a view of the indexed sources, so a result can be reused
    /// only while the tree it describes is still the tree on disk.
    pub fn with_sources(mut self, sources: Arc<dyn SourceHashes>) -> Self {
        self.sources = Some(sources);
        self
    }

    pub async fn run(&self, params: ExecParams) -> CallToolResult {
        let kind = match Kind::parse(&params.kind) {
            Some(kind) => kind,
            None => {
                return CallToolResult::error(format!(
                    "kind inválido: `{}`. Use build, check, test ou clippy.",
                    params.kind
                ))
            }
        };

        let _guard = self.gate.lock().await;

        let sources = self.sources.as_deref();
        let (entry, from_cache) = self
            .cache
            .get_or_run(kind, self.runner.root(), sources, params.refresh, || async {
                self.execute(kind).await
            })
            .await;

        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        CallToolResult::text(self.render(&entry, from_cache, budget_tokens))
    }

    async fn execute(&self, kind: Kind) -> Entry {
        let outcome = match self.runner.run(kind).await {
            Ok(outcome) => outcome,
            Err(e) => {
                return Entry {
                    key: String::new(),
                    sections: vec![Section {
                        heading: "### brain_exec não pôde executar\n".to_string(),
                        body: format!("{e}\n"),
                        priority: PRIORITY_RAW,
                    }],
                    success: false,
                    duration: Duration::ZERO,
                };
            }
        };

        let sections = self.sections_for(kind, &outcome);
        let success = outcome.success();

        Entry {
            key: String::new(),
            sections,
            success,
            duration: outcome.duration,
        }
    }

    /// Splits the outcome into budgeted sections. The key is filled by the cache.
    fn sections_for(&self, kind: Kind, outcome: &Outcome) -> Vec<Section> {
        let report = parse_cargo::parse_stdout(&outcome.stdout);
        let mut sections = Vec::new();

        let tests = if kind == Kind::Test {
            let text = report.other_lines.join("\n");
            Some(parse_libtest::parse(&text))
        } else {
            None
        };

        let error_total = report.error_count
            + tests.as_ref().map_or(0, |t| t.failures.len());

        sections.push(Section {
            heading: format!("### {}\n", headline(kind, outcome, &report, tests.as_ref())),
            body: String::new(),
            priority: PRIORITY_HEADLINE,
        });

        if let Some(tests) = &tests {
            for failure in &tests.failures {
                sections.push(Section {
                    heading: format!("#### `{}` — falhou", failure.name),
                    body: format!("```\n{}\n```\n", failure.excerpt),
                    priority: PRIORITY_ERROR,
                });
            }
        }

        let errors: Vec<parse_cargo::Group> = report.errors().cloned().collect();
        // One section per (code, file), split into chunks when a single group has
        // many occurrences: the budget must be able to keep the first block and
        // leave the rest recoverable instead of dropping the whole report.
        for (heading, body) in parse_cargo::error_sections(&errors, self.runner.root()) {
            sections.push(Section {
                heading,
                body,
                priority: PRIORITY_ERROR,
            });
        }

        let warning_digest =
            parse_cargo::render_warning_digest(&report.groups, self.runner.root());
        if !warning_digest.is_empty() {
            sections.push(Section {
                heading: String::new(),
                body: warning_digest,
                priority: PRIORITY_WARNINGS,
            });
        }

        // The safety net: a non-zero exit with nothing parsed (linker, build
        // script, a cargo error) must never render as "0 errors".
        if !outcome.success() && error_total == 0 {
            sections.push(Section {
                heading: "### Saída bruta (parser não reconheceu o erro)".to_string(),
                body: format!("```\n{}\n```\n", tail(&outcome.stderr, RAW_TAIL_LINES)),
                priority: PRIORITY_RAW,
            });
        }

        if outcome.timed_out {
            sections.push(Section {
                heading: "### Tempo esgotado".to_string(),
                body: format!(
                    "O processo foi interrompido após {}s; o relatório acima é o que foi capturado antes.",
                    self.runner.timeout().as_secs()
                ),
                priority: PRIORITY_RAW,
            });
        }

        sections
    }

    fn render(&self, entry: &Entry, from_cache: bool, budget_tokens: usize) -> String {
        let cache_note = if from_cache {
            "\n> `(cache)` — nada mudou desde a última execução. Use `refresh: true` para reexecutar.\n"
        } else {
            ""
        };

        let selection = budget::select(&entry.sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        let rendered = budget::render(&selection, handle.as_deref());

        format!("{rendered}{cache_note}")
    }
}

fn headline(kind: Kind, outcome: &Outcome, report: &Report, tests: Option<&TestReport>) -> String {
    let duration = format!("{:.1}s", outcome.duration.as_secs_f64());

    if outcome.timed_out {
        return format!("`{}` — tempo esgotado ({duration})", kind.as_str());
    }

    let mut parts = Vec::new();
    parts.push(format!("`{}`", kind.as_str()));

    if report.error_count > 0 {
        let files = report
            .errors()
            .map(|g| g.file.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        parts.push(format!(
            "{} erro(s) em {files} arquivo(s)",
            report.error_count
        ));
    }

    if let Some(tests) = tests {
        if !tests.is_empty() {
            parts.push(format!("{} falharam, {} passaram", tests.failed, tests.passed));
        }
    }

    if report.warning_count > 0 {
        parts.push(format!("{} warning(s)", report.warning_count));
    }

    let verdict = if outcome.success() {
        "ok".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("saiu com código {code}"),
            None => "falhou".to_string(),
        }
    };
    parts.push(format!("{verdict} · {duration}"));

    parts.join(" · ")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("[…{start} linhas omitidas]\n"));
    }
    out.push_str(&all[start..].join("\n"));
    out
}

impl ToolPlugin for ExecTool {
    fn name(&self) -> &'static str {
        "brain_exec"
    }

    fn schema(&self) -> Tool {
        brain_exec_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(async move {
            let params: ExecParams = match serde_json::from_value(args) {
                Ok(p) => p,
                Err(e) => {
                    return CallToolResult::error(format!("Invalid arguments for brain_exec: {e}"))
                }
            };
            self.run(params).await
        })
    }
}
