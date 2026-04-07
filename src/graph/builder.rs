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
