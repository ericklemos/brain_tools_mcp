use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use petgraph::Direction;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::model::ProjectCodeGraph;

/// At most this many symbols per file: a 60-function file would otherwise be
/// one section too large to ever fit the budget.
const MAX_SYMBOLS_PER_FILE: usize = 12;

pub struct MapTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct MapParams {
    pub path: Option<String>,
    pub budget_tokens: Option<usize>,
}

impl MapTool {
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

    pub async fn handle_map(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: MapParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let sections = self.sections(&lock, params.path.as_deref());
        if sections.is_empty() {
            let subject = params
                .path
                .as_deref()
                .map(|p| format!(" sob `{p}`"))
                .unwrap_or_default();
            return CallToolResult::text(format!(
                "Nenhum arquivo Rust/Kotlin indexado{subject}."
            ));
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

    /// One section per file, ordered by how much the rest of the code depends
    /// on it. The agent reads the skeleton from the top until its budget ends.
    fn sections(&self, graph: &ProjectCodeGraph, path_filter: Option<&str>) -> Vec<Section> {
        let filter = path_filter.map(|p| p.trim_start_matches("./"));

        let mut files: Vec<(String, &Vec<petgraph::stable_graph::NodeIndex>)> = graph
            .file_nodes
            .iter()
            .filter_map(|(path, nodes)| {
                let relative = path
                    .strip_prefix(&self.root_dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                match filter {
                    Some(f) if !relative.starts_with(f) => None,
                    _ => Some((relative, nodes)),
                }
            })
            .collect();

        if files.is_empty() {
            return Vec::new();
        }

        // Fan-in is the cheapest useful centrality: a file everyone calls is the
        // file the agent should know first. Ties fall back to path for
        // determinism, since HashMap iteration order is not stable.
        let score = |nodes: &[petgraph::stable_graph::NodeIndex]| -> usize {
            nodes
                .iter()
                .map(|&idx| graph.graph.edges_directed(idx, Direction::Incoming).count())
                .sum()
        };
        files.sort_by(|(path_a, nodes_a), (path_b, nodes_b)| {
            score(nodes_b)
                .cmp(&score(nodes_a))
                .then_with(|| path_a.cmp(path_b))
        });

        let total_symbols: usize = files.iter().map(|(_, nodes)| nodes.len()).sum();
        let mut sections = vec![Section {
            heading: format!(
                "### Mapa do projeto — {} arquivos, {total_symbols} símbolos (mais centrais primeiro)",
                files.len()
            ),
            body: graph.resolution.disclosure(),
            priority: 255,
        }];

        for (relative, nodes) in files {
            let mut symbols: Vec<(&str, &str)> = nodes
                .iter()
                .filter_map(|&idx| graph.graph.node_weight(idx))
                .map(|node| (node.name.as_str(), node.signature.as_str()))
                .collect();
            symbols.sort_by(|a, b| a.0.cmp(b.0));

            let mut body = String::new();
            for (name, signature) in symbols.iter().take(MAX_SYMBOLS_PER_FILE) {
                if signature.is_empty() {
                    body.push_str(&format!("- `{name}`\n"));
                } else {
                    body.push_str(&format!("- `{name}` — `{signature}`\n"));
                }
            }
            if symbols.len() > MAX_SYMBOLS_PER_FILE {
                body.push_str(&format!(
                    "- … e mais {} símbolos\n",
                    symbols.len() - MAX_SYMBOLS_PER_FILE
                ));
            }

            let fan_in = score(nodes);
            sections.push(Section {
                heading: format!(
                    "### `{relative}` — {} símbolos, {fan_in} chamadas recebidas",
                    nodes.len()
                ),
                body,
                // Centrality decides survival; 254 is reserved for the header.
                priority: (150 + fan_in.min(100)) as u8,
            });
        }

        sections
    }
}

impl ToolPlugin for MapTool {
    fn name(&self) -> &'static str {
        "brain_map"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_map".to_string(),
            description: "Esqueleto do projeto: arquivos e símbolos com assinaturas, ordenados pela centralidade (quantas chamadas recebem). Use para se orientar sem abrir arquivos com Read.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Opcional: restringe o mapa a um prefixo de caminho (ex: 'src/auth')."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto aproximado de tokens; o excedente fica recuperável via brain_peek."
                    }
                }
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_map(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::fs;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    fn fixture() -> (TempDir, Arc<RwLock<ProjectCodeGraph>>) {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("core.rs"),
            "pub fn core_fn() {}\n\npub fn helper() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("feature_a.rs"),
            "pub fn alpha() {\n    core_fn();\n    core_fn();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("feature_b.rs"),
            "pub fn beta() {\n    core_fn();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn main() {\n    alpha();\n    beta();\n}\n",
        )
        .unwrap();

        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        (dir, Arc::new(RwLock::new(graph)))
    }

    fn tool(dir: &TempDir, graph: Arc<RwLock<ProjectCodeGraph>>) -> MapTool {
        MapTool::new(
            dir.path(),
            graph,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        )
    }

    #[tokio::test]
    async fn the_most_called_file_comes_first() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_map(serde_json::json!({}))
            .await;
        let text = &out.content[0].text;

        let core = text.find("core.rs").expect("core must be listed");
        let alpha = text.find("feature_a.rs").expect("alpha must be listed");
        assert!(core < alpha, "core.rs should lead the map: {text}");
        assert!(text.contains("pub fn core_fn"), "signatures are the point: {text}");
        assert!(text.contains("0 chamadas recebidas") || text.contains("chamadas recebidas"));
    }

    #[tokio::test]
    async fn a_path_prefix_restricts_the_map() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_map(serde_json::json!({ "path": "feature_" }))
            .await;
        let text = &out.content[0].text;

        assert!(text.contains("feature_a.rs"));
        assert!(!text.contains("core.rs"), "filter must exclude other files: {text}");
    }

    #[tokio::test]
    async fn a_small_budget_truncates_but_leaves_a_handle() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph);
        let out = tool
            .handle_map(serde_json::json!({ "budget_tokens": 30 }))
            .await;
        let text = &out.content[0].text;

        assert!(text.contains("Resposta truncada"), "expected truncation: {text}");
        assert!(text.contains("brain_peek"));
    }

    #[tokio::test]
    async fn an_empty_graph_is_a_categorical_answer() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(RwLock::new(ProjectCodeGraph::new()));
        let out = tool(&dir, graph)
            .handle_map(serde_json::json!({}))
            .await;
        assert!(out.content[0].text.contains("Nenhum arquivo"));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(RwLock::new(ProjectCodeGraph::new()));
        let p = tool(&dir, graph);
        assert_eq!(p.name(), "brain_map");
        assert_eq!(p.schema().name, "brain_map");
    }
}
