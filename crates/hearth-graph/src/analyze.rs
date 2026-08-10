use std::path::Path;

use compact_str::CompactString;
use tree_sitter::Tree;

use crate::imports::js;
use crate::symbols::{MAX_SYMBOLS_PER_FILE, extract_symbols_from_tree};
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
/// Symbol and import extraction share syntax trees. Registered injection
/// queries parse embedded source with another language from the same registry
/// while preserving locations in the containing file. Unsupported paths,
/// parser failures, and sources whose byte offsets cannot fit in `u32` produce
/// empty results.
#[must_use]
pub fn analyze_source(
    source: &str,
    path: &str,
    content_hash: u64,
    pool: &mut ParserPool<'_>,
) -> FileAnalysis {
    analyze(source, path, content_hash, pool, true)
}

pub(crate) fn extract_symbols_only(
    source: &str,
    path: &str,
    pool: &mut ParserPool<'_>,
) -> Vec<Symbol> {
    analyze(source, path, 0, pool, false).symbols
}

fn analyze(
    source: &str,
    path: &str,
    content_hash: u64,
    pool: &mut ParserPool<'_>,
    include_imports: bool,
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

    let has_direct_symbols = spec.tags_query.is_some();
    let has_injections = spec.injections_query.is_some();
    let import_extractor = include_imports
        .then(|| spec.imports.as_ref().map(import_extractor))
        .flatten();

    let tree = {
        let Some(parser) = pool.parser(id) else {
            return analysis;
        };
        let Some(tree) = parser.parse(source, None) else {
            return analysis;
        };
        tree
    };

    if has_direct_symbols {
        analysis.symbols = extract_symbols_from_tree(source, &tree, id, pool);
    }
    if let Some(extractor) = import_extractor {
        extract_imports(source, &tree, id, extractor, pool, &mut analysis);
    }

    if has_injections {
        analyze_injections(source, &tree, id, include_imports, pool, &mut analysis);
    }

    analysis.symbols.sort_unstable_by(|a, b| {
        a.def_start
            .cmp(&b.def_start)
            .then(b.def_end.cmp(&a.def_end))
            .then(a.line.cmp(&b.line))
            .then(a.name_start.cmp(&b.name_start))
    });
    analysis.symbols.truncate(MAX_SYMBOLS_PER_FILE);
    analysis.imports.sort_by_key(|import| import.span.0);
    analysis
}

fn import_extractor(imports: &ImportSpec) -> ImportExtractor {
    match imports {
        ImportSpec::Query { kind_map, .. } => ImportExtractor::Query(*kind_map),
        ImportSpec::Custom(extract) => ImportExtractor::Custom(*extract),
    }
}

fn extract_imports(
    source: &str,
    tree: &Tree,
    id: crate::LanguageId,
    extractor: ImportExtractor,
    pool: &mut ParserPool<'_>,
    analysis: &mut FileAnalysis,
) {
    match extractor {
        ImportExtractor::Query(kind_map) => {
            if let Some(query) = pool.imports_query(id) {
                let extracted = js::extract(source, tree, query, kind_map);
                analysis.imports.extend(extracted.imports);
                analysis.has_opaque_imports |= extracted.opaque_count != 0;
            }
        }
        ImportExtractor::Custom(extract) => {
            analysis.imports.extend(extract(source, tree));
        }
    }
}

fn analyze_injections(
    source: &str,
    tree: &Tree,
    parent_id: crate::LanguageId,
    include_imports: bool,
    pool: &mut ParserPool<'_>,
    analysis: &mut FileAnalysis,
) {
    let injections = match pool.injections_query(parent_id) {
        Some(query) => crate::injections::extract(tree, source, query),
        None => return,
    };
    for injection in injections {
        let Some(id) = pool.registry().for_name(&injection.language) else {
            continue;
        };
        let Some(spec) = pool.registry().get(id) else {
            continue;
        };
        let has_symbols = spec.tags_query.is_some();
        let import_extractor = include_imports
            .then(|| spec.imports.as_ref().map(import_extractor))
            .flatten();
        if !has_symbols && import_extractor.is_none() {
            continue;
        }

        // Included ranges preserve the containing file's byte and point
        // coordinates, so symbol and import extractors need no manual rebasing.
        let injected_tree = {
            let Some(parser) = pool.parser(id) else {
                continue;
            };
            if parser
                .set_included_ranges(std::slice::from_ref(&injection.range))
                .is_err()
            {
                continue;
            }
            let tree = parser.parse(source, None);
            if parser.set_included_ranges(&[]).is_err() {
                continue;
            }
            let Some(tree) = tree else {
                continue;
            };
            tree
        };

        if has_symbols {
            analysis
                .symbols
                .extend(extract_symbols_from_tree(source, &injected_tree, id, pool));
        }

        if let Some(extractor) = import_extractor {
            extract_imports(source, &injected_tree, id, extractor, pool, analysis);
        }
    }
}
