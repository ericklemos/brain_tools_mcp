use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::model::{ProjectCodeGraph, SymbolNode};

const DEFAULT_MAX_COMMITS: usize = 10;
const MAX_MAX_COMMITS: usize = 50;

pub struct HistoryTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct WhyParams {
    pub symbol: String,
    pub max_commits: Option<usize>,
    pub budget_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

/// Parses the `--format=%h%x09%an%x09%ad%x09%s` lines out of `git log -L`.
///
/// The patch is interleaved with the format lines; only the latter become
/// commits. A diff line can never be mistaken for one because it starts with
/// `+`, `-`, a space, or a git keyword.
pub fn parse_history(output: &str) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();

    for line in output.lines() {
        let mut parts = line.splitn(4, '\t');
        let (Some(hash), Some(author), Some(date), Some(subject)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if hash.len() < 6 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if commits.iter().any(|existing| existing.hash == hash) {
            continue;
        }
        commits.push(Commit {
            hash: hash.to_string(),
            author: author.to_string(),
            date: date.to_string(),
            subject: subject.to_string(),
        });
    }

    commits
}

impl HistoryTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: Arc<RwLock<ProjectCodeGraph>>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            graph,
            store,
            default_budget_tokens,
        }
    }

    pub async fn handle_why(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: WhyParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let nodes = match lock.symbol_lookup.get(&params.symbol) {
            Some(indices) if !indices.is_empty() => indices,
            _ => {
                return CallToolResult::text(format!(
                    "Símbolo `{}` não encontrado no índice.",
                    params.symbol
                ))
            }
        };

        let Some(node) = nodes.iter().filter_map(|&idx| lock.graph.node_weight(idx)).next() else {
            return CallToolResult::text(format!(
                "Símbolo `{}` não encontrado no índice.",
                params.symbol
            ));
        };

        let relative = self.relative(node);
        let max_commits = params
            .max_commits
            .unwrap_or(DEFAULT_MAX_COMMITS)
            .clamp(1, MAX_MAX_COMMITS);

        let output = match Command::new("git")
            .current_dir(&self.root_dir)
            .args([
                "log",
                "--format=%h%x09%an%x09%ad%x09%s",
                "--date=short",
                "-n",
                &max_commits.to_string(),
                "-L",
                &format!("{},{}:{}", node.start_line, node.end_line, relative),
            ])
            .output()
        {
            Ok(output) => output,
            Err(e) => {
                return CallToolResult::error(format!("Não foi possível executar git log: {e}"))
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return CallToolResult::error(format!("git log -L falhou: {}", stderr.trim()));
        }

        let commits = parse_history(&String::from_utf8_lossy(&output.stdout));
        if commits.is_empty() {
            return CallToolResult::text(format!(
                "Nenhum commit toca `{}` (`{relative}:{}-{}`); o símbolo pode ser novo ou não versionado.",
                params.symbol, node.start_line, node.end_line
            ));
        }

        let mut sections = vec![Section {
            heading: format!(
                "### Histórico de `{}` (`{relative}:{}-{}`) — {} commits",
                params.symbol,
                node.start_line,
                node.end_line,
                commits.len()
            ),
            body: String::new(),
            priority: 255,
        }];

        for commit in commits {
            sections.push(Section {
                heading: format!(
                    "#### `{}` (`{}`, {}) — {}",
                    commit.hash, commit.date, commit.author, commit.subject
                ),
                body: String::new(),
                priority: 200,
            });
        }

        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }

    fn relative(&self, node: &SymbolNode) -> String {
        node.file_path
            .strip_prefix(&self.root_dir)
            .unwrap_or(&node.file_path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

impl ToolPlugin for HistoryTool {
    fn name(&self) -> &'static str {
        "brain_why"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_why".to_string(),
            description: "Histórico git de um símbolo: commits que tocaram suas linhas, com autor, data e assunto. Use para entender por que o código é assim sem abrir o log inteiro.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Nome exato da função/struct."
                    },
                    "max_commits": {
                        "type": "integer",
                        "default": 10,
                        "description": "Máximo de commits (1 a 50)."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
                    }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_why(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::fs;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    const LOG: &str = "\
abc1234\tAna\t2026-01-02\tfix: guard empty token

diff --git a/core.rs b/core.rs
--- a/core.rs
+++ b/core.rs
@@ -1,1 +1,1 @@
-pub fn core_fn() {}
+pub fn core_fn() { todo!() }

def5678\tBia\t2026-01-01\tfeat: add core
";

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn fixture() -> (TempDir, Arc<RwLock<ProjectCodeGraph>>) {
        let dir = tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("core.rs"), "pub fn core_fn() {}\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "feat: add core"]);

        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        (dir, Arc::new(RwLock::new(graph)))
    }

    fn tool(dir: &TempDir, graph: Arc<RwLock<ProjectCodeGraph>>) -> HistoryTool {
        HistoryTool::new(
            dir.path(),
            graph,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        )
    }

    #[test]
    fn the_parser_ignores_patch_lines_and_deduplicates() {
        let commits = parse_history(LOG);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "abc1234");
        assert_eq!(commits[0].subject, "fix: guard empty token");
        assert_eq!(commits[1].author, "Bia");
    }

    #[tokio::test]
    async fn a_real_repository_yields_its_commit() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_why(serde_json::json!({ "symbol": "core_fn" }))
            .await;
        let text = &out.content[0].text;

        assert_eq!(out.is_error, None, "{text}");
        assert!(text.contains("feat: add core"), "{text}");
        assert!(text.contains("Histórico de `core_fn`"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_symbol_is_a_categorical_answer() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_why(serde_json::json!({ "symbol": "ghost" }))
            .await;
        assert!(out.content[0].text.contains("não encontrado"));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(RwLock::new(ProjectCodeGraph::new()));
        let p = tool(&dir, graph);
        assert_eq!(p.name(), "brain_why");
        assert_eq!(p.schema().name, "brain_why");
    }
}
