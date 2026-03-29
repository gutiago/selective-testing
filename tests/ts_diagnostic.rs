#[test]
fn test_tree_sitter_swift_setup() {
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_swift::LANGUAGE;
    let lang: tree_sitter::Language = language.into();
    eprintln!("Language version: {}", lang.version());
    eprintln!("Node kind count: {}", lang.node_kind_count());
    parser.set_language(&lang).expect("set_language should work");

    let source = "class Foo { }";
    let tree = parser.parse(source, None).expect("parse should succeed");
    eprintln!("Root: {}", tree.root_node().to_sexp());
    assert!(tree.root_node().child_count() > 0);
}
