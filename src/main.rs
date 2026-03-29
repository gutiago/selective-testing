mod cli;
mod diff;
mod graph;
mod output;
mod sources;
mod swift;
mod xcode;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use petgraph::visit::EdgeRef;
use tracing::info;

use cli::args::{Cli, Command};
use sources::DataSource;
use walkdir::WalkDir;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up tracing/logging based on verbosity.
    let filter = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match &cli.command {
        Command::Index {
            force,
            derived_data,
            index_store,
            helper_path,
        } => {
            cmd_index(
                &cli,
                *force,
                derived_data.clone(),
                index_store.clone(),
                helper_path.clone(),
            )?;
        }
        Command::Resolve {
            base,
            kind,
            format,
            test_plan,
            dry_run,
        } => {
            cmd_resolve(&cli, base, kind, *format, test_plan.clone(), *dry_run)?;
        }
        Command::Graph { file, cycles, dot } => {
            cmd_graph(&cli, file.clone(), *cycles, *dot)?;
        }
        Command::Verify {
            full_results,
            diff_range,
        } => {
            cmd_verify(&cli, full_results, diff_range)?;
        }
    }

    Ok(())
}

fn resolve_repo_root(cli: &Cli) -> Result<PathBuf> {
    if let Some(ref root) = cli.repo_root {
        std::fs::canonicalize(root)
            .with_context(|| format!("Failed to resolve repo root: {}", root.display()))
    } else {
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        diff::git::find_repo_root(&cwd)
    }
}

/// Compare file mtimes against cached graph to find changed/new files.
fn find_changed_files(
    cached: &graph::model::DependencyGraph,
    swift_files: &[PathBuf],
    repo_root: &Path,
) -> Vec<String> {
    let mut changed = Vec::new();

    for path in swift_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let current_mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        match cached.get_node(&rel) {
            Some(node) => {
                // File exists in cache — check if mtime changed.
                if node.mtime != current_mtime {
                    changed.push(rel);
                }
            }
            None => {
                // New file not in cache.
                changed.push(rel);
            }
        }
    }

    changed
}

/// Discover .swift files using git index (fast) with walkdir fallback.
fn discover_swift_files(repo_root: &Path) -> Vec<PathBuf> {
    match diff::git::tracked_swift_files(repo_root) {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(reason = %e, "git index unavailable, falling back to walkdir");
            WalkDir::new(repo_root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "swift")
                        .unwrap_or(false)
                })
                .filter(|e| {
                    let path = e.path().to_string_lossy();
                    !path.contains("/DerivedData/")
                        && !path.contains("/derived-data/")
                        && !path.contains("/build/")
                        && !path.contains("/.build/")
                        && !path.contains("/Pods/")
                        && !path.contains("/Carthage/")
                        && !path.contains("/checkouts/")
                        && !path.contains("/caches/")
                        && !path.contains("/.swiftpm/")
                })
                .map(|e| e.into_path())
                .collect()
        }
    }
}

fn cmd_index(
    cli: &Cli,
    force: bool,
    derived_data: Option<PathBuf>,
    index_store: Option<PathBuf>,
    helper_path: Option<PathBuf>,
) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;
    info!(repo = %repo_root.display(), "Indexing project");

    let swift_files = discover_swift_files(&repo_root);
    info!(count = swift_files.len(), "Discovered Swift files");

    // Try incremental update if cached graph exists and not forced.
    if !force {
        if let Some(mut cached_graph) = graph::cache::load(&repo_root)? {
            let changed = find_changed_files(&cached_graph, &swift_files, &repo_root);
            if changed.is_empty() {
                info!(
                    files = cached_graph.metadata.file_count,
                    edges = cached_graph.metadata.edge_count,
                    "Graph is up to date"
                );
                return Ok(());
            }

            info!(
                changed = changed.len(),
                "Incremental update: re-indexing changed files with tree-sitter"
            );

            // Re-parse only changed files with tree-sitter.
            let changed_paths: Vec<PathBuf> = changed
                .iter()
                .map(|id| repo_root.join(id))
                .filter(|p| p.exists())
                .collect();

            let ts_source = sources::treesitter::TreeSitterSource;
            if let Ok((mut new_nodes, new_edges)) = ts_source.analyze(&repo_root, &changed_paths) {
                // Set current mtimes on updated nodes.
                for node in &mut new_nodes {
                    node.mtime = std::fs::metadata(&node.path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                }
                let changed_ids: Vec<String> = changed;
                graph::builder::update_graph_incremental(
                    &mut cached_graph,
                    &changed_ids,
                    new_edges,
                    new_nodes,
                );
            }

            // Add nodes for any new files not yet in the graph.
            for path in &swift_files {
                let rel = path
                    .strip_prefix(&repo_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                if !cached_graph.file_index.contains_key(&rel) {
                    let role = swift::file_classifier::classify_by_path(path);
                    let mtime = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    cached_graph.ensure_node(graph::model::FileNode {
                        id: rel,
                        path: path.clone(),
                        role,
                        module: None,
                        defined_symbols: vec![],
                        content_hash: None,
                        mtime,
                    });
                }
            }

            cached_graph.update_metadata();
            graph::cache::save(&cached_graph, &repo_root)?;

            eprintln!(
                "Updated {} files, {} edges (incremental)",
                cached_graph.metadata.file_count,
                cached_graph.metadata.edge_count,
            );
            return Ok(());
        }
    }

    // Full index — try IndexStoreDB first, fall back to tree-sitter.
    let indexstore_result = try_indexstore(&repo_root, &swift_files, index_store, helper_path);

    let mut dep_graph = match indexstore_result {
        Ok((nodes, edges)) => {
            let mut g = graph::builder::build_graph(&repo_root, nodes, edges)?;
            g.metadata.data_sources_used.push("indexstore".to_string());
            g
        }
        Err(e) => {
            info!(reason = %e, "IndexStoreDB unavailable, falling back to tree-sitter");
            let ts_source = sources::treesitter::TreeSitterSource;
            let (nodes, edges) = ts_source
                .analyze(&repo_root, &swift_files)
                .context("tree-sitter analysis failed")?;
            let mut g = graph::builder::build_graph(&repo_root, nodes, edges)?;
            g.metadata.data_sources_used.push("tree-sitter".to_string());
            g
        }
    };

    // Store mtimes for incremental updates.
    for path in &swift_files {
        let rel = path
            .strip_prefix(&repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if let Some(&idx) = dep_graph.file_index.get(&rel) {
            let mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            dep_graph.graph[idx].mtime = mtime;
        }
    }

    // If DerivedData is available, supplement with .d files.
    if let Some(dd_path) = derived_data {
        info!(path = %dd_path.display(), "Supplementing with .d file data");
        let dd_source = sources::dot_d::DotDSource {
            derived_data_path: dd_path,
        };
        if let Ok((dd_nodes, dd_edges)) = dd_source.analyze(&repo_root, &swift_files) {
            for node in dd_nodes {
                dep_graph.ensure_node(node);
            }
            for edge in &dd_edges {
                if dep_graph.file_index.contains_key(&edge.from)
                    && dep_graph.file_index.contains_key(&edge.to)
                {
                    dep_graph.add_edge(&edge.from, &edge.to, edge.kind);
                }
            }
            dep_graph
                .metadata
                .data_sources_used
                .push("dot-d".to_string());
        }
    }

    dep_graph.update_metadata();
    graph::cache::save(&dep_graph, &repo_root)?;

    eprintln!(
        "Indexed {} files, {} edges (sources: {:?})",
        dep_graph.metadata.file_count,
        dep_graph.metadata.edge_count,
        dep_graph.metadata.data_sources_used
    );

    Ok(())
}

fn try_indexstore(
    repo_root: &Path,
    swift_files: &[PathBuf],
    index_store: Option<PathBuf>,
    helper_path: Option<PathBuf>,
) -> Result<(Vec<graph::model::FileNode>, Vec<sources::SourceEdge>)> {
    // Resolve the helper binary path.
    let helper = helper_path.unwrap_or_else(|| {
        // Look for index-helper next to the selective-testing binary.
        let self_path = std::env::current_exe().unwrap_or_default();
        let self_dir = self_path.parent().unwrap_or(Path::new("."));
        self_dir.join("index-helper")
    });

    if !helper.exists() {
        anyhow::bail!(
            "index-helper not found at: {}. Build it with: cd swift-helper && swift build -c release",
            helper.display()
        );
    }

    // Resolve the index store path.
    let source = if let Some(store_path) = index_store {
        sources::indexstore::IndexStoreSource {
            helper_path: helper,
            store_path,
            db_path: repo_root.join(".selective-testing/indexstore-db"),
        }
    } else {
        sources::indexstore::IndexStoreSource::detect(repo_root, helper)
            .ok_or_else(|| anyhow::anyhow!("Could not auto-detect index store location"))?
    };

    source.analyze(repo_root, swift_files)
}

fn cmd_resolve(
    cli: &Cli,
    base: &str,
    kinds: &[graph::model::TestKind],
    format: cli::args::OutputFormat,
    test_plan: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;

    // Load the graph.
    let dep_graph = graph::cache::load(&repo_root)?
        .context("No cached graph found. Run `selective-testing index` first.")?;

    // Get changed files.
    let changed_files = diff::git::changed_swift_files(&repo_root, base)?;
    if changed_files.is_empty() {
        info!("No Swift files changed");
        return Ok(());
    }

    // Resolve all affected tests in a single pass.
    let result = graph::traversal::resolve_affected_tests(&dep_graph, &changed_files, kinds);

    info!(
        affected = result.total_count(),
        visited = result.files_visited,
        "Resolution complete"
    );

    // Optionally filter by test plan targets (and modify the plan unless --dry-run).
    if let Some(plan_path) = test_plan {
        let mut plan = xcode::testplan::read(&plan_path)?;
        let affected_files: Vec<String> = result
            .all_tests()
            .iter()
            .map(|t| t.file_id.clone())
            .collect();
        let disabled = xcode::testplan::disable_unaffected_targets(&mut plan, &affected_files);

        if !dry_run {
            xcode::testplan::write(&plan, &plan_path)?;
        }

        // Collect enabled targets as (name, container_path) pairs for filtering and xcodebuild output.
        let enabled_targets: Vec<(String, String)> = plan
            .test_targets
            .iter()
            .filter(|t| t.enabled != Some(false))
            .map(|t| {
                let path = t
                    .target
                    .container_path
                    .strip_prefix("container:")
                    .unwrap_or(&t.target.container_path)
                    .to_string();
                (t.target.name.clone(), path)
            })
            .collect();

        // Filter result to only show files belonging to enabled targets.
        let filtered = result.filter_by_targets(&enabled_targets);

        let formatted = output::format_result(&filtered, format);
        if !formatted.is_empty() {
            println!("{}", formatted);
        }

        info!(
            enabled_targets = plan.test_targets.len() - disabled.len(),
            disabled_targets = disabled.len(),
            dry_run = dry_run,
            "Test plan targets resolved"
        );
    } else {
        let formatted = output::format_result(&result, format);
        if !formatted.is_empty() {
            println!("{}", formatted);
        }
    }

    Ok(())
}

fn cmd_graph(
    cli: &Cli,
    file: Option<PathBuf>,
    cycles: bool,
    _dot: bool,
) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;
    let dep_graph = graph::cache::load(&repo_root)?
        .context("No cached graph found. Run `selective-testing index` first.")?;

    if let Some(ref file_path) = file {
        let file_id = file_path.to_string_lossy().to_string();
        if let Some(node) = dep_graph.get_node(&file_id) {
            println!("File: {}", node.id);
            println!("Role: {:?}", node.role);
            println!("Symbols: {:?}", node.defined_symbols);

            if let Some(&idx) = dep_graph.file_index.get(&file_id) {
                println!("\nDependents (files that depend on this):");
                for edge in dep_graph.graph.edges(idx) {
                    let target = &dep_graph.graph[edge.target()];
                    println!("  → {} ({:?})", target.id, edge.weight().kind);
                }
            }
        } else {
            println!("File not found in graph: {}", file_id);
        }
    }

    if cycles {
        let report = graph::cycles::detect_cycles(&dep_graph);
        if report.cycles.is_empty() {
            println!("No dependency cycles detected.");
        } else {
            println!("Found {} dependency cycles:", report.cycles.len());
            for (i, cycle) in report.cycles.iter().enumerate() {
                println!("\n  Cycle {} ({} files):", i + 1, cycle.len());
                for file in cycle.iter().take(10) {
                    println!("    {}", file);
                }
                if cycle.len() > 10 {
                    println!("    ... and {} more", cycle.len() - 10);
                }
            }
            for warning in &report.warnings {
                eprintln!("\n⚠ {}", warning);
            }
        }
    }

    if file.is_none() && !cycles {
        println!(
            "Graph: {} files, {} edges",
            dep_graph.metadata.file_count, dep_graph.metadata.edge_count
        );
        println!("Sources: {:?}", dep_graph.metadata.data_sources_used);
    }

    Ok(())
}

fn cmd_verify(
    _cli: &Cli,
    _full_results: &PathBuf,
    _diff_range: &str,
) -> Result<()> {
    anyhow::bail!("verify command is not yet implemented")
}
