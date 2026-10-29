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
        // the full incoming/outgoing report per symbol costs more than the file
        // it describes. The agent refines with a symbol when it needs detail.
        if matching_nodes.len() > 1 {
            return Self::digest_sections(graph, target, &matching_nodes, root_dir);
        }

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
                incoming.push_str("- (Nenhuma chamada de entrada detectada no grafo)\n");
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
                outgoing.push_str("- (Nenhuma chamada externa consumida detectada)\n");
            }
            if !outgoing.is_empty() {
                sections.push(Section {
                    heading: "#### Outgoing (o que consome):".to_string(),
                    body: outgoing,
                    priority: 150,
                });
            }
        }

        sections
    }

    /// One line per symbol for a file or an ambiguous name: fan-in/fan-out is
    /// what tells the agent where to look next, and a full caller list per
    /// symbol would cost more than reading the file.
    fn digest_sections(
        graph: &ProjectCodeGraph,
        target: &str,
        nodes: &[petgraph::stable_graph::NodeIndex],
        root_dir: &Path,
    ) -> Vec<Section> {
        struct Row {
            path: String,
            start: usize,
            end: usize,
            signature: String,
            name: String,
            incoming: usize,
            outgoing: usize,
        }

        let mut rows = Vec::with_capacity(nodes.len());
        for &idx in nodes {
            let Some(node) = graph.graph.node_weight(idx) else {
                continue;
            };
            rows.push(Row {
                path: node
                    .file_path
                    .strip_prefix(root_dir)
                    .unwrap_or(&node.file_path)
                    .display()
                    .to_string(),
                start: node.start_line,
                end: node.end_line,
                signature: node.signature.clone(),
                name: node.name.clone(),
                incoming: graph.graph.edges_directed(idx, Direction::Incoming).count(),
                outgoing: graph.graph.edges_directed(idx, Direction::Outgoing).count(),
            });
        }

        if rows.is_empty() {
            return Vec::new();
        }

        let with_incoming = rows.iter().filter(|row| row.incoming > 0).count();
        let with_outgoing = rows.iter().filter(|row| row.outgoing > 0).count();

        // Most central first: the agent reads the digest top-down until its
        // question is answered.
        rows.sort_by(|a, b| {
            b.incoming
                .cmp(&a.incoming)
                .then_with(|| b.outgoing.cmp(&a.outgoing))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.start.cmp(&b.start))
        });

        let subject = {
            let first = &rows[0].path;
            if rows.iter().all(|row| &row.path == first) {
                first.clone()
            } else {
                target.to_string()
            }
        };

        // When every row belongs to the same file, the subject already names
        // it; repeating the path on every line spends ~1/3 of the digest.
        let single_path = rows.iter().all(|row| row.path == rows[0].path);

        let mut sections = vec![Section {
            heading: format!("### `{subject}` — {} símbolos", rows.len()),
            body: format!(
                "> {with_incoming} com chamadas de entrada, {with_outgoing} com consumo externo. Para ver os chamadores de um símbolo, passe o nome em `target_symbol_or_file`.\n"
            ),
            priority: 255,
        }];

        for row in &rows {
            let signature = compact_signature(&row.signature);
            let label = if signature.is_empty() {
                row.name.clone()
            } else {
                signature
            };
            let location = if single_path {
                format!(":{}-{}", row.start, row.end)
            } else {
                format!("{}:{}-{}", row.path, row.start, row.end)
            };
            sections.push(Section {
                heading: format!(
                    "- `{location}` `{label}` ←{} →{}",
                    row.incoming, row.outgoing
                ),
                body: String::new(),
                // Centrality decides what survives a tight budget.
                priority: (150 + row.incoming.min(100)) as u8,
            });
        }

        sections
    }

    /// One `Section` per impacted file and depth: direct callers outrank
    /// transitive ones, which is where a budget saves the most. Grouping by
    /// file pays the path once instead of once per caller.
    pub fn impact_sections(
        graph: &ProjectCodeGraph,
        symbol_name: &str,
        root_dir: &Path,
    ) -> Vec<Section> {
        Self::impact_sections_with_options(graph, symbol_name, root_dir, true)
    }

    /// Same report, optionally collapsing test-only callers into a count.
    pub fn impact_sections_with_options(
        graph: &ProjectCodeGraph,
        symbol_name: &str,
        root_dir: &Path,
        include_tests: bool,
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

            let mut visited = HashSet::new();
            visited.insert(target_idx);

            let mut queue = std::collections::VecDeque::new();
            queue.push_back((target_idx, 1usize));

            let mut impacted = Vec::new();
            let mut hidden_tests = 0usize;

            while let Some((curr_idx, distance)) = queue.pop_front() {
                for edge_ref in graph.graph.edges_directed(curr_idx, Direction::Incoming) {
                    let caller_idx = edge_ref.source();
                    if !visited.insert(caller_idx) {
                        continue;
                    }
                    let Some(caller) = graph.graph.node_weight(caller_idx) else {
                        continue;
                    };
                    if !include_tests && caller.is_test {
                        hidden_tests += 1;
                        continue;
                    }
                    impacted.push((caller, distance, edge_ref.weight().call_site_line));
                    if distance < 3 {
                        queue.push_back((caller_idx, distance + 1));
                    }
                }
            }

            let total = impacted.len();
            let direct = impacted.iter().filter(|(_, distance, _)| *distance == 1).count();

            if total == 0 {
                // "No callers" and "no callers the graph could prove" are
                // different claims, and this is the one an agent acts on when
                // deleting code.
                let body = if hidden_tests > 0 {
                    format!(
                        "Nenhum chamador de produção; {hidden_tests} de teste omitido(s) (include_tests: true)."
                    )
                } else {
                    "Nenhuma aresta de chamada chega a este símbolo.".to_string()
                };
                sections.push(Section {
                    heading: format!(
                        "#### `{display_name}` (`{rel_path}:{}`) — impacto:",
                        target_node.start_line
                    ),
                    body,
                    priority: 255,
                });
                continue;
            }

            let hidden_note = if hidden_tests > 0 {
                format!(
                    "> {hidden_tests} chamada(s) de teste omitida(s); include_tests: true para ver.\n"
                )
            } else {
                String::new()
            };

            sections.push(Section {
                heading: format!(
                    "#### `{display_name}` (`{rel_path}:{}`) — {total} impacto(s) · {direct} D1",
                    target_node.start_line
                ),
                body: hidden_note,
                priority: 255,
            });

            // Group by (depth, file): one path and one depth label per file,
            // not per caller. The heading stays self-contained, so truncation
            // can drop a whole file without losing the context of the rest.
            let mut groups: std::collections::BTreeMap<(usize, String), Vec<(usize, usize, String)>> =
                std::collections::BTreeMap::new();
            for (caller, distance, call_site) in impacted {
                let path = caller
                    .file_path
                    .strip_prefix(root_dir)
                    .unwrap_or(&caller.file_path)
                    .display()
                    .to_string();
                groups
                    .entry((distance, path))
                    .or_default()
                    .push((call_site, caller.start_line, caller.name.clone()));
            }

            for ((distance, path), mut sites) in groups {
                sites.sort();
                let label = if distance == 1 { "direto" } else { "transitivo" };
                let mut body = String::new();
                for (call_site, start_line, name) in sites {
                    if call_site == start_line {
                        body.push_str(&format!("- :{start_line} -> `{name}`\n"));
                    } else {
                        body.push_str(&format!("- :{start_line} -> `{name}` @{call_site}\n"));
                    }
                }
                sections.push(Section {
                    heading: format!("#### D{distance} · {label} — `{path}`"),
                    body,
                    priority: if distance == 1 { 255 } else { 120 },
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
    fn a_file_target_becomes_a_digest_instead_of_repeating_sections() {
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
            headline.heading.contains("3 símbolos"),
            "{:?}",
            headline.heading
        );
        assert!(
            headline.body.contains("1 com chamadas de entrada"),
            "{:?}",
            headline.body
        );
        assert!(
            headline.body.contains("1 com consumo externo"),
            "{:?}",
            headline.body
        );
        assert_eq!(
            sections.len(),
            4,
            "one header plus one line per symbol, not per-section reports"
        );
        assert!(
            sections
                .iter()
                .any(|s| s.heading.contains("pub fn f()") && s.heading.contains("←1 →0")),
            "fan-in/fan-out must be on the symbol line: {:?}",
            sections.iter().map(|s| &s.heading).collect::<Vec<_>>()
        );
        assert!(
            !sections
                .iter()
                .any(|s| s.heading.contains("Incoming") || s.heading.contains("Outgoing")),
            "a digest does not repeat caller sections per symbol"
        );
        assert!(
            !sections.iter().any(|s| s.body.contains("Nenhuma chamada")),
            "the absence is counted in the header, not repeated per symbol"
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

    #[test]
    fn impact_groups_by_file_and_collapses_test_callers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "pub fn target() {}\n").unwrap();
        fs::write(
            dir.path().join("caller.rs"),
            "pub fn caller() {\n    target();\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn target_is_called() {\n        target();\n    }\n}\n",
        )
        .unwrap();
        let graph = graph_of(dir.path());

        let production =
            GraphTraversal::impact_sections_with_options(&graph, "target", dir.path(), false);
        let header = production
            .iter()
            .find(|section| section.heading.contains("impacto(s)"))
            .expect("target header");
        assert!(
            header.body.contains("1 chamada(s) de teste omitida"),
            "hidden tests must be declared: {header:?}"
        );
        let group = production
            .iter()
            .find(|section| section.heading.contains("D1 · direto"))
            .expect("direct group");
        assert!(group.heading.contains("caller.rs"), "{group:?}");
        assert!(group.body.contains("caller"), "{group:?}");
        assert!(
            !group.body.contains("target_is_called"),
            "test callers are hidden by default: {group:?}"
        );

        let all = GraphTraversal::impact_sections_with_options(&graph, "target", dir.path(), true);
        let group = all
            .iter()
            .find(|section| section.heading.contains("D1 · direto"))
            .expect("direct group with tests");
        assert!(group.body.contains("target_is_called"), "{group:?}");
    }

}
