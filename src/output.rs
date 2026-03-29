use crate::cli::args::OutputFormat;
use crate::graph::model::TestKind;
use crate::graph::traversal::ResolveResult;

/// Format the resolve result according to the requested output format.
/// Groups output by test kind with headers.
pub fn format_result(result: &ResolveResult, format: OutputFormat) -> String {
    // Output kinds in consistent order.
    let kind_order = [TestKind::Unit, TestKind::Snapshot];

    match format {
        OutputFormat::List => {
            let mut sections = Vec::new();
            for &kind in &kind_order {
                if let Some(tests) = result.by_kind.get(&kind) {
                    if !tests.is_empty() {
                        let mut lines = vec![format!("--- {:?} ({}) ---", kind, tests.len())];
                        for t in tests {
                            lines.push(t.file_id.clone());
                        }
                        sections.push(lines.join("\n"));
                    }
                }
            }
            sections.join("\n\n")
        }

        OutputFormat::Json => {
            let mut map = serde_json::Map::new();
            for &kind in &kind_order {
                if let Some(tests) = result.by_kind.get(&kind) {
                    let entries: Vec<serde_json::Value> = tests
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "file": t.file_id,
                                "target": t.test_target,
                            })
                        })
                        .collect();
                    map.insert(format!("{:?}", kind).to_lowercase(), entries.into());
                }
            }
            map.insert("total".into(), result.total_count().into());
            serde_json::to_string_pretty(&map).unwrap_or_default()
        }

        OutputFormat::Xcodebuild => {
            let mut sections = Vec::new();
            for &kind in &kind_order {
                if let Some(tests) = result.by_kind.get(&kind) {
                    let flags: Vec<String> = tests
                        .iter()
                        .filter_map(|t| {
                            t.test_target.as_ref().map(|target| {
                                let class_name = t
                                    .file_id
                                    .strip_suffix(".swift")
                                    .unwrap_or(&t.file_id);
                                format!("-only-testing:{}/{}", target, class_name)
                            })
                        })
                        .collect();
                    if !flags.is_empty() {
                        sections.push(flags.join(" "));
                    }
                }
            }
            sections.join(" ")
        }
    }
}
