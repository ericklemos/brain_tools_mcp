use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use petgraph::stable_graph::NodeIndex;
use petgraph::Direction;
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::model::{ProjectCodeGraph, SymbolNode};

/// How far reverse dependencies are followed when looking for affected tests.
const TEST_SEARCH_DEPTH: usize = 3;

pub struct DiffTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Deserialize)]
pub struct DiffParams {
    /// Git revision to diff against. `HEAD` means "the working tree changes".
    pub base: Option<String>,
    pub path: Option<String>,
    /// Subset of `["impact", "tests"]`; default is both.
    pub include: Option<Vec<String>>,
    pub budget_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// Changed new-side line ranges, 1-based and inclusive.
    pub ranges: Vec<(usize, usize)>,
}

/// Reads `git diff --unified=0` output and keeps only file paths and changed
/// line ranges. The diff text is never shown to the agent; it is an index.
pub fn parse_unified_diff(diff: &str) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut current: Option<usize> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.trim();
            if path == "/dev/null" {
                current = None;
                continue;
            }
            let path = path.strip_prefix("b/").unwrap_or(path);
            files.push(ChangedFile {
                path: path.to_string(),
                ranges: Vec::new(),
            });
            current = Some(files.len() - 1);
            continue;
        }

        if line.starts_with("@@ ") {
            let Some(index) = current else { continue };
            let Some(plus) = line.split_whitespace().find(|part| part.starts_with('+')) else {
                continue;
            };
            let spec = plus.trim_start_matches('+');
            let mut parts = spec.split(',');
            let start: usize = match parts.next().and_then(|n| n.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            let count: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(1);
            // A pure deletion has count 0: attribute it to the line it touched
            // so it still lands inside the enclosing symbol.
            let end = start + count.saturating_sub(1).max(if count == 0 { 1 } else { 0 });
            files[index].ranges.push((start, end));
        }
    }

    files.retain(|file| !file.ranges.is_empty());
    files
}

#[derive(Debug, Clone, Copy)]
struct Include {
    impact: bool,
    tests: bool,
}

impl Include {
    fn from_param(value: Option<&Vec<String>>) -> Self {
        match value {
            None => Self {
                impact: true,
                tests: true,
            },
            Some(values) => Self {
                impact: values.iter().any(|v| v == "impact"),
                tests: values.iter().any(|v| v == "tests"),
            },
        }
    }
}

impl DiffTool {
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

    pub async fn handle_diff(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: DiffParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let base = params.base.as_deref().unwrap_or("HEAD");
        let output = match Command::new("git")
            .current_dir(&self.root_dir)
            .args(["diff", base, "--unified=0", "--no-color"])
            .output()
        {
            Ok(output) => output,
            Err(e) => {
                return CallToolResult::error(format!("Não foi possível executar git diff: {e}"))
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return CallToolResult::error(format!(
                "git diff {base} falhou: {}",
                stderr.trim()
            ));
        }

        let diff = String::from_utf8_lossy(&output.stdout);
        let mut changed = parse_unified_diff(&diff);
        if let Some(filter) = params.path.as_deref() {
            let filter = filter.trim_start_matches("./");
            changed.retain(|file| file.path.starts_with(filter));
        }

        if changed.is_empty() {
            return CallToolResult::text(format!(
                "Nenhuma alteração em relação a `{base}`."
            ));
        }

        let lock = self.graph.read().await;
        let include = Include::from_param(params.include.as_ref());
        let sections = self.sections(&lock, &changed, include, base);
        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }

    fn relative(&self, node: &SymbolNode) -> String {
        node.file_path
            .strip_prefix(&self.root_dir)
            .unwrap_or(&node.file_path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// The graph stores absolute paths; the diff stores repo-relative ones.
    fn nodes_for(&self, graph: &ProjectCodeGraph, changed: &ChangedFile) -> Vec<NodeIndex> {
        graph
            .file_nodes
            .iter()
            .filter(|(path, _)| {
                self.relative_path(path) == changed.path
                    || self.relative_path(path).ends_with(&format!("/{}", changed.path))
            })
            .flat_map(|(_, nodes)| nodes.iter().copied())
            .collect()
    }

    fn relative_path(&self, path: &std::path::Path) -> String {
        path.strip_prefix(&self.root_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn sections(
        &self,
        graph: &ProjectCodeGraph,
        changed: &[ChangedFile],
        include: Include,
        base: &str,
    ) -> Vec<Section> {
        let mut changed_symbols: Vec<NodeIndex> = Vec::new();
        let mut changed_files_hit: HashSet<String> = HashSet::new();
        let mut missing_files: Vec<&str> = Vec::new();

        for file in changed {
            let nodes = self.nodes_for(graph, file);
            if nodes.is_empty() {
                missing_files.push(&file.path);
                continue;
            }
            changed_files_hit.insert(file.path.clone());
            for node_idx in nodes {
                let Some(node) = graph.graph.node_weight(node_idx) else {
                    continue;
                };
                let touched = file
                    .ranges
                    .iter()
                    .any(|&(start, end)| start <= node.end_line && end >= node.start_line);
                if touched && !changed_symbols.contains(&node_idx) {
                    changed_symbols.push(node_idx);
                }
            }
        }

        let mut sections = vec![Section {
            heading: format!(
                "### Alterações em relação a `{base}` — {} arquivos, {} símbolos",
                changed.len(),
                changed_symbols.len()
            ),
            body: String::new(),
            priority: 255,
        }];

        if !changed_symbols.is_empty() {
            let mut body = String::new();
            for &idx in &changed_symbols {
                if let Some(node) = graph.graph.node_weight(idx) {
                    body.push_str(&format!(
                        "- `{}` (`{}:{}-{}`)\n",
                        node.name,
                        self.relative(node),
                        node.start_line,
                        node.end_line
                    ));
                }
            }
            sections.push(Section {
                heading: "### Símbolos alterados".to_string(),
                body,
                priority: 255,
            });
        }

        if !missing_files.is_empty() {
            sections.push(Section {
                heading: "### Arquivos alterados sem símbolos indexados".to_string(),
                body: missing_files
                    .iter()
                    .map(|path| format!("- `{path}`\n"))
                    .collect(),
                priority: 100,
            });
        }

        if include.impact {
            let impact = self.external_impact(graph, &changed_symbols, &changed_files_hit);
            let body = if impact.is_empty() {
                "Nenhuma chamada externa. A mudança está contida nos arquivos alterados.\n"
                    .to_string()
            } else {
                impact
                    .iter()
                    .map(|(path, line, symbol)| format!("- `{path}:{line}` chama `{symbol}`\n"))
                    .collect()
            };
            sections.push(Section {
                heading: format!("### Impacto externo — {} chamadas", impact.len()),
                body,
                priority: 254,
            });
        }

        if include.tests {
            let tests = self.affected_tests(graph, &changed_symbols);
            let body = if tests.is_empty() {
                "Nenhum teste indexado depende destes símbolos.\n".to_string()
            } else {
                tests
                    .iter()
                    .map(|node| {
                        format!(
                            "- `{}` (`{}:{}-{}`)\n",
                            node.name,
                            self.relative(node),
                            node.start_line,
                            node.end_line
                        )
                    })
                    .collect()
            };
            sections.push(Section {
                heading: format!("### Testes afetados — {}", tests.len()),
                body,
                priority: 200,
            });
        }

        sections
    }

    fn external_impact(
        &self,
        graph: &ProjectCodeGraph,
        changed_symbols: &[NodeIndex],
        changed_files: &HashSet<String>,
    ) -> Vec<(String, usize, String)> {
        let mut sites: BTreeSet<(String, usize, String)> = BTreeSet::new();

        for &target in changed_symbols {
            let Some(target_node) = graph.graph.node_weight(target) else {
                continue;
            };
            for caller in graph.graph.neighbors_directed(target, Direction::Incoming) {
                let Some(caller_node) = graph.graph.node_weight(caller) else {
                    continue;
                };
                let caller_file = self.relative(caller_node);
                if changed_files.contains(&caller_file) {
                    continue;
                }
                let line = graph
                    .graph
                    .find_edge(caller, target)
                    .and_then(|edge| graph.graph.edge_weight(edge))
                    .map(|edge| edge.call_site_line)
                    .unwrap_or(caller_node.start_line);
                sites.insert((caller_file, line, target_node.name.clone()));
            }
        }

        sites.into_iter().collect()
    }

    fn affected_tests<'a>(
        &self,
        graph: &'a ProjectCodeGraph,
        changed_symbols: &[NodeIndex],
    ) -> Vec<&'a SymbolNode> {
        let mut seen: HashSet<NodeIndex> = changed_symbols.iter().copied().collect();
        let mut queue: VecDeque<(NodeIndex, usize)> =
            changed_symbols.iter().map(|&idx| (idx, 0)).collect();
        let mut tests: BTreeSet<(String, usize)> = BTreeSet::new();
        let mut result: Vec<&SymbolNode> = Vec::new();

        while let Some((idx, depth)) = queue.pop_front() {
            if depth > 0 {
                if let Some(node) = graph.graph.node_weight(idx) {
                    if is_test(node) {
                        let key = (self.relative(node), node.start_line);
                        if tests.insert(key) {
                            result.push(node);
                        }
                    }
                }
            }
            if depth == TEST_SEARCH_DEPTH {
                continue;
            }
            for caller in graph.graph.neighbors_directed(idx, Direction::Incoming) {
                if seen.insert(caller) {
                    queue.push_back((caller, depth + 1));
                }
            }
        }

        result
    }
}

pub(crate) fn is_test(node: &SymbolNode) -> bool {
    let path = node.file_path.to_string_lossy().replace('\\', "/");
    let in_test_tree = path.contains("/tests/") || path.ends_with("/tests.rs");
    let test_file = path.ends_with("_test.rs")
        || path.ends_with("_tests.rs")
        || path.ends_with(".test.kt");
    in_test_tree || test_file || node.name.starts_with("test_") || node.name == "test"
}

impl ToolPlugin for DiffTool {
    fn name(&self) -> &'static str {
        "brain_diff"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_diff".to_string(),
            description: "Analisa o diff atual: quais símbolos mudaram, quem fora dos arquivos alterados chama esses símbolos e quais testes dependem deles. Responde 'o que quebrei?' sem ler o diff inteiro.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "string",
                        "default": "HEAD",
                        "description": "Revisão base do git (ex: `HEAD`, `main`). O padrão compara com a árvore de trabalho."
                    },
                    "path": {
                        "type": "string",
                        "description": "Opcional: restringe a um prefixo de caminho."
                    },
                    "include": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["impact", "tests"] },
                        "description": "Seções desejadas; padrão é ambas. Peça só `impact` quando o custo do teste não importa."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto de tokens (excedente em brain_peek)."
                    }
                }
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_diff(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::fs;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    const DIFF: &str = "\
diff --git a/core.rs b/core.rs
index 1111111..2222222 100644
--- a/core.rs
+++ b/core.rs
@@ -1 +1 @@
-old
+new
@@ -10,0 +11,1 @@
+extra
";

    fn fixture() -> (TempDir, Arc<RwLock<ProjectCodeGraph>>) {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("core.rs"),
            "pub fn core_fn() {}\n\npub fn untouched() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("feature.rs"),
            "pub fn feature() {\n    core_fn();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("core_test.rs"),
            "#[test]\nfn test_core() {\n    core_fn();\n}\n",
        )
        .unwrap();

        let graph = GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
        (dir, Arc::new(RwLock::new(graph)))
    }

    fn tool(dir: &TempDir, graph: Arc<RwLock<ProjectCodeGraph>>) -> DiffTool {
        DiffTool::new(
            dir.path(),
            graph,
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        )
    }

    fn report(tool: &DiffTool, graph: &ProjectCodeGraph, include: Include) -> String {
        let changed = parse_unified_diff(DIFF);
        budget::render(
            &budget::select(&tool.sections(graph, &changed, include, "HEAD"), 4000),
            None,
        )
    }

    #[test]
    fn the_parser_keeps_paths_and_changed_ranges_only() {
        let changed = parse_unified_diff(DIFF);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "core.rs");
        assert_eq!(changed[0].ranges, vec![(1, 1), (11, 11)]);
        assert!(!format!("{changed:?}").contains("+new"), "diff text must not survive");
    }

    #[test]
    fn changed_symbols_are_resolved_from_lines() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph.clone());
        let graph = graph.blocking_read();
        let text = report(
            &tool,
            &graph,
            Include {
                impact: true,
                tests: true,
            },
        );
        assert!(text.contains("`core_fn`"), "{text}");
        assert!(!text.contains("`untouched`"), "line 11 is past untouched: {text}");
    }

    #[test]
    fn external_callers_and_dependent_tests_are_listed() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph.clone());
        let graph = graph.blocking_read();
        let text = report(
            &tool,
            &graph,
            Include {
                impact: true,
                tests: true,
            },
        );
        assert!(text.contains("feature.rs:2"), "external caller missing: {text}");
        assert!(text.contains("test_core"), "dependent test missing: {text}");
    }

    #[test]
    fn include_selects_the_sections() {
        let (dir, graph) = fixture();
        let tool = tool(&dir, graph.clone());
        let graph = graph.blocking_read();
        let text = report(
            &tool,
            &graph,
            Include {
                impact: true,
                tests: false,
            },
        );
        assert!(text.contains("Impacto externo"));
        assert!(!text.contains("Testes afetados"), "{text}");
    }

    #[tokio::test]
    async fn no_changes_is_a_categorical_answer() {
        let (dir, graph) = fixture();
        // A plain temp dir is not a git repo, so this must surface the failure
        // rather than pretend there is nothing to report.
        let out = tool(&dir, graph)
            .handle_diff(serde_json::json!({}))
            .await;
        assert_eq!(out.is_error, Some(true), "{}", out.content[0].text);
        assert!(out.content[0].text.contains("git"));
    }

    #[test]
    fn plugin_announces_its_name_and_schema() {
        let dir = tempdir().unwrap();
        let graph = Arc::new(RwLock::new(ProjectCodeGraph::new()));
        let p = tool(&dir, graph);
        assert_eq!(p.name(), "brain_diff");
        assert_eq!(p.schema().name, "brain_diff");
    }
}
