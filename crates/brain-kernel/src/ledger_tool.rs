use crate::budget::{self, Section};
use crate::plugin::ToolPlugin;
use crate::protocol::{CallToolResult, Tool};
use crate::store::ResponseStore;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Facts and decisions that outlive a conversation, kept next to the code.
///
/// The ledger is append-only JSONL so a corrupt line loses one entry, never the
/// file. Entries are deduplicated by the hash of their normalized text.
pub struct LedgerTool {
    path: PathBuf,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct LedgerParams {
    pub action: LedgerAction,
    /// The fact to record, or the query to search for.
    pub text: Option<String>,
    pub tags: Option<Vec<String>>,
    pub budget_tokens: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerAction {
    Record,
    List,
    Search,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    hash: String,
    text: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    recorded_at: u64,
}

impl LedgerTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
    ) -> Self {
        Self {
            path: root_dir.into().join(".brain").join("ledger.jsonl"),
            store,
            default_budget_tokens,
        }
    }

    pub fn handle_ledger(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: LedgerParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let sections = match params.action {
            LedgerAction::Record => return self.record(&params),
            LedgerAction::List => self.list_sections(&self.entries()),
            LedgerAction::Search => {
                let Some(query) = params.text.as_deref().map(str::trim).filter(|q| !q.is_empty())
                else {
                    return CallToolResult::error(
                        "A ação `search` exige `text` com os termos da busca.".to_string(),
                    );
                };
                self.search_sections(&self.entries(), query)
            }
        };

        if sections.is_empty() {
            return CallToolResult::text("Ledger vazio.".to_string());
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

    fn record(&self, params: &LedgerParams) -> CallToolResult {
        let Some(text) = params.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            return CallToolResult::error(
                "A ação `record` exige `text` não vazio.".to_string(),
            );
        };

        let normalized = normalize(text);
        let hash = hash_of(&normalized);
        let entries = self.entries();

        if let Some(existing) = entries.iter().find(|entry| entry.hash == hash) {
            return CallToolResult::text(format!(
                "Já registrado — `{}` — {}",
                short(&existing.hash),
                existing.text
            ));
        }

        let entry = Entry {
            hash: hash.clone(),
            text: normalized,
            tags: params.tags.clone().unwrap_or_default(),
            recorded_at: now_secs(),
        };

        if let Err(e) = self.append(&entry) {
            return CallToolResult::error(format!("Falha ao gravar o ledger: {e}"));
        }

        CallToolResult::text(format!("Registrado — `{}` — {}", short(&hash), entry.text))
    }

    fn entries(&self) -> Vec<Entry> {
        let Ok(content) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };

        content
            .lines()
            .filter_map(|line| serde_json::from_str::<Entry>(line.trim()).ok())
            .collect()
    }

    fn append(&self, entry: &Entry) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")
    }

    fn list_sections(&self, entries: &[Entry]) -> Vec<Section> {
        let mut ordered: Vec<&Entry> = entries.iter().collect();
        // Newest first: the most recent decision is the one being asked about.
        ordered.sort_by_key(|entry| std::cmp::Reverse(entry.recorded_at));

        ordered
            .into_iter()
            .map(|entry| entry_section(entry, 200))
            .collect()
    }

    fn search_sections(&self, entries: &[Entry], query: &str) -> Vec<Section> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();

        let mut matches: Vec<(usize, &Entry)> = entries
            .iter()
            .filter_map(|entry| {
                let haystack = entry.text.to_lowercase();
                let score = terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count();
                (score > 0).then_some((score, entry))
            })
            .collect();

        matches.sort_by(|(score_a, entry_a), (score_b, entry_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| entry_b.recorded_at.cmp(&entry_a.recorded_at))
        });

        matches
            .into_iter()
            .map(|(score, entry)| {
                // Better matches survive the budget.
                let priority = (100 + score * 30).min(250) as u8;
                entry_section(entry, priority)
            })
            .collect()
    }
}

fn entry_section(entry: &Entry, priority: u8) -> Section {
    let tags = if entry.tags.is_empty() {
        String::new()
    } else {
        format!("\nTags: {}\n", entry.tags.join(", "))
    };
    Section {
        heading: format!("#### `{}`", short(&entry.hash)),
        body: format!("{}{tags}", entry.text),
        priority,
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hash_of(normalized: &str) -> String {
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

impl ToolPlugin for LedgerTool {
    fn name(&self) -> &'static str {
        "brain_ledger"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_ledger".to_string(),
            description: "Registra e consulta fatos e decisões duráveis do projeto (append-only em .brain/ledger.jsonl, deduplicado por conteúdo). Use para não redescobrir a mesma decisão depois.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["record", "list", "search"],
                        "description": "`record` grava um fato, `list` devolve os mais recentes, `search` filtra por termos."
                    },
                    "text": {
                        "type": "string",
                        "description": "O fato a gravar (record) ou os termos da busca (search)."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Opcional: rótulos do fato gravado."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(async move { self.handle_ledger(args) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn tool(dir: &std::path::Path) -> LedgerTool {
        LedgerTool::new(
            dir,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
        )
    }

    fn text(result: CallToolResult) -> String {
        result.content[0].text.clone()
    }

    #[test]
    fn a_recorded_fact_is_listed_back() {
        let dir = tempdir().unwrap();
        let tool = tool(dir.path());

        let recorded = text(tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "Decisão: o corte de resposta é por seção, nunca por caractere.",
            "tags": ["response-contract"]
        })));
        assert!(recorded.contains("Registrado"), "{recorded}");

        let listed = text(tool.handle_ledger(serde_json::json!({ "action": "list" })));
        assert!(listed.contains("nunca por caractere"), "{listed}");
        assert!(listed.contains("response-contract"), "{listed}");
    }

    #[test]
    fn whitespace_variants_do_not_duplicate_an_entry() {
        let dir = tempdir().unwrap();
        let tool = tool(dir.path());

        tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "usar brain_peek para recuperar o excedente"
        }));
        let second = text(tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "  usar   brain_peek para recuperar o excedente  "
        })));
        assert!(second.starts_with("Já registrado"), "{second}");

        let listed = text(tool.handle_ledger(serde_json::json!({ "action": "list" })));
        assert_eq!(listed.matches("brain_peek").count(), 1, "{listed}");
    }

    #[test]
    fn search_filters_and_ranks_by_terms() {
        let dir = tempdir().unwrap();
        let tool = tool(dir.path());

        tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "O budget é medido em tokens estimados."
        }));
        tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "O grafo usa fan-in como centralidade."
        }));

        let found = text(tool.handle_ledger(serde_json::json!({
            "action": "search",
            "text": "budget tokens"
        })));
        assert!(found.contains("tokens estimados"), "{found}");
        assert!(!found.contains("fan-in"), "{found}");
    }

    #[test]
    fn an_empty_ledger_says_so() {
        let dir = tempdir().unwrap();
        let out = text(tool(dir.path()).handle_ledger(serde_json::json!({ "action": "list" })));
        assert_eq!(out, "Ledger vazio.");
    }

    #[test]
    fn a_corrupt_line_loses_only_itself() {
        let dir = tempdir().unwrap();
        let tool = tool(dir.path());
        tool.handle_ledger(serde_json::json!({
            "action": "record",
            "text": "entrada válida"
        }));

        let path = dir.path().join(".brain").join("ledger.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{ not json").unwrap();

        let listed = text(tool.handle_ledger(serde_json::json!({ "action": "list" })));
        assert!(listed.contains("entrada válida"), "{listed}");
    }

    #[test]
    fn record_without_text_is_an_error() {
        let dir = tempdir().unwrap();
        let out = tool(dir.path()).handle_ledger(serde_json::json!({ "action": "record" }));
        assert_eq!(out.is_error, Some(true));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let p = tool(dir.path());
        assert_eq!(p.name(), "brain_ledger");
        assert_eq!(p.schema().name, "brain_ledger");
    }
}
