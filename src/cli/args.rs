use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::graph::model::TestKind;

#[derive(Parser)]
#[command(
    name = "selective-testing",
    version,
    about = "Test Impact Analysis for Swift/Xcode projects"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to the Xcode project (.xcodeproj) or workspace (.xcworkspace)
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,

    /// Path to repo root (defaults to git root discovery)
    #[arg(long, global = true)]
    pub repo_root: Option<PathBuf>,

    /// Verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build or update the dependency graph
    Index {
        /// Force full re-index (ignore cache)
        #[arg(long)]
        force: bool,

        /// Path to DerivedData for .d file parsing
        #[arg(long)]
        derived_data: Option<PathBuf>,

        /// Path to Index.noindex/DataStore for IndexStoreDB
        #[arg(long)]
        index_store: Option<PathBuf>,

        /// Path to the index-helper Swift binary
        #[arg(long)]
        helper_path: Option<PathBuf>,
    },

    /// Find affected tests for a given diff
    #[command(trailing_var_arg = true)]
    Resolve {
        /// Git ref to diff against (default: origin/master)
        #[arg(long, default_value = "origin/master")]
        base: String,

        /// Filter by test kind (can be specified multiple times: --kind unit --kind snapshot)
        #[arg(long, value_enum)]
        kind: Vec<TestKind>,

        /// Output format
        #[arg(long, value_enum, default_value = "list")]
        format: OutputFormat,

        /// Ignored (accepts removed flags for backwards compatibility)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        _extra: Vec<String>,
    },

    /// Inspect the dependency graph
    Graph {
        /// Show dependencies for a specific file
        #[arg(long)]
        file: Option<PathBuf>,

        /// Detect and report cycles
        #[arg(long)]
        cycles: bool,

        /// Export graph as DOT format
        #[arg(long)]
        dot: bool,
    },

    /// Compare selective results against a full test run
    Verify {
        /// Path to full test run .xcresult
        #[arg(long)]
        full_results: PathBuf,

        /// Git diff range to verify against
        #[arg(long)]
        diff_range: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    /// One test per line
    List,
    /// JSON array
    Json,
    /// -only-testing flags for xcodebuild
    Xcodebuild,
}
