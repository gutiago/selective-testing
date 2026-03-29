use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, warn};
use walkdir::WalkDir;

use super::{DataSource, SourceEdge};
use crate::graph::model::{EdgeKind, FileNode};
use crate::swift::file_classifier;

pub struct DotDSource {
    /// Path to DerivedData or build directory containing .d files.
    pub derived_data_path: PathBuf,
}

impl DataSource for DotDSource {
    fn name(&self) -> &str {
        "dot-d"
    }

    fn analyze(
        &self,
        repo_root: &Path,
        _swift_files: &[PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        let d_files = discover_d_files(&self.derived_data_path)?;
        debug!(count = d_files.len(), "Discovered .d files");

        let mut all_files: HashMap<String, PathBuf> = HashMap::new();
        let mut edges = Vec::new();

        for d_file in &d_files {
            match parse_d_file(d_file) {
                Ok(deps) => {
                    // The output file determines the "dependent" (the file being compiled).
                    // The input files are its dependencies.
                    if let Some(output_swift) = find_swift_source(&deps.output, repo_root) {
                        let output_rel = make_relative(&output_swift, repo_root);
                        all_files.insert(output_rel.clone(), output_swift);

                        for input in &deps.inputs {
                            if let Some(input_path) = resolve_to_project_swift(input, repo_root) {
                                let input_rel = make_relative(&input_path, repo_root);
                                if input_rel != output_rel {
                                    all_files
                                        .entry(input_rel.clone())
                                        .or_insert_with(|| input_path);
                                    // Edge: dependency (input) → dependent (output)
                                    edges.push(SourceEdge {
                                        from: input_rel,
                                        to: output_rel.clone(),
                                        kind: EdgeKind::DirectReference,
                                    });
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(file = %d_file.display(), error = %e, "Failed to parse .d file");
                }
            }
        }

        // Build file nodes.
        let nodes: Vec<FileNode> = all_files
            .into_iter()
            .map(|(id, path)| {
                let role = file_classifier::classify_by_path(&path);
                FileNode {
                    id,
                    path,
                    role,
                    module: None,
                    defined_symbols: vec![],
                    content_hash: None,
                    mtime: None,
                }
            })
            .collect();

        debug!(
            nodes = nodes.len(),
            edges = edges.len(),
            ".d file analysis complete"
        );

        Ok((nodes, edges))
    }
}

struct DFileDeps {
    output: String,
    inputs: Vec<String>,
}

/// Parse a makefile-format .d file.
/// Format: `output.o : input1.swift input2.swift ...`
fn parse_d_file(path: &Path) -> Result<DFileDeps> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read .d file: {}", path.display()))?;

    // Join continuation lines (lines ending with \).
    let joined = content.replace("\\\n", " ");

    // Find the first line with a colon separator.
    for line in joined.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(colon_pos) = line.find(" : ") {
            let output = line[..colon_pos].trim().to_string();
            let inputs: Vec<String> = line[colon_pos + 3..]
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            return Ok(DFileDeps { output, inputs });
        }
    }

    anyhow::bail!("No dependency line found in .d file: {}", path.display())
}

/// Discover all .d files in the DerivedData directory.
fn discover_d_files(derived_data: &Path) -> Result<Vec<PathBuf>> {
    let mut d_files = Vec::new();
    for entry in WalkDir::new(derived_data)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path().extension().map(|e| e == "d").unwrap_or(false) {
            d_files.push(entry.into_path());
        }
    }
    Ok(d_files)
}

/// Try to map an output .o path back to its source .swift file.
fn find_swift_source(output: &str, repo_root: &Path) -> Option<PathBuf> {
    // The .o file name often matches the .swift file name.
    let stem = Path::new(output).file_stem()?.to_str()?;
    // Search for a matching .swift file in the repo.
    let swift_name = format!("{}.swift", stem);
    find_file_in_repo(repo_root, &swift_name)
}

/// Resolve a dependency path to a project .swift file (filter out SDK/framework paths).
fn resolve_to_project_swift(input: &str, repo_root: &Path) -> Option<PathBuf> {
    let path = Path::new(input);

    // Only consider .swift files.
    if path.extension()?.to_str()? != "swift" {
        return None;
    }

    // If it's already an absolute path within the repo, use it.
    let repo_str = repo_root.to_string_lossy();
    if input.starts_with(repo_str.as_ref()) {
        return Some(path.to_path_buf());
    }

    // Skip SDK/framework/toolchain paths.
    if input.contains("/Xcode.app/")
        || input.contains("/SDKs/")
        || input.contains("/usr/lib/")
        || input.contains("/.build/")
        || input.contains("/SourcePackages/checkouts/")
    {
        return None;
    }

    // Try to find it relative to repo root.
    let candidate = repo_root.join(input);
    if candidate.exists() {
        return Some(candidate);
    }

    None
}

fn find_file_in_repo(repo_root: &Path, filename: &str) -> Option<PathBuf> {
    WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy() == filename)
        .map(|e| e.into_path())
}

fn make_relative(path: &Path, repo_root: &Path) -> String {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}
