use crate::error::Result;
use crate::graph::builder::GraphBuilder;
use crate::graph::model::ProjectCodeGraph;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct GraphWatcher {
    _watcher: RecommendedWatcher,
}

impl GraphWatcher {
    pub fn start(
        root_dir: PathBuf,
        graph: Arc<RwLock<ProjectCodeGraph>>,
        builder: Arc<GraphBuilder>,
    ) -> Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(100);

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                            for path in event.paths {
                                let ext = path.extension().and_then(|e| e.to_str());
                                if ext == Some("rs") || ext == Some("kt") || ext == Some("kts") {
                                    let _ = tx.blocking_send(path);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
            Config::default(),
        )
        .map_err(|e| crate::error::BrainError::Graph(format!("Notify error: {e}")))?;

        watcher
            .watch(&root_dir, RecursiveMode::Recursive)
            .map_err(|e| crate::error::BrainError::Graph(format!("Watch error: {e}")))?;

        let root_clone = root_dir.clone();
        tokio::spawn(async move {
            while let Some(path) = rx.recv().await {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                let mut lock = graph.write().await;
                match builder.update_file(&mut lock, &path, &root_clone) {
                    Ok(changed) => {
                        if changed {
                            tracing::info!(path = %path.display(), "Graph incrementally updated");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "Incremental update error");
                    }
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}
