use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use super::model::DependencyGraph;

const CACHE_DIR: &str = ".selective-testing";

/// Resolve the cache file path for a given repo root and data source.
pub fn cache_path(repo_root: &Path, source: &str) -> std::path::PathBuf {
    repo_root
        .join(CACHE_DIR)
        .join(format!("graph-{}.bin", source))
}

/// Save the graph to a MessagePack binary cache file.
pub fn save(graph: &DependencyGraph, repo_root: &Path) -> Result<()> {
    let source = graph
        .metadata
        .data_sources_used
        .first()
        .map(|s| s.as_str())
        .unwrap_or("unknown");
    let path = cache_path(repo_root, source);
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

/// Load the graph from cache, preferring indexstore over tree-sitter.
pub fn load(repo_root: &Path) -> Result<Option<DependencyGraph>> {
    // Prefer indexstore cache, fall back to tree-sitter.
    for source in &["indexstore", "tree-sitter"] {
        let path = cache_path(repo_root, source);
        if path.exists() {
            let data = fs::read(&path)
                .with_context(|| format!("Failed to read cache file: {}", path.display()))?;
            let graph: DependencyGraph =
                rmp_serde::from_slice(&data).context("Failed to deserialize graph cache")?;
            tracing::info!(
                path = %path.display(),
                source = source,
                files = graph.metadata.file_count,
                edges = graph.metadata.edge_count,
                "Graph cache loaded"
            );
            return Ok(Some(graph));
        }
    }
    Ok(None)
}
