//! Task file: the fixed questions the benchmark replays.
//!
//! A task is a question, the ordered MCP calls an agent would make to answer
//! it, and the files that same agent would have had to open without the
//! server. The baseline is declared, never inferred: inferring it from the
//! paths the server happened to mention would let the server grade its own
//! homework.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// Tools whose output depends on prior machine state. Replaying them would
/// produce a baseline that a last build, an evicted handle or a previous
/// ledger entry can silently move.
const STATEFUL_TOOLS: &[&str] = &["brain_exec", "brain_peek", "brain_ledger"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSet {
    #[serde(default)]
    pub description: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// The question an agent would be answering. Documentation, not input.
    pub question: String,
    pub calls: Vec<Call>,
    /// Files a `Read`-only agent would open. Relative to the benchmark root;
    /// a file or a directory (the directory is counted file by file).
    pub naive: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub tool: String,
    #[serde(default = "empty_arguments")]
    pub arguments: serde_json::Value,
}

fn empty_arguments() -> serde_json::Value {
    serde_json::json!({})
}

impl TaskSet {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("falha ao ler o arquivo de tarefas {}", path.display()))?;
        let set: TaskSet = serde_json::from_str(&raw)
            .with_context(|| format!("tarefas inválidas em {}", path.display()))?;
        set.validate()?;
        Ok(set)
    }

    pub fn validate(&self) -> Result<()> {
        if self.tasks.is_empty() {
            bail!("o arquivo de tarefas não declara nenhuma tarefa");
        }

        let mut seen = HashSet::new();
        for task in &self.tasks {
            if task.id.trim().is_empty() {
                bail!("tarefa sem id");
            }
            if !task
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                bail!(
                    "id de tarefa `{}` inválido: use apenas a-z, 0-9 e `-`",
                    task.id
                );
            }
            if !seen.insert(task.id.clone()) {
                bail!("id de tarefa duplicado: `{}`", task.id);
            }
            if task.question.trim().is_empty() {
                bail!(
                    "tarefa `{}` sem pergunta: o id não explica o que está sendo medido",
                    task.id
                );
            }
            if task.calls.is_empty() {
                bail!("tarefa `{}` não declara nenhuma chamada", task.id);
            }
            if task.naive.is_empty() {
                bail!(
                    "tarefa `{}` não declara baseline `naive`: sem arquivos de comparação a razão não mede nada",
                    task.id
                );
            }

            for call in &task.calls {
                if call.tool.trim().is_empty() {
                    bail!(
                        "tarefa `{}` tem uma chamada sem nome de ferramenta",
                        task.id
                    );
                }
                if STATEFUL_TOOLS.contains(&call.tool.as_str()) {
                    bail!(
                        "tarefa `{}` chama `{}`, cuja saída depende de estado da máquina e não é reproduzível; remova-a do benchmark",
                        task.id,
                        call.tool
                    );
                }
                if !call.arguments.is_object() {
                    bail!(
                        "tarefa `{}`: os argumentos de `{}` precisam ser um objeto JSON",
                        task.id,
                        call.tool
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            question: "pergunta".to_string(),
            calls: vec![Call {
                tool: "brain_read".to_string(),
                arguments: serde_json::json!({ "path": "a.rs" }),
            }],
            naive: vec!["a.rs".to_string()],
        }
    }

    fn set(tasks: Vec<Task>) -> TaskSet {
        TaskSet {
            description: String::new(),
            tasks,
        }
    }

    #[test]
    fn a_well_formed_task_set_is_accepted() {
        assert!(set(vec![task("a"), task("b")]).validate().is_ok());
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        let err = set(vec![task("a"), task("a")]).validate().unwrap_err();
        assert!(err.to_string().contains("duplicado"), "{err}");
    }

    #[test]
    fn an_empty_task_set_is_rejected() {
        assert!(set(vec![]).validate().is_err());
    }

    #[test]
    fn a_task_without_naive_baseline_is_rejected() {
        let mut t = task("a");
        t.naive.clear();
        let err = set(vec![t]).validate().unwrap_err();
        assert!(err.to_string().contains("baseline"), "{err}");
    }

    #[test]
    fn a_stateful_tool_is_rejected_instead_of_being_measured() {
        for tool in STATEFUL_TOOLS {
            let mut t = task("a");
            t.calls[0].tool = tool.to_string();
            let err = set(vec![t]).validate().unwrap_err();
            assert!(err.to_string().contains("estado da máquina"), "{err}");
        }
    }

    #[test]
    fn arguments_default_to_an_empty_object() {
        let raw = r#"{"tasks":[{"id":"a","question":"q","naive":["a.rs"],"calls":[{"tool":"brain_map"}]}]}"#;
        let parsed: TaskSet = serde_json::from_str(raw).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.tasks[0].calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn a_task_without_question_is_rejected() {
        let mut t = task("a");
        t.question = "  ".to_string();
        assert!(set(vec![t]).validate().is_err());
    }
}
