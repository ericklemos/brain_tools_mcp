//! One-shot cache of the last run per operation.
//!
//! Agents re-run builds without changing anything more often than they change
//! code. The cache is deliberately minimal — one entry per [`Kind`], in memory,
//! no TTL, no disk — because that is enough for the pattern and anything larger
//! would need its own invalidation story.

use crate::runner::Kind;
use brain_kernel::budget::Section;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// Source of the content hashes the graph keeps up to date.
///
/// A trait rather than a direct dependency on `brain-graph`: this crate only
/// needs to *ask* what the project looks like, and keeping it that way means
/// the cache can be tested with a plain map.
pub trait SourceHashes: Send + Sync {
    fn hashes(&self) -> Pin<Box<dyn Future<Output = BTreeMap<PathBuf, [u8; 32]>> + Send + '_>>;
}

/// A cache entry: the report's sections and enough context to label it.
///
/// Sections, not a rendered string: the budget is chosen per call, so the same
/// run must be re-cuttable at a different size without recompiling anything.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub sections: Vec<Section>,
    pub success: bool,
    pub duration: Duration,
}

pub struct BuildCache {
    entries: std::sync::Mutex<BTreeMap<&'static str, Entry>>,
}

impl Default for BuildCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildCache {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Runs `compute` unless the tree state matches the last run of this kind.
    ///
    /// Returns the entry and whether it came from the cache, so the caller can
    /// label it: a cached result must never look like a fresh one.
    pub async fn get_or_run<F, Fut>(
        &self,
        kind: Kind,
        root: &Path,
        sources: Option<&dyn SourceHashes>,
        refresh: bool,
        compute: F,
    ) -> (Entry, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Entry>,
    {
        let key = self.key_for(kind, root, sources).await;

        if !refresh {
            let cached = self
                .entries
                .lock()
                .unwrap()
                .get(kind.as_str())
                .cloned();

            if let Some(entry) = cached {
                if entry.key == key && self.sources_still_match(sources).await {
                    return (entry, true);
                }
            }
        }

        // The key is captured *after* the run, describing the state the next
        // caller will find. Cargo writes `Cargo.lock` and `target/` during the
        // run, so a key taken beforehand would never match again.
        let mut entry = compute().await;
        entry.key = self.key_for(kind, root, sources).await;
        self.entries
            .lock()
            .unwrap()
            .insert(kind.as_str(), entry.clone());
        (entry, false)
    }

    /// `kind ‖ hashes of indexed sources ‖ state of the manifests`
    ///
    /// The manifests are not in the graph's index — the watcher only tracks
    /// `.rs`/`.kt`/`.kts` — so their mtime and size enter the key separately.
    async fn key_for(&self, kind: Kind, root: &Path, sources: Option<&dyn SourceHashes>) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind.as_str().as_bytes());

        if let Some(sources) = sources {
            let hashes = sources.hashes().await;
            for (path, hash) in &hashes {
                hasher.update(path.to_string_lossy().as_bytes());
                hasher.update(hash);
            }
        }

        for manifest in ["Cargo.toml", "Cargo.lock"] {
            hasher.update(&manifest_state(&root.join(manifest)));
        }

        hasher.finalize().to_hex().to_string()
    }

    /// Guards against the watcher's debounce: a file saved a few milliseconds
    /// ago may not be in the graph yet, and serving a pre-edit result would make
    /// the agent act on a stale answer. Re-hashing from disk is cheap next to a
    /// wrong build.
    async fn sources_still_match(&self, sources: Option<&dyn SourceHashes>) -> bool {
        let Some(sources) = sources else {
            return true;
        };

        let indexed = sources.hashes().await;
        for (path, expected) in &indexed {
            match std::fs::read(path) {
                Ok(content) => {
                    if blake3::hash(&content).as_bytes() != expected {
                        return false;
                    }
                }
                // A file that vanished since indexing also invalidates.
                Err(_) => return false,
            }
        }
        true
    }
}

fn manifest_state(path: &Path) -> Vec<u8> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mut out = Vec::new();
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            out.extend_from_slice(&modified.to_le_bytes());
            out.extend_from_slice(&meta.len().to_le_bytes());
            out
        }
        Err(_) => vec![0u8; 16],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fixed(Vec<(PathBuf, [u8; 32])>);

    impl SourceHashes for Fixed {
        fn hashes(&self) -> Pin<Box<dyn Future<Output = BTreeMap<PathBuf, [u8; 32]>> + Send + '_>> {
            Box::pin(async move { self.0.iter().cloned().collect() })
        }
    }

    fn entry(body: &str, success: bool) -> Entry {
        Entry {
            key: String::new(),
            sections: vec![Section {
                heading: String::new(),
                body: body.to_string(),
                priority: 255,
            }],
            success,
            duration: Duration::from_millis(1),
        }
    }

    fn body_of(entry: &Entry) -> String {
        entry
            .sections
            .iter()
            .map(|s| format!("{}{}", s.heading, s.body))
            .collect()
    }

    #[tokio::test]
    async fn second_call_with_unchanged_state_does_not_recompute() {
        let cache = BuildCache::new();
        let root = Path::new(".");
        let calls = std::sync::atomic::AtomicUsize::new(0);

        for _ in 0..2 {
            let (_, from_cache) = cache
                .get_or_run(Kind::Build, root, None, false, || async {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    entry("ok", true)
                })
                .await;
            let _ = from_cache;
        }

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_forces_a_new_run() {
        let cache = BuildCache::new();
        let root = Path::new(".");
        let calls = std::sync::atomic::AtomicUsize::new(0);

        cache
            .get_or_run(Kind::Check, root, None, false, || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry("first", true)
            })
            .await;

        let (out, from_cache) = cache
            .get_or_run(Kind::Check, root, None, true, || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry("second", true)
            })
            .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!from_cache);
        assert_eq!(body_of(&out), "second");
    }

    #[tokio::test]
    async fn a_changed_source_file_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}").unwrap();
        let digest = blake3::hash(b"fn a() {}");

        let cache = BuildCache::new();
        let root = dir.path();
        let sources = Fixed(vec![(file.clone(), *digest.as_bytes())]);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        cache
            .get_or_run(Kind::Build, root, Some(&sources), false, || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry("one", true)
            })
            .await;

        // Editing the file without telling the index is exactly the watcher-lag
        // case: the key looks the same, but the disk no longer matches.
        std::fs::write(&file, "fn a() { }").unwrap();

        let (_, from_cache) = cache
            .get_or_run(Kind::Build, root, Some(&sources), false, || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry("two", true)
            })
            .await;

        assert!(!from_cache);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn kinds_do_not_share_an_entry() {
        let cache = BuildCache::new();
        let root = Path::new(".");

        let (a, hit_a) = cache
            .get_or_run(Kind::Build, root, None, false, || async { entry("build", true) })
            .await;
        let (b, hit_b) = cache
            .get_or_run(Kind::Clippy, root, None, false, || async { entry("clippy", true) })
            .await;

        assert!(!hit_a);
        assert!(!hit_b);
        assert_eq!(body_of(&a), "build");
        assert_eq!(body_of(&b), "clippy");
    }
}
