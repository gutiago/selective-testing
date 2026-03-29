use petgraph::algo::tarjan_scc;

use super::model::DependencyGraph;

const LARGE_CYCLE_THRESHOLD: usize = 10;

#[derive(Debug)]
pub struct CycleReport {
    pub cycles: Vec<Vec<String>>,
    pub warnings: Vec<String>,
}

/// Detect all cycles in the dependency graph using Tarjan's SCC algorithm.
/// Returns strongly connected components with more than one node.
pub fn detect_cycles(graph: &DependencyGraph) -> CycleReport {
    let sccs = tarjan_scc(&graph.graph);
    let mut cycles = Vec::new();
    let mut warnings = Vec::new();

    for scc in sccs {
        if scc.len() > 1 {
            let file_ids: Vec<String> = scc
                .iter()
                .map(|&idx| graph.graph[idx].id.clone())
                .collect();

            if file_ids.len() > LARGE_CYCLE_THRESHOLD {
                warnings.push(format!(
                    "Dependency cycle detected ({} files):\n  {}\n  \
                     These files will always be tested together, reducing TIA effectiveness.\n  \
                     Consider breaking the cycle to improve test isolation.",
                    file_ids.len(),
                    file_ids
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" -> ")
                        + " -> ..."
                ));
            }

            cycles.push(file_ids);
        }
    }

    CycleReport { cycles, warnings }
}
