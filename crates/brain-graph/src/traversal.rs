use crate::model::ProjectCodeGraph;
use brain_kernel::budget::Section;
use brain_kernel::text::compact_signature;
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
            return format!(
                "Nenhum símbolo ou arquivo correspondente encontrado para: `{target}`."
            );
        }
        sections.iter().map(section_text).collect()
    }

    pub fn impact_check(graph: &ProjectCodeGraph, symbol_name: &str, root_dir: &Path) -> String {
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

        // A file (or a name with several definitions) is a digest: repeating
        // "nenhuma chamada" per symbol costs more than the file it describes.
        // The absence is counted once in a headline instead of dropped.
        let digest = matching_nodes.len() > 1;
        let mut without_incoming = 0usize;
        let mut without_outgoing = 0usize;

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
                "### `{}` (`{}:{}-{}`)",
                center_node.name, rel_path, center_node.start_line, center_node.end_line
            );

            let mut body = String::new();
            let signature = compact_signature(&center_node.signature);
            if !signature.is_empty() {
                body.push_str(&format!("`{signature}`\n"));
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
                    incoming.push_str(&edge_line(
                        &caller.file_path,
                        root_dir,
                        caller.start_line,
                        &caller.signature,
                        &format!("{:?}", edge.kind),
                        edge.call_site_line,
                    ));
                }
            }
            if incoming.is_empty() {
                if digest {
                    without_incoming += 1;
                } else {
                    incoming.push_str("- (Nenhuma chamada de entrada detectada no grafo)\n");
                }
            }
            if !incoming.is_empty() {
                sections.push(Section {
                    heading: "#### Incoming (quem chama):".to_string(),
                    body: incoming,
                    priority: 200,
                });
            }

            // Outgoing dependencies.
            let mut outgoing = String::new();
            for edge_ref in graph.graph.edges_directed(center_idx, Direction::Outgoing) {
                let callee_idx = edge_ref.target();
                if let Some(callee) = graph.graph.node_weight(callee_idx) {
                    let edge = edge_ref.weight();
                    outgoing.push_str(&edge_line(
                        &callee.file_path,
                        root_dir,
                        callee.start_line,
                        &callee.signature,
                        &format!("{:?}", edge.kind),
                        edge.call_site_line,
                    ));
                }
            }
            if outgoing.is_empty() {
                if digest {
                    without_outgoing += 1;
                } else {
                    outgoing.push_str("- (Nenhuma chamada externa consumida detectada)\n");
                }
            }
            if !outgoing.is_empty() {
                sections.push(Section {
                    heading: "#### Outgoing (o que consome):".to_string(),
                    body: outgoing,
                    priority: 150,
                });
            }
        }

        if digest && (without_incoming > 0 || without_outgoing > 0) {
            sections.insert(
                0,
                Section {
                    heading: format!(
                        "### {} símbolos — {without_incoming} sem chamadas de entrada, {without_outgoing} sem consumo externo",
                        matching_nodes.len()
                    ),
                    body: "> Seções vazias omitidas; a contagem acima é a declaração da ausência.\n"
                        .to_string(),
                    priority: 255,
                },
            );
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

        // The disclosure rides on the headline section, so it survives every
        // truncation: a budget must never hide that the answer is partial.
        sections.push(Section {
            heading: format!("### Relatório de Análise de Impacto: `{symbol_name}`"),
            body: graph.resolution.disclosure(),
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

            let display_name = match &target_node.container {
                Some(container) => format!("{}::{}", container, target_node.name),
                None => target_node.name.clone(),
            };

            sections.push(Section {
                heading: format!(
                    "#### `{}` (`{}:{}`) — impacto:",
                    display_name, rel_path, target_node.start_line
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
                // "No callers" and "no callers the graph could prove" are
                // different claims, and this is the one an agent acts on when
                // deleting code.
                sections.push(Section {
                    heading: "⚠️ **Nenhum chamador encontrado no grafo** — não é o mesmo que não haver chamadores.".to_string(),
                    // The disclosure already rides on the headline; repeating it
                    // here would spend tokens on the same sentence.
                    body: "Nenhuma aresta de chamada chega a este símbolo.".to_string(),
                    priority: 255,
                });
                continue;
            }

            let total = impacted.len();
            sections.push(Section {
                heading: format!("**{total} impacto(s)**:"),
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
                        "- [{label} D{dist}] `{path}:{line_at}` -> `{name}` @{line}",
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

fn edge_line(
    path: &Path,
    root_dir: &Path,
    start_line: usize,
    signature: &str,
    kind: &str,
    call_site_line: usize,
) -> String {
    let relative = path.strip_prefix(root_dir).unwrap_or(path).display();
    let signature = compact_signature(signature);
    // The call site is the caller's own first line for one-line functions and
    // many tests; spelling it twice is pure overhead.
    if call_site_line == start_line {
        format!("- `{relative}:{start_line}` -> `{signature}` [{kind}]\n")
    } else {
        format!("- `{relative}:{start_line}` -> `{signature}` [{kind} @{call_site_line}]\n")
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

        let sections = GraphTraversal::context_sections(&graph, "verify_token", 1, dir.path());

        assert!(!sections.is_empty());
        assert_eq!(sections[0].priority, 255, "centre node must lead");
        let incoming = sections.iter().find(|s| s.heading.contains("Incoming"));
        let outgoing = sections.iter().find(|s| s.heading.contains("Outgoing"));
        assert!(incoming.unwrap().priority > outgoing.unwrap().priority);
    }

    #[test]
    fn a_call_matched_by_two_query_patterns_yields_one_edge() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn render() {}\npub fn caller() { crate::render(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections = GraphTraversal::context_sections(&graph, "caller", 1, dir.path());
        let outgoing = sections
            .iter()
            .find(|s| s.heading.contains("Outgoing"))
            .expect("caller consumes render");
        let edges: Vec<_> = outgoing
            .body
            .lines()
            .filter(|line| line.starts_with("- `"))
            .collect();
        assert_eq!(
            edges.len(),
            1,
            "uma chamada é uma aresta, não uma por padrão de query: {:?}",
            outgoing.body
        );
    }

    #[test]
    fn two_call_sites_of_the_same_function_are_two_edges() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn f() {}\npub fn caller() { f(); f(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections = GraphTraversal::context_sections(&graph, "caller", 1, dir.path());
        let outgoing = sections
            .iter()
            .find(|s| s.heading.contains("Outgoing"))
            .expect("caller consumes f");
        let edges = outgoing
            .body
            .lines()
            .filter(|line| line.starts_with("- `"))
            .count();
        assert_eq!(
            edges, 2,
            "merging must be per call site, not per callee: {:?}",
            outgoing.body
        );
        assert!(
            !outgoing.body.contains("na linha"),
            "call sites are compact: {:?}",
            outgoing.body
        );
    }

    #[test]
    fn a_file_target_declares_the_empty_sections_once_instead_of_per_symbol() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub struct P;\npub fn f() {}\npub fn g() { f(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections = GraphTraversal::context_sections(&graph, "a.rs", 1, dir.path());
        let headline = &sections[0];
        assert_eq!(headline.priority, 255);
        assert!(
            headline.heading.contains("2 sem chamadas de entrada"),
            "{:?}",
            headline.heading
        );
        assert_eq!(
            sections
                .iter()
                .filter(|s| s.heading.contains("Incoming"))
                .count(),
            1,
            "only `f` has a caller"
        );
        assert_eq!(
            sections
                .iter()
                .filter(|s| s.heading.contains("Outgoing"))
                .count(),
            1,
            "only `g` consumes something"
        );
        assert!(
            !sections.iter().any(|s| s.body.contains("Nenhuma chamada")),
            "the absence is counted in the headline, not repeated per symbol"
        );
    }

    #[test]
    fn a_single_target_still_states_the_absence_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn f() {}\npub fn g() { f(); }\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let sections = GraphTraversal::context_sections(&graph, "g", 1, dir.path());
        assert!(
            sections
                .iter()
                .any(|s| s.body.contains("Nenhuma chamada de entrada detectada")),
            "answering about one symbol must say there is no caller"
        );
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
        assert!(
            direct.unwrap().heading.contains("D1"),
            "{:?}",
            direct.unwrap().heading
        );
        assert!(
            !direct.unwrap().heading.contains("Grau"),
            "{:?}",
            direct.unwrap().heading
        );
    }
}
