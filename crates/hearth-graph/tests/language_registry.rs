#![cfg(feature = "bundled-languages")]

use std::path::Path;

use hearth_graph::{
    LanguageRegistry, LanguageSpec, MAX_SYMBOLS_PER_FILE, ParserPool, extract_symbols,
};

#[test]
fn host_registers_a_custom_grammar_through_the_public_builder_api() {
    let extensions: &[&str] = &["host-rs"];
    let spec = LanguageSpec::new("host-rust", tree_sitter_rust::LANGUAGE.into(), extensions);
    assert!(spec.tags_query.is_none());
    assert!(spec.injections_query.is_none());
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

#[test]
fn host_registers_an_injection_grammar_through_the_public_builder_api() {
    let mut registry = LanguageRegistry::empty();
    registry.register(
        LanguageSpec::new(
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            ["host-js"],
        )
        .with_tags_query(tree_sitter_javascript::TAGS_QUERY),
    );
    let vue_id = registry.register(
        LanguageSpec::new("host-vue", tree_sitter_vue3::LANGUAGE.into(), ["host-vue"])
            .with_injections_query(tree_sitter_vue3::INJECTIONS_QUERY),
    );

    assert_eq!(registry.for_name("host-vue"), Some(vue_id));
    assert!(registry.supports_symbols(Path::new("src/App.host-vue")));

    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(
        "<script>export function injectedSymbol() {}</script>\n",
        "src/App.host-vue",
        &mut pool,
    );

    assert_eq!(
        symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["injectedSymbol"]
    );
}

#[test]
fn injected_symbols_are_merged_before_the_global_file_limit() {
    const VUE_TAGS_QUERY: &str = "\
((start_tag (tag_name) @name) @definition.class)
((self_closing_tag (tag_name) @name) @definition.class)
";

    let mut registry = LanguageRegistry::empty();
    registry.register(
        LanguageSpec::new(
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            ["host-js"],
        )
        .with_tags_query(tree_sitter_javascript::TAGS_QUERY),
    );
    registry.register(
        LanguageSpec::new("host-vue", tree_sitter_vue3::LANGUAGE.into(), ["host-vue"])
            .with_tags_query(VUE_TAGS_QUERY)
            .with_injections_query(tree_sitter_vue3::INJECTIONS_QUERY),
    );

    let mut source = String::from(
        "<script>export function injectedBeforeDirectLimit() {}</script>\n<template>\n",
    );
    for _ in 0..MAX_SYMBOLS_PER_FILE {
        source.push_str("<x />\n");
    }
    source.push_str("</template>\n");

    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(&source, "src/Limited.host-vue", &mut pool);

    assert_eq!(symbols.len(), MAX_SYMBOLS_PER_FILE);
    assert_eq!(symbols[0].name, "script");
    assert_eq!(symbols[1].name, "injectedBeforeDirectLimit");
}
