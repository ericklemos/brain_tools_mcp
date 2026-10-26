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

#[derive(Default)]
pub struct ProjectCodeGraph {
    pub graph: StableDiGraph<SymbolNode, RelationEdge>,
    pub symbol_lookup: HashMap<String, Vec<NodeIndex>>,
    pub qualified_lookup: HashMap<String, NodeIndex>,
    pub file_nodes: HashMap<PathBuf, Vec<NodeIndex>>,
    pub file_hashes: HashMap<PathBuf, blake3::Hash>,
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
    }
}
