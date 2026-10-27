use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Command;

/// Applies a unified diff (the output of `git diff`) inside the root.
///
/// This is the MCP equivalent of `patch`/`git apply`: it handles new files,
/// deletions and edits outside Rust symbol bodies. It never accepts a shell
/// command — only diff text — and it runs `git apply --check` before touching
/// the tree.
pub struct PatchTool {
    root_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct PatchParams {
    pub patch: String,
    #[serde(default)]
    pub check_only: bool,
    #[serde(default)]
    pub reverse: bool,
}

impl PatchTool {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub async fn apply(&self, params: PatchParams) -> CallToolResult {
        if params.patch.trim().is_empty() {
            return CallToolResult::error("Patch vazio; nada foi aplicado.");
        }
        if contains_symlink_creation(&params.patch) {
            return CallToolResult::error(
                "Patch cria um symlink (mode 120000); recusado para não escapar do root.",
            );
        }

        let patch_path = match write_temp_patch(&params.patch) {
            Ok(path) => path,
            Err(e) => return CallToolResult::error(format!("Falha ao preparar o patch: {e}")),
        };

        let result = self.apply_inner(&params, &patch_path).await;
        let _ = std::fs::remove_file(&patch_path);
        result
    }

    async fn apply_inner(&self, params: &PatchParams, patch_path: &Path) -> CallToolResult {
        let stats = match self.numstat(patch_path, params.reverse).await {
            Ok(stats) => stats,
            Err(message) => return CallToolResult::error(message),
        };
        if stats.is_empty() {
            return CallToolResult::error(
                "O patch não altera nenhum arquivo reconhecível; nada foi aplicado.",
            );
        }
        for (path, _, _) in &stats {
            if !is_safe_relative(path) {
                return CallToolResult::error(format!(
                    "Patch toca caminho fora do root ou proibido: `{path}`. Nada foi aplicado."
                ));
            }
        }

        if let Err(message) = self.run_git(patch_path, params.reverse, true).await {
            return CallToolResult::error(format!(
                "O patch não aplica sobre o estado atual; nada foi escrito.\n\n{message}"
            ));
        }

        if params.check_only {
            return CallToolResult::text(format!(
                "### patch aplicável (check-only) — {} arquivo(s)\n\n{}",
                stats.len(),
                render_stats(&stats)
            ));
        }

        if let Err(message) = self.run_git(patch_path, params.reverse, false).await {
            return CallToolResult::error(format!(
                "A aplicação do patch falhou; a árvore pode ter sido parcialmente alterada.\n\n{message}"
            ));
        }

        CallToolResult::text(format!(
            "### patch aplicado — {} arquivo(s)\n\n{}",
            stats.len(),
            render_stats(&stats)
        ))
    }

    async fn numstat(
        &self,
        patch_path: &Path,
        reverse: bool,
    ) -> std::result::Result<Vec<(String, String, String)>, String> {
        let mut command = Command::new("git");
        command.arg("apply").arg("--numstat").arg("--whitespace=nowarn");
        if reverse {
            command.arg("-R");
        }
        command.arg(patch_path).current_dir(&self.root_dir);
        let output = command
            .output()
            .await
            .map_err(|e| format!("Falha ao executar `git apply`: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "`git apply --numstat` recusou o patch: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut stats = Vec::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let Some(added) = parts.next() else { continue };
            let Some(deleted) = parts.next() else { continue };
            let Some(path) = parts.next() else { continue };
            stats.push((path.to_string(), added.to_string(), deleted.to_string()));
        }
        Ok(stats)
    }

    async fn run_git(
        &self,
        patch_path: &Path,
        reverse: bool,
        check: bool,
    ) -> std::result::Result<(), String> {
        let mut command = Command::new("git");
        command.arg("apply").arg("--whitespace=nowarn");
        if check {
            command.arg("--check");
        }
        if reverse {
            command.arg("-R");
        }
        command.arg(patch_path).current_dir(&self.root_dir);

        let output = command
            .output()
            .await
            .map_err(|e| format!("Falha ao executar `git apply`: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "`git apply` falhou com saída: {}\n{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

fn write_temp_patch(patch: &str) -> std::io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "brain-patch-{}-{}.patch",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, patch)?;
    Ok(path)
}

fn contains_symlink_creation(patch: &str) -> bool {
    patch.lines().any(|line| {
        line.starts_with("new file mode 120000")
            || line.starts_with("new mode 120000")
            || line.starts_with("new symlink mode")
    })
}

fn is_safe_relative(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    if path.contains("..") {
        return false;
    }
    let normalized = path.replace('\\', "/");
    !normalized.starts_with(".git/") && !normalized.contains("/.git/")
}

fn render_stats(stats: &[(String, String, String)]) -> String {
    let mut out = String::new();
    for (path, added, deleted) in stats.iter().take(20) {
        out.push_str(&format!("- `{path}` (+{added}/-{deleted})\n"));
    }
    if stats.len() > 20 {
        out.push_str(&format!("- … {} arquivo(s) omitido(s)\n", stats.len() - 20));
    }
    out
}

impl ToolPlugin for PatchTool {
    fn name(&self) -> &'static str {
        "brain_patch"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_patch".to_string(),
            description: "Aplica um diff unificado (saída de `git diff`) sob o diretório raiz, incluindo arquivos novos e remoções. Substitui `patch`/`git apply` e cobre o que `brain_edit` não cobre (arquivos não-Rust, imports, arquivos novos). Roda `git apply --check` antes de escrever e recusa caminhos fora do root e criação de symlink.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Diff unificado completo, no formato `git diff` (inclui `diff --git`, `---`, `+++` e hunks)."
                    },
                    "check_only": {
                        "type": "boolean",
                        "default": false,
                        "description": "Só verifica se o patch aplica; não escreve."
                    },
                    "reverse": {
                        "type": "boolean",
                        "default": false,
                        "description": "Aplica o patch ao contrário (`git apply -R`), útil para desfazer."
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: PatchParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.apply(params).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn new_file_patch() -> String {
        "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\nindex 0000000..3b18e51\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n".to_string()
    }

    #[tokio::test]
    async fn applies_a_new_file_patch() {
        let dir = tempdir().unwrap();
        let out = PatchTool::new(dir.path())
            .apply(PatchParams {
                patch: new_file_patch(),
                check_only: false,
                reverse: false,
            })
            .await;
        assert_eq!(out.is_error, None, "{}", out.content[0].text);
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "hello\nworld\n"
        );
    }

    #[tokio::test]
    async fn check_only_does_not_write() {
        let dir = tempdir().unwrap();
        let out = PatchTool::new(dir.path())
            .apply(PatchParams {
                patch: new_file_patch(),
                check_only: true,
                reverse: false,
            })
            .await;
        assert_eq!(out.is_error, None, "{}", out.content[0].text);
        assert!(!dir.path().join("notes.txt").exists());
    }

    #[tokio::test]
    async fn a_patch_that_does_not_fit_is_refused() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "actual\n").unwrap();
        let patch = "diff --git a/a.txt b/a.txt\nindex 3b18e51..0000000\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-different\n+new\n";
        let out = PatchTool::new(dir.path())
            .apply(PatchParams {
                patch: patch.to_string(),
                check_only: false,
                reverse: false,
            })
            .await;
        assert_eq!(out.is_error, Some(true));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "actual\n");
    }

    #[test]
    fn path_guards_reject_traversal_and_git_metadata() {
        assert!(!is_safe_relative("../evil.txt"));
        assert!(!is_safe_relative("/etc/passwd"));
        assert!(!is_safe_relative(".git/config"));
        assert!(is_safe_relative("src/main.rs"));
    }
}
