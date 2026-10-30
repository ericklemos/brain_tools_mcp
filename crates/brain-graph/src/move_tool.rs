//! `brain_move`: move ou copia um arquivo (ou diretório) dentro do root e
//! prova o resultado. Substitui `git mv` e `cp`, que a guarda do shell nega.
//!
//! O valor não está em apagar a origem: está em recusar o que `mv` aceita —
//! destino fora do root, sobrescrita silenciosa, origem que mudou desde a
//! leitura — e em devolver, de forma curta, quem importava o arquivo movido.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::edit_tool::{hash_matches, hash_prefix};
use crate::model::ProjectCodeGraph;
use crate::scope::ImportTarget;
use crate::write_tool::resolve_for_write;

/// Quantas referências cabem antes do resumo "+N".
const MAX_IMPORTERS: usize = 10;

#[derive(Debug, Deserialize)]
pub struct MoveParams {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub copy: bool,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub create_parents: bool,
    pub expected_hash: Option<String>,
}

pub struct MoveTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
}

impl MoveTool {
    pub fn new(root_dir: impl Into<PathBuf>, graph: Arc<RwLock<ProjectCodeGraph>>) -> Self {
        let root_dir = root_dir.into();
        let root_dir = root_dir.canonicalize().unwrap_or(root_dir);
        Self { root_dir, graph }
    }

    pub async fn relocate(&self, params: MoveParams) -> CallToolResult {
        let (from, from_exists) = match resolve_for_write(&self.root_dir, &params.from) {
            Ok(resolved) => resolved,
            Err(message) => return CallToolResult::error(message),
        };
        if !from_exists {
            return CallToolResult::error(format!("Origem não existe: `{}`.", params.from));
        }
        let metadata = match fs::symlink_metadata(&from) {
            Ok(metadata) => metadata,
            Err(e) => {
                return CallToolResult::error(format!(
                    "Falha ao inspecionar `{}`: {e}.",
                    params.from
                ))
            }
        };
        if metadata.file_type().is_symlink() {
            return CallToolResult::error(format!(
                "`{}` é um symlink; mova o alvo, não o link.",
                params.from
            ));
        }
        let is_dir = metadata.is_dir();
        if is_dir && params.expected_hash.is_some() {
            return CallToolResult::error(
                "`expected_hash` vale para arquivo; diretório não tem hash.",
            );
        }

        let (to, to_exists) = match resolve_for_write(&self.root_dir, &params.to) {
            Ok(resolved) => resolved,
            Err(message) => return CallToolResult::error(message),
        };
        if to_exists {
            if to.is_dir() {
                return CallToolResult::error(format!(
                    "Destino `{}` é um diretório; informe o caminho completo do arquivo.",
                    params.to
                ));
            }
            if !(params.copy && params.overwrite) {
                return CallToolResult::error(format!(
                    "Destino `{}` já existe; nada foi escrito. Use `copy` e `overwrite` se a substituição é intencional.",
                    params.to
                ));
            }
        }
        if from == to {
            return CallToolResult::error("Origem e destino são o mesmo caminho; nada foi feito.");
        }
        if is_dir && to.starts_with(&from) {
            return CallToolResult::error(
                "Destino está dentro da origem; mover um diretório para si mesmo o corrompe.",
            );
        }

        let before_hash = if is_dir {
            None
        } else {
            match fs::read(&from) {
                Ok(bytes) => {
                    if let Some(expected) = params.expected_hash.as_deref() {
                        if !hash_matches(&bytes, expected) {
                            return CallToolResult::error(format!(
                                "`{}` mudou desde a leitura (blake3 atual `{}`); reenvie com `expected_hash` atualizado.",
                                params.from,
                                hash_prefix(&bytes)
                            ));
                        }
                    }
                    Some(blake3::hash(&bytes))
                }
                Err(e) => {
                    return CallToolResult::error(format!(
                        "Falha ao ler `{}`: {e}. Nada foi movido.",
                        params.from
                    ))
                }
            }
        };

        if let Some(parent) = to.parent() {
            if !parent.exists() {
                if !params.create_parents {
                    return CallToolResult::error(format!(
                        "Diretório pai de `{}` não existe; use `create_parents: true`.",
                        params.to
                    ));
                }
                if let Err(e) = fs::create_dir_all(parent) {
                    return CallToolResult::error(format!(
                        "Falha ao criar `{}`: {e}. Nada foi movido.",
                        self.relative(parent)
                    ));
                }
            }
        }

        // Quem importa o arquivo é lido antes: depois do move o grafo ainda não
        // reprocessou o caminho novo, e a pergunta é sobre o caminho antigo.
        let importers = if params.copy || is_dir {
            Vec::new()
        } else {
            self.importers(&from).await
        };

        let done = if params.copy {
            if is_dir {
                copy_tree(&from, &to)
            } else {
                fs::copy(&from, &to).map(|_| ())
            }
        } else {
            rename_or_copy(&from, &to)
        };
        if let Err(e) = done {
            return CallToolResult::error(format!(
                "Falha ao {} `{}` para `{}`: {e}.",
                if params.copy { "copiar" } else { "mover" },
                self.relative(&from),
                self.relative(&to)
            ));
        }

        // Nunca anunciar o que se quis fazer: reler o destino e comparar.
        if !to.exists() {
            return CallToolResult::error(format!(
                "A operação retornou sucesso, mas `{}` não existe.",
                self.relative(&to)
            ));
        }
        if !is_dir {
            let after = match fs::read(&to) {
                Ok(bytes) => bytes,
                Err(e) => {
                    return CallToolResult::error(format!(
                        "Destino gravado, mas não foi possível reler `{}`: {e}.",
                        self.relative(&to)
                    ))
                }
            };
            let expected = before_hash.expect("arquivo tem hash");
            if blake3::hash(&after) != expected {
                return CallToolResult::error(format!(
                    "A verificação pós-escrita falhou: o conteúdo de `{}` difere da origem.",
                    self.relative(&to)
                ));
            }
        }
        if !params.copy && from.exists() {
            return CallToolResult::error(format!(
                "O destino foi escrito, mas a origem `{}` continua no lugar.",
                self.relative(&from)
            ));
        }

        let verb = if params.copy {
            "copiado"
        } else if is_dir {
            "diretório movido"
        } else {
            "movido"
        };
        let mut body = format!(
            "blake3 `{}`; destino verificado.\n",
            before_hash
                .map(|hash| hash.to_hex().to_string()[..12].to_string())
                .unwrap_or_else(|| "—".to_string())
        );
        if let (Some(old), Some(new)) = (from.parent(), to.parent()) {
            if old != new && !is_dir {
                body.push_str(&format!(
                    "Módulo mudou de `{}` para `{}`; a declaração `mod {};` precisa acompanhar.\n",
                    self.relative(old),
                    self.relative(new),
                    from.file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("…")
                ));
            }
        }
        if params.copy {
            body.push_str("A cópia ainda não é declarada por nenhum `mod`.\n");
        }
        if !importers.is_empty() {
            body.push_str(&format!(
                "\n{} arquivo(s) importam o caminho antigo:\n",
                importers.len()
            ));
            for (file, names) in importers.iter().take(MAX_IMPORTERS) {
                body.push_str(&format!("- `{}` ({names})\n", file));
            }
            if importers.len() > MAX_IMPORTERS {
                body.push_str(&format!(
                    "… +{} arquivo(s); use `brain_impact_check` no símbolo movido.\n",
                    importers.len() - MAX_IMPORTERS
                ));
            }
        }

        CallToolResult::text(format!(
            "### {verb} — `{}` → `{}`\n\n{body}",
            self.relative(&from),
            self.relative(&to)
        ))
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root_dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    async fn importers(&self, target: &Path) -> Vec<(String, String)> {
        let graph = self.graph.read().await;
        let mut found: BTreeMap<String, String> = BTreeMap::new();
        for (file, scope) in &graph.scopes {
            if file == target {
                continue;
            }
            let mut names: Vec<String> = scope
                .imports
                .iter()
                .filter(|(_, imported)| matches!(imported, ImportTarget::File(path) if path == target))
                .map(|(name, _)| name.clone())
                .collect();
            if names.is_empty() {
                continue;
            }
            names.sort();
            found.insert(self.relative(file), names.join(", "));
        }
        found.into_iter().collect()
    }
}

fn rename_or_copy(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            if from.is_dir() {
                copy_tree(from, to)?;
                fs::remove_dir_all(from)
            } else {
                fs::copy(from, to)?;
                fs::remove_file(from)
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn is_cross_device(error: &io::Error) -> bool {
    // EXDEV: rename entre sistemas de arquivos distintos.
    const EXDEV: i32 = 18;
    error.raw_os_error() == Some(EXDEV)
}

#[cfg(not(unix))]
fn is_cross_device(_error: &io::Error) -> bool {
    false
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        for entry in fs::read_dir(&src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let target = dst.join(entry.file_name());
            if file_type.is_dir() {
                fs::create_dir_all(&target)?;
                stack.push((entry.path(), target));
            } else if file_type.is_file() {
                fs::copy(entry.path(), &target)?;
            } else {
                return Err(io::Error::other(format!(
                    "`{}` é symlink ou entrada especial; a cópia de árvore recusa escapar do root.",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(())
}

impl ToolPlugin for MoveTool {
    fn name(&self) -> &'static str {
        "brain_move"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_move".to_string(),
            description: "Move ou copia arquivo/diretório dentro do root; recusa destino existente e hash velho, e lista quem importava o caminho antigo.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Origem, relativa ao diretório raiz do projeto."
                    },
                    "to": {
                        "type": "string",
                        "description": "Destino, relativo ao diretório raiz. O pai precisa existir, salvo `create_parents`."
                    },
                    "copy": {
                        "type": "boolean",
                        "default": false,
                        "description": "Copia em vez de mover; a origem permanece."
                    },
                    "overwrite": {
                        "type": "boolean",
                        "default": false,
                        "description": "Só com `copy`: substitui um destino existente. Sem isso, destino existente é recusado."
                    },
                    "create_parents": {
                        "type": "boolean",
                        "default": false,
                        "description": "Cria os diretórios intermediários do destino."
                    },
                    "expected_hash": {
                        "type": "string",
                        "description": "Opcional: blake3 atual da origem; recusa se o arquivo mudou desde a leitura."
                    }
                },
                "required": ["from", "to"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: MoveParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.relocate(params).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(dir: &tempfile::TempDir) -> MoveTool {
        MoveTool::new(dir.path(), Arc::new(RwLock::new(ProjectCodeGraph::new())))
    }

    fn params(from: &str, to: &str) -> MoveParams {
        MoveParams {
            from: from.to_string(),
            to: to.to_string(),
            copy: false,
            overwrite: false,
            create_parents: false,
            expected_hash: None,
        }
    }

    fn text(out: &CallToolResult) -> String {
        serde_json::to_string(&out.content).unwrap_or_default()
    }

    fn write(dir: &tempfile::TempDir, path: &str, content: &str) {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[tokio::test]
    async fn moves_a_file_and_leaves_no_source_behind() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "pub fn a() {}\n");
        fs::create_dir_all(dir.path().join("src/b")).unwrap();

        let out = tool(&dir).relocate(params("src/a.rs", "src/b/a.rs")).await;
        assert!(!out.is_error.unwrap_or(false), "{}", text(&out));
        assert!(!dir.path().join("src/a.rs").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b/a.rs")).unwrap(),
            "pub fn a() {}\n"
        );
        assert!(text(&out).contains("mod a;"));
    }

    #[tokio::test]
    async fn copy_keeps_the_source_and_duplicates_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "pub fn a() {}\n");

        let mut p = params("src/a.rs", "src/a_copy.rs");
        p.copy = true;
        let out = tool(&dir).relocate(p).await;
        assert!(!out.is_error.unwrap_or(false), "{}", text(&out));
        assert!(dir.path().join("src/a.rs").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a_copy.rs")).unwrap(),
            "pub fn a() {}\n"
        );
    }

    #[tokio::test]
    async fn an_existing_destination_is_refused_and_nothing_moves() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "a\n");
        write(&dir, "src/b.rs", "b\n");

        let out = tool(&dir).relocate(params("src/a.rs", "src/b.rs")).await;
        assert!(out.is_error.unwrap_or(false));
        assert!(dir.path().join("src/a.rs").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/b.rs")).unwrap(),
            "b\n"
        );
    }

    #[tokio::test]
    async fn copy_with_overwrite_replaces_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "a\n");
        write(&dir, "src/b.rs", "b\n");

        let mut p = params("src/a.rs", "src/b.rs");
        p.copy = true;
        p.overwrite = true;
        let out = tool(&dir).relocate(p).await;
        assert!(!out.is_error.unwrap_or(false), "{}", text(&out));
        assert_eq!(fs::read_to_string(dir.path().join("src/b.rs")).unwrap(), "a\n");
    }

    #[tokio::test]
    async fn a_stale_hash_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "a\n");

        let mut p = params("src/a.rs", "src/b.rs");
        p.expected_hash = Some("000000000000".to_string());
        let out = tool(&dir).relocate(p).await;
        assert!(out.is_error.unwrap_or(false));
        assert!(dir.path().join("src/a.rs").exists());
    }

    #[tokio::test]
    async fn the_destination_parent_is_not_created_without_permission() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "a\n");

        let out = tool(&dir).relocate(params("src/a.rs", "novo/a.rs")).await;
        assert!(out.is_error.unwrap_or(false));
        assert!(!dir.path().join("novo").exists());

        let mut p = params("src/a.rs", "novo/a.rs");
        p.create_parents = true;
        let out = tool(&dir).relocate(p).await;
        assert!(!out.is_error.unwrap_or(false), "{}", text(&out));
        assert!(dir.path().join("novo/a.rs").exists());
    }

    #[tokio::test]
    async fn a_path_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/a.rs", "a\n");

        let out = tool(&dir).relocate(params("src/a.rs", "../fora.rs")).await;
        assert!(out.is_error.unwrap_or(false));
        assert!(dir.path().join("src/a.rs").exists());
    }

    #[tokio::test]
    async fn a_directory_moves_with_its_subtree() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/old/a.rs", "a\n");
        write(&dir, "src/old/deep/b.rs", "b\n");

        let out = tool(&dir).relocate(params("src/old", "src/new")).await;
        assert!(!out.is_error.unwrap_or(false), "{}", text(&out));
        assert!(!dir.path().join("src/old").exists());
        assert!(dir.path().join("src/new/deep/b.rs").exists());
    }
}
