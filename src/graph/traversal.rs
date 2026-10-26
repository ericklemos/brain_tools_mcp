use crate::graph::model::ProjectCodeGraph;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::collections::HashSet;
use std::path::Path;

pub struct GraphTraversal;

impl GraphTraversal {
    pub fn context_graph(
        graph: &ProjectCodeGraph,
        target: &str,
        _depth: usize,
        root_dir: &Path,
    ) -> String {
        let matching_nodes: Vec<_> = if let Some(indices) = graph.symbol_lookup.get(target) {
            indices.clone()
        } else if let Some(&idx) = graph.qualified_lookup.get(target) {
            vec![idx]
        } else {
            // Check if matches a file path
            graph
                .file_nodes
                .iter()
                .filter(|(path, _)| path.to_string_lossy().contains(target))
                .flat_map(|(_, indices)| indices.iter().copied())
                .collect()
        };

        if matching_nodes.is_empty() {
            return format!("Nenhum símbolo ou arquivo correspondente encontrado para: `{target}`.");
        }

        let mut out = String::new();

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

            out.push_str(&format!(
                "### Alvo: `{}` (`{}:{}-{}`)\n",
                center_node.name, rel_path, center_node.start_line, center_node.end_line
            ));

            if !center_node.signature.is_empty() {
                out.push_str(&format!("**Assinatura:** `{}`\n\n", center_node.signature));
            }

            // 1. Quem chama este símbolo (Incoming)
            out.push_str("#### Quem chama este símbolo (Incoming Calls):\n");
            let mut incoming_count = 0;
            for edge_ref in graph.graph.edges_directed(center_idx, Direction::Incoming) {
                let caller_idx = edge_ref.source();
                if let Some(caller) = graph.graph.node_weight(caller_idx) {
                    let edge = edge_ref.weight();
                    let caller_path = caller
                        .file_path
                        .strip_prefix(root_dir)
                        .unwrap_or(&caller.file_path)
                        .display();

                    out.push_str(&format!(
                        "- `{}:{}` -> `{}` [{:?} na linha {}]\n",
                        caller_path, caller.start_line, caller.signature, edge.kind, edge.call_site_line
                    ));
                    incoming_count += 1;
                }
            }
            if incoming_count == 0 {
                out.push_str("- (Nenhuma chamada de entrada detectada no grafo)\n");
            }
            out.push('\n');

            // 2. O que este símbolo consome (Outgoing)
            out.push_str("#### O que este símbolo consome (Outgoing Calls & Types):\n");
            let mut outgoing_count = 0;
            for edge_ref in graph.graph.edges_directed(center_idx, Direction::Outgoing) {
                let callee_idx = edge_ref.target();
                if let Some(callee) = graph.graph.node_weight(callee_idx) {
                    let edge = edge_ref.weight();
                    let callee_path = callee
                        .file_path
                        .strip_prefix(root_dir)
                        .unwrap_or(&callee.file_path)
                        .display();

                    out.push_str(&format!(
                        "- `{}:{}` -> `{}` [{:?} na linha {}]\n",
                        callee_path, callee.start_line, callee.signature, edge.kind, edge.call_site_line
                    ));
                    outgoing_count += 1;
                }
            }
            if outgoing_count == 0 {
                out.push_str("- (Nenhuma chamada externa consumida detectada)\n");
            }
            out.push_str("\n---\n\n");
        }

        out
    }

    pub fn impact_check(
        graph: &ProjectCodeGraph,
        symbol_name: &str,
        root_dir: &Path,
    ) -> String {
        let matching_nodes = match graph.symbol_lookup.get(symbol_name) {
            Some(indices) => indices.clone(),
            None => {
                return format!("Símbolo `{symbol_name}` não encontrado no índice do projeto.");
            }
        };

        let mut out = String::new();
        out.push_str(&format!(
            "### Relatório de Análise de Impacto: `{symbol_name}`\n\n"
        ));

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

            out.push_str(&format!(
                "Alterações em `{}` (`{}:{}`) impactarão diretamente ou indiretamente os seguintes pontos:\n\n",
                target_node.qualified_name, rel_path, target_node.start_line
            ));

            let mut visited = HashSet::new();
            visited.insert(target_idx);

            // BFS over incoming edges (callers of this node)
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
                out.push_str("✅ **Impacto Seguro**: Nenhuma chamada ou referência ativa encontrada no projeto para este símbolo.\n\n");
            } else {
                out.push_str(&format!(
                    "⚠️ **{} local(is) impactado(s)**:\n\n",
                    impacted.len()
                ));

                for (caller, dist, line) in impacted {
                    let path = caller
                        .file_path
                        .strip_prefix(root_dir)
                        .unwrap_or(&caller.file_path)
                        .display();

                    out.push_str(&format!(
                        "- [Grau {}] `{}:{}` -> `{}` (Call-site na linha {})\n",
                        dist, path, caller.start_line, caller.name, line
                    ));
                }
                out.push('\n');
            }
        }

        out
    }
}
