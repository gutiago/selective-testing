mod cli;
mod diff;
mod graph;
mod output;
mod sources;
mod swift;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use petgraph::visit::EdgeRef;
use tracing::{debug, info};

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
            _extra: _,
        } => {
            cmd_resolve(&cli, base, kind, *format)?;
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
    blob_shas: &HashMap<String, String>,
) -> Vec<String> {
    let mut changed = Vec::new();

    for path in swift_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let current_sha = blob_shas.get(&rel);

        match cached.get_node(&rel) {
            Some(node) => {
                // File exists in cache — check if content changed via git blob SHA.
                if node.content_hash.as_deref() != current_sha.map(|s| s.as_str()) {
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

    let blob_shas = diff::git::git_blob_shas(&repo_root).unwrap_or_default();

    // Try incremental update if cached graph exists and not forced.
    if !force {
        if let Some(mut cached_graph) = graph::cache::load(&repo_root)? {
            let changed = find_changed_files(&cached_graph, &swift_files, &repo_root, &blob_shas);
            if changed.is_empty() {
                info!(
                    files = cached_graph.metadata.file_count,
                    edges = cached_graph.metadata.edge_count,
                    "Graph is up to date"
                );
                return Ok(());
            }

            let changed_paths: Vec<PathBuf> = changed
                .iter()
                .map(|id| repo_root.join(id))
                .filter(|p| p.exists())
                .collect();

            // Try incremental IndexStoreDB for changed files.
            if let Ok(source) = resolve_indexstore(&repo_root, index_store.clone(), helper_path.clone()) {
                match source.analyze(&repo_root, &changed_paths) {
                    Ok((mut new_nodes, new_edges)) => {
                        info!(changed = changed.len(), "Incremental update (indexstore)");
                        for node in &mut new_nodes {
                            let rel = node.path.strip_prefix(&repo_root)
                                .unwrap_or(&node.path)
                                .to_string_lossy()
                                .to_string();
                            node.content_hash = blob_shas.get(&rel).cloned();
                        }
                        let changed_ids: Vec<String> = changed;
                        graph::builder::update_graph_incremental(
                            &mut cached_graph,
                            &changed_ids,
                            new_edges,
                            new_nodes,
                        );
                        add_new_file_nodes(&mut cached_graph, &swift_files, &repo_root, &blob_shas);
                        supplement_a11y_edges(&mut cached_graph, &repo_root, &swift_files);
                        cached_graph.update_metadata();
                        graph::cache::save(&cached_graph, &repo_root)?;
                        eprintln!(
                            "Updated {} files, {} edges (incremental, source: indexstore)",
                            cached_graph.metadata.file_count,
                            cached_graph.metadata.edge_count,
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        info!(reason = %e, "Incremental IndexStoreDB failed, trying full IndexStoreDB");
                        // Fall through — will attempt full index below.
                    }
                }
            }

            // Incremental IndexStoreDB failed — try full IndexStoreDB.
            if let Ok(source) = resolve_indexstore(&repo_root, index_store.clone(), helper_path.clone()) {
                if let Ok((nodes, edges)) = source.analyze(&repo_root, &swift_files) {
                    info!("Full IndexStoreDB re-index (incremental failed)");
                    // Don't use the cache — replace with fresh IndexStoreDB graph.
                    // Jump to the full index path below.
                    let mut g = graph::builder::build_graph(&repo_root, nodes, edges)?;
                    g.metadata.data_sources_used.push("indexstore".to_string());
                    return finish_full_index(g, &swift_files, &repo_root, derived_data, &blob_shas);
                }
            }

            // IndexStoreDB fully unavailable — fall back to tree-sitter cache.
            info!("IndexStoreDB unavailable, using tree-sitter incremental on cache");
            let ts = sources::treesitter::TreeSitterSource;
            let (mut new_nodes, new_edges) = ts.analyze(&repo_root, &changed_paths)?;
            for node in &mut new_nodes {
                let rel = node.path.strip_prefix(&repo_root)
                    .unwrap_or(&node.path)
                    .to_string_lossy()
                    .to_string();
                node.content_hash = blob_shas.get(&rel).cloned();
            }
            let changed_ids: Vec<String> = changed;
            graph::builder::update_graph_incremental(
                &mut cached_graph,
                &changed_ids,
                new_edges,
                new_nodes,
            );
            add_new_file_nodes(&mut cached_graph, &swift_files, &repo_root, &blob_shas);
            cached_graph.update_metadata();
            graph::cache::save(&cached_graph, &repo_root)?;
            eprintln!(
                "Updated {} files, {} edges (incremental, source: tree-sitter)",
                cached_graph.metadata.file_count,
                cached_graph.metadata.edge_count,
            );
            return Ok(());
        }
    }

    // Full index — try IndexStoreDB first, fall back to tree-sitter.
    let dep_graph = match resolve_indexstore(&repo_root, index_store, helper_path)
        .and_then(|source| source.analyze(&repo_root, &swift_files))
    {
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

    finish_full_index(dep_graph, &swift_files, &repo_root, derived_data, &blob_shas)
}

/// Store blob SHAs, supplement with a11y edges and .d files, save cache.
fn finish_full_index(
    mut dep_graph: graph::model::DependencyGraph,
    swift_files: &[PathBuf],
    repo_root: &Path,
    derived_data: Option<PathBuf>,
    blob_shas: &HashMap<String, String>,
) -> Result<()> {
    // Store git blob SHAs for incremental change detection.
    for path in swift_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if let Some(&idx) = dep_graph.file_index.get(&rel) {
            dep_graph.graph[idx].content_hash = blob_shas.get(&rel).cloned();
        }
    }

    supplement_a11y_edges(&mut dep_graph, repo_root, swift_files);

    // If DerivedData is available, supplement with .d files.
    if let Some(dd_path) = derived_data {
        info!(path = %dd_path.display(), "Supplementing with .d file data");
        let dd_source = sources::dot_d::DotDSource {
            derived_data_path: dd_path,
        };
        if let Ok((dd_nodes, dd_edges)) = dd_source.analyze(repo_root, swift_files) {
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
    graph::cache::save(&dep_graph, repo_root)?;

    eprintln!(
        "Indexed {} files, {} edges (sources: {:?})",
        dep_graph.metadata.file_count,
        dep_graph.metadata.edge_count,
        dep_graph.metadata.data_sources_used
    );

    Ok(())
}

/// Supplement an IndexStoreDB graph with AccessibilityBinding + ViewEmbedding edges via tree-sitter.
/// IndexStoreDB only produces DirectReference edges; a11y extraction and view-body
/// detection require tree-sitter.
fn supplement_a11y_edges(
    dep_graph: &mut graph::model::DependencyGraph,
    repo_root: &Path,
    swift_files: &[PathBuf],
) {
    if !dep_graph.metadata.data_sources_used.contains(&"indexstore".to_string()) {
        return;
    }

    info!("Supplementing IndexStoreDB graph with a11y + view-embedding edges via tree-sitter");

    // Remove existing a11y and view-embedding edges before re-supplementing
    // to avoid duplicates on incremental updates.
    dep_graph.remove_edges_by_kind(graph::model::EdgeKind::AccessibilityBinding);
    dep_graph.remove_edges_by_kind(graph::model::EdgeKind::ViewEmbedding);

    let ts_source = sources::treesitter::TreeSitterSource;
    if let Ok((ts_nodes, ts_edges)) = ts_source.analyze(repo_root, swift_files) {
        // Patch FileNodes with a11y data (setters, queries, test_methods).
        for node in ts_nodes {
            if let Some(&idx) = dep_graph.file_index.get(&node.id) {
                dep_graph.graph[idx].a11y_setters = node.a11y_setters;
                dep_graph.graph[idx].a11y_queries = node.a11y_queries;
                dep_graph.graph[idx].test_methods = node.test_methods;
            }
        }
        // Add AccessibilityBinding and ViewEmbedding edges.
        // ViewEmbedding edges allow unlimited depth traversal through SwiftUI view
        // hierarchies — without them, deep view trees hit the depth-2 DirectReference
        // limit and miss UI tests connected via a11y bindings.
        for edge in &ts_edges {
            if (edge.kind == graph::model::EdgeKind::AccessibilityBinding
                || edge.kind == graph::model::EdgeKind::ViewEmbedding)
                && dep_graph.file_index.contains_key(&edge.from)
                && dep_graph.file_index.contains_key(&edge.to)
            {
                dep_graph.add_edge(&edge.from, &edge.to, edge.kind);
            }
        }
        if !dep_graph.metadata.data_sources_used.contains(&"treesitter-supplement".to_string()) {
            dep_graph.metadata.data_sources_used.push("treesitter-supplement".to_string());
        }
    }
}

/// Add nodes for new files not yet in the graph.
fn add_new_file_nodes(
    graph: &mut graph::model::DependencyGraph,
    swift_files: &[PathBuf],
    repo_root: &Path,
    blob_shas: &HashMap<String, String>,
) {
    for path in swift_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if !graph.file_index.contains_key(&rel) {
            let role = swift::file_classifier::classify_by_path(path);
            let module = swift::file_classifier::infer_module(Path::new(&rel));
            graph.ensure_node(graph::model::FileNode {
                id: rel.clone(),
                path: path.clone(),
                role,
                module,
                defined_symbols: vec![],
                content_hash: blob_shas.get(&rel).cloned(),
                mtime: None,
                a11y_setters: vec![],
                a11y_queries: vec![],
                test_methods: vec![],
            });
        }
    }
}

/// Resolve the IndexStoreDB source without running analysis.
fn resolve_indexstore(
    repo_root: &Path,
    index_store: Option<PathBuf>,
    helper_path: Option<PathBuf>,
) -> Result<sources::indexstore::IndexStoreSource> {
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
    if let Some(store_path) = index_store {
        Ok(sources::indexstore::IndexStoreSource {
            helper_path: helper,
            store_path,
            db_path: repo_root.join(".selective-testing/indexstore-db"),
        })
    } else {
        sources::indexstore::IndexStoreSource::detect(repo_root, helper)
            .ok_or_else(|| anyhow::anyhow!("Could not auto-detect index store location"))
    }
}

fn cmd_resolve(
    cli: &Cli,
    base: &str,
    kinds: &[graph::model::TestKind],
    format: cli::args::OutputFormat,
) -> Result<()> {
    let repo_root = resolve_repo_root(cli)?;

    // Load the graph.
    let dep_graph = graph::cache::load(&repo_root)?
        .context("No cached graph found. Run `selective-testing index` first.")?;

    debug!(sources = ?dep_graph.metadata.data_sources_used, "Graph data sources");

    // Get changed files.
    let changed_files = diff::git::changed_swift_files(&repo_root, base)?;
    if changed_files.is_empty() {
        info!("No Swift files changed");
        return Ok(());
    }

    for f in &changed_files {
        let in_graph = dep_graph.file_index.contains_key(f);
        debug!(file = %f, in_graph = in_graph, "Changed file");
    }

    // Resolve all affected tests in a single pass.
    let result = graph::traversal::resolve_affected_tests(&dep_graph, &changed_files, kinds);

    info!(
        affected = result.total_count(),
        visited = result.files_visited,
        "Resolution complete"
    );

    let formatted = output::format_result(&result, format);
    if !formatted.is_empty() {
        println!("{}", formatted);
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

                use petgraph::Direction;
                println!("\nDependencies (files this depends on):");
                for edge in dep_graph.graph.edges_directed(idx, Direction::Incoming) {
                    let source = &dep_graph.graph[edge.source()];
                    println!("  ← {} ({:?})", source.id, edge.weight().kind);
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
