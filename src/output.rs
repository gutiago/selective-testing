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
            let mut lines = Vec::new();
            for &kind in &kind_order {
                if let Some(tests) = result.by_kind.get(&kind) {
                    for t in tests {
                        let class_name = std::path::Path::new(&t.file_id)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&t.file_id);
                        if let Some(target) = &t.test_target {
                            lines.push(format!("{}/{}", target, class_name));
                        } else {
                            lines.push(class_name.to_string());
                        }
                    }
                }
            }
            lines.join("\n")
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
            let mut flags = Vec::new();
            for &kind in &kind_order {
                if let Some(tests) = result.by_kind.get(&kind) {
                    for t in tests {
                        if let Some(target) = &t.test_target {
                            let class_name = std::path::Path::new(&t.file_id)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&t.file_id);
                            let flag = format!("-only-testing:{}/{}", target, class_name);
                            // Quote flags that contain spaces for shell safety.
                            if flag.contains(' ') {
                                flags.push(format!("'{}'", flag));
                            } else {
                                flags.push(flag);
                            }
                        }
                    }
                }
            }
            flags.join(" ")
        }
    }
}
