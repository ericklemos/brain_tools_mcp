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

use crate::model::{ProjectCodeGraph, SymbolNode};

/// Answers a yes/no question about the index with one categorical word.
///
/// The point is to end the "is this used anywhere?" read-and-grep loop: the
/// agent gets SIM or NÃO plus the evidence, not another file to interpret.
pub struct AssertTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct AssertParams {
    pub kind: AssertionKind,
    pub symbol: String,
    /// For `used_outside`: the module or file prefix that counts as "inside".
    pub scope: Option<String>,
    pub budget_tokens: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssertionKind {
    Exists,
    Public,
    UsedOutside,
}

impl AssertTool {
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

    pub async fn handle_assert(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: AssertParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let answer = match params.kind {
            AssertionKind::Exists => self.answer_exists(&lock, &params.symbol),
            AssertionKind::Public => self.answer_public(&lock, &params.symbol),
            AssertionKind::UsedOutside => {
                self.answer_used_outside(&lock, &params.symbol, params.scope.as_deref())
            }
        };

        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let section = Section {
            heading: answer,
            body: String::new(),
            priority: 255,
        };
        let selection = budget::select(&[section], budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }

    fn nodes<'a>(&self, graph: &'a ProjectCodeGraph, symbol: &str) -> Vec<&'a SymbolNode> {
        graph
            .symbol_lookup
            .get(symbol)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(|&idx| graph.graph.node_weight(idx))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn relative(&self, node: &SymbolNode) -> String {
        node.file_path
            .strip_prefix(&self.root_dir)
            .unwrap_or(&node.file_path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn answer_exists(&self, graph: &ProjectCodeGraph, symbol: &str) -> String {
        let nodes = self.nodes(graph, symbol);
        if nodes.is_empty() {
            return format!("NÃO — `{symbol}` não possui nenhuma definição no índice.");
        }

        let places: Vec<String> = nodes
            .iter()
            .map(|node| format!("`{}:{}`", self.relative(node), node.start_line))
            .collect();
        format!(
            "SIM — `{symbol}` existe em {} ({} definição{}).",
            places.join(", "),
            nodes.len(),
            if nodes.len() == 1 { "" } else { "ões" }
        )
    }

    fn answer_public(&self, graph: &ProjectCodeGraph, symbol: &str) -> String {
        let nodes = self.nodes(graph, symbol);
        if nodes.is_empty() {
            return format!("NÃO — `{symbol}` não possui nenhuma definição no índice.");
        }

        let public: Vec<&SymbolNode> = nodes.iter().copied().filter(|n| is_public(n)).collect();
        if public.is_empty() {
            let node = nodes[0];
            return format!(
                "NÃO — `{symbol}` não é pública (`{}:{}`, assinatura: `{}`).",
                self.relative(node),
                node.start_line,
                node.signature
            );
        }

        let places: Vec<String> = public
            .iter()
            .map(|node| format!("`{}:{}`", self.relative(node), node.start_line))
            .collect();
        format!(
            "SIM — `{symbol}` é pública em {} ({}/{} definições públicas).",
            places.join(", "),
            public.len(),
            nodes.len()
        )
    }

    fn answer_used_outside(
        &self,
        graph: &ProjectCodeGraph,
        symbol: &str,
        scope: Option<&str>,
    ) -> String {
        let nodes = self.nodes(graph, symbol);
        if nodes.is_empty() {
            return format!("NÃO — `{symbol}` não possui nenhuma definição no índice.");
        }

        let scope = scope.map(|s| s.trim_start_matches("./"));
        let mut call_sites = Vec::new();

        for node in &nodes {
            let defined_in = self.relative(node);
            let definition_idx = graph
                .qualified_lookup
                .get(&node.qualified_name)
                .copied()
                .or_else(|| {
                    graph
                        .file_nodes
                        .get(&node.file_path)
                        .and_then(|idxs| idxs.iter().copied().find(|&idx| {
                            graph
                                .graph
                                .node_weight(idx)
                                .is_some_and(|n| n.qualified_name == node.qualified_name)
                        }))
                });

            let Some(idx) = definition_idx else { continue };
            for caller in graph.graph.neighbors_directed(idx, Direction::Incoming) {
                let Some(caller_node) = graph.graph.node_weight(caller) else {
                    continue;
                };
                let caller_file = self.relative(caller_node);
                let inside_definition_file = caller_file == defined_in;
                let inside_scope = scope.is_some_and(|s| caller_file.starts_with(s));
                if inside_definition_file || inside_scope {
                    continue;
                }

                let line = graph
                    .graph
                    .find_edge(caller, idx)
                    .and_then(|edge| graph.graph.edge_weight(edge))
                    .map(|edge| edge.call_site_line)
                    .unwrap_or(caller_node.start_line);
                call_sites.push(format!("`{caller_file}:{line}`"));
            }
        }

        call_sites.sort();
        call_sites.dedup();

        if call_sites.is_empty() {
            let boundary = scope
                .map(|s| format!(" de fora de `{s}`"))
                .unwrap_or_else(|| " de outro arquivo".to_string());
            // A "SIM" is backed by an edge; a "NÃO" is only as strong as the
            // graph's coverage, and the graph drops calls it cannot place. The
            // difference matters most exactly here, where the answer is used to
            // decide a symbol is dead.
            let caveat = match graph.resolution.disclosure() {
                d if d.is_empty() => String::new(),
                d => format!("\n{d}"),
            };
            return format!("NÃO — `{symbol}` não é chamada{boundary}.{caveat}");
        }

        format!(
            "SIM — `{symbol}` é chamada de fora: {} ({} local{}).",
            call_sites.join(", "),
            call_sites.len(),
            if call_sites.len() == 1 { "" } else { "is" }
        )
    }
}

fn is_public(node: &SymbolNode) -> bool {
    let signature = node.signature.trim_start();
    let is_kotlin = node
        .file_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext == "kt");

    if is_kotlin {
        return !signature.starts_with("private")
            && !signature.starts_with("internal")
            && !signature.starts_with("protected");
    }

    if !signature.starts_with("pub") {
        return false;
    }
    // `pub(crate)`, `pub(super)` and `pub(in ...)` are visible outside the
    // defining file but not outside the crate; the honest answer is "não" for
    // a question about a public API.
    !(signature.starts_with("pub(") || signature.starts_with("pub ("))
}

impl ToolPlugin for AssertTool {
    fn name(&self) -> &'static str {
        "brain_assert"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_assert".to_string(),
            description: "Responde SIM/NÃO sobre o índice: o símbolo existe, é público, ou é usado fora do seu arquivo/módulo. Evita greps de confirmação.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["exists", "public", "used_outside"],
                        "description": "A pergunta: `exists`, `public` (visibilidade fora do arquivo) ou `used_outside` (há chamadas de outro arquivo/módulo)."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Nome exato do símbolo."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Para `used_outside`: prefixo de caminho considerado 'dentro' (ex: 'src/auth')."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens; a resposta é uma linha categórica."
                    }
                },
                "required": ["kind", "symbol"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_assert(args))
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
            "pub fn core_fn() {}\n\nfn secret_fn() {}\n\npub(crate) fn crate_fn() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("feature_a.rs"),
            "pub fn feature() {\n    core_fn();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("feature_b.rs"),
            "pub fn other() {\n    core_fn();\n}\n",
        )
        .unwrap();

        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        (dir, Arc::new(RwLock::new(graph)))
    }

    fn tool(dir: &TempDir, graph: Arc<RwLock<ProjectCodeGraph>>) -> AssertTool {
        AssertTool::new(
            dir.path(),
            graph,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            2000,
        )
    }

    async fn ask(tool: &AssertTool, args: serde_json::Value) -> String {
        tool.handle_assert(args).await.content[0].text.clone()
    }

    #[tokio::test]
    async fn an_existing_symbol_is_yes() {
        let (dir, graph) = fixture();
        let text = ask(&tool(&dir, graph), serde_json::json!({ "kind": "exists", "symbol": "core_fn" })).await;
        assert!(text.starts_with("SIM"), "{text}");
        assert!(text.contains("core.rs:1"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_symbol_is_a_categorical_no() {
        let (dir, graph) = fixture();
        let text = ask(&tool(&dir, graph), serde_json::json!({ "kind": "exists", "symbol": "ghost" })).await;
        assert!(text.starts_with("NÃO"), "{text}");
    }

    #[tokio::test]
    async fn visibility_is_answered_from_the_signature() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph);

        let public = ask(&tool, serde_json::json!({ "kind": "public", "symbol": "core_fn" })).await;
        assert!(public.starts_with("SIM"), "{public}");

        let private = ask(&tool, serde_json::json!({ "kind": "public", "symbol": "secret_fn" })).await;
        assert!(private.starts_with("NÃO"), "{private}");

        let crate_only = ask(&tool, serde_json::json!({ "kind": "public", "symbol": "crate_fn" })).await;
        assert!(crate_only.starts_with("NÃO"), "pub(crate) is not public API: {crate_only}");
    }

    #[tokio::test]
    async fn use_from_another_file_is_yes_and_scope_can_narrow_it() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph);

        let anywhere = ask(&tool, serde_json::json!({ "kind": "used_outside", "symbol": "core_fn" })).await;
        assert!(anywhere.starts_with("SIM"), "{anywhere}");
        assert!(anywhere.contains("feature_a.rs"), "{anywhere}");

        // Both callers live under feature_, so none is outside that scope.
        let scoped = ask(
            &tool,
            serde_json::json!({ "kind": "used_outside", "symbol": "core_fn", "scope": "feature_" }),
        )
        .await;
        assert!(scoped.starts_with("NÃO"), "{scoped}");
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(RwLock::new(ProjectCodeGraph::new()));
        let p = tool(&dir, graph);
        assert_eq!(p.name(), "brain_assert");
        assert_eq!(p.schema().name, "brain_assert");
    }
}
