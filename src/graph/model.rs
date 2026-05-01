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
    ///
    /// Indices are removed in descending order to avoid petgraph's swap-remove
    /// invalidating indices that are still pending removal.
    pub fn remove_edges_from(&mut self, id: &FileId) {
        if let Some(&idx) = self.file_index.get(id) {
            let mut edges_to_remove: Vec<_> = self
                .graph
                .edges(idx)
                .map(|e| e.id())
                .collect();
            edges_to_remove.sort_unstable_by(|a, b| b.cmp(a));
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
            let mut edges_to_remove: Vec<_> = self
                .graph
                .edges_directed(target_idx, Direction::Incoming)
                .filter(|e| e.weight().kind == EdgeKind::AccessibilityBinding)
                .map(|e| e.id())
                .collect();
            edges_to_remove.sort_unstable_by(|a, b| b.cmp(a));
            for edge_id in edges_to_remove {
                self.graph.remove_edge(edge_id);
            }
        }
    }

    /// Remove nodes (and their edges) for the given file IDs.
    ///
    /// petgraph's `remove_node` swap-removes, invalidating other indices, so we
    /// rebuild `file_index` from scratch after all removals.
    pub fn remove_nodes(&mut self, ids: &[FileId]) {
        let mut indices: Vec<NodeIndex> = ids
            .iter()
            .filter_map(|id| self.file_index.get(id).copied())
            .collect();
        if indices.is_empty() {
            return;
        }
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for idx in indices {
            self.graph.remove_node(idx);
        }
        self.file_index.clear();
        for idx in self.graph.node_indices() {
            self.file_index.insert(self.graph[idx].id.clone(), idx);
        }
    }

    /// Remove all edges of a given kind from the entire graph.
    ///
    /// Indices are removed in descending order to avoid petgraph's swap-remove
    /// invalidating indices that are still pending removal.
    pub fn remove_edges_by_kind(&mut self, kind: EdgeKind) {
        let mut edges_to_remove: Vec<_> = self
            .graph
            .edge_indices()
            .filter(|&e| self.graph[e].kind == kind)
            .collect();
        edges_to_remove.sort_unstable_by(|a, b| b.cmp(a));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(id: &str) -> FileNode {
        FileNode {
            id: id.to_string(),
            path: PathBuf::from(id),
            role: FileRole::Source,
            module: None,
            defined_symbols: vec![],
            content_hash: None,
            mtime: None,
            a11y_setters: vec![],
            a11y_queries: vec![],
            test_methods: vec![],
        }
    }

    #[test]
    fn remove_edges_by_kind_removes_all() {
        let mut g = DependencyGraph::new(PathBuf::from("/repo"));
        g.ensure_node(make_node("A"));
        g.ensure_node(make_node("B"));
        g.ensure_node(make_node("C"));
        g.ensure_node(make_node("D"));
        g.ensure_node(make_node("E"));

        // Interleave DirectReference and AccessibilityBinding edges
        // to stress the swap-remove ordering.
        g.add_edge(&"A".into(), &"B".into(), EdgeKind::DirectReference);
        g.add_edge(&"A".into(), &"C".into(), EdgeKind::AccessibilityBinding);
        g.add_edge(&"B".into(), &"D".into(), EdgeKind::DirectReference);
        g.add_edge(&"C".into(), &"D".into(), EdgeKind::AccessibilityBinding);
        g.add_edge(&"D".into(), &"E".into(), EdgeKind::AccessibilityBinding);
        g.add_edge(&"B".into(), &"E".into(), EdgeKind::DirectReference);

        g.remove_edges_by_kind(EdgeKind::AccessibilityBinding);

        let remaining: Vec<_> = g
            .graph
            .edge_indices()
            .map(|e| g.graph[e].kind)
            .collect();
        assert!(
            remaining.iter().all(|k| *k == EdgeKind::DirectReference),
            "All AccessibilityBinding edges should be removed, but got: {:?}",
            remaining
        );
        assert_eq!(remaining.len(), 3, "3 DirectReference edges should survive");
    }

    #[test]
    fn remove_nodes_drops_edges_and_reindexes() {
        let mut g = DependencyGraph::new(PathBuf::from("/repo"));
        g.ensure_node(make_node("A"));
        g.ensure_node(make_node("B"));
        g.ensure_node(make_node("C"));
        g.ensure_node(make_node("D"));

        g.add_edge(&"A".into(), &"B".into(), EdgeKind::DirectReference);
        g.add_edge(&"B".into(), &"C".into(), EdgeKind::DirectReference);
        g.add_edge(&"C".into(), &"D".into(), EdgeKind::DirectReference);

        // Remove B — should drop A→B and B→C, leaving only C→D.
        g.remove_nodes(&["B".into()]);

        assert_eq!(g.graph.node_count(), 3);
        assert_eq!(g.graph.edge_count(), 1);
        assert!(!g.file_index.contains_key("B"));

        // file_index must still resolve surviving nodes correctly after swap-remove.
        for id in ["A", "C", "D"] {
            let idx = g.file_index[id];
            assert_eq!(g.graph[idx].id, id);
        }
    }

    #[test]
    fn remove_edges_from_removes_all_outgoing() {
        let mut g = DependencyGraph::new(PathBuf::from("/repo"));
        g.ensure_node(make_node("A"));
        g.ensure_node(make_node("B"));
        g.ensure_node(make_node("C"));
        g.ensure_node(make_node("D"));

        g.add_edge(&"A".into(), &"B".into(), EdgeKind::DirectReference);
        g.add_edge(&"A".into(), &"C".into(), EdgeKind::DirectReference);
        g.add_edge(&"A".into(), &"D".into(), EdgeKind::DirectReference);
        // Edge from another node — should survive.
        g.add_edge(&"B".into(), &"D".into(), EdgeKind::DirectReference);

        g.remove_edges_from(&"A".into());

        assert_eq!(g.graph.edge_count(), 1, "Only B→D should survive");
        let edge = g.graph.edge_indices().next().unwrap();
        let (src, dst) = g.graph.edge_endpoints(edge).unwrap();
        assert_eq!(g.graph[src].id, "B");
        assert_eq!(g.graph[dst].id, "D");
    }
}
