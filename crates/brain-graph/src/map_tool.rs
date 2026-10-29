use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use brain_kernel::text::compact_signature;
use petgraph::Direction;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::model::{ProjectCodeGraph, SymbolNode};

/// At most this many symbols per file: a 60-function file would otherwise be
/// one section too large to ever fit the budget.
const MAX_SYMBOLS_PER_FILE: usize = 12;

pub struct MapTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MapDepth {
    #[default]
    Files,
    Dirs,
}

#[derive(Debug, Deserialize)]
pub struct MapParams {
    pub path: Option<String>,
    pub budget_tokens: Option<usize>,
    /// `files` (default) lists symbols per file; `dirs` answers "where am I"
    /// with one section per directory, which is much cheaper on a large repo.
    pub depth: Option<MapDepth>,
    /// Test-only symbols are hidden by default and counted in the heading.
    pub include_tests: Option<bool>,
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

        // Sections own their strings, so the read guard is released before the
        // budget pass and the render: the watcher's writer is never blocked by
        // formatting.
        let sections = {
            let lock = self.graph.read().await;
            self.sections(
                &lock,
                params.path.as_deref(),
                params.depth.unwrap_or_default(),
                params.include_tests.unwrap_or(false),
            )
        };
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

    /// One section per file (or per directory), ordered by how much the rest
    /// of the code depends on it. The agent reads the skeleton from the top
    /// until its budget ends.
    fn sections(
        &self,
        graph: &ProjectCodeGraph,
        path_filter: Option<&str>,
        depth: MapDepth,
        include_tests: bool,
    ) -> Vec<Section> {
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

        let hidden_tests: usize = files
            .iter()
            .map(|(_, nodes)| {
                nodes
                    .iter()
                    .filter(|&&idx| {
                        graph
                            .graph
                            .node_weight(idx)
                            .is_some_and(|node| node.is_test)
                    })
                    .count()
            })
            .sum();

        // A file with no visible (non-test) symbol is not orientation.
        files.retain(|(_, nodes)| {
            include_tests
                || nodes.iter().any(|&idx| {
                    graph
                        .graph
                        .node_weight(idx)
                        .is_some_and(|node| !node.is_test)
                })
        });

        if files.is_empty() {
            if hidden_tests > 0 && !include_tests {
                return vec![Section {
                    heading: "### Mapa do projeto — 0 símbolos de produção".to_string(),
                    body: format!(
                        "> {hidden_tests} símbolo(s) de teste oculto(s); include_tests: true para ver.\n"
                    ),
                    priority: 255,
                }];
            }
            return Vec::new();
        }

        let visible = |nodes: &Vec<petgraph::stable_graph::NodeIndex>| -> Vec<&SymbolNode> {
            nodes
                .iter()
                .filter_map(|&idx| graph.graph.node_weight(idx))
                .filter(|node| include_tests || !node.is_test)
                .collect()
        };

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

        let total_symbols: usize = files.iter().map(|(_, nodes)| visible(nodes).len()).sum();
        let mut sections = vec![Section {
            heading: format!(
                "### Mapa do projeto — {} arquivos, {total_symbols} símbolos (mais centrais primeiro)",
                files.len()
            ),
            body: graph.resolution.disclosure(),
            priority: 255,
        }];

        if depth == MapDepth::Dirs {
            struct DirRow {
                files: usize,
                symbols: usize,
                tests: usize,
                fan_in: usize,
                top: Vec<(String, usize, usize)>,
            }

            let mut dirs: std::collections::BTreeMap<String, DirRow> =
                std::collections::BTreeMap::new();
            for (relative, nodes) in files {
                let symbols = visible(nodes);
                let hidden = nodes.len().saturating_sub(symbols.len());
                let (dir, file_name) = match relative.rsplit_once('/') {
                    Some((dir, file)) => (dir.to_string(), file.to_string()),
                    None => (".".to_string(), relative.clone()),
                };
                let fan_in = score(nodes);
                let row = dirs.entry(dir).or_insert(DirRow {
                    files: 0,
                    symbols: 0,
                    tests: 0,
                    fan_in: 0,
                    top: Vec::new(),
                });
                row.files += 1;
                row.symbols += symbols.len();
                row.tests += hidden;
                row.fan_in += fan_in;
                row.top.push((file_name, symbols.len(), fan_in));
            }

            let mut rows: Vec<(String, DirRow)> = dirs.into_iter().collect();
            rows.sort_by(|(dir_a, a), (dir_b, b)| {
                b.fan_in.cmp(&a.fan_in).then_with(|| dir_a.cmp(dir_b))
            });

            for (dir, mut row) in rows {
                row.top
                    .sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
                let mut body = String::new();
                for (file, symbols, fan_in) in row.top.iter().take(5) {
                    body.push_str(&format!(
                        "- `{file}` — {symbols} símbolos, {fan_in} chamadas\n"
                    ));
                }
                if row.top.len() > 5 {
                    body.push_str(&format!("- … e mais {} arquivos\n", row.top.len() - 5));
                }
                let tests_note = if row.tests > 0 {
                    format!(" ({} teste(s))", row.tests)
                } else {
                    String::new()
                };
                sections.push(Section {
                    heading: format!(
                        "### `{dir}` — {} arquivos, {} símbolos{tests_note}, {} chamadas recebidas",
                        row.files, row.symbols, row.fan_in
                    ),
                    body,
                    priority: (150 + row.fan_in.min(100)) as u8,
                });
            }
            return sections;
        }

        for (relative, nodes) in files {
            let symbols = visible(nodes);
            let hidden = nodes.len().saturating_sub(symbols.len());

            let mut body = String::new();
            for node in symbols.iter().take(MAX_SYMBOLS_PER_FILE) {
                let name = node.name.as_str();
                let signature = compact_signature(&node.signature);
                if signature.is_empty() {
                    body.push_str(&format!("- `{name}`\n"));
                } else if signature.contains(name) {
                    // The signature already names the symbol (`pub fn foo(...)`),
                    // so prefixing it with the name pays twice for the same word.
                    body.push_str(&format!("- `{signature}`\n"));
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
            let tests_note = if hidden > 0 {
                format!(" ({hidden} teste(s))")
            } else {
                String::new()
            };
            sections.push(Section {
                heading: format!(
                    "### `{relative}` — {} símbolos{tests_note}, {fan_in} chamadas recebidas",
                    symbols.len()
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
            description: "Mapa do projeto por centralidade: arquivos/símbolos ou resumo por diretório.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Opcional: restringe o mapa a um prefixo de caminho (ex: 'src/auth')."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
                    },
                    "depth": {
                        "type": "string",
                        "enum": ["files", "dirs"],
                        "default": "files",
                        "description": "files: símbolos por arquivo; dirs: um resumo por diretório (mais barato para 'onde estou')."
                    },
                    "include_tests": {
                        "type": "boolean",
                        "default": false,
                        "description": "Inclui símbolos de teste; por padrão eles são contados no cabeçalho e omitidos."
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
        assert!(
            !text.contains("`core_fn` — `pub fn core_fn"),
            "the signature already names the symbol: {text}"
        );
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

    #[tokio::test]
    async fn a_directory_digest_groups_files_and_skips_signatures() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), "pub fn beta() {}\n").unwrap();
        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        let out = tool(&dir, Arc::new(RwLock::new(graph)))
            .handle_map(serde_json::json!({ "depth": "dirs" }))
            .await;
        let text = &out.content[0].text;

        assert!(text.contains("### `src` — 2 arquivos"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("pub fn alpha"), "a dir digest is not a symbol list: {text}");
    }

    #[tokio::test]
    async fn test_symbols_are_hidden_unless_requested() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn production() {}\n\n#[test]\nfn hidden_test() {}\n",
        )
        .unwrap();
        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        let tool = tool(&dir, Arc::new(RwLock::new(graph)));

        let default = tool.handle_map(serde_json::json!({})).await.content[0].text.clone();
        assert!(default.contains("production"), "{default}");
        assert!(!default.contains("hidden_test"), "{default}");
        assert!(default.contains("1 teste(s)"), "{default}");

        let all = tool
            .handle_map(serde_json::json!({ "include_tests": true }))
            .await
            .content[0]
            .text
            .clone();
        assert!(all.contains("hidden_test"), "{all}");
    }

}
