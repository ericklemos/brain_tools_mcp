//! Bridges the code graph's file hashes into `brain_exec`'s cache.
//!
//! `brain-exec` must not depend on `brain-graph` (the graph is a tool crate, and
//! the cache only needs to ask "what does the tree look like"), so the adapter
//! lives here, at the composition point.

use brain_exec::SourceHashes;
use brain_graph::model::ProjectCodeGraph;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct GraphSources {
    graph: Arc<RwLock<ProjectCodeGraph>>,
}

impl GraphSources {
    pub fn new(graph: Arc<RwLock<ProjectCodeGraph>>) -> Self {
        Self { graph }
    }
}

impl SourceHashes for GraphSources {
    fn hashes(&self) -> Pin<Box<dyn Future<Output = BTreeMap<PathBuf, [u8; 32]>> + Send + '_>> {
        Box::pin(async move {
            let lock = self.graph.read().await;
            lock.file_hashes
                .iter()
                .map(|(path, hash)| (path.clone(), *hash.as_bytes()))
                .collect()
        })
    }
}

/// Wraps the graph role, since `brain_tools` re-exports the graph modules.
pub fn sources_from(graph: Arc<RwLock<ProjectCodeGraph>>) -> Arc<dyn SourceHashes> {
    Arc::new(GraphSources::new(graph))
}
