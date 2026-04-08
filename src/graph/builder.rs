use std::path::Path;

use anyhow::Result;
use tracing::info;

use super::model::DependencyGraph;
use crate::graph::model::FileNode;
use crate::sources::SourceEdge;

/// Build a dependency graph from discovered files and extracted edges.
pub fn build_graph(
    repo_root: &Path,
    files: Vec<FileNode>,
    edges: Vec<SourceEdge>,
) -> Result<DependencyGraph> {
    let mut graph = DependencyGraph::new(repo_root.to_path_buf());

    // Add all file nodes.
    for file in files {
        graph.ensure_node(file);
    }

    // Add all edges.
    let mut edge_count = 0;
    for edge in &edges {
        if graph.file_index.contains_key(&edge.from) && graph.file_index.contains_key(&edge.to) {
            graph.add_edge(&edge.from, &edge.to, edge.kind);
            edge_count += 1;
        }
    }

    graph.metadata.indexed_at = chrono_now();
    graph.update_metadata();

    info!(
        files = graph.metadata.file_count,
        edges = edge_count,
        "Graph built"
    );

    Ok(graph)
}

/// Incrementally update a graph: re-index only the changed files.
pub fn update_graph_incremental(
    graph: &mut DependencyGraph,
    changed_file_ids: &[String],
    new_edges: Vec<SourceEdge>,
    updated_nodes: Vec<FileNode>,
) {
    // Remove stale edges from changed files.
    for id in changed_file_ids {
        graph.remove_edges_from(id);
    }

    // Update node metadata (content hash, mtime, symbols).
    for node in updated_nodes {
        if let Some(&idx) = graph.file_index.get(&node.id) {
            let existing = &mut graph.graph[idx];
            existing.content_hash = node.content_hash;
            existing.mtime = node.mtime;
            existing.defined_symbols = node.defined_symbols;
            existing.module = node.module;
        } else {
            graph.ensure_node(node);
        }
    }

    // Add new edges.
    for edge in &new_edges {
        if graph.file_index.contains_key(&edge.from) && graph.file_index.contains_key(&edge.to) {
            graph.add_edge(&edge.from, &edge.to, edge.kind);
        }
    }

    graph.update_metadata();
}

fn chrono_now() -> String {
    // Simple ISO 8601 timestamp without external dependency.
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{EdgeKind, FileNode, FileRole};
    use crate::sources::SourceEdge;
    use petgraph::visit::EdgeRef;

    fn make_node(id: &str) -> FileNode {
        FileNode {
            id: id.to_string(),
            path: std::path::PathBuf::from(id),
            role: FileRole::Source,
            module: None,
            defined_symbols: vec![],
            content_hash: None,
            mtime: None,
            a11y_setters: vec![],
            a11y_queries: vec![],
            test_methods: vec![],
        }
    }

    /// Simulates the CI incremental bug: when only the changed file's edges
    /// are returned by the source, edges to unchanged files must be preserved
    /// if they appear in the new_edges list.
    #[test]
    fn incremental_update_preserves_cross_boundary_edges() {
        // Build an initial graph: A→B, A→C, B→C
        let nodes = vec![make_node("A"), make_node("B"), make_node("C")];
        let edges = vec![
            SourceEdge { from: "A".into(), to: "B".into(), kind: EdgeKind::DirectReference },
            SourceEdge { from: "A".into(), to: "C".into(), kind: EdgeKind::DirectReference },
            SourceEdge { from: "B".into(), to: "C".into(), kind: EdgeKind::DirectReference },
        ];
        let mut graph = build_graph(std::path::Path::new("/repo"), nodes, edges).unwrap();
        assert_eq!(graph.graph.edge_count(), 3);

        // Simulate incremental update where A changed.
        // The source returns edges from A to both B and C (cross-boundary).
        let new_edges = vec![
            SourceEdge { from: "A".into(), to: "B".into(), kind: EdgeKind::DirectReference },
            SourceEdge { from: "A".into(), to: "C".into(), kind: EdgeKind::DirectReference },
        ];
        update_graph_incremental(&mut graph, &["A".into()], new_edges, vec![make_node("A")]);

        // A→B and A→C should exist (re-added), plus B→C (untouched).
        assert_eq!(graph.graph.edge_count(), 3);

        // Verify A still has outgoing edges to B and C.
        let a_idx = graph.file_index["A"];
        let a_targets: std::collections::HashSet<String> = graph
            .graph
            .edges(a_idx)
            .map(|e| graph.graph[e.target()].id.clone())
            .collect();
        assert!(a_targets.contains("B"), "A→B should exist");
        assert!(a_targets.contains("C"), "A→C should exist");
    }

    /// Regression: if the source only returns edges between changed files
    /// (the old bug), cross-boundary edges are lost.
    #[test]
    fn incremental_update_without_scope_loses_edges() {
        let nodes = vec![make_node("A"), make_node("B"), make_node("C")];
        let edges = vec![
            SourceEdge { from: "A".into(), to: "B".into(), kind: EdgeKind::DirectReference },
            SourceEdge { from: "A".into(), to: "C".into(), kind: EdgeKind::DirectReference },
            SourceEdge { from: "B".into(), to: "C".into(), kind: EdgeKind::DirectReference },
        ];
        let mut graph = build_graph(std::path::Path::new("/repo"), nodes, edges).unwrap();

        // Simulate the bug: source only returns edges between changed files (A),
        // with no edges to B or C because they weren't in the analysis set.
        let new_edges: Vec<SourceEdge> = vec![];
        update_graph_incremental(&mut graph, &["A".into()], new_edges, vec![make_node("A")]);

        // A's outgoing edges are gone — only B→C survives.
        assert_eq!(graph.graph.edge_count(), 1);
        let a_idx = graph.file_index["A"];
        assert_eq!(
            graph.graph.edges(a_idx).count(),
            0,
            "A should have no outgoing edges (demonstrating the bug)"
        );
    }
}
