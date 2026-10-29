use crate::builder::GraphBuilder;
use crate::model::ProjectCodeGraph;
use brain_kernel::{BrainError, Result};
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
        .map_err(|e| BrainError::Graph(format!("Notify error: {e}")))?;

        watcher
            .watch(&root_dir, RecursiveMode::Recursive)
            .map_err(|e| BrainError::Graph(format!("Watch error: {e}")))?;

        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                // One save often touches several files. Draining and deduping
                // the queue means the batch below pays for a single write lock
                // instead of one per event (and a rebuild per event).
                let mut batch = std::collections::BTreeSet::new();
                batch.insert(first);
                while let Ok(path) = rx.try_recv() {
                    batch.insert(path);
                }

                // Snapshot the hashes under a short read lock, then release it:
                // reading and tree-sitter parsing run on the blocking pool.
                let known: Vec<(PathBuf, Option<blake3::Hash>)> = {
                    let lock = graph.read().await;
                    batch
                        .iter()
                        .map(|path| (path.clone(), lock.file_hashes.get(path).copied()))
                        .collect()
                };

                let builder_for_parse = builder.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    known
                        .into_iter()
                        .map(|(path, hash)| {
                            let result = builder_for_parse.prepare_update(hash, &path);
                            (path, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();

                // Only the merge holds the write lock; readers are no longer
                // blocked for the duration of every parse in the batch.
                let mut lock = graph.write().await;
                for (path, result) in prepared {
                    match result {
                        Ok(update) => match builder.apply_update(&mut lock, update) {
                            Ok(true) => {
                                tracing::info!(path = %path.display(), "Graph incrementally updated")
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::debug!(path = %path.display(), error = %e, "Incremental update error")
                            }
                        },
                        Err(e) => {
                            tracing::debug!(path = %path.display(), error = %e, "Incremental update error")
                        }
                    }
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}
