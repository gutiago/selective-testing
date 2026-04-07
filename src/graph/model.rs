use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Canonical relative path from repo root, used as unique node identifier.
pub type FileId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileRole {
    Source,
    UnitTest,
    SnapshotTest,
    UITest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum TestKind {
    Unit,
    Snapshot,
    #[value(alias = "ui")]
    UITest,
}

impl TestKind {
    /// Which edge kinds this test kind follows during graph traversal.
    pub fn allowed_edges(&self) -> &[EdgeKind] {
        match self {
            TestKind::Unit => &[EdgeKind::DirectReference],
            TestKind::Snapshot => &[EdgeKind::DirectReference, EdgeKind::ViewEmbedding],
            TestKind::UITest => &[
                EdgeKind::DirectReference,
                EdgeKind::ViewEmbedding,
                EdgeKind::AccessibilityBinding,
            ],
        }
    }
}

impl FileRole {
    pub fn test_kind(&self) -> Option<TestKind> {
        match self {
            FileRole::UnitTest => Some(TestKind::Unit),
            FileRole::SnapshotTest => Some(TestKind::Snapshot),
            FileRole::UITest => Some(TestKind::UITest),
            FileRole::Source => None,
        }
    }

    pub fn is_test(&self) -> bool {
        !matches!(self, FileRole::Source)
    }
}

/// An accessibility identifier — either a string literal or a symbolic path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum A11yIdentifier {
    /// A raw string literal: "login_button"
    Literal(String),
    /// A symbolic dotted path: "AccessibilityIdentifiers.Login.loginButton"
    Symbolic(String),
}

impl A11yIdentifier {
    /// The string key used for matching (the literal value or symbolic path).
    pub fn key(&self) -> &str {
        match self {
            A11yIdentifier::Literal(s) | A11yIdentifier::Symbolic(s) => s.as_str(),
        }
    }
}

/// Per-method a11y data extracted from a UI test file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestMethodInfo {
    /// Method name (e.g., "testLogin").
    pub name: String,
    /// A11y IDs directly queried within this method body.
    pub a11y_queries: Vec<A11yIdentifier>,
    /// Type names referenced in this method (for page object resolution).
    pub referenced_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileNode {
    pub id: FileId,
    pub path: PathBuf,
    pub role: FileRole,
    pub module: Option<String>,
    pub defined_symbols: Vec<String>,
    pub content_hash: Option<String>,
    pub mtime: Option<u64>,
    /// A11y identifiers this file SETS via .accessibilityIdentifier(...) (source/view files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a11y_setters: Vec<A11yIdentifier>,
    /// A11y identifiers this file QUERIES via subscript (page objects and UI test files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a11y_queries: Vec<A11yIdentifier>,
    /// Per-method a11y data (only populated for UITest files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_methods: Vec<TestMethodInfo>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    DirectReference,
    ViewEmbedding,
    /// Connects a source file that sets an a11y ID to the file that queries it.
    AccessibilityBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdge {
    pub kind: EdgeKind,
}

/// The core dependency graph.
/// Edge direction: dependency → dependent ("is used by").
/// Traversal from a changed file follows outgoing edges to find all affected consumers.
#[derive(Debug, Serialize, Deserialize)]
pub struct DependencyGraph {
    pub graph: DiGraph<FileNode, FileEdge>,
    pub file_index: HashMap<FileId, NodeIndex>,
    pub metadata: GraphMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphMetadata {
    pub repo_root: PathBuf,
    pub indexed_at: String,
    pub file_count: usize,
    pub edge_count: usize,
    pub data_sources_used: Vec<String>,
}

impl DependencyGraph {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            graph: DiGraph::new(),
            file_index: HashMap::new(),
            metadata: GraphMetadata {
                repo_root,
                indexed_at: String::new(),
                file_count: 0,
                edge_count: 0,
                data_sources_used: Vec::new(),
            },
        }
    }

    /// Get or create a node for the given file.
    pub fn ensure_node(&mut self, file: FileNode) -> NodeIndex {
        if let Some(&idx) = self.file_index.get(&file.id) {
            return idx;
        }
        let id = file.id.clone();
        let idx = self.graph.add_node(file);
        self.file_index.insert(id, idx);
        idx
    }

    /// Add an edge from dependency to dependent.
    pub fn add_edge(&mut self, from: &FileId, to: &FileId, kind: EdgeKind) {
        if let (Some(&from_idx), Some(&to_idx)) = (self.file_index.get(from), self.file_index.get(to))
        {
            self.graph.add_edge(from_idx, to_idx, FileEdge { kind });
        }
    }

    /// Look up a node by file ID.
    pub fn get_node(&self, id: &str) -> Option<&FileNode> {
        self.file_index
            .get(id)
            .map(|&idx| &self.graph[idx])
    }

    /// Remove all edges originating from a node (for incremental re-index).
    pub fn remove_edges_from(&mut self, id: &FileId) {
        if let Some(&idx) = self.file_index.get(id) {
            let edges_to_remove: Vec<_> = self
                .graph
                .edges(idx)
                .map(|e| e.id())
                .collect();
            for edge_id in edges_to_remove {
                self.graph.remove_edge(edge_id);
            }
        }
    }

    /// Remove all AccessibilityBinding edges that point TO the given node.
    /// Used during incremental updates when a UI test or page object file changes.
    pub fn remove_a11y_edges_to(&mut self, id: &FileId) {
        if let Some(&target_idx) = self.file_index.get(id) {
            use petgraph::Direction;
            let edges_to_remove: Vec<_> = self
                .graph
                .edges_directed(target_idx, Direction::Incoming)
                .filter(|e| e.weight().kind == EdgeKind::AccessibilityBinding)
                .map(|e| e.id())
                .collect();
            for edge_id in edges_to_remove {
                self.graph.remove_edge(edge_id);
            }
        }
    }

    /// Remove all edges of a given kind from the entire graph.
    pub fn remove_edges_by_kind(&mut self, kind: EdgeKind) {
        let edges_to_remove: Vec<_> = self
            .graph
            .edge_indices()
            .filter(|&e| self.graph[e].kind == kind)
            .collect();
        for edge_id in edges_to_remove {
            self.graph.remove_edge(edge_id);
        }
    }

    /// Update metadata counts.
    pub fn update_metadata(&mut self) {
        self.metadata.file_count = self.graph.node_count();
        self.metadata.edge_count = self.graph.edge_count();
    }
}
