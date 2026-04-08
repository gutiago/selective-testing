use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

use super::{DataSource, SourceEdge};
use crate::graph::model::{EdgeKind, FileNode};
use crate::swift::file_classifier;

/// Data source that uses the Swift index-helper binary to query IndexStoreDB.
pub struct IndexStoreSource {
    pub helper_path: PathBuf,
    pub store_path: PathBuf,
    pub db_path: PathBuf,
}

// JSON output matching the Swift helper's edge-based format.
#[derive(Deserialize)]
struct IndexOutput {
    edges: Vec<EdgeOutput>,
    #[serde(rename = "fileCount")]
    file_count: usize,
    #[serde(rename = "edgeCount")]
    edge_count: usize,
}

#[derive(Deserialize)]
struct EdgeOutput {
    from: String,
    to: String,
}

impl IndexStoreSource {
    /// Try to auto-detect the index store path.
    /// Prefers ~/Library/Developer/Xcode/DerivedData (freshest from Xcode builds)
    /// over project-local build directories.
    pub fn detect(repo_root: &Path, helper_path: PathBuf) -> Option<Self> {
        let db_path = repo_root.join(".selective-testing/indexstore-db");

        // 1. Prefer ~/Library/Developer/Xcode/DerivedData.
        if let Some(found) = find_xcode_derived_data(repo_root, &helper_path, &db_path) {
            return Some(found);
        }

        // 2. Fall back to project-local build directories.
        let candidates = [
            repo_root.join("build/derived-data/Index.noindex/DataStore"),
            repo_root.join("DerivedData/Index.noindex/DataStore"),
        ];

        for store_path in &candidates {
            if store_path.exists() {
                info!(path = %store_path.display(), "Auto-detected index store (project-local)");
                return Some(Self {
                    helper_path,
                    store_path: store_path.clone(),
                    db_path,
                });
            }
        }

        None
    }
}

/// Search ~/Library/Developer/Xcode/DerivedData for the freshest matching index store.
fn find_xcode_derived_data(
    repo_root: &Path,
    helper_path: &Path,
    db_path: &Path,
) -> Option<IndexStoreSource> {
    let home = dirs_or_home()?;
    let dd = home.join("Library/Developer/Xcode/DerivedData");
    if !dd.exists() {
        return None;
    }

    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dd) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&repo_name) {
                continue;
            }
            let store = entry.path().join("Index.noindex/DataStore");
            if store.exists() {
                let mtime = std::fs::metadata(&store)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((store, mtime));
            }
        }
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    if let Some((store_path, _)) = candidates.into_iter().next() {
        info!(path = %store_path.display(), "Found index store in ~/Library/Developer/Xcode/DerivedData");
        return Some(IndexStoreSource {
            helper_path: helper_path.to_path_buf(),
            store_path,
            db_path: db_path.to_path_buf(),
        });
    }

    None
}

impl IndexStoreSource {
    /// Incremental analysis: scan definitions from `changed_files` but allow
    /// references from any file in `all_project_files`.
    /// This ensures edges from changed files to unchanged dependents are preserved.
    pub fn analyze_incremental(
        &self,
        repo_root: &Path,
        changed_files: &[PathBuf],
        all_project_files: &[PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        self.run_helper(repo_root, changed_files, Some(all_project_files))
    }

    fn run_helper(
        &self,
        repo_root: &Path,
        swift_files: &[PathBuf],
        project_scope: Option<&[PathBuf]>,
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        info!(
            helper = %self.helper_path.display(),
            store = %self.store_path.display(),
            "Querying IndexStoreDB via Swift helper"
        );

        let st_dir = repo_root.join(".selective-testing");
        std::fs::create_dir_all(&st_dir)?;

        // Write file list to a temp file for --files-from.
        let files_list_path = st_dir.join("file-list.txt");
        {
            let mut f = std::fs::File::create(&files_list_path)?;
            for path in swift_files {
                let rel = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy();
                writeln!(f, "{}", rel)?;
            }
        }

        // Write project scope file for --project-scope (incremental only).
        let scope_list_path = st_dir.join("project-scope.txt");
        if let Some(scope_files) = project_scope {
            let mut f = std::fs::File::create(&scope_list_path)?;
            for path in scope_files {
                let rel = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy();
                writeln!(f, "{}", rel)?;
            }
        }

        // Run the Swift helper.
        let mut cmd = Command::new(&self.helper_path);
        cmd.arg(&self.store_path)
            .arg(&self.db_path)
            .arg("--repo-root")
            .arg(repo_root)
            .arg("--files-from")
            .arg(&files_list_path);

        if project_scope.is_some() {
            cmd.arg("--project-scope").arg(&scope_list_path);
        }

        let output = cmd.output().with_context(|| {
            format!(
                "Failed to run index-helper at: {}",
                self.helper_path.display()
            )
        })?;

        // Log stderr.
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            debug!(helper_log = %line);
        }

        if !output.status.success() {
            anyhow::bail!(
                "index-helper failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            );
        }

        // Clean up temp files.
        let _ = std::fs::remove_file(&files_list_path);
        let _ = std::fs::remove_file(&scope_list_path);

        // Parse edge-based JSON output.
        debug!(stdout_bytes = output.stdout.len(), "Parsing helper JSON output");
        let stdout_trimmed = {
            let s = &output.stdout;
            let end = s.iter().rposition(|&b| b == b'}').map(|i| i + 1).unwrap_or(s.len());
            &s[..end]
        };
        let index_output: IndexOutput = match serde_json::from_slice::<IndexOutput>(stdout_trimmed) {
            Ok(v) => v,
            Err(e) => {
                let preview = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(200)]);
                anyhow::bail!(
                    "JSON parse error: {} (at line {} col {}, {} bytes)\nPreview: {}",
                    e, e.line(), e.column(), output.stdout.len(), preview
                );
            }
        };

        info!(
            edges = index_output.edge_count,
            files = index_output.file_count,
            "IndexStoreDB query complete"
        );

        build_from_edges(&index_output, repo_root, swift_files)
    }
}

impl DataSource for IndexStoreSource {
    fn analyze(
        &self,
        repo_root: &Path,
        swift_files: &[PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        self.run_helper(repo_root, swift_files, None)
    }
}

fn build_from_edges(
    index: &IndexOutput,
    repo_root: &Path,
    swift_files: &[PathBuf],
) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
    // Build file set from the discovered swift files.
    let mut file_set: HashMap<String, PathBuf> = HashMap::new();
    for path in swift_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        file_set.insert(rel, path.clone());
    }

    // Convert edges, filtering to project files only.
    let mut edges = Vec::new();
    for edge in &index.edges {
        if should_skip_file(&edge.from) || should_skip_file(&edge.to) {
            continue;
        }

        // Register files we see in edges even if not in swift_files.
        if !file_set.contains_key(&edge.from) {
            file_set.insert(edge.from.clone(), repo_root.join(&edge.from));
        }
        if !file_set.contains_key(&edge.to) {
            file_set.insert(edge.to.clone(), repo_root.join(&edge.to));
        }

        edges.push(SourceEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            kind: EdgeKind::DirectReference,
        });
    }

    // Build file nodes.
    let nodes: Vec<FileNode> = file_set
        .into_iter()
        .map(|(id, path)| {
            let role = file_classifier::classify_by_path(&path);
            let module = file_classifier::infer_module(std::path::Path::new(&id));
            FileNode {
                id,
                path,
                role,
                module,
                defined_symbols: vec![],
                content_hash: None,
                mtime: None,
                a11y_setters: vec![],
                a11y_queries: vec![],
                test_methods: vec![],
            }
        })
        .collect();

    info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "IndexStoreDB graph built"
    );

    Ok((nodes, edges))
}

fn should_skip_file(path: &str) -> bool {
    path.contains("/DerivedData/")
        || path.contains("/derived-data/")
        || path.contains("/build/")
        || path.contains("/.build/")
        || path.contains("/checkouts/")
        || path.contains("/caches/")
        || path.contains("/Pods/")
        || path.contains("/Carthage/")
        || path.starts_with("/Applications/")
        || path.starts_with("/usr/")
        || path.contains(".swiftmodule/")
        || path.contains(".swiftinterface")
        || path.ends_with(".h")
        || path.ends_with(".m")
        || path.ends_with(".pbobjc.h")
}

fn dirs_or_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}
