//! `brain_setup`: escreve a documentação que ensina o agente a usar
//! este MCP — um bloco marcado em `AGENTS.md` e uma skill portátil.
//!
//! A lista de tools vem do registro vivo, não de uma cópia à mão: a documentação
//! não pode ensinar uma assinatura que o servidor não responde mais.

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use crate::plugin::ToolPlugin;
use crate::protocol::{CallToolResult, Tool};

/// Delimitadores do bloco gerenciado em `AGENTS.md`. Reexecutar o setup
/// substitui só o que está entre eles.
pub const BEGIN_MARKER: &str = "<!-- brain-tools:begin -->";
pub const END_MARKER: &str = "<!-- brain-tools:end -->";
pub const DEFAULT_SKILLS_DIR: &str = "skills/brain-tools";
pub const DEFAULT_AGENTS_FILE: &str = "AGENTS.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupAction {
    /// Mostra o que seria escrito, sem tocar no disco.
    Preview,
    /// Escreve de fato.
    Apply,
}

fn default_action() -> SetupAction {
    SetupAction::Preview
}

#[derive(Debug, serde::Deserialize)]
pub struct SetupParams {
    #[serde(default = "default_action")]
    pub action: SetupAction,
    /// Diretório da skill, relativo à raiz do projeto.
    pub skills_dir: Option<String>,
    /// Arquivo de instruções do agente, relativo à raiz do projeto.
    pub agents_file: Option<String>,
}

pub struct SetupTool {
    root_dir: PathBuf,
    tools: Vec<Tool>,
}

impl SetupTool {
    pub fn new(root_dir: impl Into<PathBuf>, tools: Vec<Tool>) -> Self {
        Self {
            root_dir: root_dir.into(),
            tools,
        }
    }

    pub fn handle_setup(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: SetupParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let skills_dir = params.skills_dir.as_deref().unwrap_or(DEFAULT_SKILLS_DIR);
        let agents_file = params.agents_file.as_deref().unwrap_or(DEFAULT_AGENTS_FILE);

        let skill_path = match self.resolve(skills_dir).map(|p| p.join("SKILL.md")) {
            Ok(p) => p,
            Err(message) => return CallToolResult::error(message),
        };
        let reference_path = skill_path.with_file_name("reference.md");
        let agents_path = match self.resolve(agents_file) {
            Ok(p) => p,
            Err(message) => return CallToolResult::error(message),
        };

        let block = self.agents_block();
        let skill = self.skill_markdown();
        let reference = self.reference_markdown();

        let existing_agents = std::fs::read_to_string(&agents_path).unwrap_or_default();
        let merged_agents = upsert_block(&existing_agents, &block);

        if params.action == SetupAction::Preview {
            return CallToolResult::text(format!(
                "### Preview — nada foi escrito\n\nEscreveria sob `{}`:\n\n\
                 #### `{}` (bloco entre marcadores)\n\n{}\n\
                 #### `{}`\n\n{}\n\
                 #### `{}`\n\n{}\n\
                 Use `action: apply` para gravar.",
                self.root_dir.display(),
                relative(&self.root_dir, &agents_path),
                block,
                relative(&self.root_dir, &skill_path),
                truncate(&skill, 1200),
                relative(&self.root_dir, &reference_path),
                truncate(&reference, 800),
            ));
        }

        let mut written = Vec::new();
        if merged_agents != existing_agents {
            if let Err(message) = write_verified(&agents_path, &merged_agents) {
                return CallToolResult::error(message);
            }
            written.push(format!(
                "`{}` (bloco atualizado)",
                relative(&self.root_dir, &agents_path)
            ));
        }
        for (path, body) in [(&skill_path, &skill), (&reference_path, &reference)] {
            if let Err(message) = write_verified(path, body) {
                return CallToolResult::error(message);
            }
            written.push(format!("`{}`", relative(&self.root_dir, path)));
        }

        CallToolResult::text(format!(
            "### Setup aplicado\n\n{}\n\n\
             A skill está em `{}`. Aponte o diretório de skills do seu cliente \
             para ela, ou copie/simlinke para o local que ele descobre \
             (`.opencode/skills/`, `.claude/skills/`, …).\n\n\
             Registre o servidor no cliente, por exemplo:\n\n\
             ```json\n{}\n```\n\n\
             O bloco em `{}` é delimitado — reexecutar substitui só ele e \
             preserva o resto do arquivo.",
            written.join("\n"),
            relative(&self.root_dir, &skill_path),
            mcp_snippet(&self.root_dir),
            relative(&self.root_dir, &agents_path),
        ))
    }

    fn resolve(&self, requested: &str) -> std::result::Result<PathBuf, String> {
        let path = Path::new(requested);
        if path.is_absolute() {
            return Err(format!(
                "`{requested}` é absoluto. Use um caminho relativo à raiz do projeto."
            ));
        }
        if path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(format!(
                "`{requested}` sai da raiz do projeto. Nada foi escrito."
            ));
        }
        Ok(self.root_dir.join(path))
    }

    fn agents_block(&self) -> String {
        let count = self.tools.len();
        format!(
            "{BEGIN_MARKER}\n\
             ## brain-tools — fonte de verdade do código\n\n\
             Este repositório expõe o MCP `brain-tools` ({count} tools). O resultado dele **é** a \
             leitura do arquivo: trecho verbatim com `path:start-end`. Trate \
             `brain_search`, `brain_context_graph` e `brain_impact_check` como já lido e não reabra o \
             mesmo trecho com `Read` ou `cat`.\n\n\
             - `grep`, `rg`, `find`, `cat`, `sed` → `brain_search`, `brain_read`, `brain_map`\n\
             - editar símbolo → `brain_edit` com o hash do `brain_read`; arquivo inteiro → `brain_write`; diff → `brain_patch`\n\
             - `cargo build/check/test/clippy` → `brain_exec`\n\
             - truncou? o handle vai para `brain_peek`\n\n\
             Fluxo detalhado: skill `brain-tools`. Se uma chamada falhar, diga isso \
             explicitamente — não caia em `grep` silencioso.\n\
             {END_MARKER}"
        )
    }

    fn skill_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "---\n\
             name: brain-tools\n\
             description: Explora e edita código neste repositório pelo MCP brain-tools — busca estrutural, grafo de chamadas, leitura por símbolo e edição com hash. Use no lugar de grep, rg, cat, find, sed e da tool Read.\n\
             ---\n\n\
             # brain-tools\n\n\
             O resultado das tools do `brain-tools` **é** a leitura do arquivo:\n\
             trecho verbatim, com `path:start-end`. Não reabra o mesmo trecho com\n\
             `Read`, `cat` ou `sed`.\n\n\
             ## Ordem de prioridade\n\n\
             1. `brain_search` — onde está e como funciona algo.\n\
             2. `brain_context_graph` / `brain_impact_check` — quem chama, o que quebra.\n\
             3. `brain_read` — esqueleto do arquivo ou corpo de um símbolo.\n\
             4. `brain_map` — orientação quando você não sabe onde está.\n\
             5. `brain_edit` — editar corpo de símbolo com o hash do `brain_read`.\n\
             6. `brain_write` / `brain_patch` — arquivo inteiro ou diff unificado.\n\
             7. `brain_exec` — build, check, test, clippy.\n\
             8. `brain_shell` — só git e formatador; qualquer outro comando é negado.\n\n\
             ## Não use → use\n\n\
             | Não use | Use |\n|---|---|\n",
        );
        for (dont, use_it) in tool_map() {
            if self.has(first_tool(use_it)) {
                out.push_str(&format!("| {dont} | {use_it} |\n"));
            }
        }
        out.push_str(
            "\n## Contrato de resposta\n\n\
             - Saída truncada vem com um handle (`h_xxxxxxxx`): recupere com `brain_peek`.\n\
             - Chamada que falha é dita explicitamente. Nada de contornar por script próprio.\n\
             - `brain_edit` recusa hash divergente e corpo com sintaxe inválida: releia e reenvie.\n\n\
             ## Tools disponíveis\n\n\
             Gerado de `tools/list` — nomes e descrições são os que o servidor responde.\n\n",
        );
        for tool in &self.tools {
            out.push_str(&format!("- `{}` — {}\n", tool.name, tool.description));
        }
        out
    }

    fn reference_markdown(&self) -> String {
        let mut out = String::from("# brain-tools — referência\n\n");
        out.push_str(&format!(
            "Root do projeto: `{}`\n\n",
            self.root_dir.display()
        ));
        for tool in &self.tools {
            out.push_str(&format!("## `{}`\n\n{}\n\n", tool.name, tool.description));
        }
        out
    }

    fn has(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }
}

/// Linhas da tabela `não use → use`, filtradas pelo que o perfil expõe.
fn tool_map() -> &'static [(&'static str, &'static str)] {
    &[
        ("`grep`, `rg`, `ag`, `ack`", "`brain_search`"),
        ("`cat`, `head`, `tail`, `sed`, `awk`", "`brain_read`"),
        ("`find`, `ls`, `tree`, `fd`", "`brain_map`"),
        (
            "`sed -i`, `tee`, redirecionamento",
            "`brain_edit` / `brain_write`",
        ),
        ("`patch`, `git apply`", "`brain_patch`"),
        ("`cargo build/check/test/clippy`", "`brain_exec`"),
        ("`git diff`, `git show`", "`brain_diff`"),
        ("`git log -L`, `git blame`", "`brain_why`"),
    ]
}

fn first_tool(use_it: &str) -> &str {
    let start = match use_it.find('`') {
        Some(i) => i + 1,
        None => return use_it,
    };
    let end = use_it[start..]
        .find('`')
        .map(|i| start + i)
        .unwrap_or(use_it.len());
    &use_it[start..end]
}

/// Insere ou substitui o bloco marcado, preservando todo o resto do arquivo.
pub fn upsert_block(existing: &str, block: &str) -> String {
        if let (Some(begin), Some(end)) = (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
            if end > begin {
                let rest = &existing[end + END_MARKER.len()..];
                let rest = rest.strip_prefix('\n').unwrap_or(rest);
                let mut merged = String::with_capacity(existing.len() + block.len());
                merged.push_str(&existing[..begin]);
                merged.push_str(block);
                if rest.is_empty() {
                    // O primeiro apply grava bloco + "\n"; sem isso o segundo
                    // apply removeria o newline e o arquivo mudaria sozinho.
                    merged.push('\n');
                } else {
                    if !rest.starts_with('\n') {
                        merged.push('\n');
                    }
                    merged.push_str(rest);
                }
                return merged;
            }
        }

        let trimmed = existing.trim_end();
        if trimmed.is_empty() {
            format!("{block}\n")
        } else {
            format!("{trimmed}\n\n{block}\n")
        }
    }

fn write_verified(path: &Path, body: &str) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Falha ao criar {}: {e}. Nada foi escrito.",
                    parent.display()
                )
            })?;
        }
    }
    std::fs::write(path, body).map_err(|e| format!("Falha ao escrever {}: {e}", path.display()))?;
    match std::fs::read_to_string(path) {
        Ok(on_disk) if on_disk == body => Ok(()),
        Ok(_) => Err(format!(
            "Verificação pós-escrita falhou: o conteúdo em disco difere do enviado ({}).",
            path.display()
        )),
        Err(e) => Err(format!(
            "A escrita foi feita, mas não foi possível reler {}: {e}",
            path.display()
        )),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n… (truncado no preview; `apply` grava o arquivo inteiro)")
}

fn mcp_snippet(root: &Path) -> String {
    format!(
        "{{\n  \"mcp\": {{\n    \"brain-tools\": {{\n      \"type\": \"local\",\n      \"command\": [\"brain-tools\"],\n      \"cwd\": \"{}\"\n    }}\n  }}\n}}",
        root.display()
    )
}

impl ToolPlugin for SetupTool {
    fn name(&self) -> &'static str {
        "brain_setup"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_setup".to_string(),
            description: "Gera a documentação do MCP (bloco em AGENTS.md + skill brain-tools) a partir da lista viva de tools. Começa em preview; só grava com action=apply."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["preview", "apply"],
                        "description": "preview mostra o que seria escrito; apply grava os arquivos."
                    },
                    "skills_dir": {
                        "type": "string",
                        "description": "Diretório da skill, relativo à raiz. Padrão: skills/brain-tools."
                    },
                    "agents_file": {
                        "type": "string",
                        "description": "Arquivo de instruções do agente. Padrão: AGENTS.md."
                    }
                }
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(async move { self.handle_setup(args) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn tools() -> Vec<Tool> {
        ["brain_search", "brain_read", "brain_edit", "brain_exec"]
            .iter()
            .map(|name| Tool {
                name: (*name).to_string(),
                description: format!("desc de {name}"),
                input_schema: serde_json::json!({ "type": "object" }),
            })
            .collect()
    }

    fn result_text(result: CallToolResult) -> String {
        result.content[0].text.clone()
    }

    fn tool(dir: &Path) -> SetupTool {
        SetupTool::new(dir, tools())
    }

    #[test]
    fn preview_writes_nothing() {
        let dir = tempdir().unwrap();
        let output = result_text(tool(dir.path()).handle_setup(serde_json::json!({})));
        assert!(output.contains("nada foi escrito"), "{output}");
        assert!(!dir.path().join("AGENTS.md").exists());
    }

    #[test]
    fn apply_writes_agents_and_skill() {
        let dir = tempdir().unwrap();
        result_text(tool(dir.path()).handle_setup(serde_json::json!({ "action": "apply" })));

        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents.contains(BEGIN_MARKER) && agents.contains(END_MARKER));

        let skill =
            std::fs::read_to_string(dir.path().join("skills/brain-tools/SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: brain-tools"), "{skill}");
        assert!(
            skill.contains("`brain_edit` — desc de brain_edit"),
            "{skill}"
        );
    }

    #[test]
    fn apply_is_idempotent() {
        let dir = tempdir().unwrap();
        let t = tool(dir.path());
        t.handle_setup(serde_json::json!({ "action": "apply" }));
        let first = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        t.handle_setup(serde_json::json!({ "action": "apply" }));
        let second = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(first, second);
        assert_eq!(second.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn user_content_outside_the_block_survives() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# Regras\n\nNão use unwrap.\n",
        )
        .unwrap();

        let t = tool(dir.path());
        t.handle_setup(serde_json::json!({ "action": "apply" }));
        t.handle_setup(serde_json::json!({ "action": "apply" }));

        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("# Regras"));
        assert!(agents.contains("Não use unwrap."));
        assert_eq!(agents.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        let dir = tempdir().unwrap();
        let output = result_text(tool(dir.path()).handle_setup(serde_json::json!({
            "action": "apply",
            "skills_dir": "../fora"
        })));
        assert!(output.contains("sai da raiz"), "{output}");
    }

    #[test]
    fn the_skill_lists_exactly_the_registered_tools() {
        let dir = tempdir().unwrap();
        let t = tool(dir.path());
        let skill = t.skill_markdown();
        for tool in &t.tools {
            assert!(
                skill.contains(&format!("`{}` — {}", tool.name, tool.description)),
                "{skill}"
            );
        }
        assert!(!skill.contains("brain_peek —"));
    }
}
