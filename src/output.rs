use crate::cli::args::OutputFormat;
use crate::graph::model::TestKind;
use crate::graph::traversal::ResolveResult;

/// Format the resolve result according to the requested output format.
/// Groups output by test kind with headers.
pub fn format_result(result: &ResolveResult, format: OutputFormat) -> String {
    // Output kinds in consistent order.
    let kind_order = [TestKind::Unit, TestKind::Snapshot, TestKind::UITest];

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
                        match (&t.test_target, &t.test_methods) {
                            (Some(target), Some(methods)) if !methods.is_empty() => {
                                for method in methods {
                                    lines.push(format!("{}/{}/{}", target, class_name, method));
                                }
                            }
                            (Some(target), _) => {
                                lines.push(format!("{}/{}", target, class_name));
                            }
                            (None, Some(methods)) if !methods.is_empty() => {
                                for method in methods {
                                    lines.push(format!("{}/{}", class_name, method));
                                }
                            }
                            (None, _) => {
                                lines.push(class_name.to_string());
                            }
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
                            let mut obj = serde_json::json!({
                                "file": t.file_id,
                                "target": t.test_target,
                            });
                            if let Some(methods) = &t.test_methods {
                                if !methods.is_empty() {
                                    obj["methods"] = serde_json::json!(methods);
                                }
                            }
                            obj
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
                            match &t.test_methods {
                                Some(methods) if !methods.is_empty() => {
                                    for method in methods {
                                        let flag = format!(
                                            "-only-testing:{}/{}/{}",
                                            target, class_name, method
                                        );
                                        flags.push(flag);
                                    }
                                }
                                _ => {
                                    let flag =
                                        format!("-only-testing:{}/{}", target, class_name);
                                    if flag.contains(' ') {
                                        flags.push(format!("'{}'", flag));
                                    } else {
                                        flags.push(flag);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            flags.join(" ")
        }
    }
}
