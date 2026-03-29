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

    // UI test paths → treat as Source (not supported).
    if path_lower.contains("uitest")
        || path_lower.contains("ui_test")
        || path_lower.contains("/uitests/")
        || path_lower.contains("e2etest")
        || path_lower.contains("/e2etests/")
    {
        return FileRole::Source;
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
    fn test_ui_test_treated_as_source() {
        let path = PathBuf::from("UITests/CheckoutUITests.swift");
        assert_eq!(classify_by_path(&path), FileRole::Source);
    }

    #[test]
    fn test_snapshot_by_import() {
        let path = PathBuf::from("Tests/SomeTests.swift");
        let imports = vec!["SnapshotTesting".to_string()];
        assert_eq!(classify(&path, &imports), FileRole::SnapshotTest);
    }
}
