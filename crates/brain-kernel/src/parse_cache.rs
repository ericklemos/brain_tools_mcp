//! Shared cache of parsed tree-sitter trees.
//!
//! The graph already parses every file at startup; search used to parse the
//! same files again on every request. Entries are keyed by path *and* content
//! digest, so an edited file is a miss instead of a stale answer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tree_sitter::{Parser, Tree};

use crate::ast::SupportedLanguage;
use crate::error::{BrainError, Result};

/// Cap on cached *source* bytes. A tree costs several times its source, so this
/// is deliberately conservative: past it, the oldest files are reparsed.
const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

struct Entry {
    digest: blake3::Hash,
    tree: Arc<Tree>,
    bytes: usize,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<PathBuf, Entry>,
    /// Insertion order, used for fifo eviction. Entries carry a generation so
    /// a stale queue item cannot evict a newer version of the same path.
    order: VecDeque<(PathBuf, u64)>,
    bytes: usize,
    generation: u64,
}

/// Parsed trees shared by the graph builder, search and read.
pub struct ParseCache {
    max_bytes: usize,
    inner: RwLock<Inner>,
}

impl ParseCache {
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_BYTES)
    }

    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Returns the tree for `path` only when it was parsed from `digest`.
    pub fn get(&self, path: &Path, digest: blake3::Hash) -> Option<Arc<Tree>> {
        let inner = self.inner.read().ok()?;
        let entry = inner.entries.get(path)?;
        (entry.digest == digest).then(|| entry.tree.clone())
    }

    /// Stores `tree` and returns the canonical handle for this file.
    pub fn insert(&self, path: &Path, digest: blake3::Hash, tree: Arc<Tree>) -> Arc<Tree> {
        let Ok(mut inner) = self.inner.write() else {
            return tree;
        };

        inner.generation += 1;
        let generation = inner.generation;
        let bytes = tree.root_node().end_byte();
        if let Some(old) = inner.entries.remove(path) {
            inner.bytes = inner.bytes.saturating_sub(old.bytes);
        }
        inner.bytes += bytes;
        inner.entries.insert(
            path.to_path_buf(),
            Entry {
                digest,
                tree: tree.clone(),
                bytes,
                generation,
            },
        );
        inner.order.push_back((path.to_path_buf(), generation));

        while inner.bytes > self.max_bytes {
            let Some((old_path, old_generation)) = inner.order.pop_front() else {
                break;
            };
            let current = inner
                .entries
                .get(&old_path)
                .is_some_and(|entry| entry.generation == old_generation);
            if current {
                if let Some(removed) = inner.entries.remove(&old_path) {
                    inner.bytes = inner.bytes.saturating_sub(removed.bytes);
                }
            }
        }

        tree
    }

    /// Drops an entry, e.g. when the watcher sees the file removed.
    pub fn invalidate(&self, path: &Path) {
        let Ok(mut inner) = self.inner.write() else {
            return;
        };
        if let Some(removed) = inner.entries.remove(path) {
            inner.bytes = inner.bytes.saturating_sub(removed.bytes);
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .map(|inner| inner.entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}

/// A tree-sitter parser that keeps its grammar loaded between files.
///
/// One worker per thread: `Parser` is not `Sync`. Reusing it avoids paying
/// `set_language` for every file in a directory walk.
pub struct ParseWorker {
    parser: Parser,
    language: Option<u8>,
}

impl ParseWorker {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            language: None,
        }
    }

    /// Parses `content`, reloading the grammar only when the language changes.
    pub fn parse(&mut self, lang: SupportedLanguage, content: &str) -> Result<Tree> {
        let id = match lang {
            SupportedLanguage::Rust => 0,
            SupportedLanguage::Kotlin => 1,
        };
        if self.language != Some(id) {
            self.parser
                .set_language(&lang.grammar())
                .map_err(|e| BrainError::Ast(format!("{} grammar load error: {e}", lang.name())))?;
            self.language = Some(id);
        }
        self.parser
            .parse(content, None)
            .ok_or_else(|| BrainError::Ast("Failed to parse AST".to_string()))
    }
}

impl Default for ParseWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(source: &str) -> Arc<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&SupportedLanguage::Rust.grammar())
            .unwrap();
        Arc::new(parser.parse(source, None).unwrap())
    }

    fn digest(source: &str) -> blake3::Hash {
        blake3::hash(source.as_bytes())
    }

    #[test]
    fn a_tree_comes_back_only_for_the_same_digest() {
        let cache = ParseCache::new();
        let path = Path::new("src/lib.rs");
        let first = tree("fn a() {}");
        cache.insert(path, digest("fn a() {}"), first.clone());

        assert!(cache.get(path, digest("fn a() {}")).is_some());
        assert!(cache.get(path, digest("fn b() {}")).is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn replacing_a_file_keeps_only_the_new_tree() {
        let cache = ParseCache::new();
        let path = Path::new("src/lib.rs");
        cache.insert(path, digest("fn a() {}"), tree("fn a() {}"));
        cache.insert(path, digest("fn b() {}"), tree("fn b() {}"));

        assert!(cache.get(path, digest("fn a() {}")).is_none());
        assert!(cache.get(path, digest("fn b() {}")).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn invalidate_removes_the_entry() {
        let cache = ParseCache::new();
        let path = Path::new("src/lib.rs");
        cache.insert(path, digest("fn a() {}"), tree("fn a() {}"));
        cache.invalidate(path);

        assert!(cache.get(path, digest("fn a() {}")).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn the_byte_ceiling_evicts_the_oldest_entry() {
        // Two 9-byte sources: one fits, two do not.
        let cache = ParseCache::with_max_bytes(12);
        let first = Path::new("src/a.rs");
        let second = Path::new("src/b.rs");
        cache.insert(first, digest("fn a() {}"), tree("fn a() {}"));
        cache.insert(second, digest("fn b() {}"), tree("fn b() {}"));

        assert!(cache.get(first, digest("fn a() {}")).is_none());
        assert_eq!(cache.len(), 1);
    }
}
