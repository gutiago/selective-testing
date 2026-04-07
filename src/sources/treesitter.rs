use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, warn};
use tree_sitter::Parser;

use super::{DataSource, SourceEdge};
use crate::graph::model::{A11yIdentifier, EdgeKind, FileNode, FileRole, TestMethodInfo};
use crate::swift::file_classifier;

// XCUIElement query accessor method names.
const XCUI_ACCESSORS: &[&str] = &[
    "buttons",
    "staticTexts",
    "textFields",
    "textViews",
    "switches",
    "sliders",
    "images",
    "cells",
    "tables",
    "collectionViews",
    "otherElements",
    "scrollViews",
    "alerts",
    "sheets",
    "navigationBars",
    "tabBars",
    "toolbars",
    "searchFields",
    "activityIndicators",
    "progressIndicators",
    "segmentedControls",
    "pickers",
    "datePickers",
    "steppers",
    "webViews",
    "maps",
    "links",
    "icons",
    "checkBoxes",
    "radioButtons",
    "menuItems",
    "menus",
    "popovers",
    "draggableFiles",
];

/// All symbols extracted from a single Swift file.
#[derive(Debug)]
struct ParsedFile {
    file_id: String,
    path: PathBuf,
    defines: Vec<String>,
    references: Vec<String>,
    view_embeddings: Vec<String>,
    role: FileRole,
    // A11y data
    a11y_setters: Vec<A11yIdentifier>,
    a11y_queries: Vec<A11yIdentifier>,
    test_methods: Vec<TestMethodInfo>,
    a11y_constants: HashMap<String, String>,
    typealiases: HashMap<String, String>,
}

pub struct TreeSitterSource;

impl DataSource for TreeSitterSource {
    fn analyze(
        &self,
        repo_root: &Path,
        swift_files: &[PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        let file_data: Vec<ParsedFile> = swift_files
            .par_iter()
            .filter_map(|path| {
                let rel_path = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                match parse_swift_file(path, &rel_path) {
                    Ok(parsed) => Some(parsed),
                    Err(e) => {
                        warn!(file = %rel_path, error = %e, "Failed to parse Swift file");
                        None
                    }
                }
            })
            .collect();

        // Build symbol-to-file index for existing dependency edges.
        let mut symbol_to_file: HashMap<String, Vec<String>> = HashMap::new();
        for fd in &file_data {
            for symbol in &fd.defines {
                symbol_to_file
                    .entry(symbol.clone())
                    .or_default()
                    .push(fd.file_id.clone());
            }
        }

        // Build FileNodes with a11y data populated.
        let nodes: Vec<FileNode> = file_data
            .iter()
            .map(|fd| FileNode {
                id: fd.file_id.clone(),
                path: fd.path.clone(),
                role: fd.role.clone(),
                module: file_classifier::infer_module(std::path::Path::new(&fd.file_id)),
                defined_symbols: fd.defines.clone(),
                content_hash: None,
                mtime: None,
                a11y_setters: fd.a11y_setters.clone(),
                a11y_queries: fd.a11y_queries.clone(),
                test_methods: fd.test_methods.clone(),
            })
            .collect();

        let mut edges = Vec::new();

        // DirectReference and ViewEmbedding edges from symbol matching.
        for fd in &file_data {
            for ref_name in &fd.references {
                if let Some(defining_files) = symbol_to_file.get(ref_name) {
                    for def_file in defining_files {
                        if def_file != &fd.file_id {
                            edges.push(SourceEdge {
                                from: def_file.clone(),
                                to: fd.file_id.clone(),
                                kind: EdgeKind::DirectReference,
                            });
                        }
                    }
                }
            }

            for view_ref in &fd.view_embeddings {
                if let Some(defining_files) = symbol_to_file.get(view_ref) {
                    for def_file in defining_files {
                        if def_file != &fd.file_id {
                            edges.push(SourceEdge {
                                from: def_file.clone(),
                                to: fd.file_id.clone(),
                                kind: EdgeKind::ViewEmbedding,
                            });
                        }
                    }
                }
            }
        }

        // AccessibilityBinding edges from a11y identifier matching.
        let a11y_edges = build_a11y_edges(&file_data);
        edges.extend(a11y_edges);

        debug!(
            nodes = nodes.len(),
            edges = edges.len(),
            "tree-sitter analysis complete"
        );

        Ok((nodes, edges))
    }
}

/// Build AccessibilityBinding edges by matching a11y setters to queries across files.
fn build_a11y_edges(files: &[ParsedFile]) -> Vec<SourceEdge> {
    // Global typealias and constants maps (merged from all files).
    let mut typealiases: HashMap<String, String> = HashMap::new();
    let mut constants: HashMap<String, String> = HashMap::new();

    for fd in files {
        typealiases.extend(fd.typealiases.iter().map(|(k, v)| (k.clone(), v.clone())));
        constants.extend(fd.a11y_constants.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    // setter_index: canonical_key → Vec<file_id>
    let mut setter_index: HashMap<String, Vec<String>> = HashMap::new();
    for fd in files {
        for setter in &fd.a11y_setters {
            for key in canonical_keys(setter, &typealiases, &constants) {
                setter_index
                    .entry(key)
                    .or_default()
                    .push(fd.file_id.clone());
            }
        }
    }

    let mut edges = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for fd in files {
        // File-level queries (page objects and direct UI test queries).
        let all_queries: Vec<&A11yIdentifier> = fd
            .a11y_queries
            .iter()
            .chain(fd.test_methods.iter().flat_map(|m| m.a11y_queries.iter()))
            .collect();

        for query in all_queries {
            for key in canonical_keys(query, &typealiases, &constants) {
                if let Some(setter_files) = setter_index.get(&key) {
                    for setter_file in setter_files {
                        if setter_file != &fd.file_id {
                            let edge_key = (setter_file.clone(), fd.file_id.clone());
                            if seen.insert(edge_key) {
                                edges.push(SourceEdge {
                                    from: setter_file.clone(),
                                    to: fd.file_id.clone(),
                                    kind: EdgeKind::AccessibilityBinding,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    edges
}

/// Compute the set of canonical matching keys for an a11y identifier.
/// Returns both the raw form and any resolvable aliases or string literals.
fn canonical_keys(
    id: &A11yIdentifier,
    typealiases: &HashMap<String, String>,
    constants: &HashMap<String, String>,
) -> Vec<String> {
    match id {
        A11yIdentifier::Literal(s) => vec![s.clone()],
        A11yIdentifier::Symbolic(path) => {
            let mut keys = vec![path.clone()];

            let expanded = expand_typealias(path, typealiases);
            if expanded != *path {
                keys.push(expanded.clone());
            }

            for p in &[path.as_str(), expanded.as_str()] {
                if let Some(literal) = constants.get(*p) {
                    if !keys.contains(literal) {
                        keys.push(literal.clone());
                    }
                }
            }

            keys
        }
    }
}

/// Expand the first component of a dotted path using the typealias map.
/// E.g., "AccessibilityIDs.x" → "AccessibilityIdentifiers.Login.x"
fn expand_typealias(path: &str, typealiases: &HashMap<String, String>) -> String {
    let (first, rest) = match path.find('.') {
        Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
        None => (path, None),
    };

    if let Some(expansion) = typealiases.get(first) {
        match rest {
            Some(rest) => format!("{}.{}", expansion, rest),
            None => expansion.clone(),
        }
    } else {
        path.to_string()
    }
}

// ---------------------------------------------------------------------------
// File parsing
// ---------------------------------------------------------------------------

fn parse_swift_file(path: &Path, file_id: &str) -> Result<ParsedFile> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read: {}", path.display()))?;

    let mut parser = Parser::new();
    let language = tree_sitter_swift::LANGUAGE;
    parser
        .set_language(&language.into())
        .context("Failed to set tree-sitter Swift language")?;

    let tree = parser
        .parse(&source, None)
        .context("tree-sitter parse returned None")?;

    let source_bytes = source.as_bytes();
    let mut defines = Vec::new();
    let mut references = Vec::new();
    let mut conformances = Vec::new();
    let mut view_embeddings = Vec::new();
    let mut imports = Vec::new();

    let mut cursor = tree.root_node().walk();
    extract_symbols(
        &mut cursor,
        source_bytes,
        &mut defines,
        &mut references,
        &mut conformances,
        &mut imports,
        &mut view_embeddings,
        false,
        None,
    );

    defines.sort();
    defines.dedup();
    references.sort();
    references.dedup();

    let role = file_classifier::classify(path, &imports);

    // A11y extraction (second pass, separate from symbol extraction).
    let mut a11y_out = A11yOutput::default();
    walk_a11y(
        tree.root_node(),
        source_bytes,
        &role,
        None,
        false,
        &mut a11y_out,
    );

    Ok(ParsedFile {
        file_id: file_id.to_string(),
        path: path.to_path_buf(),
        defines,
        references,
        view_embeddings,
        role,
        a11y_setters: a11y_out.setters,
        a11y_queries: a11y_out.queries,
        test_methods: a11y_out.test_methods,
        a11y_constants: a11y_out.constants,
        typealiases: a11y_out.typealiases,
    })
}

// ---------------------------------------------------------------------------
// A11y extraction
// ---------------------------------------------------------------------------

#[derive(Default)]
struct A11yOutput {
    setters: Vec<A11yIdentifier>,
    queries: Vec<A11yIdentifier>,
    test_methods: Vec<TestMethodInfo>,
    constants: HashMap<String, String>,
    typealiases: HashMap<String, String>,
}

/// Recursively walk the tree and collect a11y data.
/// `parent_enum_path` tracks the current nesting path for enum constant extraction.
/// `in_string_enum` is true when we are inside an enum that declares `: String` raw value.
fn walk_a11y(
    node: tree_sitter::Node,
    source: &[u8],
    role: &FileRole,
    parent_enum_path: Option<&str>,
    in_string_enum: bool,
    out: &mut A11yOutput,
) {
    match node.kind() {
        // ----- a11y setter call: .accessibilityIdentifier("string") -----
        "call_expression" => {
            if let Some(suffix) = get_nav_suffix(node, source) {
                if suffix == "accessibilityIdentifier" {
                    if let Some(id) = extract_first_call_arg(node, source) {
                        out.setters.push(id);
                    }
                } else if XCUI_ACCESSORS.contains(&suffix) {
                    if let Some(id) = extract_first_call_arg(node, source) {
                        out.queries.push(id);
                    }
                }
            } else if !matches!(role, FileRole::UITest) {
                // Source/page-object files: capture symbolic a11y args ending in
                // .id / .rawValue / .identifier / .value — catches patterns like
                // `element(Accessibility.closeButton.id)` used by page objects.
                if let Some(id) = extract_symbolic_accessor_arg(node, source) {
                    out.queries.push(id);
                }
            }
        }

        // ----- a11y setter assignment: view.accessibilityIdentifier = "..." -----
        "assignment" => {
            if let Some(id) = extract_a11y_assignment(node, source) {
                out.setters.push(id);
            }
        }

        // ----- typealias_declaration -----
        "typealias_declaration" => {
            if let Some((alias, target)) = extract_typealias_decl(node, source) {
                out.typealiases.insert(alias, target);
            }
        }

        // ----- test function declaration (UITest files only) -----
        "function_declaration" => {
            if matches!(role, FileRole::UITest) {
                if let Some(name) = get_function_name(node, source) {
                    if name.starts_with("test") {
                        let method = extract_test_method(node, source, name);
                        out.test_methods.push(method);
                        // Don't recurse into test method bodies from the outer walk.
                        return;
                    }
                }
            }
        }

        // ----- enum/struct/class declarations — track nesting path for raw values -----
        // Extensions produce `class_declaration` nodes with a `user_type` child (not a
        // `name` field), so we check for the extension keyword first.
        "class_declaration" | "struct_declaration" | "enum_declaration" => {
            let name: Option<String> = get_extension_type_path(node, source)
                .or_else(|| get_decl_name(node, source).map(String::from));
            let new_path: Option<String> = match (parent_enum_path, &name) {
                (Some(p), Some(n)) => Some(format!("{}.{}", p, n)),
                (None, Some(n)) => Some(n.clone()),
                _ => parent_enum_path.map(str::to_string),
            };
            let new_is_string = has_string_conformance(node, source);

            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    walk_a11y(
                        child,
                        source,
                        role,
                        new_path.as_deref(),
                        new_is_string,
                        out,
                    );
                }
            }
            return;
        }

        // ----- enum_entry: extract raw value if parent is a String enum -----
        "enum_entry" => {
            if in_string_enum {
                if let Some(path) = parent_enum_path {
                    if let Some(case_name) = get_enum_entry_name(node, source) {
                        // Use explicit raw value if present; otherwise fall back to
                        // the implicit Swift String enum raw value (the case name itself).
                        let raw = extract_enum_entry_raw(node, source)
                            .unwrap_or_else(|| case_name.clone());
                        let full_path = format!("{}.{}", path, case_name);
                        out.constants.insert(full_path, raw);
                    }
                }
            }
        }

        _ => {}
    }

    // Default: recurse into all children.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_a11y(child, source, role, parent_enum_path, in_string_enum, out);
        }
    }
}

/// Extract the a11y queries and referenced types from a test method body.
fn extract_test_method(
    func_node: tree_sitter::Node,
    source: &[u8],
    name: String,
) -> TestMethodInfo {
    let mut queries = Vec::new();
    let mut ref_types: HashSet<String> = HashSet::new();

    // Walk the function body.
    for i in 0..func_node.child_count() {
        if let Some(child) = func_node.child(i) {
            if child.kind() == "function_body" {
                collect_test_body_data(child, source, &mut queries, &mut ref_types);
            }
        }
    }

    TestMethodInfo {
        name,
        a11y_queries: queries,
        referenced_types: ref_types.into_iter().collect(),
    }
}

/// Recursively collect a11y queries and page object type references from a test body.
fn collect_test_body_data(
    node: tree_sitter::Node,
    source: &[u8],
    queries: &mut Vec<A11yIdentifier>,
    ref_types: &mut HashSet<String>,
) {
    match node.kind() {
        "call_expression" => {
            // XCUIElement subscript query
            if let Some(suffix) = get_nav_suffix(node, source) {
                if XCUI_ACCESSORS.contains(&suffix) {
                    if let Some(id) = extract_first_call_arg(node, source) {
                        queries.push(id);
                    }
                }
            }
            // Page object instantiation: SomePage(app: app)
            if let Some(callee) = get_simple_callee_name(node, source) {
                if callee.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    ref_types.insert(callee.to_string());
                }
            }
        }
        _ => {}
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_test_body_data(child, source, queries, ref_types);
        }
    }
}

// ---------------------------------------------------------------------------
// Node inspection helpers
// ---------------------------------------------------------------------------

/// Get the navigation suffix name from a `call_expression` node.
/// Returns e.g. "accessibilityIdentifier" or "buttons".
fn get_nav_suffix<'a>(call_expr: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    // Find the navigation_expression child (may not be first due to anonymous nodes).
    let nav_expr = find_child_of_kind(call_expr, "navigation_expression")?;
    get_nav_suffix_of(nav_expr, source)
}

/// Get the navigation suffix name from a `navigation_expression` node.
fn get_nav_suffix_of<'a>(nav_expr: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    let suffix_field = nav_expr.child_by_field_name("suffix")?;
    if suffix_field.kind() != "navigation_suffix" {
        return None;
    }
    let ident = suffix_field.child_by_field_name("suffix")?;
    ident.utf8_text(source).ok()
}

/// Extract the first value argument from a `call_expression`.
fn extract_first_call_arg(call_expr: tree_sitter::Node, source: &[u8]) -> Option<A11yIdentifier> {
    let call_suffix = find_child_of_kind(call_expr, "call_suffix")?;
    let value_args = find_child_of_kind(call_suffix, "value_arguments")?;
    for i in 0..value_args.child_count() {
        let child = value_args.child(i)?;
        if child.kind() == "value_argument" {
            // The argument value is in the "value" field or is the first named child.
            let value_node = child
                .child_by_field_name("value")
                .or_else(|| find_named_child(child))?;
            return a11y_id_from_node(value_node, source);
        }
    }
    None
}

/// Extract an a11y identifier from an `assignment` node targeting `.accessibilityIdentifier`.
fn extract_a11y_assignment(node: tree_sitter::Node, source: &[u8]) -> Option<A11yIdentifier> {
    let target = node.child_by_field_name("target")?;
    if target.kind() != "directly_assignable_expression" {
        return None;
    }
    let nav_expr = find_child_of_kind(target, "navigation_expression")?;
    if get_nav_suffix_of(nav_expr, source)? == "accessibilityIdentifier" {
        let result = node.child_by_field_name("result")?;
        a11y_id_from_node(result, source)
    } else {
        None
    }
}

/// Convert a tree node (string literal or navigation expression) into an A11yIdentifier.
fn a11y_id_from_node(node: tree_sitter::Node, source: &[u8]) -> Option<A11yIdentifier> {
    match node.kind() {
        "line_string_literal" => {
            extract_string_literal(node, source).map(A11yIdentifier::Literal)
        }
        "navigation_expression" => {
            let path = extract_nav_path(node, source);
            let stripped = strip_accessor_suffix(&path).to_string();
            if stripped.is_empty() || is_builtin_type(&stripped) {
                None
            } else {
                Some(A11yIdentifier::Symbolic(stripped))
            }
        }
        "simple_identifier" => {
            let name = node.utf8_text(source).ok()?;
            if name.is_empty() || is_builtin_type(name) {
                None
            } else {
                Some(A11yIdentifier::Symbolic(name.to_string()))
            }
        }
        _ => None,
    }
}

/// Extract text from a `line_string_literal` node. Returns None if interpolated.
fn extract_string_literal(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut text = String::new();
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        match child.kind() {
            "line_str_text" => {
                if let Ok(s) = child.utf8_text(source) {
                    text.push_str(s);
                }
            }
            "string_interpolation" => {
                // Dynamic — skip
                return None;
            }
            _ => {}
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Extract the full dotted path from a `navigation_expression`.
/// E.g. `AccessibilityIdentifiers.Login.loginButton` → "AccessibilityIdentifiers.Login.loginButton"
fn extract_nav_path(node: tree_sitter::Node, source: &[u8]) -> String {
    let mut parts = Vec::new();
    collect_nav_parts(node, source, &mut parts);
    parts.join(".")
}

fn collect_nav_parts(node: tree_sitter::Node, source: &[u8], parts: &mut Vec<String>) {
    match node.kind() {
        "navigation_expression" => {
            if let Some(target) = node.child_by_field_name("target") {
                collect_nav_parts(target, source, parts);
            }
            if let Some(suffix) = node.child_by_field_name("suffix") {
                if suffix.kind() == "navigation_suffix" {
                    if let Some(ident) = suffix.child_by_field_name("suffix") {
                        if let Ok(name) = ident.utf8_text(source) {
                            parts.push(name.to_string());
                        }
                    }
                }
            }
        }
        "simple_identifier" => {
            if let Ok(name) = node.utf8_text(source) {
                parts.push(name.to_string());
            }
        }
        _ => {
            // Try children
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    collect_nav_parts(child, source, parts);
                }
            }
        }
    }
}

/// Strip known accessor suffixes like `.rawValue`, `.id`, `.identifier`.
fn strip_accessor_suffix(path: &str) -> &str {
    for suffix in &[".rawValue", ".id", ".identifier", ".value"] {
        if let Some(stripped) = path.strip_suffix(suffix) {
            return stripped;
        }
    }
    path
}

/// Extract a typealias declaration: returns (alias_name, target_path).
fn extract_typealias_decl(node: tree_sitter::Node, source: &[u8]) -> Option<(String, String)> {
    let mut alias_name: Option<String> = None;
    let mut target_parts: Vec<String> = Vec::new();

    for i in 0..node.child_count() {
        let child = node.child(i)?;
        match child.kind() {
            "type_identifier" if alias_name.is_none() => {
                alias_name = child.utf8_text(source).ok().map(String::from);
            }
            "user_type" => {
                for j in 0..child.child_count() {
                    let part = child.child(j)?;
                    if part.kind() == "type_identifier" {
                        if let Ok(name) = part.utf8_text(source) {
                            target_parts.push(name.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let alias = alias_name?;
    if target_parts.is_empty() {
        return None;
    }
    Some((alias, target_parts.join(".")))
}

/// Check if a type declaration has `: String` in its inheritance clause.
/// In tree-sitter-swift the AST is: inheritance_specifier → user_type → type_identifier.
/// There is no named "inherits_from" field, so we iterate children directly.
fn has_string_conformance(decl_node: tree_sitter::Node, source: &[u8]) -> bool {
    for i in 0..decl_node.child_count() {
        let child = match decl_node.child(i) {
            Some(c) => c,
            None => continue,
        };
        if child.kind() != "inheritance_specifier" {
            continue;
        }
        for j in 0..child.child_count() {
            if let Some(type_node) = child.child(j) {
                if type_node.kind() == "user_type" {
                    for k in 0..type_node.child_count() {
                        if let Some(ident) = type_node.child(k) {
                            if ident.kind() == "type_identifier"
                                && ident.utf8_text(source).ok() == Some("String")
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Get the name of a type declaration (class/struct/enum).
fn get_decl_name<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(str::to_string)
}

/// Get the simple_identifier name of an enum_entry.
fn get_enum_entry_name<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(str::to_string)
}

/// Extract the raw string value from an `enum_entry` that has a string literal.
fn extract_enum_entry_raw(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let raw_value = node.child_by_field_name("raw_value")?;
    if raw_value.kind() == "line_string_literal" {
        extract_string_literal(raw_value, source)
    } else {
        None
    }
}

/// Get the function name from a `function_declaration` node.
fn get_function_name<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(str::to_string)
}

/// Get the simple callee name if a call_expression calls a plain identifier (e.g., `LoginPage(...)`).
fn get_simple_callee_name<'a>(call_expr: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    for i in 0..call_expr.child_count() {
        let child = call_expr.child(i)?;
        if child.kind() == "simple_identifier" {
            return child.utf8_text(source).ok();
        }
        if child.kind() == "navigation_expression" {
            // Don't recurse further — we only want top-level callee
            break;
        }
    }
    None
}

/// Find the first child node with a specific kind.
fn find_child_of_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == kind {
                return Some(child);
            }
        }
    }
    None
}

/// Find the first named (non-anonymous) child of a node.
fn find_named_child(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.is_named() {
                return Some(child);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Existing symbol extraction (unchanged)
// ---------------------------------------------------------------------------

/// Recursively walk the syntax tree and extract symbols.
/// `parent_type` tracks the enclosing type for nested type disambiguation.
fn extract_symbols(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    defines: &mut Vec<String>,
    references: &mut Vec<String>,
    conformances: &mut Vec<String>,
    imports: &mut Vec<String>,
    view_embeddings: &mut Vec<String>,
    in_view_body: bool,
    parent_type: Option<&str>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();

        match kind {
            // Type declarations — extract the name child.
            // Nested types are prefixed with parent to avoid collisions
            // (e.g., Avatar.Constants vs TrainingInteractorImpl.Constants).
            "class_declaration" | "struct_declaration" | "enum_declaration"
            | "protocol_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(source) {
                        if let Some(parent) = parent_type {
                            // Nested type: define both prefixed and unprefixed for matching.
                            defines.push(format!("{}.{}", parent, name));
                        } else {
                            defines.push(name.to_string());
                        }

                        // Recurse into this type's body with it as parent context.
                        if cursor.goto_first_child() {
                            let owned_name = name.to_string();
                            extract_symbols(
                                cursor,
                                source,
                                defines,
                                references,
                                conformances,
                                imports,
                                view_embeddings,
                                in_view_body,
                                Some(&owned_name),
                            );
                            cursor.goto_parent();
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                        continue;
                    }
                }
                // Check for inheritance clause within this declaration.
                extract_inheritance(node, source, conformances);
            }

            // Import declarations.
            "import_declaration" => {
                // The module name is typically the second child (after `import` keyword).
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "identifier" || child.kind() == "simple_identifier" {
                            if let Ok(name) = child.utf8_text(source) {
                                imports.push(name.to_string());
                            }
                        }
                    }
                }
            }

            // Type references (type annotations like `: SomeType`).
            "type_identifier" => {
                if let Ok(name) = node.utf8_text(source) {
                    if !is_builtin_type(name) {
                        references.push(name.to_string());
                        if in_view_body {
                            let first_char = name.chars().next().unwrap_or('a');
                            if first_char.is_uppercase() {
                                view_embeddings.push(name.to_string());
                            }
                        }
                    }
                }
            }

            // Constructor calls in view body (e.g., `MyChildView(...)` in SwiftUI).
            // tree-sitter-swift parses these as simple_identifier, not type_identifier.
            // Only capture when parent is call_expression (actual constructor call),
            // not navigation_expression (property access like Colors.primary).
            "simple_identifier" if in_view_body => {
                if let Ok(name) = node.utf8_text(source) {
                    let first_char = name.chars().next().unwrap_or('a');
                    if first_char.is_uppercase()
                        && !is_builtin_type(name)
                        && node
                            .parent()
                            .map(|p| p.kind() == "call_expression")
                            .unwrap_or(false)
                    {
                        view_embeddings.push(name.to_string());
                    }
                }
            }

            // Property declarations — check if this is `var body: some View`.
            // AST: property_declaration → pattern → simple_identifier "body"
            //                           → computed_property → statements → ...
            "property_declaration" => {
                let is_body = (0..node.child_count()).any(|i| {
                    node.child(i)
                        .filter(|c| c.kind() == "pattern")
                        .and_then(|c| c.child(0))
                        .and_then(|n| n.utf8_text(source).ok())
                        .map(|name| name == "body")
                        .unwrap_or(false)
                });

                if is_body {
                    // Recurse into children with view_body flag set.
                    if cursor.goto_first_child() {
                        extract_symbols(
                            cursor,
                            source,
                            defines,
                            references,
                            conformances,
                            imports,
                            view_embeddings,
                            true,
                            parent_type,
                        );
                        cursor.goto_parent();
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                        continue;
                    }
                }
            }

            _ => {}
        }

        // Recurse into children.
        if cursor.goto_first_child() {
            extract_symbols(
                cursor,
                source,
                defines,
                references,
                conformances,
                imports,
                view_embeddings,
                in_view_body,
                parent_type,
            );
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Extract protocol conformances from inheritance clauses.
fn extract_inheritance(
    decl_node: tree_sitter::Node,
    source: &[u8],
    conformances: &mut Vec<String>,
) {
    for i in 0..decl_node.child_count() {
        if let Some(child) = decl_node.child(i) {
            if child.kind() == "inheritance_specifier"
                || child.kind() == "type_identifier"
                    && child.parent().map(|p| p.kind()) == Some("inheritance_specifier")
            {
                walk_inheritance(child, source, conformances);
            }
            if child.kind().contains("inheritance") {
                walk_inheritance(child, source, conformances);
            }
        }
    }
}

fn walk_inheritance(node: tree_sitter::Node, source: &[u8], conformances: &mut Vec<String>) {
    if node.kind() == "type_identifier" {
        if let Ok(name) = node.utf8_text(source) {
            if !is_builtin_type(name) {
                conformances.push(name.to_string());
            }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_inheritance(child, source, conformances);
        }
    }
}

/// Extract the full dotted type path from an extension `class_declaration`.
/// `extension Foo.Bar { ... }` produces a `class_declaration` with an `extension`
/// keyword child and a `user_type` child instead of a named `name` field.
fn get_extension_type_path(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let is_extension = (0..node.child_count())
        .filter_map(|i| node.child(i))
        .any(|c| c.kind() == "extension");
    if !is_extension {
        return None;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "user_type" {
                let parts: Vec<String> = (0..child.child_count())
                    .filter_map(|j| child.child(j))
                    .filter(|c| c.kind() == "type_identifier")
                    .filter_map(|c| c.utf8_text(source).ok().map(String::from))
                    .collect();
                if !parts.is_empty() {
                    return Some(parts.join("."));
                }
            }
        }
    }
    None
}

/// For source/page-object files: extract a symbolic a11y ID from the first argument
/// of a call_expression, but only when that argument is a navigation_expression
/// ending with a known accessor suffix (.id, .rawValue, .identifier, .value).
/// Catches patterns like `element(Accessibility.closeButton.id)`.
fn extract_symbolic_accessor_arg(call_expr: tree_sitter::Node, source: &[u8]) -> Option<A11yIdentifier> {
    let call_suffix = find_child_of_kind(call_expr, "call_suffix")?;
    let value_args = find_child_of_kind(call_suffix, "value_arguments")?;
    for i in 0..value_args.child_count() {
        let child = value_args.child(i)?;
        if child.kind() == "value_argument" {
            let value_node = child
                .child_by_field_name("value")
                .or_else(|| find_named_child(child))?;
            if value_node.kind() == "navigation_expression" {
                let path = extract_nav_path(value_node, source);
                let stripped = strip_accessor_suffix(&path);
                if stripped != path && !stripped.is_empty() && !is_builtin_type(stripped) {
                    return Some(A11yIdentifier::Symbolic(stripped.to_string()));
                }
            }
            return None;
        }
    }
    None
}

fn is_builtin_type(name: &str) -> bool {
    // Single-character or very short type params (generic type parameters like T, U, V).
    if name.len() <= 1 {
        return true;
    }

    // Common ambiguous names that exist in many modules and create false edges.
    if matches!(
        name,
        "Constants" | "Strings" | "Config" | "Configuration"
            | "Context" | "State" | "Action" | "Event" | "Kind"
            | "Style" | "Theme" | "Model" | "ViewModel" | "Interactor"
            | "Router" | "Builder" | "Factory" | "Mapper" | "Coordinator"
            | "Presenter" | "Provider" | "Service" | "Repository"
            | "Manager" | "Handler" | "Delegate" | "DataSource"
            | "Request" | "Response" | "Section" | "Item" | "Cell"
    ) {
        return true;
    }

    matches!(
        name,
        // Swift standard library.
        "String" | "Int" | "Int8" | "Int16" | "Int32" | "Int64"
            | "UInt" | "UInt8" | "UInt16" | "UInt32" | "UInt64"
            | "Double" | "Float" | "Float16" | "Bool"
            | "Array" | "Dictionary" | "Set" | "Optional"
            | "Any" | "AnyObject" | "AnyHashable" | "AnyClass"
            | "Void" | "Never" | "Data" | "Date" | "URL" | "UUID"
            | "Error" | "Result" | "Character" | "Substring"
            | "Range" | "ClosedRange" | "PartialRangeFrom"
            // Swift protocols.
            | "Codable" | "Encodable" | "Decodable"
            | "Hashable" | "Equatable" | "Comparable"
            | "Identifiable" | "Sendable" | "CustomStringConvertible"
            | "CustomDebugStringConvertible" | "RawRepresentable"
            | "CaseIterable" | "IteratorProtocol" | "Sequence" | "Collection"
            | "RandomAccessCollection" | "BidirectionalCollection"
            | "ExpressibleByStringLiteral" | "ExpressibleByIntegerLiteral"
            | "ExpressibleByArrayLiteral" | "ExpressibleByDictionaryLiteral"
            | "LosslessStringConvertible"
            // Swift keywords/pseudo-types.
            | "View" | "some" | "Self" | "Type"
            // Foundation.
            | "NSObject" | "NSCoding" | "NSSecureCoding" | "NSCopying"
            | "NSError" | "NSString" | "NSNumber" | "NSArray" | "NSDictionary"
            | "NSNotification" | "Notification" | "NotificationCenter"
            | "JSONEncoder" | "JSONDecoder" | "PropertyListEncoder" | "PropertyListDecoder"
            | "DispatchQueue" | "DispatchGroup" | "DispatchSemaphore"
            | "OperationQueue" | "Operation"
            | "UserDefaults" | "FileManager" | "Bundle" | "ProcessInfo"
            | "Timer" | "TimeInterval" | "DateFormatter" | "DateComponents"
            | "Calendar" | "Locale" | "TimeZone"
            | "URLRequest" | "URLResponse" | "URLSession" | "URLSessionTask"
            | "HTTPURLResponse"
            // UIKit.
            | "UIView" | "UIViewController" | "UINavigationController"
            | "UITabBarController" | "UITableView" | "UITableViewCell"
            | "UICollectionView" | "UICollectionViewCell"
            | "UITableViewDataSource" | "UITableViewDelegate"
            | "UICollectionViewDataSource" | "UICollectionViewDelegate"
            | "UILabel" | "UIButton" | "UIImageView" | "UIImage"
            | "UITextField" | "UITextView" | "UISwitch" | "UISlider"
            | "UIStackView" | "UIScrollView" | "UIControl"
            | "UIColor" | "UIFont" | "UIEdgeInsets" | "UIApplication"
            | "UIWindow" | "UIScene" | "UIScreen"
            | "UIStoryboard" | "UIStoryboardSegue" | "UINib"
            | "UIBarButtonItem" | "UIAlertController" | "UIAlertAction"
            | "UIGestureRecognizer" | "UITapGestureRecognizer"
            | "UILongPressGestureRecognizer" | "UIPanGestureRecognizer"
            | "UIResponder" | "UIEvent" | "UITouch"
            | "CGFloat" | "CGPoint" | "CGSize" | "CGRect" | "CGAffineTransform"
            | "NSLayoutConstraint" | "NSLayoutAnchor"
            // SwiftUI.
            | "State" | "Binding" | "ObservedObject" | "StateObject"
            | "EnvironmentObject" | "Environment" | "Published"
            | "ObservableObject" | "PreviewProvider" | "App" | "Scene"
            | "WindowGroup" | "NavigationView" | "NavigationLink"
            | "List" | "ForEach" | "HStack" | "VStack" | "ZStack" | "LazyVStack" | "LazyHStack"
            | "Text" | "Image" | "Button" | "Toggle" | "Slider" | "Picker" | "Label"
            | "Spacer" | "Divider" | "EmptyView" | "AnyView"
            | "ScrollView" | "ScrollViewReader" | "TabView" | "NavigationStack"
            | "Menu" | "ProgressView" | "ContentUnavailableView" | "Section"
            | "ToolbarItem" | "ToolbarItemGroup" | "DisclosureGroup" | "Group"
            | "Color" | "Font" | "EdgeInsets" | "Alignment"
            | "GeometryReader" | "GeometryProxy"
            | "ViewModifier" | "ViewBuilder"
            | "Task" | "MainActor"
            // Combine.
            | "Publisher" | "Subscriber" | "Subscription"
            | "AnyPublisher" | "AnyCancellable" | "Cancellable"
            | "PassthroughSubject" | "CurrentValueSubject" | "Future"
            // XCTest.
            | "XCTestCase" | "XCTestExpectation" | "XCTAssert"
            | "XCUIApplication" | "XCUIElement" | "XCUIElementQuery"
    )
}
