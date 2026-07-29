use std::path::Path;

use compact_str::CompactString;
use tree_sitter::Tree;

use crate::imports::js;
use crate::symbols::extract_symbols_from_tree;
use crate::{ImportKind, ImportSpec, ParserPool, RawImport, Symbol};

/// Symbols and imports extracted from one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnalysis {
    /// Repository-relative source path.
    pub path: CompactString,
    /// Content hash supplied by the caller.
    pub content_hash: u64,
    /// Resolved language name, or `None` when the path is unsupported.
    pub language: Option<CompactString>,
    /// Symbols in source order.
    pub symbols: Vec<Symbol>,
    /// Literal imports in source order.
    pub imports: Vec<RawImport>,
    /// Whether at least one dynamic or CommonJS import had a non-literal argument.
    pub has_opaque_imports: bool,
}

#[derive(Clone, Copy)]
enum ImportExtractor {
    Query(fn(&str) -> ImportKind),
    Custom(fn(&str, &Tree) -> Vec<RawImport>),
}

/// Parses and analyzes one source file.
///
/// Symbol and import extraction share the same syntax tree. Unsupported paths,
/// parser failures, and sources whose byte offsets cannot fit in `u32` produce
/// empty results.
#[must_use]
pub fn analyze_source(
    source: &str,
    path: &str,
    content_hash: u64,
    pool: &mut ParserPool<'_>,
) -> FileAnalysis {
    let mut analysis = FileAnalysis {
        path: CompactString::from(path),
        content_hash,
        language: None,
        symbols: Vec::new(),
        imports: Vec::new(),
        has_opaque_imports: false,
    };

    let Some(id) = pool.registry().for_path(Path::new(path)) else {
        return analysis;
    };
    let Some(spec) = pool.registry().get(id) else {
        return analysis;
    };
    analysis.language = Some(spec.name.clone());

    if u32::try_from(source.len()).is_err() {
        return analysis;
    }

    let import_extractor = spec.imports.as_ref().map(|imports| match imports {
        ImportSpec::Query { kind_map, .. } => ImportExtractor::Query(*kind_map),
        ImportSpec::Custom(extract) => ImportExtractor::Custom(*extract),
    });

    let tree = {
        let Some(parser) = pool.parser(id) else {
            return analysis;
        };
        let Some(tree) = parser.parse(source, None) else {
            return analysis;
        };
        tree
    };

    analysis.symbols = extract_symbols_from_tree(source, &tree, id, pool);
    match import_extractor {
        Some(ImportExtractor::Query(kind_map)) => {
            if let Some(query) = pool.imports_query(id) {
                let extracted = js::extract(source, &tree, query, kind_map);
                analysis.imports = extracted.imports;
                analysis.has_opaque_imports = extracted.opaque_count != 0;
            }
        }
        Some(ImportExtractor::Custom(extract)) => {
            analysis.imports = extract(source, &tree);
        }
        None => {}
    }

    analysis
}
