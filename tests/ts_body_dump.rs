#[test]
fn test_view_body_detection_sets_in_view_body() {
    // Verify that tree-sitter-swift parses `var body: some View` as
    // property_declaration → pattern → simple_identifier "body"
    // and that constructor calls inside are simple_identifier nodes.
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_swift::LANGUAGE;
    parser.set_language(&language.into()).unwrap();

    let source = r#"struct MyView: View {
    var body: some View {
        ChildView(name: "test")
        OtherView()
    }
}"#;

    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    // Find the property_declaration for `body`.
    fn find_node<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = find_node(cursor.node(), kind) {
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }

    let prop = find_node(root, "property_declaration").expect("should find property_declaration");

    // The name "body" is in a pattern child, not on computed_property.
    let has_body_pattern = (0..prop.child_count()).any(|i| {
        prop.child(i)
            .filter(|c| c.kind() == "pattern")
            .and_then(|c| c.child(0))
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(|name| name == "body")
            .unwrap_or(false)
    });
    assert!(has_body_pattern, "property_declaration should have pattern child with 'body'");

    // The computed_property child should NOT have a "name" field.
    let computed = find_node(prop, "computed_property").expect("should find computed_property");
    assert!(
        computed.child_by_field_name("name").is_none(),
        "computed_property should NOT have a 'name' field (it's on the parent)"
    );

    // Constructor calls inside body should be simple_identifier, not type_identifier.
    fn collect_simple_ids<'a>(node: tree_sitter::Node<'a>, source: &'a [u8], ids: &mut Vec<String>) {
        if node.kind() == "simple_identifier" {
            if let Ok(text) = node.utf8_text(source) {
                if text.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    ids.push(text.to_string());
                }
            }
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                collect_simple_ids(cursor.node(), source, ids);
                if !cursor.goto_next_sibling() { break; }
            }
        }
    }

    let mut upper_ids = Vec::new();
    collect_simple_ids(computed, source.as_bytes(), &mut upper_ids);
    assert!(
        upper_ids.contains(&"ChildView".to_string()),
        "ChildView should appear as uppercase simple_identifier in body. Found: {:?}",
        upper_ids
    );
    assert!(
        upper_ids.contains(&"OtherView".to_string()),
        "OtherView should appear as uppercase simple_identifier in body. Found: {:?}",
        upper_ids
    );

    eprintln!("Uppercase simple_identifiers in body: {:?}", upper_ids);
}
