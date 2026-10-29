use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::model::ProjectCodeGraph;
use crate::schema::{brain_context_graph_tool, brain_impact_check_tool};
use crate::traversal::GraphTraversal;

pub struct GraphHandler {
    root_dir: PathBuf,
    graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
    store: Option<Arc<ResponseStore>>,
    default_budget_tokens: usize,
}

impl GraphHandler {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            graph,
            store: None,
            default_budget_tokens: 2000,
        }
    }

    /// Opts into the response contract: truncation with a recoverable handle.
    ///
    /// Without it the handler renders everything, preserving the behaviour
    /// existing callers and integration tests rely on.
    pub fn with_budget(mut self, store: Arc<ResponseStore>, default_budget_tokens: usize) -> Self {
        self.store = Some(store);
        self.default_budget_tokens = default_budget_tokens;
        self
    }

    pub async fn handle_context_graph(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            target_symbol_or_file: String,
            depth: Option<usize>,
            budget_tokens: Option<usize>,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let depth = params.depth.unwrap_or(1);
        let lock = self.graph.read().await;
        let sections = GraphTraversal::context_sections(
            &lock,
            &params.target_symbol_or_file,
            depth,
            &self.root_dir,
        );
        if sections.is_empty() {
            return CallToolResult::text(format!(
                "Nenhum símbolo ou arquivo correspondente encontrado para: `{}`.",
                params.target_symbol_or_file
            ));
        }
        CallToolResult::text(self.render(sections, params.budget_tokens))
    }

    pub async fn handle_impact_check(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            symbol_name: String,
            budget_tokens: Option<usize>,
            include_tests: Option<bool>,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let sections = GraphTraversal::impact_sections_with_options(
            &lock,
            &params.symbol_name,
            &self.root_dir,
            params.include_tests.unwrap_or(false),
        );
        if sections.is_empty() {
            return CallToolResult::text(format!(
                "Símbolo `{}` não encontrado no índice do projeto.",
                params.symbol_name
            ));
        }
        CallToolResult::text(self.render(sections, params.budget_tokens))
    }

    /// Fits sections into the budget, storing the omitted body when the handler
    /// has opted into the contract.
    fn render(&self, sections: Vec<Section>, budget_tokens: Option<usize>) -> String {
        let store = match &self.store {
            Some(store) => store,
            None => {
                return sections
                    .iter()
                    .map(|s| format!("{}\n{}\n", s.heading, s.body))
                    .collect();
            }
        };

        let budget = budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(store.insert(selection.omitted_body()))
        };
        budget::render(&selection, handle.as_deref())
    }
}

/// Exposes `handle_context_graph` as the `brain_context_graph` plugin.
///
/// A wrapper rather than an impl on `GraphHandler` itself: one handler backs
/// two distinct tools, and `ToolPlugin::name` is stateless.
pub struct GraphContextTool {
    handler: Arc<GraphHandler>,
}

impl GraphContextTool {
    pub fn new(handler: Arc<GraphHandler>) -> Self {
        Self { handler }
    }
}

impl ToolPlugin for GraphContextTool {
    fn name(&self) -> &'static str {
        "brain_context_graph"
    }

    fn schema(&self) -> Tool {
        brain_context_graph_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handler.handle_context_graph(args))
    }
}

/// Exposes `handle_impact_check` as the `brain_impact_check` plugin.
pub struct GraphImpactTool {
    handler: Arc<GraphHandler>,
}

impl GraphImpactTool {
    pub fn new(handler: Arc<GraphHandler>) -> Self {
        Self { handler }
    }
}

impl ToolPlugin for GraphImpactTool {
    fn name(&self) -> &'static str {
        "brain_impact_check"
    }

    fn schema(&self) -> Tool {
        brain_impact_check_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handler.handle_impact_check(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_kernel::plugin::ToolPlugin;

    fn handler() -> GraphHandler {
        GraphHandler::new(
            ".",
            Arc::new(tokio::sync::RwLock::new(ProjectCodeGraph::new())),
        )
    }

    #[test]
    fn context_graph_plugin_announces_its_name_and_schema() {
        let p = GraphContextTool::new(Arc::new(handler()));
        assert_eq!(p.name(), "brain_context_graph");
        assert_eq!(p.schema().name, "brain_context_graph");
    }

    #[test]
    fn impact_check_plugin_announces_its_name_and_schema() {
        let p = GraphImpactTool::new(Arc::new(handler()));
        assert_eq!(p.name(), "brain_impact_check");
        assert_eq!(p.schema().name, "brain_impact_check");
    }
}
