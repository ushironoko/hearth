//! Import extraction types and per-language extractors.
//!
//! [`ImportSpec`]: crate::lang::ImportSpec

use compact_str::CompactString;

pub(crate) mod js;
pub(crate) mod rust;

/// How an import reference appears in source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportKind {
    /// `import ... from "x"`.
    EsStatic,
    /// `export ... from "x"`.
    EsReexport,
    /// `import("x")` with a literal specifier.
    EsDynamic,
    /// `require("x")`.
    CommonJs,
    /// TypeScript `import x = require("x")`.
    TsImportRequire,
    /// Rust `use` path, flattened to one leaf per import. The specifier is
    /// the normalized leaf path (`a::b::c`), not the byte-for-byte source
    /// text — grouped and aliased use-trees have no single as-written form
    /// per leaf.
    RustUse,
    /// Rust `mod name;` without a body.
    RustMod,
}

/// One import extracted from a file, with the specifier as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// Import specifier text as written in the source.
    pub specifier: CompactString,
    /// Syntactic form the import took.
    pub kind: ImportKind,
    /// 1-based line of the specifier.
    pub line: u32,
    /// Byte range of the specifier — the grep-backstop anchor.
    pub span: (u32, u32),
}
