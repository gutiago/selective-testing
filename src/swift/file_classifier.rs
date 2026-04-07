use std::path::Path;

use crate::graph::model::FileRole;

/// Classify a Swift file's role based on its path and imports.
pub fn classify(path: &Path, imports: &[String]) -> FileRole {
    if has_snapshot_import(imports) {
        return FileRole::SnapshotTest;
    }

    classify_by_path(path)
}

/// Classify based on file path conventions only.
/// UI test files are treated as Source (not supported for selective testing).
pub fn classify_by_path(path: &Path) -> FileRole {
    let path_str = path.to_string_lossy();
    let path_lower = path_str.to_lowercase();

    // UI test paths.
    if path_lower.contains("uitest")
        || path_lower.contains("ui_test")
        || path_lower.contains("/uitests/")
        || path_lower.contains("e2etest")
        || path_lower.contains("/e2etests/")
    {
        return FileRole::UITest;
    }

    // Check if it's in a test directory at all.
    let is_in_test_dir = path_lower.contains("/tests/")
        || path_lower.contains("/test/")
        || path_lower.contains("tests/");

    if !is_in_test_dir {
        let file_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let name_lower = file_name.to_lowercase();

        if !name_lower.ends_with("test") && !name_lower.ends_with("tests") {
            return FileRole::Source;
        }
    }

    // Snapshot test detection by path.
    if path_lower.contains("snapshot")
        || path_lower.contains("snapshottest")
        || path_lower.contains("/snapshottests/")
    {
        return FileRole::SnapshotTest;
    }

    // Default test files are unit tests.
    if is_in_test_dir {
        return FileRole::UnitTest;
    }

    // Check by file name suffix.
    let file_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name_lower = file_name.to_lowercase();

    if name_lower.ends_with("snapshottest") || name_lower.ends_with("snapshottests") {
        FileRole::SnapshotTest
    } else if name_lower.ends_with("test") || name_lower.ends_with("tests") {
        FileRole::UnitTest
    } else {
        FileRole::Source
    }
}

/// Infer the Xcode test target / SPM module name from a file's relative path.
///
/// Heuristics (tried in order):
///   1. Named test-target directory: first directory component whose name ends with
///      Tests/UITests/E2ETests (e.g. `AppUITests`, `App Tests`).
///   2. SPM layout: `…/Tests/<ModuleName>/…` → `ModuleName`, but only when no
///      named target directory was found above.
pub fn infer_module(rel_path: &Path) -> Option<String> {
    let components: Vec<&str> = rel_path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Heuristic 1: walk from the root and pick the first directory whose name
    // looks like a test target (but skip the bare "Tests" directory itself).
    for comp in &components[..components.len().saturating_sub(1)] {
        let lower = comp.to_lowercase();
        if lower == "tests" || lower == "test" {
            continue;
        }
        if lower.ends_with("tests") || lower.ends_with("test") {
            return Some(comp.to_string());
        }
    }

    // Heuristic 2: SPM `…/Tests/<ModuleName>/…`
    // The directory immediately after a "Tests" component is the module,
    // but only when there's still a file deeper (i.e., at least 2 more components).
    for (i, comp) in components.iter().enumerate() {
        if *comp == "Tests" && i + 2 < components.len() {
            return Some(components[i + 1].to_string());
        }
    }

    None
}

fn has_snapshot_import(imports: &[String]) -> bool {
    imports.iter().any(|i| {
        i == "SnapshotTesting"
            || i == "iOSSnapshotTestCase"
            || i == "FBSnapshotTestCase"
            || i == "SnapshotTestingSupport"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_source_file() {
        let path = PathBuf::from("Sources/CartService.swift");
        assert_eq!(classify_by_path(&path), FileRole::Source);
    }

    #[test]
    fn test_unit_test_by_path() {
        let path = PathBuf::from("Tests/CartServiceTests.swift");
        assert_eq!(classify_by_path(&path), FileRole::UnitTest);
    }

    #[test]
    fn test_snapshot_test_by_path() {
        let path = PathBuf::from("Tests/SnapshotTests/ProfileScreenSnapshotTests.swift");
        assert_eq!(classify_by_path(&path), FileRole::SnapshotTest);
    }

    #[test]
    fn test_ui_test_classified_as_uitest() {
        let path = PathBuf::from("UITests/CheckoutUITests.swift");
        assert_eq!(classify_by_path(&path), FileRole::UITest);
    }

    #[test]
    fn test_snapshot_by_import() {
        let path = PathBuf::from("Tests/SomeTests.swift");
        let imports = vec!["SnapshotTesting".to_string()];
        assert_eq!(classify(&path, &imports), FileRole::SnapshotTest);
    }

    #[test]
    fn test_infer_module_named_target_over_spm() {
        let path = PathBuf::from("App/AppUITests/Tests/More/SomeUITests.swift");
        assert_eq!(infer_module(&path), Some("AppUITests".into()));
    }

    #[test]
    fn test_infer_module_spm_package() {
        let path = PathBuf::from("Packages/Networking/Tests/NetworkingTests/APIClientTests.swift");
        assert_eq!(infer_module(&path), Some("NetworkingTests".into()));
    }

    #[test]
    fn test_infer_module_top_level_tests_dir() {
        let path = PathBuf::from("Tests/FooTests/BarTest.swift");
        assert_eq!(infer_module(&path), Some("FooTests".into()));
    }

    #[test]
    fn test_infer_module_e2e() {
        let path = PathBuf::from("App/AppE2ETests/Tests/OnboardingE2ETests.swift");
        assert_eq!(infer_module(&path), Some("AppE2ETests".into()));
    }

    #[test]
    fn test_infer_module_source_file() {
        let path = PathBuf::from("Sources/CartService.swift");
        assert_eq!(infer_module(&path), None);
    }
}
