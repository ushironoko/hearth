//! Standalone symbol-index and module-graph engine for Hearth.
//!
//! `hearth-graph` performs parsing and graph analysis without ambient I/O.
//! Hosts supply source data explicitly and inject cancellation through
//! [`CancelSignal`].

mod analyze;
mod build;
#[cfg(feature = "bundled-languages")]
mod bundled;
mod cancel;
#[cfg(feature = "fs")]
mod fs_loader;
pub mod graph;
pub mod imports;
mod lang;
mod parse;
pub mod resolve;
pub mod symbols;

pub use analyze::{FileAnalysis, analyze_source};
pub use build::{AnalyzeBuild, BuildOptions, IndexBuild, SourceLoader, analyze_paths, build_index};
pub use cancel::{CancelSignal, NeverCancelled};
#[cfg(feature = "fs")]
pub use fs_loader::FsLoader;
pub use imports::{ImportKind, RawImport};
pub use lang::{ImportSpec, LanguageId, LanguageRegistry, LanguageSpec};
pub use parse::ParserPool;
#[cfg(feature = "resolve-js")]
pub use resolve::js::{JsResolveOptions, js_resolver, js_resolver_with_fs};
#[cfg(feature = "resolve-rust")]
pub use resolve::rust::{RustResolveOptions, rust_resolver};
pub use resolve::{
    FailedKind, ResolutionCompleteness, ResolutionOutcome, Resolve, Resolved, ResolverSet,
    UnresolvedReason,
};
pub use symbols::{
    FileSymbols, MAX_SYMBOLS_PER_FILE, Symbol, SymbolIndex, SymbolKind, SymbolRef, UpsertOutcome,
    extract_symbols,
};
