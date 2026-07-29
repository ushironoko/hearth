#![cfg(feature = "bundled-languages")]

use std::borrow::Cow;
use std::collections::HashMap;

use hearth_graph::{
    AnalyzeBuild, BuildOptions, FileAnalysis, FileSymbols, ImportKind, ImportSpec, IndexBuild,
    LanguageRegistry, LanguageSpec, NeverCancelled, ParserPool, SourceLoader, SymbolIndex,
    analyze_paths, analyze_source, build_index, extract_symbols,
};

#[derive(Default)]
struct MemoryLoader {
    sources: HashMap<String, String>,
    probe_only: HashMap<String, u64>,
}

impl SourceLoader for MemoryLoader {
    fn verify(&self) -> Result<(), String> {
        Ok(())
    }

    fn probe(&self, path: &str) -> Option<u64> {
        self.sources
            .get(path)
            .map(|source| source.len() as u64)
            .or_else(|| self.probe_only.get(path).copied())
    }

    fn load(&self, path: &str) -> Option<String> {
        self.sources.get(path).cloned()
    }
}

fn completed_analysis(build: AnalyzeBuild) -> (Vec<FileAnalysis>, usize) {
    match build {
        AnalyzeBuild::Completed {
            files,
            scanned_files,
        } => (files, scanned_files),
        AnalyzeBuild::Cancelled { scanned_files } => {
            panic!("analysis cancelled after scanning {scanned_files} files")
        }
        AnalyzeBuild::Failed { message } => panic!("analysis failed: {message}"),
    }
}

fn completed_index(build: IndexBuild) -> SymbolIndex {
    match build {
        IndexBuild::Completed(index) => index,
        IndexBuild::Cancelled { scanned_files } => {
            panic!("index build cancelled after scanning {scanned_files} files")
        }
        IndexBuild::Failed { message } => panic!("index build failed: {message}"),
    }
}

#[test]
fn analyze_source_symbols_match_standalone_extraction() {
    let fixtures = [
        (
            "import \"./dep.js\";\nexport function jsValue() {}\n",
            "src/value.js",
        ),
        (
            "import type { T } from \"./types\";\nexport class TsValue {}\n",
            "src/value.ts",
        ),
        (
            "import View from \"./view\";\nexport const App = () => <View />;\n",
            "src/app.tsx",
        ),
        (
            "import View from \"./view\";\nexport const App = () => <View />;\n",
            "src/app.jsx",
        ),
        ("use crate::dep;\npub struct RustValue;\n", "src/value.rs"),
    ];
    let registry = LanguageRegistry::bundled();
    let mut pool = ParserPool::new(&registry);

    for (source, path) in fixtures {
        let analysis = analyze_source(source, path, 42, &mut pool);
        let standalone = extract_symbols(source, path, &mut pool);
        assert_eq!(analysis.symbols, standalone, "{path}");
        assert!(analysis.language.is_some(), "{path}");
    }
}

#[test]
fn analyze_paths_folds_to_the_same_symbol_index_as_build_index() {
    let loader = MemoryLoader {
        sources: HashMap::from([
            (
                "src/a.js".to_owned(),
                "import \"./dep\";\nexport function alpha() {}\n".to_owned(),
            ),
            ("src/b.ts".to_owned(), "export class Beta {}\n".to_owned()),
            (
                "src/c.rs".to_owned(),
                "use crate::other;\npub fn gamma() {}\n".to_owned(),
            ),
            ("notes.txt".to_owned(), "unsupported\n".to_owned()),
        ]),
        probe_only: HashMap::new(),
    };
    let paths = vec![
        "src/c.rs".to_owned(),
        "notes.txt".to_owned(),
        "src/a.js".to_owned(),
        "src/b.ts".to_owned(),
    ];
    let registry = LanguageRegistry::bundled();
    let options = BuildOptions::default();

    let built = completed_index(build_index(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &options,
    ));
    let (analyses, scanned_files) = completed_analysis(analyze_paths(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &options,
    ));
    let folded = SymbolIndex::from_files(
        analyses
            .into_iter()
            .map(|analysis| FileSymbols {
                path: analysis.path,
                content_hash: analysis.content_hash,
                symbols: analysis.symbols,
            })
            .collect(),
        registry.generation(),
    );

    assert_eq!(scanned_files, built.scanned_file_count());
    assert_eq!(folded.symbol_count(), built.symbol_count());
    let built_paths: Vec<_> = built.paths().map(str::to_owned).collect();
    let folded_paths: Vec<_> = folded.paths().map(str::to_owned).collect();
    assert_eq!(folded_paths, built_paths);
    for path in built_paths {
        assert_eq!(folded.file_hash(&path), built.file_hash(&path), "{path}");
        assert_eq!(
            folded.file_symbols(&path),
            built.file_symbols(&path),
            "{path}"
        );
    }
}

#[test]
fn analyze_paths_scanned_count_skips_unsupported_and_counts_unreadable() {
    let loader = MemoryLoader {
        sources: HashMap::from([
            (
                "src/readable.js".to_owned(),
                "export function readable() {}\n".to_owned(),
            ),
            (
                "src/readable.rs".to_owned(),
                "pub fn readable() {}\n".to_owned(),
            ),
            ("notes.txt".to_owned(), "unsupported\n".to_owned()),
        ]),
        probe_only: HashMap::from([("src/unreadable.ts".to_owned(), 128)]),
    };
    let paths = vec![
        "src/readable.js".to_owned(),
        "notes.txt".to_owned(),
        "src/unreadable.ts".to_owned(),
        "src/readable.rs".to_owned(),
    ];
    let registry = LanguageRegistry::bundled();

    let (files, scanned_files) = completed_analysis(analyze_paths(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &BuildOptions::default(),
    ));

    assert_eq!(scanned_files, 3);
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["src/readable.js", "src/readable.rs"]
    );
}

#[test]
fn analyze_paths_precancelled_scans_nothing() {
    let loader = MemoryLoader {
        sources: HashMap::from([(
            "src/value.js".to_owned(),
            "export const value = 1;\n".to_owned(),
        )]),
        probe_only: HashMap::new(),
    };
    let registry = LanguageRegistry::bundled();
    let cancelled = || true;

    match analyze_paths(
        &registry,
        &loader,
        &["src/value.js".to_owned()],
        &cancelled,
        &BuildOptions::default(),
    ) {
        AnalyzeBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
        other => panic!("pre-cancelled analysis did not cancel: {other:?}"),
    }
}

fn static_import_kind(capture: &str) -> ImportKind {
    assert_eq!(capture, "import.source.static");
    ImportKind::EsStatic
}

#[test]
fn analyze_and_index_drivers_keep_distinct_prefilters() {
    let mut registry = LanguageRegistry::empty();
    registry.register(LanguageSpec {
        name: "imports-only".into(),
        language: tree_sitter_javascript::LANGUAGE.into(),
        extensions: ["dep".into()].into_iter().collect(),
        tags_query: None,
        merge_adjacent_same_name_definitions: false,
        imports: Some(ImportSpec::Query {
            source: Cow::Borrowed("(import_statement source: (string) @import.source.static)"),
            kind_map: static_import_kind,
        }),
    });
    let loader = MemoryLoader {
        sources: HashMap::from([(
            "module.dep".to_owned(),
            "import \"dependency\";\n".to_owned(),
        )]),
        probe_only: HashMap::new(),
    };
    let paths = ["module.dep".to_owned()];
    let options = BuildOptions::default();

    let (files, scanned_files) = completed_analysis(analyze_paths(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &options,
    ));
    assert_eq!(scanned_files, 1);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].imports[0].specifier, "dependency");

    let index = completed_index(build_index(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &options,
    ));
    assert_eq!(index.scanned_file_count(), 0);
    assert_eq!(index.file_count(), 0);
}
