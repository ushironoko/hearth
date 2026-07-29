//! Tree-sitter tags based symbol extraction.
//!
//! Symbols come from concrete syntax trees, so definition-like text in
//! comments and strings does not pollute file outlines.

use compact_str::CompactString;

mod extract;
mod index;
mod score;

pub use extract::extract_symbols;
pub(crate) use extract::extract_symbols_from_tree;
pub use index::{SymbolIndex, UpsertOutcome};

/// Maximum number of symbols retained per file.
///
/// This bounds the memory used by outlines for pathological generated source.
pub const MAX_SYMBOLS_PER_FILE: usize = 10_000;

/// The kind of a named entity, derived from an `@definition.*` capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    /// A free-standing function.
    Function,
    /// A function attached to a type or object.
    Method,
    /// A class-like declaration, including structs, enums, and unions.
    Class,
    /// An interface-like declaration, including traits and protocols.
    Interface,
    /// A module-like declaration, including namespaces and packages.
    Module,
    /// A macro definition.
    Macro,
    /// A constant definition.
    Constant,
    /// A type alias or other type declaration.
    Type,
    /// A field declaration.
    Field,
    /// A property declaration.
    Property,
    /// A Markdown heading.
    Heading,
}

impl SymbolKind {
    /// Maps a tags capture name to a symbol kind.
    fn from_capture(capture: &str) -> Option<Self> {
        match capture.strip_prefix("definition.")? {
            "function" => Some(Self::Function),
            "method" => Some(Self::Method),
            "class" | "struct" | "enum" | "union" => Some(Self::Class),
            "interface" | "trait" | "protocol" => Some(Self::Interface),
            "module" | "namespace" | "package" => Some(Self::Module),
            "macro" => Some(Self::Macro),
            "constant" => Some(Self::Constant),
            "type" => Some(Self::Type),
            "field" => Some(Self::Field),
            "property" => Some(Self::Property),
            "heading" => Some(Self::Heading),
            _ => None,
        }
    }

    /// Returns the single-character glyph used in outline and search rows.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Function => "ƒ",
            Self::Method => "m",
            Self::Class => "C",
            Self::Interface => "I",
            Self::Module => "M",
            Self::Macro => "!",
            Self::Constant => "c",
            Self::Type => "T",
            Self::Field => "f",
            Self::Property => "p",
            Self::Heading => "#",
        }
    }
}

/// A named entity in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The source spelling of the symbol name.
    pub name: CompactString,
    /// The definition kind reported by the tags query.
    pub kind: SymbolKind,
    /// The 1-based line of the symbol name.
    pub line: u32,
    /// The 0-based character column of the symbol name.
    pub column: u32,
    /// The nesting depth of enclosing definitions.
    pub depth: u16,
    /// The byte offset where the symbol name begins.
    pub name_start: u32,
    /// The byte offset where the definition begins.
    pub def_start: u32,
    /// The exclusive byte offset where the definition ends.
    pub def_end: u32,
}

/// Symbols extracted from one file and tied to its source content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSymbols {
    /// Repository-relative source path.
    pub path: CompactString,
    /// Content hash supplied by the host.
    pub content_hash: u64,
    /// Symbols in source order.
    pub symbols: Vec<Symbol>,
}

/// A symbol together with the file it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRef<'a> {
    /// Repository-relative source path.
    pub path: &'a str,
    /// The referenced symbol.
    pub symbol: &'a Symbol,
}

impl SymbolRef<'_> {
    /// Formats a symbol-search row as `ƒ name  path:line`.
    pub fn search_label(&self) -> String {
        format!(
            "{} {}  {}:{}",
            self.symbol.kind.glyph(),
            self.symbol.name,
            self.path,
            self.symbol.line
        )
    }
}

/// Returns the preference rank for duplicate captures of one name node.
///
/// Lower values are more specific and win before definition-span length.
pub fn kind_specificity(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Method => 0,
        SymbolKind::Property => 1,
        SymbolKind::Field => 2,
        SymbolKind::Constant => 3,
        SymbolKind::Macro => 4,
        SymbolKind::Function => 5,
        SymbolKind::Interface => 6,
        SymbolKind::Class => 7,
        SymbolKind::Type => 8,
        SymbolKind::Module => 9,
        SymbolKind::Heading => 10,
    }
}

/// Returns the ordering rank for choosing a likely jump destination.
///
/// Lower values represent definitions users are more likely to seek.
pub fn jump_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Type => 0,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Macro => 1,
        SymbolKind::Constant | SymbolKind::Module => 2,
        SymbolKind::Field | SymbolKind::Property | SymbolKind::Heading => 3,
    }
}
