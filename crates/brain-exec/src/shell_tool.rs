use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Runs an arbitrary shell command inside the project root.
///
/// This is the explicit escape hatch for what `brain_exec`'s closed enum does
/// not cover (git, formatting, package managers). It is registered by default
/// and can be removed with `--shell-disabled`; it is not a sandbox.
pub struct ShellTool {
    root_dir: PathBuf,
    timeout: Duration,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct ShellParams {
    pub command: String,
    pub timeout_secs: Option<u64>,
    pub budget_tokens: Option<usize>,
}

impl ShellTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            timeout,
            store,
            default_budget_tokens,
        }
    }

    pub async fn run(&self, params: ShellParams) -> CallToolResult {
        if params.command.trim().is_empty() {
            return CallToolResult::error("Comando vazio.");
        }
        let timeout = params
            .timeout_secs
            .map(|secs| Duration::from_secs(secs.max(1)))
            .unwrap_or(self.timeout);

        let started = std::time::Instant::now();
        let mut child = match Command::new("bash")
            .arg("-lc")
            .arg(&params.command)
            .current_dir(&self.root_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                return CallToolResult::error(format!("Falha ao iniciar o shell: {e}"))
            }
        };

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let read_out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let read_err = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => (status.ok().and_then(|s| s.code()), false),
            Err(_) => {
                let _ = child.kill().await;
                (None, true)
            }
        };

        let stdout = String::from_utf8_lossy(&read_out.await.unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&read_err.await.unwrap_or_default()).into_owned();
        let elapsed = started.elapsed();

        let headline = if timed_out {
            format!(
                "### shell — tempo esgotado após {}s\n",
                timeout.as_secs()
            )
        } else {
            format!(
                "### shell — exit {} ({:.2}s)\n",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "sinal".to_string()),
                elapsed.as_secs_f64()
            )
        };

        let mut sections = vec![Section {
            heading: headline,
            body: String::new(),
            priority: 255,
        }];
        if !stdout.trim().is_empty() {
            sections.push(Section {
                heading: "#### stdout".to_string(),
                body: format!("```\n{}\n```\n", cap(&stdout)),
                priority: 250,
            });
        }
        if !stderr.trim().is_empty() {
            sections.push(Section {
                heading: "#### stderr".to_string(),
                body: format!("```\n{}\n```\n", cap(&stderr)),
                priority: 240,
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
}

/// Keeps a runaway command from filling memory before the budget is applied.
fn cap(text: &str) -> String {
    const MAX_CHARS: usize = 400_000;
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_CHARS).collect();
    format!("{truncated}\n… (saída truncada em {MAX_CHARS} caracteres)")
}

impl ToolPlugin for ShellTool {
    fn name(&self) -> &'static str {
        "brain_shell"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_shell".to_string(),
            description: "Executa comando em `bash -lc` no root e devolve stdout/stderr/código. Substitui `shell` para git e formatadores; não é sandbox.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Comando executado por `bash -lc`, com cwd no diretório raiz."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Teto de tempo para este comando; o padrão vem de `--shell-timeout-secs`."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: ShellParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.run(params).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ShellTool {
        ShellTool::new(
            ".",
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
            Duration::from_secs(5),
        )
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let out = tool()
            .run(ShellParams {
                command: "echo hello".into(),
                timeout_secs: None,
                budget_tokens: None,
            })
            .await;
        assert_eq!(out.is_error, None);
        assert!(out.content[0].text.contains("hello"));
        assert!(out.content[0].text.contains("exit 0"));
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_reported_not_hidden() {
        let out = tool()
            .run(ShellParams {
                command: "echo bad >&2; exit 7".into(),
                timeout_secs: None,
                budget_tokens: None,
            })
            .await;
        assert!(out.content[0].text.contains("exit 7"));
        assert!(out.content[0].text.contains("bad"));
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let out = tool()
            .run(ShellParams {
                command: "   ".into(),
                timeout_secs: None,
                budget_tokens: None,
            })
            .await;
        assert_eq!(out.is_error, Some(true));
    }
}
