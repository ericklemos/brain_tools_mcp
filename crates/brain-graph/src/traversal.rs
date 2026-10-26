use crate::model::ProjectCodeGraph;
use brain_kernel::budget::Section;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::collections::HashSet;
use std::path::Path;

pub struct GraphTraversal;

impl GraphTraversal {
    /// Renders the legacy string form. Kept for callers that have not opted
    /// into the response contract; it is exactly the concatenation of the
    /// sections, so both paths stay in sync by construction.
    pub fn context_graph(
        graph: &ProjectCodeGraph,
        target: &str,
        depth: usize,
        root_dir: &Path,
    ) -> String {
        let sections = Self::context_sections(graph, target, depth, root_dir);
        if sections.is_empty() {
            return format!("Nenhum símbolo ou arquivo correspondente encontrado para: `{target}`.");
        }
        sections.iter().map(section_text).collect()
    }

    pub fn impact_check(
        graph: &ProjectCodeGraph,
        symbol_name: &str,
        root_dir: &Path,
    ) -> String {
        let sections = Self::impact_sections(graph, symbol_name, root_dir);
        if sections.is_empty() {
            return format!("Símbolo `{symbol_name}` não encontrado no índice do projeto.");
        }
        sections.iter().map(section_text).collect()
    }

    /// One `Section` per node: the centre, who calls it, and what it consumes.
    ///
    /// Priorities are what decide truncation, so incoming callers outrank
    /// outgoing dependencies — an agent editing a symbol needs its callers
    /// before its callees.
    pub fn context_sections(
        graph: &ProjectCodeGraph,
        target: &str,
        _depth: usize,
        root_dir: &Path,
    ) -> Vec<Section> {
        let matching_nodes: Vec<_> = if let Some(indices) = graph.symbol_lookup.get(target) {
            indices.clone()
        } else if let Some(&idx) = graph.qualified_lookup.get(target) {
            vec![idx]
        } else {
            graph
                .file_nodes
                .iter()
                .filter(|(path, _)| path.to_string_lossy().contains(target))
                .flat_map(|(_, indices)| indices.iter().copied())
                .collect()
        };

        let mut sections = Vec::new();

        for &center_idx in &matching_nodes {
            let center_node = match graph.graph.node_weight(center_idx) {
                Some(n) => n,
                None => continue,
            };

            let rel_path = center_node
                .file_path
                .strip_prefix(root_dir)
                .unwrap_or(&center_node.file_path)
                .display();

            let heading = format!(
                "### Alvo: `{}` (`{}:{}-{}`)",
                center_node.name, rel_path, center_node.start_line, center_node.end_line
            );

            let mut body = String::new();
            if !center_node.signature.is_empty() {
                body.push_str(&format!("**Assinatura:** `{}`
", center_node.signature));
            }
            sections.push(Section {
                heading,
                body,
                priority: 255,
            });

            // Incoming callers.
            let mut incoming = String::new();
            for edge_ref in graph.graph.edges_directed(center_idx, Direction::Incoming) {
                let caller_idx = edge_ref.source();
                if let Some(caller) = graph.graph.node_weight(caller_idx) {
                    let edge = edge_ref.weight();
                    let caller_path = caller
                        .file_path
                        .strip_prefix(root_dir)
                        .unwrap_or(&caller.file_path)
                        .display();
                    incoming.push_str(&format!(
                        "- `{}:{}` -> `{}` [{:?} na linha {}]\n",
                        caller_path, caller.start_line, caller.signature, edge.kind, edge.call_site_line
                    ));
                }
            }
            if incoming.is_empty() {
                incoming.push_str("- (Nenhuma chamada de entrada detectada no grafo)\n");
            }
            sections.push(Section {
                heading: "#### Quem chama este símbolo (Incoming Calls):".to_string(),
                body: incoming,
                priority: 200,
            });

            // Outgoing dependencies.
            let mut outgoing = String::new();
            for edge_ref in graph.graph.edges_directed(center_idx, Direction::Outgoing) {
                let callee_idx = edge_ref.target();
                if let Some(callee) = graph.graph.node_weight(callee_idx) {
                    let edge = edge_ref.weight();
                    let callee_path = callee
                        .file_path
                        .strip_prefix(root_dir)
                        .unwrap_or(&callee.file_path)
                        .display();
                    outgoing.push_str(&format!(
                        "- `{}:{}` -> `{}` [{:?} na linha {}]\n",
                        callee_path, callee.start_line, callee.signature, edge.kind, edge.call_site_line
                    ));
                }
            }
            if outgoing.is_empty() {
                outgoing.push_str("- (Nenhuma chamada externa consumida detectada)\n");
            }
            sections.push(Section {
                heading: "#### O que este símbolo consome (Outgoing Calls & Types):".to_string(),
                body: outgoing,
                priority: 150,
            });
        }

        sections
    }

    /// One `Section` per impacted symbol: direct callers outrank transitive ones,
    /// which is where a budget saves the most — a report often has 2 direct
    /// callers and 40 transitive, and the decision needs the 2.
    pub fn impact_sections(
        graph: &ProjectCodeGraph,
        symbol_name: &str,
        root_dir: &Path,
    ) -> Vec<Section> {
        let matching_nodes = match graph.symbol_lookup.get(symbol_name) {
            Some(indices) => indices.clone(),
            None => return Vec::new(),
        };

        let mut sections = Vec::new();

        sections.push(Section {
            heading: format!("### Relatório de Análise de Impacto: `{symbol_name}`"),
            body: String::new(),
            priority: 255,
        });

        for &target_idx in &matching_nodes {
            let target_node = match graph.graph.node_weight(target_idx) {
                Some(n) => n,
                None => continue,
            };

            let rel_path = target_node
                .file_path
                .strip_prefix(root_dir)
                .unwrap_or(&target_node.file_path)
                .display();

            sections.push(Section {
                heading: format!(
                    "#### Alterações em `{}` (`{}:{}`) impactarão diretamente ou indiretamente os seguintes pontos:",
                    target_node.qualified_name, rel_path, target_node.start_line
                ),
                body: String::new(),
                priority: 255,
            });

            let mut visited = HashSet::new();
            visited.insert(target_idx);

            let mut queue = std::collections::VecDeque::new();
            queue.push_back((target_idx, 1usize));

            let mut impacted = Vec::new();

            while let Some((curr_idx, distance)) = queue.pop_front() {
                for edge_ref in graph.graph.edges_directed(curr_idx, Direction::Incoming) {
                    let caller_idx = edge_ref.source();
                    if visited.insert(caller_idx) {
                        if let Some(caller) = graph.graph.node_weight(caller_idx) {
                            impacted.push((caller, distance, edge_ref.weight().call_site_line));
                            if distance < 3 {
                                queue.push_back((caller_idx, distance + 1));
                            }
                        }
                    }
                }
            }

            if impacted.is_empty() {
                sections.push(Section {
                    heading: "✅ **Impacto Seguro**: Nenhuma chamada ou referência ativa encontrada no projeto para este símbolo.".to_string(),
                    body: String::new(),
                    priority: 255,
                });
                continue;
            }

            let total = impacted.len();
            sections.push(Section {
                heading: format!("⚠️ **{total} local(is) impactado(s)**:"),
                body: String::new(),
                priority: 255,
            });

            for (caller, dist, line) in impacted {
                let path = caller
                    .file_path
                    .strip_prefix(root_dir)
                    .unwrap_or(&caller.file_path)
                    .display();

                let label = if dist == 1 { "direto" } else { "transitivo" };
                sections.push(Section {
                    heading: format!(
                        "- [{label}, Grau {dist}] `{path}:{line_at}` -> `{name}` (Call-site na linha {line})",
                        line_at = caller.start_line,
                        name = caller.name
                    ),
                    body: String::new(),
                    priority: if dist == 1 { 255 } else { 120 },
                });
            }
        }

        sections
    }
}

fn section_text(section: &Section) -> String {
    format!("{}\n{}\n", section.heading, section.body)
}


#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::fs;

    fn graph_of(dir: &std::path::Path) -> ProjectCodeGraph {
        GraphBuilder::new().unwrap().build_graph(dir).unwrap()
    }

    #[test]
    fn context_graph_sections_prioritise_centre_then_incoming_then_outgoing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn verify_token() {}\npub fn auth_guard() { verify_token(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections =
            GraphTraversal::context_sections(&graph, "verify_token", 1, dir.path());

        assert!(!sections.is_empty());
        assert_eq!(sections[0].priority, 255, "centre node must lead");
        let incoming = sections.iter().find(|s| s.heading.contains("Incoming"));
        let outgoing = sections.iter().find(|s| s.heading.contains("Outgoing"));
        assert!(incoming.unwrap().priority > outgoing.unwrap().priority);
    }

    #[test]
    fn impact_sections_prioritise_direct_over_transitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn verify_token() {}\npub fn auth_guard() { verify_token(); }\npub fn route() { auth_guard(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections = GraphTraversal::impact_sections(&graph, "verify_token", dir.path());
        let direct = sections.iter().find(|s| s.heading.contains("direto"));
        let transitive = sections.iter().find(|s| s.heading.contains("transitivo"));

        assert!(direct.unwrap().priority > transitive.unwrap().priority);
    }
}
