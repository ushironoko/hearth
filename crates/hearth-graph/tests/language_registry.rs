#![cfg(feature = "bundled-languages")]

use std::path::Path;

use hearth_graph::{LanguageRegistry, LanguageSpec, ParserPool, extract_symbols};

#[test]
fn host_registers_a_custom_grammar_through_the_public_builder_api() {
    let extensions: &[&str] = &["host-rs"];
    let spec = LanguageSpec::new("host-rust", tree_sitter_rust::LANGUAGE.into(), extensions);
    assert!(spec.tags_query.is_none());
    assert!(spec.imports.is_none());
    assert!(!spec.merge_adjacent_same_name_definitions);

    let spec = spec
        .with_tags_query(tree_sitter_rust::TAGS_QUERY)
        .with_merge_adjacent_same_name_definitions(true);
    let mut registry = LanguageRegistry::empty();
    let id = registry.register(spec);

    assert_eq!(registry.for_path(Path::new("src/lib.host-rs")), Some(id));
    assert_eq!(
        registry.get(id).expect("registered language").name,
        "host-rust"
    );

    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols("fn custom_symbol() {}\n", "src/lib.host-rs", &mut pool);

    assert!(symbols.iter().any(|symbol| symbol.name == "custom_symbol"));
}
