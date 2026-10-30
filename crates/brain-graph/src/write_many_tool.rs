//! `brain_write_many`: grava vários arquivos como um lote transacional.
//!
//! A ordem é validar tudo — caminho, hash, UTF-8, pai e duplicata — e só
//! então escrever. Se qualquer item for inválido, nada toca o disco. Se uma
//! escrita falhar no meio, os arquivos já gravados voltam ao conteúdo original.
//!
//! O relatório é cirúrgico: por item, só verbo, blake3 e linhas. O conteúdo
//! enviado não é repetido na resposta.

use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use serde::Deserialize;

use crate::edit_tool::{atomic_write, hash_matches, hash_prefix};
use crate::write_tool::{count_lines, resolve_for_write, WriteParams};

#[derive(Deserialize)]
pub struct WriteManyParams {
    pub writes: Vec<WriteParams>,
}

pub struct WriteManyTool {
    root_dir: PathBuf,
}

struct Planned {
    item: WriteParams,
    path: PathBuf,
    original: Vec<u8>,
    existed: bool,
    unchanged: bool,
}

struct Applied {
    path: PathBuf,
    label: String,
    original: Option<Vec<u8>>,
}

impl WriteManyTool {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn write_many(&self, params: WriteManyParams) -> CallToolResult {
        if params.writes.is_empty() {
            return CallToolResult::error("`writes` está vazio: nada a gravar.");
        }

        let mut planned = Vec::with_capacity(params.writes.len());
        let mut errors = Vec::new();
        let mut seen = HashSet::new();

        for (index, item) in params.writes.into_iter().enumerate() {
            match plan_one(&self.root_dir, item) {
                Ok(plan) if seen.insert(plan.path.clone()) => planned.push(plan),
                Ok(plan) => errors.push(format!(
                    "- item {} (`{}`): caminho repetido no lote.",
                    index + 1,
                    plan.item.path
                )),
                Err(message) => errors.push(format!("- item {}: {message}", index + 1)),
            }
        }

        if !errors.is_empty() {
            return CallToolResult::error(format!(
                "### `brain_write_many` — nada foi escrito\n\n{}\n",
                errors.join("\n")
            ));
        }

        self.apply(planned)
    }

    fn apply(&self, planned: Vec<Planned>) -> CallToolResult {
        let total = planned.len();
        let mut applied: Vec<Applied> = Vec::with_capacity(total);
        let mut lines = Vec::with_capacity(total);
        let mut created = 0usize;
        let mut updated = 0usize;
        let mut unchanged = 0usize;

        for plan in planned {
            if plan.unchanged {
                unchanged += 1;
                lines.push(format!(
                    "- `{}` — sem mudanças · blake3 `{}` · {} linhas",
                    plan.item.path,
                    hash_prefix(plan.item.content.as_bytes()),
                    count_lines(&plan.item.content)
                ));
                continue;
            }

            // A validação e a gravação são separadas por um intervalo: reler
            // aqui fecha a janela em que outro processo poderia ter mexido.
            if plan.existed {
                match fs::read(&plan.path) {
                    Ok(current) if current == plan.original => {}
                    Ok(_) => {
                        return rollback(
                            applied,
                            format!(
                                "`{}` mudou entre a validação e a gravação",
                                plan.item.path
                            ),
                        )
                    }
                    Err(e) => {
                        return rollback(
                            applied,
                            format!(
                                "não foi possível reler `{}` antes de gravar: {e}",
                                plan.item.path
                            ),
                        )
                    }
                }
            } else if plan.path.exists() {
                return rollback(
                    applied,
                    format!(
                        "`{}` passou a existir entre a validação e a gravação",
                        plan.item.path
                    ),
                );
            }

            if let Some(parent) = plan.path.parent() {
                if !parent.exists() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        return rollback(
                            applied,
                            format!("falha ao criar `{}`: {e}", parent.display()),
                        );
                    }
                }
            }

            let bytes = plan.item.content.as_bytes();
            if let Err(e) = atomic_write(&plan.path, bytes) {
                return rollback(
                    applied,
                    format!("falha ao gravar `{}`: {e}", plan.item.path),
                );
            }

            applied.push(Applied {
                path: plan.path.clone(),
                label: plan.item.path.clone(),
                original: plan.existed.then(|| plan.original.clone()),
            });

            match fs::read(&plan.path) {
                Ok(on_disk) if on_disk == bytes => {}
                Ok(_) => {
                    return rollback(
                        applied,
                        format!(
                            "verificação pós-escrita falhou em `{}`: o disco difere do enviado",
                            plan.item.path
                        ),
                    )
                }
                Err(e) => {
                    return rollback(
                        applied,
                        format!(
                            "gravação feita, mas não foi possível reler `{}`: {e}",
                            plan.item.path
                        ),
                    )
                }
            }

            if plan.existed {
                updated += 1;
            } else {
                created += 1;
            }
            lines.push(format!(
                "- `{}` — {} · blake3 `{}` · {} linhas",
                plan.item.path,
                if plan.existed { "atualizado" } else { "criado" },
                hash_prefix(bytes),
                count_lines(&plan.item.content)
            ));
        }

        CallToolResult::text(format!(
            "### `brain_write_many` — {total} itens · {created} criados · {updated} atualizados · {unchanged} sem mudança\n\n{}\n",
            lines.join("\n")
        ))
    }
}

fn plan_one(root_dir: &Path, item: WriteParams) -> Result<Planned, String> {
    let (path, existed) = resolve_for_write(root_dir, &item.path)?;
    let mut original = Vec::new();
    let mut unchanged = false;

    if existed {
        if item.create_only {
            return Err(format!(
                "`{}` já existe; `create_only` recusa o lote inteiro.",
                item.path
            ));
        }

        let current = fs::read(&path).map_err(|e| format!("falha ao ler `{}`: {e}", item.path))?;
        if String::from_utf8(current.clone()).is_err() {
            return Err(format!(
                "`{}` não é UTF-8 válido; nada foi escrito.",
                item.path
            ));
        }

        match item.expected_hash.as_deref() {
            Some(expected) if !hash_matches(&current, expected) => {
                return Err(format!(
                    "`{}` mudou desde a leitura (blake3 atual `{}`); reenvie com `expected_hash` atualizado.",
                    item.path,
                    hash_prefix(&current)
                ));
            }
            None if !item.overwrite => {
                return Err(format!(
                    "`{}` já existe; informe `expected_hash` ou `overwrite: true`.",
                    item.path
                ));
            }
            _ => {}
        }

        unchanged = current == item.content.as_bytes();
        original = current;
    } else if let Some(parent) = path.parent() {
        if !parent.exists() && !item.create_parents {
            return Err(format!(
                "diretório pai de `{}` não existe; use `create_parents: true`.",
                item.path
            ));
        }
    }

    Ok(Planned {
        item,
        path,
        original,
        existed,
        unchanged,
    })
}

fn rollback(applied: Vec<Applied>, cause: String) -> CallToolResult {
    let mut lines = vec![format!("- causa: {cause}")];
    let mut failures = Vec::new();

    for entry in applied.iter().rev() {
        let result = match &entry.original {
            Some(bytes) => atomic_write(&entry.path, bytes),
            None => fs::remove_file(&entry.path),
        };
        match result {
            Ok(()) => lines.push(format!(
                "- `{}` — {}",
                entry.label,
                if entry.original.is_some() {
                    "restaurado"
                } else {
                    "removido"
                }
            )),
            Err(e) => failures.push(format!("- `{}` — NÃO revertido: {e}", entry.label)),
        }
    }

    let header = if failures.is_empty() {
        "### `brain_write_many` — lote desfeito, nada foi escrito"
    } else {
        "### `brain_write_many` — ROLLBACK INCOMPLETO"
    };
    if !failures.is_empty() {
        lines.push(String::new());
        lines.extend(failures);
    }

    CallToolResult::error(format!("{header}\n\n{}\n", lines.join("\n")))
}

impl ToolPlugin for WriteManyTool {
    fn name(&self) -> &'static str {
        "brain_write_many"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_write_many".to_string(),
            description: "Grava vários arquivos como um lote: valida todos antes de escrever, aborta sem tocar no disco se um item for inválido e desfaz o que já gravou se uma escrita falhar no meio.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "writes": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Arquivos a gravar, na ordem do relatório. Caminhos repetidos são recusados.",
                        "items": {
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
                                    "description": "Obrigatório para sobrescrever um existente, salvo com `overwrite: true`. blake3 do conteúdo atual."
                                },
                                "overwrite": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "Substitui um arquivo existente sem conferir hash."
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
                        }
                    }
                },
                "required": ["writes"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: WriteManyParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.write_many(params) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn tool(dir: &tempfile::TempDir) -> WriteManyTool {
        WriteManyTool::new(dir.path())
    }

    fn item(path: &str, content: &str) -> WriteParams {
        WriteParams {
            path: path.into(),
            content: content.into(),
            expected_hash: None,
            overwrite: false,
            create_parents: true,
            create_only: false,
        }
    }

    fn text(out: &CallToolResult) -> &str {
        &out.content[0].text
    }

    #[test]
    fn a_batch_writes_every_file() {
        let dir = tempdir().unwrap();
        let out = tool(&dir).write_many(WriteManyParams {
            writes: vec![
                item("a.rs", "fn a() {}\n"),
                item("nested/b.rs", "fn b() {}\n"),
            ],
        });
        assert_eq!(out.is_error, None, "{}", text(&out));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("nested/b.rs")).unwrap(),
            "fn b() {}\n"
        );
        assert!(text(&out).contains("2 criados"), "{}", text(&out));
    }

    #[test]
    fn one_stale_hash_leaves_the_whole_batch_untouched() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("old.rs"), "antigo\n").unwrap();

        let mut stale = item("old.rs", "novo\n");
        stale.expected_hash = Some("000000000000".into());

        let out = tool(&dir).write_many(WriteManyParams {
            writes: vec![item("novo.rs", "novo\n"), stale],
        });

        assert_eq!(out.is_error, Some(true));
        assert_eq!(
            fs::read_to_string(dir.path().join("old.rs")).unwrap(),
            "antigo\n"
        );
        assert!(!dir.path().join("novo.rs").exists());
        assert!(text(&out).contains("nada foi escrito"), "{}", text(&out));
    }

    #[test]
    fn a_duplicate_path_is_rejected_before_writing() {
        let dir = tempdir().unwrap();
        let out = tool(&dir).write_many(WriteManyParams {
            writes: vec![item("a.rs", "um\n"), item("a.rs", "dois\n")],
        });
        assert_eq!(out.is_error, Some(true));
        assert!(!dir.path().join("a.rs").exists());
        assert!(text(&out).contains("repetido"), "{}", text(&out));
    }

    #[test]
    fn an_unchanged_item_is_reported_and_the_rest_still_writes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("same.rs"), "igual\n").unwrap();

        let mut same = item("same.rs", "igual\n");
        same.expected_hash = Some(hash_prefix(b"igual\n"));

        let out = tool(&dir).write_many(WriteManyParams {
            writes: vec![same, item("new.rs", "novo\n")],
        });

        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).contains("sem mudanças"), "{}", text(&out));
        assert_eq!(
            fs::read_to_string(dir.path().join("new.rs")).unwrap(),
            "novo\n"
        );
    }

    #[test]
    fn a_missing_parent_without_create_parents_aborts_the_batch() {
        let dir = tempdir().unwrap();
        let mut nested = item("missing/dir/a.rs", "x\n");
        nested.create_parents = false;

        let out = tool(&dir).write_many(WriteManyParams {
            writes: vec![item("ok.rs", "ok\n"), nested],
        });

        assert_eq!(out.is_error, Some(true));
        assert!(!dir.path().join("ok.rs").exists());
        assert!(text(&out).contains("create_parents"), "{}", text(&out));
    }
}
