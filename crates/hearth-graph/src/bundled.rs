use std::borrow::Cow;

use tree_sitter::Language;

use crate::{ImportKind, ImportSpec, LanguageRegistry, LanguageSpec};

const JAVASCRIPT_IMPORTS_QUERY: &str = include_str!("../queries/javascript/imports.scm");
const TYPESCRIPT_IMPORTS_QUERY: &str = include_str!("../queries/typescript/imports.scm");
const VUE_SCRIPT_INJECTIONS_QUERY: &str = include_str!("../queries/vue/injections.scm");

fn language_spec(
    name: &'static str,
    language: Language,
    extensions: &[&str],
    tags_query: Cow<'static, str>,
) -> LanguageSpec {
    language_spec_with_imports(name, language, extensions, tags_query, None)
}

fn language_spec_with_imports(
    name: &'static str,
    language: Language,
    extensions: &[&str],
    tags_query: Cow<'static, str>,
    imports: Option<ImportSpec>,
) -> LanguageSpec {
    let spec = LanguageSpec::new(name, language, extensions).with_tags_query(tags_query);
    match imports {
        Some(imports) => spec.with_imports(imports),
        None => spec,
    }
}

fn language_spec_merging_adjacent_definitions(
    name: &'static str,
    language: Language,
    extensions: &[&str],
    tags_query: Cow<'static, str>,
) -> LanguageSpec {
    language_spec(name, language, extensions, tags_query)
        .with_merge_adjacent_same_name_definitions(true)
}

fn import_kind(capture: &str) -> ImportKind {
    match capture {
        "import.source.static" => ImportKind::EsStatic,
        "import.source.reexport" => ImportKind::EsReexport,
        "import.source.dynamic" => ImportKind::EsDynamic,
        "import.source.commonjs" => ImportKind::CommonJs,
        "import.source.tsrequire" => ImportKind::TsImportRequire,
        _ => unreachable!("unexpected import capture: {capture}"),
    }
}

impl LanguageRegistry {
    /// Creates a registry containing Hearth's bundled language grammars.
    #[must_use]
    pub fn bundled() -> Self {
        let mut registry = Self::empty();

        registry.register(language_spec_with_imports(
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            &["rs"],
            Cow::Borrowed(tree_sitter_rust::TAGS_QUERY),
            Some(ImportSpec::Custom(crate::imports::rust::extract)),
        ));
        registry.register(language_spec_with_imports(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            &["ts", "mts", "cts"],
            Cow::Owned(format!(
                "{}\n{}",
                tree_sitter_javascript::TAGS_QUERY,
                tree_sitter_typescript::TAGS_QUERY
            )),
            Some(ImportSpec::Query {
                source: Cow::Owned(format!(
                    "{JAVASCRIPT_IMPORTS_QUERY}\n{TYPESCRIPT_IMPORTS_QUERY}"
                )),
                kind_map: import_kind,
            }),
        ));
        registry.register(language_spec_with_imports(
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            &["tsx"],
            Cow::Owned(format!(
                "{}\n{}",
                tree_sitter_javascript::TAGS_QUERY,
                tree_sitter_typescript::TAGS_QUERY
            )),
            Some(ImportSpec::Query {
                source: Cow::Owned(format!(
                    "{JAVASCRIPT_IMPORTS_QUERY}\n{TYPESCRIPT_IMPORTS_QUERY}"
                )),
                kind_map: import_kind,
            }),
        ));
        registry.register(language_spec_with_imports(
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            &["js", "mjs", "cjs"],
            Cow::Borrowed(tree_sitter_javascript::TAGS_QUERY),
            Some(ImportSpec::Query {
                source: Cow::Borrowed(JAVASCRIPT_IMPORTS_QUERY),
                kind_map: import_kind,
            }),
        ));
        registry.register(language_spec_with_imports(
            "jsx",
            tree_sitter_javascript::LANGUAGE.into(),
            &["jsx"],
            Cow::Borrowed(tree_sitter_javascript::TAGS_QUERY),
            Some(ImportSpec::Query {
                source: Cow::Borrowed(JAVASCRIPT_IMPORTS_QUERY),
                kind_map: import_kind,
            }),
        ));
        registry.register(language_spec(
            "go",
            tree_sitter_go::LANGUAGE.into(),
            &["go"],
            Cow::Borrowed(tree_sitter_go::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "python",
            tree_sitter_python::LANGUAGE.into(),
            &["py"],
            Cow::Borrowed(tree_sitter_python::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "ruby",
            tree_sitter_ruby::LANGUAGE.into(),
            &["rb", "rake", "gemspec"],
            Cow::Borrowed(tree_sitter_ruby::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "c",
            tree_sitter_c::LANGUAGE.into(),
            &["c", "h"],
            Cow::Borrowed(tree_sitter_c::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "cpp",
            tree_sitter_cpp::LANGUAGE.into(),
            &["cpp", "cc", "cxx", "hpp", "hxx"],
            Cow::Borrowed(tree_sitter_cpp::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "java",
            tree_sitter_java::LANGUAGE.into(),
            &["java"],
            Cow::Borrowed(tree_sitter_java::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "csharp",
            tree_sitter_c_sharp::LANGUAGE.into(),
            &["cs"],
            Cow::Borrowed(include_str!("../queries/c_sharp/tags.scm")),
        ));
        registry.register(language_spec(
            "zig",
            tree_sitter_zig::LANGUAGE.into(),
            &["zig"],
            Cow::Borrowed(include_str!("../queries/zig/tags.scm")),
        ));
        registry.register(language_spec(
            "bash",
            tree_sitter_bash::LANGUAGE.into(),
            &["sh", "bash", "zsh"],
            Cow::Borrowed(include_str!("../queries/bash/tags.scm")),
        ));
        registry.register(language_spec_merging_adjacent_definitions(
            "haskell",
            tree_sitter_haskell::LANGUAGE.into(),
            &["hs", "lhs"],
            Cow::Borrowed(include_str!("../queries/haskell/tags.scm")),
        ));
        registry.register(language_spec(
            "lua",
            tree_sitter_lua::LANGUAGE.into(),
            &["lua"],
            Cow::Borrowed(tree_sitter_lua::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "php",
            tree_sitter_php::LANGUAGE_PHP.into(),
            &["php"],
            Cow::Borrowed(tree_sitter_php::TAGS_QUERY),
        ));
        registry.register(language_spec(
            "swift",
            tree_sitter_swift::LANGUAGE.into(),
            &["swift"],
            Cow::Borrowed(tree_sitter_swift::TAGS_QUERY),
        ));
        registry.register(
            LanguageSpec::new("vue", tree_sitter_vue3::LANGUAGE.into(), ["vue"])
                .with_injections_query(VUE_SCRIPT_INJECTIONS_QUERY),
        );
        registry.register(language_spec(
            "markdown",
            tree_sitter_md::LANGUAGE.into(),
            &["md", "markdown"],
            Cow::Borrowed(include_str!("../queries/markdown/tags.scm")),
        ));

        registry
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ParserPool;

    #[test]
    fn all_bundled_tags_queries_compile_and_are_pool_reachable() {
        let registry = LanguageRegistry::bundled();
        assert_eq!(registry.iter().len(), 20);

        let mut pool = ParserPool::new(&registry);
        let mut query_count = 0;
        for (id, spec) in registry.iter() {
            let Some(source) = spec.tags_query.as_deref() else {
                assert_eq!(spec.name, "vue");
                assert!(pool.tags_query(id).is_none());
                continue;
            };
            query_count += 1;
            tree_sitter::Query::new(&spec.language, source)
                .unwrap_or_else(|error| panic!("{} tags query failed: {error}", spec.name));
            assert!(
                pool.tags_query(id).is_some(),
                "{} is not reachable through ParserPool::tags_query",
                spec.name
            );
        }
        assert_eq!(query_count, 19);
    }

    #[test]
    fn bundled_injection_queries_compile_and_are_pool_reachable() {
        let registry = LanguageRegistry::bundled();
        let mut pool = ParserPool::new(&registry);
        let mut query_count = 0;

        for (id, spec) in registry.iter() {
            let Some(source) = spec.injections_query.as_deref() else {
                assert!(pool.injections_query(id).is_none(), "{}", spec.name);
                continue;
            };
            query_count += 1;
            tree_sitter::Query::new(&spec.language, source)
                .unwrap_or_else(|error| panic!("{} injections query failed: {error}", spec.name));
            assert!(
                pool.injections_query(id).is_some(),
                "{} is not reachable through ParserPool::injections_query",
                spec.name
            );
        }

        assert_eq!(query_count, 1);
    }

    #[test]
    fn all_bundled_import_queries_compile_and_are_pool_reachable() {
        let registry = LanguageRegistry::bundled();
        let mut pool = ParserPool::new(&registry);
        let mut query_count = 0;

        for (id, spec) in registry.iter() {
            match spec.imports.as_ref() {
                Some(ImportSpec::Query { source, .. }) => {
                    query_count += 1;
                    tree_sitter::Query::new(&spec.language, source).unwrap_or_else(|error| {
                        panic!("{} imports query failed: {error}", spec.name)
                    });
                    assert!(
                        pool.imports_query(id).is_some(),
                        "{} is not reachable through ParserPool::imports_query",
                        spec.name
                    );
                }
                Some(ImportSpec::Custom(_)) | None => {
                    assert!(pool.imports_query(id).is_none(), "{}", spec.name);
                }
            }
        }

        assert_eq!(query_count, 4);
    }

    #[test]
    fn register_uses_last_extension_owner_and_increments_generation() {
        let mut registry = LanguageRegistry::bundled();
        let original_id = registry
            .for_path(Path::new("main.rs"))
            .expect("bundled Rust extension");
        let original_generation = registry.generation();

        let replacement_id = registry.register(language_spec(
            "replacement-rust",
            tree_sitter_rust::LANGUAGE.into(),
            &["rs"],
            Cow::Borrowed(tree_sitter_rust::TAGS_QUERY),
        ));

        assert_ne!(replacement_id, original_id);
        assert_eq!(
            registry.for_path(Path::new("main.rs")),
            Some(replacement_id)
        );
        assert_eq!(registry.generation(), original_generation + 1);
    }

    #[test]
    fn supports_symbols_for_bundled_module_extensions() {
        let registry = LanguageRegistry::bundled();

        for path in [
            "module.mjs",
            "module.cjs",
            "module.mts",
            "module.cts",
            "module.ts",
            "component.tsx",
            "component.vue",
            "lib.rs",
        ] {
            assert!(registry.supports_symbols(Path::new(path)), "{path}");
        }

        for path in ["style.css", "Makefile"] {
            assert!(!registry.supports_symbols(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn supports_imports_for_bundled_module_extensions() {
        let registry = LanguageRegistry::bundled();

        for path in [
            "module.mjs",
            "module.cjs",
            "module.mts",
            "module.cts",
            "module.ts",
            "component.tsx",
            "component.jsx",
            "component.vue",
            "lib.rs",
        ] {
            assert!(registry.supports_imports(Path::new(path)), "{path}");
        }

        for path in ["main.go", "style.css", "Makefile"] {
            assert!(!registry.supports_imports(Path::new(path)), "{path}");
        }
    }
}
