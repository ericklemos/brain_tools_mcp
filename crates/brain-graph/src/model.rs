use crate::scope::{FileScope, Resolution, ScopeResolver};
use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolKind {
    FileModule,
    Function,
    Method,
    StructOrClass,
    TraitOrInterface,
    Enum,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub qualified_name: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    /// The type this symbol belongs to (`impl GraphBuilder { fn build }`).
    /// `None` for a free function. Method resolution needs it: `x.build()` has
    /// to reach `GraphBuilder::build`, not the eleven other `build`s.
    #[serde(default)]
    pub container: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelationKind {
    Defines,
    Imports,
    Calls,
    UsesType,
    ImplementsTrait,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationEdge {
    pub kind: RelationKind,
    pub call_site_line: usize,
}

/// How much of the call graph the builder could actually prove.
///
/// Without this the graph looks equally confident about a call it resolved
/// through an import and one it guessed at; tools that report impact can then
/// say what they do not know instead of implying completeness.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResolutionStats {
    /// Calls linked to exactly one definition.
    pub resolved: usize,
    /// Calls whose name has several candidate definitions and no local evidence.
    pub ambiguous: usize,
    /// Calls no definition in the repository matches (std, external crates).
    pub unknown: usize,
    /// The ambiguous names, counted, so the report can name the worst offenders.
    pub ambiguous_names: HashMap<String, usize>,
}

impl ResolutionStats {
    pub fn record(&mut self, name: &str, resolution: &Resolution) {
        match resolution {
            Resolution::Resolved(_) => self.resolved += 1,
            Resolution::Ambiguous { .. } => {
                self.ambiguous += 1;
                *self.ambiguous_names.entry(name.to_string()).or_default() += 1;
            }
            Resolution::Unknown => self.unknown += 1,
        }
    }

    /// One line stating how much of the call graph is proven.
    ///
    /// Tools that report impact print this, because a graph that silently drops
    /// a third of its calls reads exactly like a complete one.
    pub fn disclosure(&self) -> String {
        let total = self.resolved + self.ambiguous + self.unknown;
        if total == 0 {
            return String::new();
        }
        let proven = (self.resolved * 100) / total;
        format!(
            "> Confiança do grafo: {proven}% das {total} chamadas resolvidas \
             ({} ambíguas descartadas, {} fora do repositório).\n",
            self.ambiguous, self.unknown
        )
    }

    /// The ambiguous names, worst first.
    pub fn worst_ambiguous(&self, limit: usize) -> Vec<(&str, usize)> {
        let mut by_count: Vec<(&str, usize)> = self
            .ambiguous_names
            .iter()
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        by_count.truncate(limit);
        by_count
    }
}

#[derive(Default)]
pub struct ProjectCodeGraph {
    pub graph: StableDiGraph<SymbolNode, RelationEdge>,
    pub symbol_lookup: HashMap<String, Vec<NodeIndex>>,
    pub qualified_lookup: HashMap<String, NodeIndex>,
    pub file_nodes: HashMap<PathBuf, Vec<NodeIndex>>,
    pub file_hashes: HashMap<PathBuf, blake3::Hash>,
    /// What each file imports, rebuilt whenever the file is re-indexed.
    pub scopes: HashMap<PathBuf, FileScope>,
    /// Maps `use` paths onto repository files.
    pub resolver: ScopeResolver,
    /// How many calls this graph could and could not prove.
    pub resolution: ResolutionStats,
}

impl ProjectCodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes all existing nodes belonging to a file before re-indexing
    pub fn remove_file_nodes(&mut self, path: &PathBuf) {
        if let Some(nodes) = self.file_nodes.remove(path) {
            for node_idx in nodes {
                if let Some(node) = self.graph.node_weight(node_idx) {
                    self.qualified_lookup.remove(&node.qualified_name);
                    if let Some(indices) = self.symbol_lookup.get_mut(&node.name) {
                        indices.retain(|&idx| idx != node_idx);
                    }
                }
                self.graph.remove_node(node_idx);
            }
        }
        self.file_hashes.remove(path);
        self.scopes.remove(path);
        self.resolver.unregister_file(path);
    }

    /// Every definition of `name`, paired with the file that declares it.
    pub fn definitions_of(&self, name: &str) -> Vec<(PathBuf, NodeIndex)> {
        self.symbol_lookup
            .get(name)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(|&idx| {
                        self.graph
                            .node_weight(idx)
                            .map(|node| (node.file_path.clone(), idx))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Definitions of `name` belonging to `container`, for method calls.
    pub fn methods_of(&self, container: &str, name: &str) -> Vec<(PathBuf, NodeIndex)> {
        self.definitions_of(name)
            .into_iter()
            .filter(|(_, idx)| {
                self.graph
                    .node_weight(*idx)
                    .and_then(|node| node.container.as_deref())
                    == Some(container)
            })
            .collect()
    }
}
