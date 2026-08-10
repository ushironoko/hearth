#![cfg(feature = "bundled-languages")]

use std::fmt::Write as _;

use hearth_graph::{
    FileAnalysis, ImportKind, LanguageRegistry, ParserPool, RawImport, analyze_source,
};

fn analyze(source: &str, path: &str) -> FileAnalysis {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    analyze_source(source, path, 0, &mut pool)
}

fn imports(source: &str, path: &str) -> Vec<(String, ImportKind)> {
    analyze(source, path)
        .imports
        .into_iter()
        .map(|import| (import.specifier.to_string(), import.kind))
        .collect()
}

#[test]
fn extracts_javascript_import_forms_and_skips_member_require() {
    let source = "\
import value from \"m\";
import \"./s\";
import \"\";
export { a } from \"./x\";
export * from \"./y\";
const dynamic = import(\"./z\");
const commonjs = require(\"./w\");
foo.require(\"not-bare\");
";

    assert_eq!(
        imports(source, "src/module.js"),
        [
            ("m".to_owned(), ImportKind::EsStatic),
            ("./s".to_owned(), ImportKind::EsStatic),
            ("".to_owned(), ImportKind::EsStatic),
            ("./x".to_owned(), ImportKind::EsReexport),
            ("./y".to_owned(), ImportKind::EsReexport),
            ("./z".to_owned(), ImportKind::EsDynamic),
            ("./w".to_owned(), ImportKind::CommonJs),
        ]
    );
}

#[test]
fn extracts_typescript_import_forms_and_ts_import_require() {
    let source = "\
import value from \"m\";
import \"./s\";
import \"\";
export { a } from \"./x\";
export * from \"./y\";
const dynamic = import(\"./z\");
const commonjs = require(\"./w\");
import type { T } from \"./t\";
import x = require(\"./r\");
";

    assert_eq!(
        imports(source, "src/module.ts"),
        [
            ("m".to_owned(), ImportKind::EsStatic),
            ("./s".to_owned(), ImportKind::EsStatic),
            ("".to_owned(), ImportKind::EsStatic),
            ("./x".to_owned(), ImportKind::EsReexport),
            ("./y".to_owned(), ImportKind::EsReexport),
            ("./z".to_owned(), ImportKind::EsDynamic),
            ("./w".to_owned(), ImportKind::CommonJs),
            ("./t".to_owned(), ImportKind::EsStatic),
            ("./r".to_owned(), ImportKind::TsImportRequire),
        ]
    );
}

#[test]
fn registered_jsx_and_tsx_queries_extract_imports() {
    assert_eq!(
        imports(
            "import React from \"react\";\nconst view = <div />;\n",
            "src/view.jsx"
        ),
        [("react".to_owned(), ImportKind::EsStatic)]
    );
    assert_eq!(
        imports(
            "import type { FC } from \"react\";\nconst View: FC = () => <div />;\n",
            "src/view.tsx"
        ),
        [("react".to_owned(), ImportKind::EsStatic)]
    );
}

#[test]
fn extracts_vue_typescript_imports_once_with_whole_file_locations() {
    let source = "\
<template><Child /></template>
<script setup lang=\"ts\">
import { ref } from \"vue\";
import Child from \"./Child.vue\";
export { helper } from \"./helper\";
const lazy = import(\"./lazy\");
const opaque = import(target);
</script>
";
    let analysis = analyze(source, "src/App.vue");

    assert_eq!(analysis.language.as_deref(), Some("vue"));
    assert_eq!(
        analysis
            .imports
            .iter()
            .map(|import| (import.specifier.as_str(), import.kind, import.line))
            .collect::<Vec<_>>(),
        [
            ("vue", ImportKind::EsStatic, 3),
            ("./Child.vue", ImportKind::EsStatic, 4),
            ("./helper", ImportKind::EsReexport, 5),
            ("./lazy", ImportKind::EsDynamic, 6),
        ]
    );
    assert!(analysis.has_opaque_imports);
    for import in &analysis.imports {
        let literal = &source[import.span.0 as usize..import.span.1 as usize];
        assert_eq!(literal, format!("\"{}\"", import.specifier));
    }
}

#[test]
fn extracts_vue_javascript_jsx_and_tsx_script_imports() {
    for attribute in [
        "",
        " lang=\"jsx\"",
        " lang=jsx",
        " lang=\"tsx\"",
        " lang=tsx",
    ] {
        let source = format!(
            "<script{attribute}>\nimport View from \"./View\";\nexport function renderView() {{}}\n</script>\n"
        );

        assert_eq!(
            imports(&source, "src/View.vue"),
            [("./View".to_owned(), ImportKind::EsStatic)],
            "{attribute}"
        );
    }
}

#[test]
fn vue_template_interpolations_are_not_module_injections() {
    let analysis = analyze(
        "<template>{{ import(\"./template-only\") }}</template>\n",
        "src/Template.vue",
    );

    assert!(analysis.imports.is_empty());
    assert!(!analysis.has_opaque_imports);
}

#[test]
fn non_literal_dynamic_and_commonjs_imports_are_opaque() {
    let source = "\
import(expr);
import(`./a${x}`);
require(cond ? \"a\" : \"b\");
other(expr);
foo.require(expr);
";
    let analysis = analyze(source, "src/opaque.js");

    assert!(analysis.imports.is_empty());
    assert!(analysis.has_opaque_imports);

    let literal = analyze(
        "import(\"./literal\");\nrequire(\"./commonjs\");\n",
        "src/literal.js",
    );
    assert!(!literal.has_opaque_imports);
    assert_eq!(literal.imports.len(), 2);
}

#[test]
fn javascript_import_line_and_span_include_quotes() {
    let analysis = analyze("// lead\nimport \"./s\";\n", "src/span.js");

    assert_eq!(
        analysis.imports,
        [RawImport {
            specifier: "./s".into(),
            kind: ImportKind::EsStatic,
            line: 2,
            span: (15, 20),
        }]
    );
}

#[test]
fn expands_rust_use_trees_and_external_modules() {
    let source = "\
use a::b;
use a::{b, c::{d, e}};
use a::b as x;
use a::*;
use crate::x;
use super::y;
use self::z;
use a::b::{self, c};
mod foo;
mod inline {
    use inner::thing;
}
";

    assert_eq!(
        imports(source, "src/lib.rs"),
        [
            ("a::b".to_owned(), ImportKind::RustUse),
            ("a::b".to_owned(), ImportKind::RustUse),
            ("a::c::d".to_owned(), ImportKind::RustUse),
            ("a::c::e".to_owned(), ImportKind::RustUse),
            ("a::b".to_owned(), ImportKind::RustUse),
            ("a::*".to_owned(), ImportKind::RustUse),
            ("crate::x".to_owned(), ImportKind::RustUse),
            ("super::y".to_owned(), ImportKind::RustUse),
            ("self::z".to_owned(), ImportKind::RustUse),
            ("a::b".to_owned(), ImportKind::RustUse),
            ("a::b::c".to_owned(), ImportKind::RustUse),
            ("foo".to_owned(), ImportKind::RustMod),
            ("inner::thing".to_owned(), ImportKind::RustUse),
        ]
    );
}

#[test]
fn keeps_imports_inside_inline_rust_modules_as_written() {
    let source = "\
mod outer {
    mod child;
    use self::child::Item;
    use crate::absolute::Item;
}
";

    assert_eq!(
        imports(source, "src/lib.rs"),
        [
            ("child".to_owned(), ImportKind::RustMod),
            ("self::child::Item".to_owned(), ImportKind::RustUse),
            ("crate::absolute::Item".to_owned(), ImportKind::RustUse),
        ]
    );
}

#[test]
fn rust_import_line_and_leaf_spans_are_exact() {
    let source = "mod outer {\n  use crate::x;\n}\nmod foo;\n";
    let analysis = analyze(source, "src/span.rs");

    assert_eq!(
        analysis.imports,
        [
            RawImport {
                specifier: "crate::x".into(),
                kind: ImportKind::RustUse,
                line: 2,
                span: (18, 26),
            },
            RawImport {
                specifier: "foo".into(),
                kind: ImportKind::RustMod,
                line: 4,
                span: (34, 37),
            },
        ]
    );
}

#[test]
fn extracts_rust_import_with_one_hundred_thousand_nested_blocks() {
    const DEPTH: usize = 100_000;
    let mut source = String::with_capacity(DEPTH * 2 + 64);
    source.push_str("use foo::bar;\nfn deeply_nested() {");
    source.extend(std::iter::repeat_n('{', DEPTH));
    source.extend(std::iter::repeat_n('}', DEPTH));
    source.push_str("}\n");
    assert!(source.len() < 2 * 1024 * 1024);

    assert_eq!(
        imports(&source, "src/deep_blocks.rs"),
        [("foo::bar".to_owned(), ImportKind::RustUse)]
    );
}

#[test]
fn extracts_leaf_from_deeply_nested_rust_use_tree() {
    const DEPTH: usize = 30_000;
    let mut source = String::with_capacity(DEPTH * 12);
    let mut expected = String::with_capacity(DEPTH * 8);
    source.push_str("use ");
    for level in 0..DEPTH {
        write!(source, "a{level}::{{").unwrap();
        if level > 0 {
            expected.push_str("::");
        }
        write!(expected, "a{level}").unwrap();
    }
    source.push_str("leaf");
    source.extend(std::iter::repeat_n('}', DEPTH));
    source.push_str(";\n");
    expected.push_str("::leaf");
    assert!(source.len() < 2 * 1024 * 1024);

    let analysis = analyze(&source, "src/deep_use_tree.rs");
    assert_eq!(analysis.imports.len(), 1);
    assert_eq!(analysis.imports[0].kind, ImportKind::RustUse);
    assert_eq!(analysis.imports[0].specifier.as_str(), expected);
}

#[test]
fn leading_comment_arguments_do_not_hide_literal_specifiers() {
    let source = "\
const a = import(/* webpackChunkName: \"chunk\" */ \"./with-comment\");
const b = require(/* preload */ \"./required\");
const c = import(/* only-comment */ expr);
";

    let analysis = analyze(source, "src/comments.js");
    assert_eq!(
        analysis
            .imports
            .iter()
            .map(|import| (import.specifier.as_str(), import.kind))
            .collect::<Vec<_>>(),
        [
            ("./with-comment", ImportKind::EsDynamic),
            ("./required", ImportKind::CommonJs),
        ],
    );
    // The comment-then-non-literal call stays opaque.
    assert!(analysis.has_opaque_imports);
}

#[test]
fn concatenation_member_and_empty_require_arguments_are_pinned() {
    let source = "\
const a = import(\"a\" + b);
const c = require(obj.name);
const d = require(\"\");
";

    let analysis = analyze(source, "src/edges.js");
    assert_eq!(
        analysis
            .imports
            .iter()
            .map(|import| (import.specifier.as_str(), import.kind))
            .collect::<Vec<_>>(),
        [("", ImportKind::CommonJs)],
    );
    assert!(analysis.has_opaque_imports);
}
