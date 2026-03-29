use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, warn};
use tree_sitter::Parser;

use super::{DataSource, SourceEdge};
use crate::graph::model::{EdgeKind, FileNode, FileRole};
use crate::swift::file_classifier;

/// Symbols extracted from a single Swift file.
#[derive(Debug)]
struct FileSymbols {
    file_id: String,
    path: PathBuf,
    defines: Vec<String>,
    references: Vec<String>,
    view_embeddings: Vec<String>,
    role: FileRole,
}

pub struct TreeSitterSource;

impl DataSource for TreeSitterSource {
    fn analyze(
        &self,
        repo_root: &Path,
        swift_files: &[PathBuf],
    ) -> Result<(Vec<FileNode>, Vec<SourceEdge>)> {
        let file_symbols: Vec<FileSymbols> = swift_files
            .par_iter()
            .filter_map(|path| {
                let rel_path = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                match parse_swift_file(path, &rel_path) {
                    Ok(symbols) => Some(symbols),
                    Err(e) => {
                        warn!(file = %rel_path, error = %e, "Failed to parse Swift file");
                        None
                    }
                }
            })
            .collect();

        // Build symbol-to-file index.
        let mut symbol_to_file: HashMap<String, Vec<String>> = HashMap::new();
        for fs in &file_symbols {
            for symbol in &fs.defines {
                symbol_to_file
                    .entry(symbol.clone())
                    .or_default()
                    .push(fs.file_id.clone());
            }
        }

        let nodes: Vec<FileNode> = file_symbols
            .iter()
            .map(|fs| FileNode {
                id: fs.file_id.clone(),
                path: fs.path.clone(),
                role: fs.role.clone(),
                module: None,
                defined_symbols: fs.defines.clone(),
                content_hash: None,
                mtime: None,
            })
            .collect();

        let mut edges = Vec::new();
        for fs in &file_symbols {
            for ref_name in &fs.references {
                if let Some(defining_files) = symbol_to_file.get(ref_name) {
                    for def_file in defining_files {
                        if def_file != &fs.file_id {
                            edges.push(SourceEdge {
                                from: def_file.clone(),
                                to: fs.file_id.clone(),
                                kind: EdgeKind::DirectReference,
                            });
                        }
                    }
                }
            }

            for view_ref in &fs.view_embeddings {
                if let Some(defining_files) = symbol_to_file.get(view_ref) {
                    for def_file in defining_files {
                        if def_file != &fs.file_id {
                            edges.push(SourceEdge {
                                from: def_file.clone(),
                                to: fs.file_id.clone(),
                                kind: EdgeKind::ViewEmbedding,
                            });
                        }
                    }
                }
            }
        }

        debug!(
            nodes = nodes.len(),
            edges = edges.len(),
            "tree-sitter analysis complete"
        );

        Ok((nodes, edges))
    }
}

/// Parse a single Swift file and extract symbols via tree walking.
fn parse_swift_file(path: &Path, file_id: &str) -> Result<FileSymbols> {
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

    // Walk the entire tree to extract symbols.
    let mut cursor = tree.root_node().walk();
    extract_symbols(
        &mut cursor,
        source_bytes,
        &mut defines,
        &mut references,
        &mut conformances,
        &mut imports,
        &mut view_embeddings,
        false, // not inside body
        None,  // no parent type at top level
    );

    // Deduplicate.
    defines.sort();
    defines.dedup();
    references.sort();
    references.dedup();

    let role = file_classifier::classify(path, &imports);

    Ok(FileSymbols {
        file_id: file_id.to_string(),
        path: path.to_path_buf(),
        defines,
        references,
        view_embeddings,
        role,
    })
}

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

            // Type references.
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

            // Computed properties — check if this is a `body` property for SwiftUI.
            "computed_property" => {
                let is_body = node
                    .child_by_field_name("name")
                    .and_then(|n| {
                        // The name might be wrapped in a pattern node.
                        if n.kind() == "pattern" {
                            n.child(0)
                        } else {
                            Some(n)
                        }
                    })
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|name| name == "body")
                    .unwrap_or(false);

                if is_body {
                    // Recurse into body with view_body flag set.
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
                // Walk inheritance specifiers for type names.
                walk_inheritance(child, source, conformances);
            }
            // Some grammars wrap in an inheritance_clause.
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
            | "List" | "ForEach" | "HStack" | "VStack" | "ZStack"
            | "Text" | "Image" | "Button" | "Toggle" | "Slider"
            | "Spacer" | "Divider" | "EmptyView" | "AnyView"
            | "Color" | "Font" | "EdgeInsets" | "Alignment"
            | "GeometryReader" | "GeometryProxy"
            | "ViewModifier" | "ViewBuilder"
            // Combine.
            | "Publisher" | "Subscriber" | "Subscription"
            | "AnyPublisher" | "AnyCancellable" | "Cancellable"
            | "PassthroughSubject" | "CurrentValueSubject" | "Future"
            // XCTest.
            | "XCTestCase" | "XCTestExpectation" | "XCTAssert"
            | "XCUIApplication" | "XCUIElement" | "XCUIElementQuery"
    )
}
