use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Represents the relevant parts of an .xctestplan JSON file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestPlan {
    #[serde(default)]
    pub configurations: Vec<TestConfiguration>,
    #[serde(default)]
    pub default_options: serde_json::Value,
    #[serde(default)]
    pub test_targets: Vec<TestTarget>,
    /// Preserve all other fields.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestConfiguration {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub options: serde_json::Value,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestTarget {
    #[serde(default)]
    pub target: TargetRef,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub skipped_tests: serde_json::Value,
    #[serde(default)]
    pub selected_tests: serde_json::Value,
    /// Preserve all other fields.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRef {
    #[serde(default)]
    pub container_path: String,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// Read an .xctestplan file.
pub fn read(path: &Path) -> Result<TestPlan> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read test plan: {}", path.display()))?;
    let plan: TestPlan =
        serde_json::from_str(&content).context("Failed to parse .xctestplan JSON")?;
    Ok(plan)
}

/// Write an .xctestplan file (preserving formatting with pretty-print).
pub fn write(plan: &TestPlan, path: &Path) -> Result<()> {
    let content = serde_json::to_string_pretty(plan).context("Failed to serialize test plan")?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write test plan: {}", path.display()))?;
    Ok(())
}

/// Disable test targets that have no affected tests.
/// Returns the list of target names that were disabled.
pub fn disable_unaffected_targets(
    plan: &mut TestPlan,
    affected_target_names: &[String],
) -> Vec<String> {
    let mut disabled = Vec::new();

    for target in &mut plan.test_targets {
        let target_name = &target.target.name;
        if !affected_target_names.contains(target_name) {
            target.enabled = Some(false);
            disabled.push(target_name.clone());
        } else {
            target.enabled = Some(true);
        }
    }

    disabled
}
