use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use tracing::info;

/// Discover the git repository root from a given path.
pub fn find_repo_root(from: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(from).context("Failed to find git repository")?;
    let workdir = repo
        .workdir()
        .context("Repository has no working directory (bare repo)")?;
    Ok(workdir.to_path_buf())
}

/// Discover all tracked .swift files using git ls-files.
/// Much faster than walkdir since git already has the file index.
pub fn tracked_swift_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;

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

    let merge_base_oid = match repo.merge_base(base_commit.id(), head_commit.id()) {
        Ok(oid) => oid,
        Err(_) if repo.is_shallow() && std::env::var_os("CI").is_some() => {
            info!("Shallow repository detected on CI, fetching changed files from GitHub API");
            return changed_swift_files_from_github(repo_root, base_ref);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to find merge-base between {} and HEAD", base_ref)
            });
        }
    };

    changed_swift_files_with_merge_base(&repo, repo_root, base_ref, merge_base_oid)

}

fn changed_swift_files_with_merge_base(
    repo: &Repository,
    _repo_root: &Path,
    base_ref: &str,
    merge_base_oid: git2::Oid,
) -> Result<Vec<String>> {
    let merge_base_commit = repo
        .find_commit(merge_base_oid)
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

/// Fetch changed files via GitHub compare API using curl.
/// Used as a fallback on CI when the local clone is too shallow for merge-base.
fn changed_swift_files_from_github(repo_root: &Path, base_ref: &str) -> Result<Vec<String>> {
    let remote_output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .context("Failed to get git remote URL")?;
    let remote_url = String::from_utf8_lossy(&remote_output.stdout).trim().to_string();
    let repo_slug = parse_github_repo_slug(&remote_url)
        .context("Failed to parse GitHub owner/repo from remote URL")?;

    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("Failed to get HEAD sha")?;
    let head_sha = String::from_utf8_lossy(&head_output.stdout).trim().to_string();

    let base = base_ref.strip_prefix("origin/").unwrap_or(base_ref);
    let url = format!(
        "https://api.github.com/repos/{}/compare/{}...{}",
        repo_slug, base, head_sha
    );

    let mut args = vec![
        "-sfL".to_string(),
        "-H".to_string(), "Accept: application/vnd.github+json".to_string(),
    ];
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        args.push("-H".to_string());
        args.push(format!("Authorization: Bearer {}", token));
    }
    args.push(url);

    let output = Command::new("curl")
        .args(&args)
        .output()
        .context("Failed to run curl for GitHub compare API")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GitHub compare API request failed: {}", stderr);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Failed to parse GitHub compare API response")?;

    let changed_files: Vec<String> = json["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|f| f["filename"].as_str())
                .filter(|f| f.ends_with(".swift"))
                .map(|f| f.to_string())
                .collect()
        })
        .unwrap_or_default();

    info!(count = changed_files.len(), base = base_ref, "Changed Swift files detected (via GitHub API)");
    Ok(changed_files)
}

/// Extract "owner/repo" from a GitHub remote URL.
/// Handles SSH (git@github.com:owner/repo.git) and HTTPS (https://github.com/owner/repo.git).
fn parse_github_repo_slug(url: &str) -> Option<String> {
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else {
        return None;
    };
    Some(path.trim_end_matches(".git").to_string())
}

/// Return a map of relative path → git blob SHA hex for all tracked .swift files.
/// Uses the git index directly so it is unaffected by filesystem mtime changes.
pub fn git_blob_shas(repo_root: &Path) -> Result<HashMap<String, String>> {
    let repo = Repository::open(repo_root).context("Failed to open git repository")?;
    let index = repo.index().context("Failed to read git index")?;
    let mut shas = HashMap::new();
    for entry in index.iter() {
        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        if path_str.ends_with(".swift") && !should_skip(&path_str) {
            shas.insert(path_str, entry.id.to_string());
        }
    }
    Ok(shas)
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
