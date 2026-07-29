use hearth_graph::{LanguageRegistry, ParserPool, SymbolKind, extract_symbols};
#[cfg(feature = "bundled-languages")]
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
        (ZIG_SOURCE, "point.zig"),
        (BASH_SOURCE, "deploy.sh"),
        (MARKDOWN_SOURCE, "README.md"),
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

#[cfg(feature = "bundled-languages")]
#[test]
fn bundled_registry_does_not_include_host_injected_moonbit() {
    assert!(!LanguageRegistry::bundled().supports_symbols(Path::new("foo.mbt")));
}
