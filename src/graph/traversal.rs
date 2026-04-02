use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use tracing::debug;

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

    /// Filter to only include tests whose file path matches one of the given targets.
    /// Each target is a (name, container_path) pair. The target name is assigned to
    /// `test_target` on matching tests for use in xcodebuild output format.
    pub fn filter_by_targets(&self, targets: &[(String, String)]) -> ResolveResult {
        let mut filtered: HashMap<TestKind, Vec<AffectedTest>> = HashMap::new();

        for (&kind, tests) in &self.by_kind {
            let matching: Vec<AffectedTest> = tests
                .iter()
                .filter_map(|t| {
                    targets
                        .iter()
                        .find(|(name, path)| {
                            t.file_id.contains(path.as_str())
                                || t.file_id.contains(name.as_str())
                        })
                        .map(|(name, _)| AffectedTest {
                            file_id: t.file_id.clone(),
                            test_target: Some(name.clone()),
                            test_methods: t.test_methods.clone(),
                        })
                })
                .collect();
            if !matching.is_empty() {
                filtered.insert(kind, matching);
            }
        }

        ResolveResult {
            files_visited: self.files_visited,
            by_kind: filtered,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AffectedTest {
    pub file_id: String,
    pub test_target: Option<String>,
    /// None means the whole test file is selected.
    /// Some(methods) means only those specific test methods are selected.
    pub test_methods: Option<Vec<String>>,
}

/// Resolve all affected tests in a single BFS pass.
///
/// Edge traversal rules:
/// - **DirectReference**: depth 2 for all kinds (changed → dependents → tests)
/// - **ViewEmbedding**: unlimited (visual changes cascade through view tree)
/// - **AccessibilityBinding**: resets DirectReference depth to 0 (bridge to UI tests)
///
/// After the BFS, UITest results are post-processed for method-level precision:
/// only test methods whose a11y queries overlap with the impacted a11y set are selected.
///
/// If `kinds` is empty, resolves all kinds.
pub fn resolve_affected_tests(
    graph: &DependencyGraph,
    changed_files: &[String],
    kinds: &[TestKind],
) -> ResolveResult {
    // If no kinds specified, resolve all.
    let requested: HashSet<TestKind> = if kinds.is_empty() {
        [TestKind::Unit, TestKind::Snapshot, TestKind::UITest]
            .into_iter()
            .collect()
    } else {
        kinds.iter().copied().collect()
    };

    // Use the widest edge set across all requested kinds.
    let allowed_edges: HashSet<EdgeKind> = requested
        .iter()
        .flat_map(|k| k.allowed_edges().iter().copied())
        .collect();

    let mut visited: HashSet<NodeIndex> = HashSet::new();
    // Queue entries: (node, direct_ref_depth, going_down)
    // direct_ref_depth counts only DirectReference hops.
    // going_down is true when we entered this node via reverse ViewEmbedding (traversing
    // down the view tree). In going_down mode, outgoing ViewEmbedding edges are suppressed
    // to prevent sideways spreading to unrelated embedders.
    let mut queue: VecDeque<(NodeIndex, usize, bool)> = VecDeque::new();
    let mut by_kind: HashMap<TestKind, Vec<AffectedTest>> = HashMap::new();

    // Track a11y IDs set by visited source files, for UITest method-level filtering.
    let needs_ui: bool = requested.contains(&TestKind::UITest);
    let mut impacted_a11y: HashSet<String> = HashSet::new();

    // Count AccessibilityBinding edges for diagnostics.
    if needs_ui {
        let a11y_edge_count = graph
            .graph
            .edge_references()
            .filter(|e| e.weight().kind == EdgeKind::AccessibilityBinding)
            .count();
        debug!(a11y_binding_edges = a11y_edge_count, "AccessibilityBinding edges in graph");
    }

    // Seed with changed files.
    let mut seeded = 0usize;
    for file_id in changed_files {
        if let Some(&idx) = graph.file_index.get(file_id) {
            queue.push_back((idx, 0, false));
            seeded += 1;
        }
    }
    debug!(seeded, "Changed files seeded into BFS");

    // BFS traversal — single pass.
    while let Some((current, direct_ref_depth, going_down)) = queue.pop_front() {
        if !visited.insert(current) {
            continue;
        }

        let node = &graph.graph[current];

        // Collect a11y setters from every visited source node (for UITest method filtering).
        if needs_ui && node.role == super::model::FileRole::Source {
            for setter in &node.a11y_setters {
                impacted_a11y.insert(setter.key().to_string());
            }
            // Also collect from page-object files (Source role in UI test dirs).
            for query in &node.a11y_queries {
                // Page objects sitting between AccessibilityBinding and UITest files
                // carry queries; include their keys so method matching works.
                impacted_a11y.insert(query.key().to_string());
            }
        }

        // Collect test nodes — only actual test classes (name ends with Test/Tests).
        if node.role.is_test() {
            let stem = std::path::Path::new(&node.id)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if stem.ends_with("Test") || stem.ends_with("Tests") {
                if let Some(node_kind) = node.role.test_kind() {
                    if requested.contains(&node_kind) {
                        by_kind.entry(node_kind).or_default().push(AffectedTest {
                            file_id: node.id.clone(),
                            test_target: node.module.clone(),
                            // Method-level filtering for UITest happens in post-processing.
                            test_methods: None,
                        });
                    }
                }
            }
            // Don't traverse beyond test files — tests use spies, not real deps.
            continue;
        }

        // Expand outgoing edges:
        // - DirectReference: limited to 2 hops
        // - ViewEmbedding: unlimited, but suppressed when going_down (prevents sideways
        //   spreading to unrelated embedders of the same view)
        // - AccessibilityBinding: resets depth to 0 (bridge edge)
        for edge in graph.graph.edges(current) {
            let edge_kind = edge.weight().kind;
            if !allowed_edges.contains(&edge_kind) {
                continue;
            }

            match edge_kind {
                EdgeKind::DirectReference => {
                    if direct_ref_depth < 2 {
                        queue.push_back((edge.target(), direct_ref_depth + 1, going_down));
                    }
                }
                EdgeKind::ViewEmbedding => {
                    if !going_down {
                        // Outgoing ViewEmbedding (up to embedders) — only in normal mode.
                        queue.push_back((edge.target(), direct_ref_depth, false));
                    }
                }
                EdgeKind::AccessibilityBinding => {
                    // A11y bridge: reset depth to 0 so page-object → test hops are allowed.
                    queue.push_back((edge.target(), 0, going_down));
                }
            }
        }

        // Expand incoming ViewEmbedding edges (down the view tree).
        // Edge direction is definer → embedder, so incoming ViewEmbedding edges at a
        // node point to views that this node's body embeds. When a container is affected,
        // its embedded child views are also visually affected.
        // Enters going_down mode to prevent sideways spreading back up.
        if allowed_edges.contains(&EdgeKind::ViewEmbedding) {
            for edge in graph.graph.edges_directed(current, petgraph::Direction::Incoming) {
                if edge.weight().kind == EdgeKind::ViewEmbedding {
                    queue.push_back((edge.source(), direct_ref_depth, true));
                }
            }
        }
    }

    // Post-process UITest results for method-level precision.
    if needs_ui {
        if let Some(ui_tests) = by_kind.get_mut(&TestKind::UITest) {
            for affected in ui_tests.iter_mut() {
                if let Some(node) = graph.get_node(&affected.file_id) {
                    let methods = filter_test_methods(node, &impacted_a11y, graph);
                    if !methods.is_empty() {
                        affected.test_methods = Some(methods);
                    }
                    // If empty, leave test_methods = None (select whole file as fallback).
                }
            }
        }
    }

    ResolveResult {
        files_visited: visited.len(),
        by_kind,
    }
}

/// Given a UITest file node, return the names of test methods whose a11y queries
/// overlap with the impacted a11y set (directly or through page objects).
fn filter_test_methods(
    node: &super::model::FileNode,
    impacted_a11y: &HashSet<String>,
    graph: &DependencyGraph,
) -> Vec<String> {
    // Build a set of page object a11y IDs reachable from this test file's referenced types.
    let mut page_object_a11y: HashSet<String> = HashSet::new();
    // Walk all incoming DirectReference edges from source nodes (page objects depend on nothing
    // in tests; tests depend on page objects — edge direction: page_obj → test).
    // We need to find page objects that point to this test file.
    if let Some(&test_idx) = graph.file_index.get(&node.id) {
        use petgraph::Direction;
        for edge in graph.graph.edges_directed(test_idx, Direction::Incoming) {
            if edge.weight().kind == EdgeKind::DirectReference {
                let page_obj = &graph.graph[edge.source()];
                for q in &page_obj.a11y_queries {
                    page_object_a11y.insert(q.key().to_string());
                }
            }
        }
    }

    let mut selected = Vec::new();
    for method in &node.test_methods {
        // Check direct a11y queries in this method.
        let direct_hit = method
            .a11y_queries
            .iter()
            .any(|q| impacted_a11y.contains(q.key()));

        // Check if any referenced type (page object) has impacted a11y IDs.
        let page_obj_hit = method.referenced_types.iter().any(|type_name| {
            // Look up the type in the graph as a page object.
            graph.file_index.keys().any(|file_id| {
                if file_id.contains(type_name.as_str()) {
                    if let Some(po_node) = graph.get_node(file_id) {
                        return po_node
                            .a11y_queries
                            .iter()
                            .any(|q| impacted_a11y.contains(q.key()));
                    }
                }
                false
            })
        });

        if direct_hit || page_obj_hit {
            selected.push(method.name.clone());
        }
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{A11yIdentifier, DependencyGraph, FileNode, FileRole};
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
            a11y_setters: vec![],
            a11y_queries: vec![],
            test_methods: vec![],
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
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("Icon.swift", FileRole::Source));
        graph.ensure_node(make_node("Avatar.swift", FileRole::Source));
        graph.ensure_node(make_node("Header.swift", FileRole::Source));
        graph.ensure_node(make_node("ProfileScreen.swift", FileRole::Source));
        graph.ensure_node(make_node("SettingsScreen.swift", FileRole::Source));
        graph.ensure_node(make_node("SettingsSnapshotTests.swift", FileRole::SnapshotTest));

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

        let result = resolve_affected_tests(
            &graph,
            &["Service.swift".to_string()],
            &[],
        );

        assert_eq!(result.by_kind.get(&TestKind::Unit).map(|v| v.len()).unwrap_or(0), 1);
        assert_eq!(result.by_kind.get(&TestKind::Snapshot).map(|v| v.len()).unwrap_or(0), 1);
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

    #[test]
    fn test_uitest_via_accessibility_binding() {
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        let mut view = make_node("LoginView.swift", FileRole::Source);
        view.a11y_setters = vec![A11yIdentifier::Literal("login_button".into())];
        graph.ensure_node(view);

        let mut ui_test = make_node("UITests/LoginUITests.swift", FileRole::UITest);
        ui_test.a11y_queries = vec![A11yIdentifier::Literal("login_button".into())];
        ui_test.test_methods = vec![
            crate::graph::model::TestMethodInfo {
                name: "testLogin".into(),
                a11y_queries: vec![A11yIdentifier::Literal("login_button".into())],
                referenced_types: vec![],
            },
            crate::graph::model::TestMethodInfo {
                name: "testSignup".into(),
                a11y_queries: vec![A11yIdentifier::Literal("signup_button".into())],
                referenced_types: vec![],
            },
        ];
        graph.ensure_node(ui_test);

        graph.add_edge(
            &"LoginView.swift".into(),
            &"UITests/LoginUITests.swift".into(),
            EdgeKind::AccessibilityBinding,
        );

        let result = resolve_affected_tests(
            &graph,
            &["LoginView.swift".to_string()],
            &[TestKind::UITest],
        );

        let ui = result.by_kind.get(&TestKind::UITest).expect("should have UITest results");
        assert_eq!(ui.len(), 1);
        assert_eq!(ui[0].file_id, "UITests/LoginUITests.swift");

        // Only testLogin should be selected (not testSignup).
        let methods = ui[0].test_methods.as_deref().unwrap_or(&[]);
        assert!(methods.contains(&"testLogin".to_string()), "testLogin should be selected");
        assert!(!methods.contains(&"testSignup".to_string()), "testSignup should NOT be selected");
    }

    #[test]
    fn test_uitest_depth_limit_via_directreference() {
        // Service → View → UITest (via a11y)
        // Service change should reach UITest via View's a11y setter.
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("LoginService.swift", FileRole::Source));

        let mut view = make_node("LoginView.swift", FileRole::Source);
        view.a11y_setters = vec![A11yIdentifier::Literal("login_button".into())];
        graph.ensure_node(view);

        let mut ui_test = make_node("UITests/LoginUITests.swift", FileRole::UITest);
        ui_test.test_methods = vec![crate::graph::model::TestMethodInfo {
            name: "testLogin".into(),
            a11y_queries: vec![A11yIdentifier::Literal("login_button".into())],
            referenced_types: vec![],
        }];
        graph.ensure_node(ui_test);

        // Service is used by View (depth 1).
        graph.add_edge(
            &"LoginService.swift".into(),
            &"LoginView.swift".into(),
            EdgeKind::DirectReference,
        );
        // View has an a11y binding to the UITest.
        graph.add_edge(
            &"LoginView.swift".into(),
            &"UITests/LoginUITests.swift".into(),
            EdgeKind::AccessibilityBinding,
        );

        let result = resolve_affected_tests(
            &graph,
            &["LoginService.swift".to_string()],
            &[TestKind::UITest],
        );

        let ui = result.by_kind.get(&TestKind::UITest).expect("UITest should be found");
        assert_eq!(ui.len(), 1);
        assert_eq!(ui[0].file_id, "UITests/LoginUITests.swift");
    }

    #[test]
    fn test_uitest_container_reaches_embedded_views() {
        // Container → Wrapper (uses Container) → embeds ChildView (ViewEmbedding)
        // ChildView sets a11y → UITest via AccessibilityBinding
        // Changing Container should reach UITest through the view tree.
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("Container.swift", FileRole::Source));
        graph.ensure_node(make_node("Wrapper.swift", FileRole::Source));

        let mut child = make_node("ChildView.swift", FileRole::Source);
        child.a11y_setters = vec![A11yIdentifier::Literal("child_button".into())];
        graph.ensure_node(child);

        let mut ui_test = make_node("UITests/ChildUITests.swift", FileRole::UITest);
        ui_test.test_methods = vec![crate::graph::model::TestMethodInfo {
            name: "testChild".into(),
            a11y_queries: vec![A11yIdentifier::Literal("child_button".into())],
            referenced_types: vec![],
        }];
        graph.ensure_node(ui_test);

        // Wrapper uses Container (depth 1 from Container change).
        graph.add_edge(
            &"Container.swift".into(),
            &"Wrapper.swift".into(),
            EdgeKind::DirectReference,
        );
        // Wrapper's body embeds ChildView: edge definer(ChildView) → embedder(Wrapper).
        graph.add_edge(
            &"ChildView.swift".into(),
            &"Wrapper.swift".into(),
            EdgeKind::ViewEmbedding,
        );
        // ChildView has a11y binding to the UITest.
        graph.add_edge(
            &"ChildView.swift".into(),
            &"UITests/ChildUITests.swift".into(),
            EdgeKind::AccessibilityBinding,
        );

        let result = resolve_affected_tests(
            &graph,
            &["Container.swift".to_string()],
            &[TestKind::UITest],
        );

        let ui = result
            .by_kind
            .get(&TestKind::UITest)
            .expect("UITest should be found via container → embedded view chain");
        assert_eq!(ui.len(), 1);
        assert_eq!(ui[0].file_id, "UITests/ChildUITests.swift");
    }

    #[test]
    fn test_unit_test_does_not_follow_reverse_view_embedding() {
        // Same structure as above but requesting Unit tests.
        // Reverse ViewEmbedding should NOT apply for unit tests.
        let mut graph = DependencyGraph::new(PathBuf::from("/repo"));

        graph.ensure_node(make_node("Container.swift", FileRole::Source));
        graph.ensure_node(make_node("Wrapper.swift", FileRole::Source));
        graph.ensure_node(make_node("ChildView.swift", FileRole::Source));
        graph.ensure_node(make_node("ChildTests.swift", FileRole::UnitTest));

        graph.add_edge(
            &"Container.swift".into(),
            &"Wrapper.swift".into(),
            EdgeKind::DirectReference,
        );
        graph.add_edge(
            &"ChildView.swift".into(),
            &"Wrapper.swift".into(),
            EdgeKind::ViewEmbedding,
        );
        graph.add_edge(
            &"ChildView.swift".into(),
            &"ChildTests.swift".into(),
            EdgeKind::DirectReference,
        );

        let result = resolve_affected_tests(
            &graph,
            &["Container.swift".to_string()],
            &[TestKind::Unit],
        );

        // Unit tests should NOT follow reverse ViewEmbedding.
        assert!(
            result.by_kind.get(&TestKind::Unit).is_none(),
            "Unit tests should not follow reverse ViewEmbedding"
        );
    }
}
