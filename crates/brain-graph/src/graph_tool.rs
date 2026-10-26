use brain_kernel::protocol::CallToolResult;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::model::ProjectCodeGraph;
use crate::traversal::GraphTraversal;

pub struct GraphHandler {
    root_dir: PathBuf,
    graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
}

impl GraphHandler {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: Arc<tokio::sync::RwLock<ProjectCodeGraph>>,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            graph,
        }
    }

    pub async fn handle_context_graph(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            target_symbol_or_file: String,
            depth: Option<usize>,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let depth = params.depth.unwrap_or(1);
        let lock = self.graph.read().await;
        let markdown = GraphTraversal::context_graph(
            &lock,
            &params.target_symbol_or_file,
            depth,
            &self.root_dir,
        );
        CallToolResult::text(markdown)
    }

    pub async fn handle_impact_check(&self, arguments: serde_json::Value) -> CallToolResult {
        #[derive(Deserialize)]
        struct Params {
            symbol_name: String,
        }

        let params: Params = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(format!("Invalid arguments: {e}")),
        };

        let lock = self.graph.read().await;
        let markdown = GraphTraversal::impact_check(&lock, &params.symbol_name, &self.root_dir);
        CallToolResult::text(markdown)
    }
}
