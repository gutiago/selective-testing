pub mod dot_d;
pub mod indexstore;
pub mod treesitter;

use std::path::Path;

use anyhow::Result;

use crate::graph::model::{EdgeKind, FileNode};

/// An edge discovered by a data source.
#[derive(Debug, Clone)]
pub struct SourceEdge {
    /// File ID of the dependency (the file being used).
    pub from: String,
    /// File ID of the dependent (the file using it).
    pub to: String,
    /// Kind of dependency relationship.
    pub kind: EdgeKind,
}

/// Trait for dependency data sources.
pub trait DataSource {
    fn name(&self) -> &str;

    /// Discover file nodes and edges from this data source.
    fn analyze(
        &self,
        repo_root: &Path,
        swift_files: &[std::path::PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)>;
}
