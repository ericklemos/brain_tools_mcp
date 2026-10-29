use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use brain_kernel::text::compact_signature;
use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::model::ProjectCodeGraph;

/// Answers "como A chega em B?" with the shortest call path, instead of asking
/// the agent to run a depth-3 context graph and read the caller lists itself.
pub struct TraceTool {
    root_dir: PathBuf,
    graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct TraceParams {
    pub from: String,
    pub to: String,
    pub max_depth: Option<usize>,
    pub budget_tokens: Option<usize>,
}

impl TraceTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
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

    pub async fn handle_trace(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: TraceParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(error) => return CallToolResult::error(format!("Invalid arguments: {error}")),
        };

        let max_depth = params.max_depth.unwrap_or(6).clamp(1, 12);

        // The section owns its strings; drop the read guard before rendering so
        // a long trace never prevents the watcher from writing.
        let section = {
            let graph = self.graph.read().await;

            let starts = resolve(&graph, &params.from);
            let targets = resolve(&graph, &params.to);
            if starts.is_empty() {
                return CallToolResult::text(format!(
                    "Símbolo `{}` não encontrado no índice.",
                    params.from
                ));
            }
            if targets.is_empty() {
                return CallToolResult::text(format!(
                    "Símbolo `{}` não encontrado no índice.",
                    params.to
                ));
            }

            let target_set: HashSet<NodeIndex> = targets.into_iter().collect();
            match shortest_path(&graph, starts, &target_set, max_depth) {
                Some((path, previous)) => {
                    found_section(&graph, &self.root_dir, &params, &path, &previous)
                }
                None => missing_section(&graph, &params, max_depth),
            }
        };

        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&[section], budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }
}

fn resolve(graph: &ProjectCodeGraph, query: &str) -> Vec<NodeIndex> {
    if let Some(indices) = graph.symbol_lookup.get(query) {
        return indices.clone();
    }
    if let Some(&index) = graph.qualified_lookup.get(query) {
        return vec![index];
    }
    graph
        .file_nodes
        .iter()
        .filter(|(path, _)| path.to_string_lossy().contains(query))
        .flat_map(|(_, indices)| indices.iter().copied())
        .collect()
}

/// Breadth-first search over outgoing call edges. BFS guarantees the shortest
/// chain, which is the one worth spending tokens on.
#[allow(clippy::type_complexity)]
fn shortest_path(
    graph: &ProjectCodeGraph,
    starts: Vec<NodeIndex>,
    targets: &HashSet<NodeIndex>,
    max_depth: usize,
) -> Option<(Vec<NodeIndex>, HashMap<NodeIndex, (NodeIndex, usize)>)> {
    let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();
    let mut seen: HashSet<NodeIndex> = HashSet::new();
    let mut previous: HashMap<NodeIndex, (NodeIndex, usize)> = HashMap::new();

    for start in starts {
        if seen.insert(start) {
            queue.push_back((start, 0));
        }
    }

    while let Some((current, depth)) = queue.pop_front() {
        if targets.contains(&current) {
            let mut path = Vec::new();
            let mut cursor = current;
            loop {
                path.push(cursor);
                match previous.get(&cursor) {
                    Some(&(parent, _)) => cursor = parent,
                    None => break,
                }
            }
            path.reverse();
            return Some((path, previous));
        }

        if depth >= max_depth {
            continue;
        }

        for edge in graph.graph.edges_directed(current, Direction::Outgoing) {
            let next = edge.target();
            if seen.insert(next) {
                previous.insert(next, (current, edge.weight().call_site_line));
                queue.push_back((next, depth + 1));
            }
        }
    }

    None
}

fn found_section(
    graph: &ProjectCodeGraph,
    root_dir: &Path,
    params: &TraceParams,
    path: &[NodeIndex],
    previous: &HashMap<NodeIndex, (NodeIndex, usize)>,
) -> Section {
    let hops = path.len().saturating_sub(1);
    let heading = if hops == 0 {
        format!("### `{}` é o mesmo símbolo que `{}`", params.from, params.to)
    } else {
        let unit = if hops == 1 { "salto" } else { "saltos" };
        format!("### Caminho `{}` → `{}` — {hops} {unit}", params.from, params.to)
    };

    let mut body = String::new();
    for (position, &index) in path.iter().enumerate() {
        let Some(node) = graph.graph.node_weight(index) else {
            continue;
        };
        let relative_path = relative(&node.file_path, root_dir);
        let signature = compact_signature(&node.signature);
        let label = if signature.is_empty() {
            node.name.clone()
        } else {
            signature
        };

        if position == 0 {
            body.push_str(&format!("- `{relative_path}:{}` `{label}`\n", node.start_line));
        } else {
            let Some(&(parent, call_line)) = previous.get(&index) else {
                body.push_str(&format!("- `{relative_path}:{}` `{label}`\n", node.start_line));
                continue;
            };
            let parent_relative = graph
                .graph
                .node_weight(parent)
                .map(|parent| relative(&parent.file_path, root_dir))
                .unwrap_or_default();
            body.push_str(&format!(
                "- `{relative_path}:{}` `{label}` @`{parent_relative}:{call_line}`\n",
                node.start_line
            ));
        }
    }

    Section {
        heading,
        body,
        priority: 255,
    }
}

fn missing_section(graph: &ProjectCodeGraph, params: &TraceParams, max_depth: usize) -> Section {
    Section {
        heading: format!(
            "### Nenhum caminho de `{}` até `{}` em até {max_depth} saltos",
            params.from, params.to
        ),
        body: format!(
            "{}> Ausência de aresta não é ausência de chamada: o grafo só prova o que resolveu.\n",
            graph.resolution.disclosure()
        ),
        priority: 255,
    }
}

fn relative(path: &Path, root_dir: &Path) -> String {
    path.strip_prefix(root_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

impl ToolPlugin for TraceTool {
    fn name(&self) -> &'static str {
        "brain_trace"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_trace".to_string(),
            description: "Caminho de chamadas entre dois símbolos (A → … → B) em uma resposta curta. Substitui `brain_context_graph` com depth alto + leitura manual dos chamadores.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Símbolo de origem (nome ou caminho do arquivo)."
                    },
                    "to": {
                        "type": "string",
                        "description": "Símbolo de destino (nome ou caminho do arquivo)."
                    },
                    "max_depth": {
                        "type": "integer",
                        "default": 6,
                        "minimum": 1,
                        "maximum": 12,
                        "description": "Máximo de saltos procurados."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
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
        Box::pin(self.handle_trace(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, Arc<tokio::sync::RwLock<ProjectCodeGraph>>) {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "pub fn alpha() {\n    beta();\n}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "pub fn beta() {\n    gamma();\n}\n").unwrap();
        fs::write(dir.path().join("c.rs"), "pub fn gamma() {}\n").unwrap();
        fs::write(dir.path().join("d.rs"), "pub fn delta() {}\n").unwrap();

        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        (dir, Arc::new(tokio::sync::RwLock::new(graph)))
    }

    fn tool(dir: &tempfile::TempDir, graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>) -> TraceTool {
        TraceTool::new(
            dir.path(),
            graph,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        )
    }

    #[tokio::test]
    async fn a_call_chain_is_returned_as_a_short_path() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_trace(serde_json::json!({ "from": "alpha", "to": "gamma" }))
            .await;
        let text = &out.content[0].text;

        assert!(text.contains("2 saltos"), "{text}");
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("gamma"), "{text}");
        assert!(text.contains("@`"), "each hop cites the call site: {text}");
    }

    #[tokio::test]
    async fn no_path_is_categorical_and_discloses_the_graph_confidence() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_trace(serde_json::json!({ "from": "alpha", "to": "delta" }))
            .await;
        let text = &out.content[0].text;

        assert!(text.contains("Nenhum caminho"), "{text}");
        assert!(text.contains("Confiança do grafo"), "{text}");
        assert!(
            text.contains("não é ausência de chamada"),
            "an absent edge is not an absent call: {text}"
        );
    }

    #[tokio::test]
    async fn the_depth_limit_is_respected() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_trace(serde_json::json!({
                "from": "alpha",
                "to": "gamma",
                "max_depth": 1
            }))
            .await;
        assert!(out.content[0].text.contains("Nenhum caminho"), "{}", out.content[0].text);
    }

    #[tokio::test]
    async fn an_unknown_symbol_is_reported_not_silently_empty() {
        let (dir, graph) = fixture();
        let out = tool(&dir, graph)
            .handle_trace(serde_json::json!({ "from": "nope", "to": "gamma" }))
            .await;
        assert!(out.content[0].text.contains("não encontrado"));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(tokio::sync::RwLock::new(ProjectCodeGraph::new()));
        let tool = tool(&dir, graph);
        assert_eq!(tool.name(), "brain_trace");
        assert_eq!(tool.schema().name, "brain_trace");
    }
}
