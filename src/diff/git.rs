use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository, StatusOptions};
use tracing::info;

/// Discover the git repository root from a given path.
pub fn find_repo_root(from: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(from).context("Failed to find git repository")?;
    let workdir = repo
        .workdir()
        .context("Repository has no working directory (bare repo)")?;
    Ok(workdir.to_path_buf())
}

/// Get the current HEAD commit hash.
pub fn head_commit_hash(repo_root: &Path) -> Result<String> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;
    let head = repo.head().context("Failed to get HEAD")?;
    let commit = head.peel_to_commit().context("HEAD is not a commit")?;
    Ok(commit.id().to_string())
}

/// Discover all tracked .swift files using git ls-files.
/// Much faster than walkdir since git already has the file index.
pub fn tracked_swift_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;

    let mut status_opts = StatusOptions::new();
    status_opts.include_untracked(false);

    // Use the index to list all tracked files.
    let index = repo.index().context("Failed to read git index")?;
    let mut files = Vec::new();

    for entry in index.iter() {
        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        if path_str.ends_with(".swift") && !should_skip(&path_str) {
            files.push(repo_root.join(&path_str));
        }
    }

    info!(count = files.len(), "Discovered Swift files from git index");
    Ok(files)
}

/// Get changed .swift files between a cached commit and the current working tree.
/// Used for incremental graph updates.
pub fn changed_swift_files_since(repo_root: &Path, since_commit: &str) -> Result<Vec<String>> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;

    let old_obj = repo
        .revparse_single(since_commit)
        .with_context(|| format!("Failed to resolve commit: {}", since_commit))?;
    let old_commit = old_obj
        .peel_to_commit()
        .with_context(|| format!("'{}' is not a commit", since_commit))?;
    let old_tree = old_commit.tree().context("Failed to get tree")?;

    // Diff cached commit against working directory (catches both committed and uncommitted changes).
    let mut diff_opts = DiffOptions::new();
    diff_opts.include_untracked(false);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&old_tree), Some(&mut diff_opts))
        .context("Failed to compute diff")?;

    let mut changed = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    let s = path.to_string_lossy().to_string();
                    if s.ends_with(".swift") && !should_skip(&s) && !changed.contains(&s) {
                        changed.push(s);
                    }
                }
            }
            true
        },
        None,
        None,
        None,
    )
    .context("Failed to iterate diff")?;

    info!(count = changed.len(), since = since_commit, "Changed Swift files since cached commit");
    Ok(changed)
}

/// Get changed .swift files between HEAD and a base ref (for resolve).
/// Uses merge-base (three-dot diff) and includes working directory changes.
pub fn changed_swift_files(repo_root: &Path, base_ref: &str) -> Result<Vec<String>> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;

    let base_obj = repo
        .revparse_single(base_ref)
        .with_context(|| format!("Failed to resolve git ref: {}", base_ref))?;
    let base_commit = base_obj
        .peel_to_commit()
        .with_context(|| format!("Ref '{}' does not point to a commit", base_ref))?;

    let head_ref = repo.head().context("Failed to get HEAD")?;
    let head_commit = head_ref
        .peel_to_commit()
        .context("HEAD does not point to a commit")?;

    let merge_base = repo
        .merge_base(base_commit.id(), head_commit.id())
        .with_context(|| format!("Failed to find merge-base between {} and HEAD", base_ref))?;
    let merge_base_commit = repo
        .find_commit(merge_base)
        .context("Failed to find merge-base commit")?;
    let merge_base_tree = merge_base_commit
        .tree()
        .context("Failed to get tree from merge-base commit")?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.include_untracked(false);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&merge_base_tree), Some(&mut diff_opts))
        .context("Failed to compute diff")?;

    let mut changed_files = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    let s = path.to_string_lossy().to_string();
                    if s.ends_with(".swift") && !changed_files.contains(&s) {
                        changed_files.push(s);
                    }
                }
            }
            true
        },
        None,
        None,
        None,
    )
    .context("Failed to iterate diff")?;

    info!(count = changed_files.len(), base = base_ref, "Changed Swift files detected");
    Ok(changed_files)
}

fn should_skip(path: &str) -> bool {
    path.contains("/DerivedData/")
        || path.contains("/derived-data/")
        || path.contains("/build/")
        || path.contains("/.build/")
        || path.contains("/Pods/")
        || path.contains("/Carthage/")
        || path.contains("/SourcePackages/checkouts/")
        || path.contains("/checkouts/")
        || path.contains("/caches/")
        || path.contains("/.swiftpm/")
}
