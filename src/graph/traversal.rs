use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use super::model::{DependencyGraph, EdgeKind, TestKind};

/// Result of resolving affected tests, grouped by test kind.
#[derive(Debug)]
pub struct ResolveResult {
    /// Affected tests grouped by kind.
    pub by_kind: HashMap<TestKind, Vec<AffectedTest>>,
    /// Total files visited during traversal.
    pub files_visited: usize,
}

impl ResolveResult {
    /// Get all affected tests across all kinds.
    pub fn all_tests(&self) -> Vec<&AffectedTest> {
        self.by_kind.values().flat_map(|v| v.iter()).collect()
    }

    /// Total number of affected tests.
    pub fn total_count(&self) -> usize {
        self.by_kind.values().map(|v| v.len()).sum()
    }
}

#[derive(Debug)]
pub struct AffectedTest {
    pub file_id: String,
    pub test_target: Option<String>,
}

/// Resolve all affected tests in a single BFS pass.
///
/// Runs one traversal using the widest edge set needed across all requested kinds,
/// then groups collected tests by kind. Each kind has its own depth limit:
/// - **Unit**: depth 2 (changed → direct dependents → their tests)
/// - **Snapshot**: depth 3 (changed → view → parent view → snapshot test)
///
/// If `kinds` is empty, resolves all kinds.
pub fn resolve_affected_tests(
    graph: &DependencyGraph,
    changed_files: &[String],
    kinds: &[TestKind],
) -> ResolveResult {
    // If no kinds specified, resolve all.
    let requested: HashSet<TestKind> = if kinds.is_empty() {
        [TestKind::Unit, TestKind::Snapshot].into_iter().collect()
    } else {
        kinds.iter().copied().collect()
    };

    // Use the widest edge set across all requested kinds.
    let allowed_edges: HashSet<EdgeKind> = requested
        .iter()
        .flat_map(|k| k.allowed_edges().iter().copied())
        .collect();


    let mut visited: HashSet<NodeIndex> = HashSet::new();
    // Queue entries: (node, direct_ref_depth)
    // direct_ref_depth counts only DirectReference hops.
    // ViewEmbedding hops don't count — they're view tree traversal.
    let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();
    let mut by_kind: HashMap<TestKind, Vec<AffectedTest>> = HashMap::new();

    // Seed with changed files.
    for file_id in changed_files {
        if let Some(&idx) = graph.file_index.get(file_id) {
            queue.push_back((idx, 0));
        }
    }

    // BFS traversal — single pass.
    while let Some((current, direct_ref_depth)) = queue.pop_front() {
        if !visited.insert(current) {
            continue;
        }

        let node = &graph.graph[current];

        // Collect test nodes.
        if node.role.is_test() {
            if let Some(node_kind) = node.role.test_kind() {
                if requested.contains(&node_kind) {
                    by_kind.entry(node_kind).or_default().push(AffectedTest {
                        file_id: node.id.clone(),
                        test_target: node.module.clone(),
                    });
                }
            }
            // Don't traverse beyond test files — tests use spies, not real deps.
            continue;
        }

        // Expand edges per type:
        // - DirectReference: limited to 2 hops (tests use spies, non-view changes don't cascade)
        // - ViewEmbedding: unlimited (visual changes cascade through the entire view tree)
        // Only DirectReference increments the depth counter.
        for edge in graph.graph.edges(current) {
            let edge_kind = edge.weight().kind;
            if !allowed_edges.contains(&edge_kind) {
                continue;
            }

            match edge_kind {
                EdgeKind::DirectReference => {
                    if direct_ref_depth < 2 {
                        queue.push_back((edge.target(), direct_ref_depth + 1));
                    }
                }
                EdgeKind::ViewEmbedding => {
                    // View embedding doesn't count toward depth — view tree is unlimited.
                    queue.push_back((edge.target(), direct_ref_depth));
                }
            }
        }
    }

    ResolveResult {
        files_visited: visited.len(),
        by_kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DependencyGraph, FileNode, FileRole};
    use std::path::PathBuf;

    fn make_node(id: &str, role: FileRole) -> FileNode {
        FileNode {
            id: id.to_string(),
            path: PathBuf::from(id),
            role,
            module: None,
            defined_symbols: vec![],
            content_hash: None,
            mtime: None,
        }
    }

    #[test]
    fn test_unit_test_direct_reference() {
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("CartService.swift", FileRole::Source));
        graph.ensure_node(make_node("NetworkProtocol.swift", FileRole::Source));
        graph.ensure_node(make_node("CartServiceTests.swift", FileRole::UnitTest));

        graph.add_edge(
            &"NetworkProtocol.swift".into(),
            &"CartService.swift".into(),
            EdgeKind::DirectReference,
        );
        graph.add_edge(
            &"CartService.swift".into(),
            &"CartServiceTests.swift".into(),
            EdgeKind::DirectReference,
        );

        let result = resolve_affected_tests(
            &graph,
            &["CartService.swift".to_string()],
            &[TestKind::Unit],
        );
        assert_eq!(result.by_kind[&TestKind::Unit].len(), 1);
        assert_eq!(
            result.by_kind[&TestKind::Unit][0].file_id,
            "CartServiceTests.swift"
        );
    }

    #[test]
    fn test_unit_test_depth_limit() {
        // A → B → C → CTests
        // Changing A should find BTests (depth 2) but NOT CTests (depth 3)
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("A.swift", FileRole::Source));
        graph.ensure_node(make_node("B.swift", FileRole::Source));
        graph.ensure_node(make_node("C.swift", FileRole::Source));
        graph.ensure_node(make_node("BTests.swift", FileRole::UnitTest));
        graph.ensure_node(make_node("CTests.swift", FileRole::UnitTest));

        graph.add_edge(&"A.swift".into(), &"B.swift".into(), EdgeKind::DirectReference);
        graph.add_edge(&"B.swift".into(), &"BTests.swift".into(), EdgeKind::DirectReference);
        graph.add_edge(&"B.swift".into(), &"C.swift".into(), EdgeKind::DirectReference);
        graph.add_edge(&"C.swift".into(), &"CTests.swift".into(), EdgeKind::DirectReference);

        let result = resolve_affected_tests(
            &graph,
            &["A.swift".to_string()],
            &[TestKind::Unit],
        );

        let unit_ids: Vec<&str> = result
            .by_kind
            .get(&TestKind::Unit)
            .map(|v| v.iter().map(|t| t.file_id.as_str()).collect())
            .unwrap_or_default();
        assert!(unit_ids.contains(&"BTests.swift"), "BTests should be found (depth 2)");
        assert!(!unit_ids.contains(&"CTests.swift"), "CTests should NOT be found (depth 3)");
    }

    #[test]
    fn test_snapshot_follows_view_embedding() {
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("ProfileAvatar.swift", FileRole::Source));
        graph.ensure_node(make_node("ProfileScreen.swift", FileRole::Source));
        graph.ensure_node(make_node("ProfileScreenSnapshotTests.swift", FileRole::SnapshotTest));

        graph.add_edge(
            &"ProfileAvatar.swift".into(),
            &"ProfileScreen.swift".into(),
            EdgeKind::ViewEmbedding,
        );
        graph.add_edge(
            &"ProfileScreen.swift".into(),
            &"ProfileScreenSnapshotTests.swift".into(),
            EdgeKind::DirectReference,
        );

        let result = resolve_affected_tests(
            &graph,
            &["ProfileAvatar.swift".to_string()],
            &[TestKind::Snapshot],
        );
        assert_eq!(result.by_kind[&TestKind::Snapshot].len(), 1);
        assert_eq!(
            result.by_kind[&TestKind::Snapshot][0].file_id,
            "ProfileScreenSnapshotTests.swift"
        );
    }

    #[test]
    fn test_snapshot_deep_view_hierarchy() {
        // Icon → Avatar → Header → Profile → Settings → SettingsSnapshotTests
        // Changing Icon at depth 0 should reach SettingsSnapshotTests at depth 5+.
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("Icon.swift", FileRole::Source));
        graph.ensure_node(make_node("Avatar.swift", FileRole::Source));
        graph.ensure_node(make_node("Header.swift", FileRole::Source));
        graph.ensure_node(make_node("ProfileScreen.swift", FileRole::Source));
        graph.ensure_node(make_node("SettingsScreen.swift", FileRole::Source));
        graph.ensure_node(make_node("SettingsSnapshotTests.swift", FileRole::SnapshotTest));

        // View embedding chain: Icon → Avatar → Header → Profile → Settings
        graph.add_edge(&"Icon.swift".into(), &"Avatar.swift".into(), EdgeKind::ViewEmbedding);
        graph.add_edge(&"Avatar.swift".into(), &"Header.swift".into(), EdgeKind::ViewEmbedding);
        graph.add_edge(&"Header.swift".into(), &"ProfileScreen.swift".into(), EdgeKind::ViewEmbedding);
        graph.add_edge(&"ProfileScreen.swift".into(), &"SettingsScreen.swift".into(), EdgeKind::ViewEmbedding);
        graph.add_edge(
            &"SettingsScreen.swift".into(),
            &"SettingsSnapshotTests.swift".into(),
            EdgeKind::DirectReference,
        );

        let result = resolve_affected_tests(
            &graph,
            &["Icon.swift".to_string()],
            &[TestKind::Snapshot],
        );
        assert_eq!(
            result.by_kind.get(&TestKind::Snapshot).map(|v| v.len()).unwrap_or(0),
            1,
            "Snapshot test should be found regardless of view hierarchy depth"
        );
        assert_eq!(
            result.by_kind[&TestKind::Snapshot][0].file_id,
            "SettingsSnapshotTests.swift"
        );
    }

    #[test]
    fn test_all_kinds_single_pass() {
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("Service.swift", FileRole::Source));
        graph.ensure_node(make_node("ServiceTests.swift", FileRole::UnitTest));
        graph.ensure_node(make_node("ServiceSnapshotTests.swift", FileRole::SnapshotTest));

        graph.add_edge(
            &"Service.swift".into(),
            &"ServiceTests.swift".into(),
            EdgeKind::DirectReference,
        );
        graph.add_edge(
            &"Service.swift".into(),
            &"ServiceSnapshotTests.swift".into(),
            EdgeKind::DirectReference,
        );

        // Resolve all kinds at once.
        let result = resolve_affected_tests(
            &graph,
            &["Service.swift".to_string()],
            &[],
        );

        assert_eq!(result.by_kind.get(&TestKind::Unit).map(|v| v.len()).unwrap_or(0), 1);
        assert_eq!(result.by_kind.get(&TestKind::Snapshot).map(|v| v.len()).unwrap_or(0), 1);
        assert_eq!(result.total_count(), 2);
    }

    #[test]
    fn test_cycle_handling() {
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("A.swift", FileRole::Source));
        graph.ensure_node(make_node("B.swift", FileRole::Source));
        graph.ensure_node(make_node("ATests.swift", FileRole::UnitTest));

        graph.add_edge(&"A.swift".into(), &"B.swift".into(), EdgeKind::DirectReference);
        graph.add_edge(&"B.swift".into(), &"A.swift".into(), EdgeKind::DirectReference);
        graph.add_edge(
            &"A.swift".into(),
            &"ATests.swift".into(),
            EdgeKind::DirectReference,
        );

        let result = resolve_affected_tests(
            &graph,
            &["B.swift".to_string()],
            &[TestKind::Unit],
        );
        assert_eq!(result.by_kind[&TestKind::Unit].len(), 1);
        assert_eq!(result.by_kind[&TestKind::Unit][0].file_id, "ATests.swift");
    }
}
