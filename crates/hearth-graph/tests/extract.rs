#![cfg(feature = "bundled-languages")]

use hearth_graph::{LanguageRegistry, ParserPool, SymbolKind, extract_symbols};
use std::path::Path;

const RUST_SOURCE: &str = "\
pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new() -> Self {
        Self { name: String::new() }
    }
}

fn helper() {}
";

const TYPESCRIPT_SOURCE: &str = "\
export interface Props { a: number }
export class Widget {
  render(): void {}
}
export function setup() {}
";

const PYTHON_SOURCE: &str = "\
class Loader:
    def load(self):
        pass

def main():
    pass
";

const GO_SOURCE: &str = "\
package main

type Config struct{}

func (c *Config) Load() {}

func main() {}
";

const C_SHARP_SOURCE: &str = "\
namespace Demo {
  public class Service {
    public void Run() {}
    public int Count { get; set; }
  }
  interface IThing {}
}
";

const C_SHARP_OVERLOAD_SOURCE: &str = "\
class Service {
    void Run(int x) {}
    void Run(string x) {}
}
";

const HASKELL_SOURCE: &str = "\
module Fixture where

data Payload = Payload Int
newtype UserId = UserId Int
type Label = String
class Renderable a where
  render :: a -> String

describe 0 = \"zero\"
describe value = show value
";

const ZIG_SOURCE: &str = "\
const Point = struct { x: i32 };
pub fn add(a: i32, b: i32) i32 { return a + b; }
";

const BASH_SOURCE: &str = "\
TOP_LEVEL=1

deploy() {
  local_var=2
  echo hi
}
";

const MARKDOWN_SOURCE: &str = "\
# Title

intro

## Usage

### Options
";

const VUE_TYPESCRIPT_SOURCE: &str = "\
<template>
  <Child />
</template>
<script setup lang=\"ts\">
import Child from \"./Child.vue\";
export interface Props { title: string }
export class Controller {
  reset(): void {}
}
export function useCounter() {}
</script>
";

const SYNTAX_ERROR_SOURCE: &str = "fn ok() {}\nfn broken( {\n";
const CJK_SOURCE: &str = "// 日本語のコメント\nfn 名前() {}\n";
const MULTIBYTE_PREFIX_SOURCE: &str = "class 構造体 { メソッド() {} }\n";

fn outline(source: &str, path: &str) -> Vec<(String, SymbolKind, u32, u16)> {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    extract_symbols(source, path, &mut pool)
        .into_iter()
        .map(|symbol| {
            (
                symbol.name.to_string(),
                symbol.kind,
                symbol.line,
                symbol.depth,
            )
        })
        .collect()
}

fn names(source: &str, path: &str) -> Vec<String> {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    extract_symbols(source, path, &mut pool)
        .into_iter()
        .map(|symbol| symbol.name.to_string())
        .collect()
}

fn byte_range(source: &str, definition: &str) -> (u32, u32) {
    let start = source.find(definition).expect("definition is in fixture");
    (
        u32::try_from(start).expect("fixture offset fits u32"),
        u32::try_from(start + definition.len()).expect("fixture offset fits u32"),
    )
}

#[test]
fn extract_rust_outline() {
    insta::assert_debug_snapshot!(outline(RUST_SOURCE, "src/config.rs"), @r#"
    [
        (
            "Config",
            Class,
            1,
            0,
        ),
        (
            "new",
            Method,
            6,
            0,
        ),
        (
            "helper",
            Function,
            11,
            0,
        ),
    ]
    "#);
}

#[test]
fn extract_typescript() {
    assert_eq!(
        names(TYPESCRIPT_SOURCE, "src/widget.ts"),
        ["Props", "Widget", "render", "setup"]
    );
}

#[test]
fn extract_python_outline() {
    insta::assert_debug_snapshot!(outline(PYTHON_SOURCE, "loader.py"), @r#"
    [
        (
            "Loader",
            Class,
            1,
            0,
        ),
        (
            "load",
            Function,
            2,
            1,
        ),
        (
            "main",
            Function,
            5,
            0,
        ),
    ]
    "#);
}

#[test]
fn extract_go() {
    assert_eq!(
        names(GO_SOURCE, "main.go"),
        ["Config", "Load", "main"],
        "go tags should yield the type, its method and the free function"
    );
}

#[test]
fn extract_c_sharp_uses_bundled_query() {
    assert_eq!(
        names(C_SHARP_SOURCE, "Service.cs"),
        ["Demo", "Service", "Run", "Count", "IThing"]
    );
}

#[test]
fn extract_c_sharp_preserves_overloaded_sibling_methods() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(C_SHARP_OVERLOAD_SOURCE, "Service.cs", &mut pool);
    let runs: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.name == "Run")
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.kind,
                symbol.line,
                symbol.column,
                symbol.depth,
                symbol.name_start,
                symbol.def_start,
                symbol.def_end,
            )
        })
        .collect();
    let first_definition = "void Run(int x) {}";
    let second_definition = "void Run(string x) {}";
    let (first_start, first_end) = byte_range(C_SHARP_OVERLOAD_SOURCE, first_definition);
    let (second_start, second_end) = byte_range(C_SHARP_OVERLOAD_SOURCE, second_definition);

    assert_eq!(
        runs,
        [
            (
                "Run",
                SymbolKind::Method,
                2,
                9,
                1,
                first_start + 5,
                first_start,
                first_end,
            ),
            (
                "Run",
                SymbolKind::Method,
                3,
                9,
                1,
                second_start + 5,
                second_start,
                second_end,
            ),
        ]
    );
    assert_ne!(runs[0].6..runs[0].7, runs[1].6..runs[1].7);
}

#[test]
fn extract_haskell_declarations_and_merges_function_equations() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(HASKELL_SOURCE, "Fixture.hs", &mut pool);
    let actual: Vec<_> = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.kind,
                symbol.line,
                symbol.column,
                symbol.depth,
                symbol.name_start,
                symbol.def_start,
                symbol.def_end,
            )
        })
        .collect();
    let declarations = [
        ("module Fixture where", "Fixture", SymbolKind::Module, 1, 7),
        (
            "data Payload = Payload Int",
            "Payload",
            SymbolKind::Class,
            3,
            5,
        ),
        (
            "newtype UserId = UserId Int",
            "UserId",
            SymbolKind::Class,
            4,
            8,
        ),
        ("type Label = String", "Label", SymbolKind::Type, 5, 5),
        (
            "class Renderable a where\n  render :: a -> String",
            "Renderable",
            SymbolKind::Interface,
            6,
            6,
        ),
        (
            "describe 0 = \"zero\"",
            "describe",
            SymbolKind::Function,
            9,
            0,
        ),
    ];
    let expected: Vec<_> = declarations
        .into_iter()
        .map(|(definition, name, kind, line, column)| {
            let (def_start, def_end) = byte_range(HASKELL_SOURCE, definition);
            (
                name,
                kind,
                line,
                column,
                0,
                def_start + column,
                def_start,
                def_end,
            )
        })
        .collect();

    assert_eq!(actual, expected);
    assert_eq!(
        symbols
            .iter()
            .filter(|symbol| symbol.name == "describe")
            .count(),
        1,
        "two Haskell equations should produce one function symbol"
    );
}

#[test]
fn extract_zig_uses_bundled_query() {
    let names = names(ZIG_SOURCE, "point.zig");
    assert!(names.contains(&"Point".to_string()), "got {names:?}");
    assert!(names.contains(&"add".to_string()), "got {names:?}");
}

#[test]
fn extract_bash_skips_function_local_assignments() {
    assert_eq!(names(BASH_SOURCE, "deploy.sh"), ["TOP_LEVEL", "deploy"]);
}

#[test]
fn extract_markdown_headings() {
    insta::assert_debug_snapshot!(outline(MARKDOWN_SOURCE, "README.md"), @r#"
    [
        (
            "Title",
            Heading,
            1,
            0,
        ),
        (
            "Usage",
            Heading,
            5,
            1,
        ),
        (
            "Options",
            Heading,
            7,
            2,
        ),
    ]
    "#);
}

#[test]
fn extract_vue_typescript_script_symbols_with_whole_file_locations() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(VUE_TYPESCRIPT_SOURCE, "src/Counter.vue", &mut pool);

    assert_eq!(
        symbols
            .iter()
            .map(|symbol| (
                symbol.name.as_str(),
                symbol.line,
                symbol.column,
                symbol.depth
            ))
            .collect::<Vec<_>>(),
        [
            ("Props", 6, 17, 0),
            ("Controller", 7, 13, 0),
            ("reset", 8, 2, 1),
            ("useCounter", 10, 16, 0),
        ]
    );
    for symbol in &symbols {
        assert_eq!(
            &VUE_TYPESCRIPT_SOURCE
                [symbol.name_start as usize..symbol.name_start as usize + symbol.name.len()],
            symbol.name.as_str()
        );
        assert!(symbol.def_start as usize >= VUE_TYPESCRIPT_SOURCE.find("<script").unwrap());
        assert!(symbol.def_end as usize <= VUE_TYPESCRIPT_SOURCE.find("</script>").unwrap());
    }
}

#[test]
fn extract_vue_unquoted_typescript_lang_attribute() {
    let source = "<script setup lang=ts>\nexport interface UnquotedProps {}\n</script>\n";

    assert_eq!(names(source, "src/Unquoted.vue"), ["UnquotedProps"]);
}

#[test]
fn extract_vue_multiple_script_languages_in_source_order() {
    let source = "<script>\nexport function fromJavaScript() {}\n</script>\n\
<script lang=\"tsx\">\nexport class FromTsx {}\n</script>\n";

    assert_eq!(
        names(source, "src/Mixed.vue"),
        ["fromJavaScript", "FromTsx"]
    );
}

#[test]
fn extract_vue_inline_script_preserves_multibyte_character_column() {
    let source = "<template>日本語</template><script>export function inlineVue() {}</script>";
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbol = extract_symbols(source, "src/Inline.vue", &mut pool)
        .into_iter()
        .find(|symbol| symbol.name == "inlineVue")
        .expect("inline Vue function");

    assert_eq!(symbol.line, 1);
    assert_eq!(
        symbol.column as usize,
        source[..source.find("inlineVue").unwrap()].chars().count()
    );
    assert_eq!(
        symbol.name_start as usize,
        source.find("inlineVue").unwrap()
    );
}

#[test]
fn vue_included_ranges_do_not_leak_into_reused_script_parsers() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);

    let vue = extract_symbols(
        "<script>export function insideVue() {}</script>\n",
        "src/App.vue",
        &mut pool,
    );
    let javascript = extract_symbols(
        "export function standaloneJavaScript() {}\n",
        "src/standalone.js",
        &mut pool,
    );

    assert_eq!(vue[0].name, "insideVue");
    assert_eq!(javascript[0].name, "standaloneJavaScript");
}

#[test]
fn vue_template_without_script_symbols_yields_an_empty_outline() {
    assert!(names("<template><div>Hello</div></template>\n", "src/App.vue").is_empty());
}

#[test]
fn empty_source_yields_no_symbols() {
    assert!(names("", "src/lib.rs").is_empty());
}

#[test]
fn unsupported_extension_yields_no_symbols() {
    assert!(names("fn main() {}", "notes.txt").is_empty());
    assert!(names("body { color: red }", "site.css").is_empty());
}

#[test]
fn file_without_extension_yields_no_symbols() {
    assert!(names("fn main() {}", "Makefile").is_empty());
}

#[test]
fn syntax_errors_still_yield_recovered_symbols() {
    let names = names(SYNTAX_ERROR_SOURCE, "src/broken.rs");
    assert!(names.contains(&"ok".to_string()), "got {names:?}");
}

#[test]
fn cjk_identifier_is_extracted_with_location() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(CJK_SOURCE, "src/cjk.rs", &mut pool);

    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "名前");
    assert_eq!(symbols[0].line, 2);
    assert_eq!(symbols[0].column, 3);
}

#[test]
fn column_after_multibyte_prefix_is_in_characters() {
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);
    let symbols = extract_symbols(MULTIBYTE_PREFIX_SOURCE, "src/cjk.ts", &mut pool);
    let method = symbols
        .iter()
        .find(|symbol| symbol.name == "メソッド")
        .expect("method symbol");

    assert_eq!(
        usize::try_from(method.column).expect("column fits usize"),
        "class 構造体 { ".chars().count()
    );
    assert_eq!(method.column, 12);
    assert_eq!("class 構造体 { ".len(), 18);
}

#[test]
fn extracted_byte_ranges_are_coherent() {
    let fixtures = [
        (RUST_SOURCE, "src/config.rs"),
        (TYPESCRIPT_SOURCE, "src/widget.ts"),
        (PYTHON_SOURCE, "loader.py"),
        (GO_SOURCE, "main.go"),
        (C_SHARP_SOURCE, "Service.cs"),
        (C_SHARP_OVERLOAD_SOURCE, "Overloads.cs"),
        (HASKELL_SOURCE, "Fixture.hs"),
        (ZIG_SOURCE, "point.zig"),
        (BASH_SOURCE, "deploy.sh"),
        (MARKDOWN_SOURCE, "README.md"),
        (VUE_TYPESCRIPT_SOURCE, "src/Counter.vue"),
        ("", "src/lib.rs"),
        ("fn main() {}", "notes.txt"),
        ("body { color: red }", "site.css"),
        ("fn main() {}", "Makefile"),
        (SYNTAX_ERROR_SOURCE, "src/broken.rs"),
        (CJK_SOURCE, "src/cjk.rs"),
        (MULTIBYTE_PREFIX_SOURCE, "src/cjk.ts"),
    ];

    for (source, path) in fixtures {
        let registry = LanguageRegistry::bundled();
        let mut pool = ParserPool::new(&registry);
        for symbol in extract_symbols(source, path, &mut pool) {
            assert!(
                symbol.def_start < symbol.def_end
                    && symbol.def_start <= symbol.name_start
                    && symbol.name_start < symbol.def_end,
                "{path}: symbol {} has incoherent byte range ({}, {}, {})",
                symbol.name,
                symbol.def_start,
                symbol.name_start,
                symbol.def_end
            );
        }
    }
}

#[test]
fn bundled_registry_does_not_include_host_injected_moonbit() {
    assert!(!LanguageRegistry::bundled().supports_symbols(Path::new("foo.mbt")));
}
