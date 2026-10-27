use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use crate::edit_tool::{atomic_write, hash_matches, hash_prefix};

/// Creates or replaces a whole text file.
///
/// This is the MCP equivalent of the harness `write` tool for files that are
/// not Rust symbols: configs, docs, new modules, generated files. Overwriting
/// an existing file needs either its blake3 (`expected_hash`) or an explicit
/// `overwrite: true`, so a stale write cannot silently erase work.
pub struct WriteTool {
    root_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct WriteParams {
    pub path: String,
    pub content: String,
    pub expected_hash: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default = "default_true")]
    pub create_parents: bool,
    #[serde(default)]
    pub create_only: bool,
}

fn default_true() -> bool {
    true
}

impl WriteTool {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn write(&self, params: WriteParams) -> CallToolResult {
        let (path, existed) = match resolve_for_write(&self.root_dir, &params.path) {
            Ok(resolved) => resolved,
            Err(message) => return CallToolResult::error(message),
        };

        if existed {
            if params.create_only {
                return CallToolResult::error(format!(
                    "Arquivo já existe: {}. Use `overwrite: true` (ou `expected_hash`) para substituir.",
                    params.path
                ));
            }

            let current = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    return CallToolResult::error(format!("Falha ao ler {}: {e}", params.path))
                }
            };
            if String::from_utf8(current.clone()).is_err() {
                return CallToolResult::error(format!(
                    "O arquivo {} não é UTF-8 válido; nada foi escrito.",
                    params.path
                ));
            }

            match params.expected_hash.as_deref() {
                Some(expected) if !hash_matches(&current, expected) => {
                    return CallToolResult::error(format!(
                        "Arquivo mudou desde a leitura. Hash atual: `{}`. \
                         Reenvie com expected_hash atualizado.",
                        hash_prefix(&current)
                    ));
                }
                None if !params.overwrite => {
                    return CallToolResult::error(format!(
                        "`{}` já existe. Informe o blake3 atual em `expected_hash` \
                         ou use `overwrite: true` para substituir sem conferência.",
                        params.path
                    ));
                }
                _ => {}
            }

            if current == params.content.as_bytes() {
                return CallToolResult::text(format!(
                    "### `{}` — sem mudanças\n\nArquivo: blake3 `{}` · {} linhas\n",
                    params.path,
                    hash_prefix(&current),
                    count_lines(&params.content)
                ));
            }
        }

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if !params.create_parents {
                    return CallToolResult::error(format!(
                        "Diretório pai não existe: {}. Use `create_parents: true` para criá-lo.",
                        parent.display()
                    ));
                }
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return CallToolResult::error(format!(
                        "Falha ao criar {}: {e}. Nada foi escrito.",
                        parent.display()
                    ));
                }
            }
        }

        let bytes = params.content.as_bytes();
        if let Err(e) = atomic_write(&path, bytes) {
            return CallToolResult::error(format!(
                "Falha ao escrever {}: {e}. Nada foi escrito.",
                params.path
            ));
        }

        match std::fs::read(&path) {
            Ok(on_disk) if on_disk == bytes => {}
            Ok(_) => {
                return CallToolResult::error(format!(
                    "A verificação pós-escrita falhou: o conteúdo em disco difere do enviado ({}).",
                    params.path
                ))
            }
            Err(e) => {
                return CallToolResult::error(format!(
                    "A escrita foi feita, mas a verificação não conseguiu reler {}: {e}",
                    params.path
                ))
            }
        }

        let verb = if existed { "atualizado" } else { "criado" };
        CallToolResult::text(format!(
            "### `{}` — {verb}\n\nArquivo: blake3 `{}` · {} linhas\n",
            params.path,
            hash_prefix(bytes),
            count_lines(&params.content)
        ))
    }
}

fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count()
    }
}

impl ToolPlugin for WriteTool {
    fn name(&self) -> &'static str {
        "brain_write"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_write".to_string(),
            description: "Cria ou substitui um arquivo de texto inteiro sob o diretório raiz. Substitui `write` para arquivos que não são símbolos Rust (configs, docs, módulos novos). Sobrescrever exige o blake3 atual em `expected_hash` ou `overwrite: true`; a escrita é atômica e relida do disco antes da resposta.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Caminho relativo ao diretório raiz do projeto."
                    },
                    "content": {
                        "type": "string",
                        "description": "Conteúdo completo do arquivo, em UTF-8. Bytes crus: sem formatação automática."
                    },
                    "expected_hash": {
                        "type": "string",
                        "description": "Opcional para arquivo novo; obrigatório para sobrescrever um existente, salvo com `overwrite: true`. blake3 do conteúdo atual."
                    },
                    "overwrite": {
                        "type": "boolean",
                        "default": false,
                        "description": "Substitui um arquivo existente sem conferir hash. Use só quando a sobrescrita é intencional."
                    },
                    "create_parents": {
                        "type": "boolean",
                        "default": true,
                        "description": "Cria diretórios intermediários dentro do root."
                    },
                    "create_only": {
                        "type": "boolean",
                        "default": false,
                        "description": "Falha se o arquivo já existir."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: WriteParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.write(params) })
    }
}

/// Resolves a target that may not exist yet, without allowing `..` or a symlink
/// to escape the root.
fn resolve_for_write(
    root_dir: &Path,
    requested: &str,
) -> std::result::Result<(PathBuf, bool), String> {
    let root = root_dir
        .canonicalize()
        .map_err(|e| format!("Diretório raiz inválido ({}): {e}", root_dir.display()))?;

    let candidate = Path::new(requested);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let normalized = lexical_normalize(&joined);
    if !normalized.starts_with(&root) {
        return Err(format!("Caminho fora do diretório raiz: {requested}"));
    }

    if normalized.exists() {
        let canonical = normalized
            .canonicalize()
            .map_err(|e| format!("Caminho inválido ({requested}): {e}"))?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "Caminho resolve para fora do diretório raiz (symlink): {requested}"
            ));
        }
        return Ok((canonical, true));
    }

    // Walk up to the nearest existing directory; canonicalize it (so symlinked
    // parents are checked) and rebuild the not-yet-existing suffix below it.
    let mut ancestor = normalized.parent();
    while let Some(dir) = ancestor {
        if dir.exists() {
            break;
        }
        ancestor = dir.parent();
    }
    let ancestor = ancestor.ok_or_else(|| format!("Caminho inválido: {requested}"))?;
    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|e| format!("Caminho inválido ({requested}): {e}"))?;
    if !canonical_ancestor.starts_with(&root) {
        return Err(format!(
            "Caminho resolve para fora do diretório raiz (symlink): {requested}"
        ));
    }
    let suffix = normalized
        .strip_prefix(ancestor)
        .map_err(|_| format!("Caminho inválido: {requested}"))?;
    Ok((canonical_ancestor.join(suffix), false))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn tool(dir: &tempfile::TempDir) -> WriteTool {
        WriteTool::new(dir.path())
    }

    #[test]
    fn creates_a_new_file_and_nested_directories() {
        let dir = tempdir().unwrap();
        let out = tool(&dir).write(WriteParams {
            path: "docs/new/notes.md".into(),
            content: "# notas\n".into(),
            expected_hash: None,
            overwrite: false,
            create_parents: true,
            create_only: false,
        });
        assert_eq!(out.is_error, None, "{}", out.content[0].text);
        assert_eq!(
            fs::read_to_string(dir.path().join("docs/new/notes.md")).unwrap(),
            "# notas\n"
        );
    }

    #[test]
    fn overwriting_without_hash_or_permission_is_refused() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old").unwrap();

        let out = tool(&dir).write(WriteParams {
            path: "a.txt".into(),
            content: "new".into(),
            expected_hash: None,
            overwrite: false,
            create_parents: true,
            create_only: false,
        });
        assert_eq!(out.is_error, Some(true));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "old");
    }

    #[test]
    fn an_overwrite_with_the_right_hash_succeeds() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old").unwrap();
        let hash = hash_prefix(b"old");

        let out = tool(&dir).write(WriteParams {
            path: "a.txt".into(),
            content: "new".into(),
            expected_hash: Some(hash),
            overwrite: false,
            create_parents: true,
            create_only: false,
        });
        assert_eq!(out.is_error, None, "{}", out.content[0].text);
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "new");
    }

    #[test]
    fn a_stale_hash_never_writes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old").unwrap();

        let out = tool(&dir).write(WriteParams {
            path: "a.txt".into(),
            content: "new".into(),
            expected_hash: Some("deadbeef0000".into()),
            overwrite: false,
            create_parents: true,
            create_only: false,
        });
        assert_eq!(out.is_error, Some(true));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "old");
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        let dir = tempdir().unwrap();
        let out = tool(&dir).write(WriteParams {
            path: "../../etc/passwd".into(),
            content: "x".into(),
            expected_hash: None,
            overwrite: true,
            create_parents: true,
            create_only: false,
        });
        assert_eq!(out.is_error, Some(true));
    }
}
