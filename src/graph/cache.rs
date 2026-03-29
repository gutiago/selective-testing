use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use super::model::DependencyGraph;

const CACHE_DIR: &str = ".selective-testing";
const GRAPH_FILE: &str = "graph.bin";

/// Resolve the cache file path for a given repo root.
pub fn cache_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join(CACHE_DIR).join(GRAPH_FILE)
}

/// Save the graph to a MessagePack binary cache file.
pub fn save(graph: &DependencyGraph, repo_root: &Path) -> Result<()> {
    let path = cache_path(repo_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache directory: {}", parent.display()))?;
    }
    let data = rmp_serde::to_vec(graph).context("Failed to serialize graph")?;
    fs::write(&path, &data)
        .with_context(|| format!("Failed to write cache file: {}", path.display()))?;
    tracing::info!(
        path = %path.display(),
        bytes = data.len(),
        "Graph cache saved"
    );
    Ok(())
}

/// Load the graph from a MessagePack binary cache file.
pub fn load(repo_root: &Path) -> Result<Option<DependencyGraph>> {
    let path = cache_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read(&path)
        .with_context(|| format!("Failed to read cache file: {}", path.display()))?;
    let graph: DependencyGraph =
        rmp_serde::from_slice(&data).context("Failed to deserialize graph cache")?;
    tracing::info!(
        path = %path.display(),
        files = graph.metadata.file_count,
        edges = graph.metadata.edge_count,
        "Graph cache loaded"
    );
    Ok(Some(graph))
}
