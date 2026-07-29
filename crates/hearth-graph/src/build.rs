use std::path::Path;

use compact_str::CompactString;
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    CancelSignal, FileAnalysis, FileSymbols, LanguageRegistry, ParserPool, SymbolIndex,
    analyze_source, extract_symbols,
};

const PREFILTER_CANCEL_POLL_INTERVAL: usize = 128;

/// Source access supplied by the host that owns the repository or file store.
///
/// The index-build driver performs all source access through this trait and
/// never reads the filesystem directly.
pub trait SourceLoader: Sync {
    /// Verifies that the source root is available for a build.
    fn verify(&self) -> Result<(), String>;

    /// Returns the byte length of a regular file, or `None` when it cannot be
    /// probed or is not a regular file.
    fn probe(&self, path: &str) -> Option<u64>;

    /// Loads UTF-8 source text, or returns `None` when reading fails.
    fn load(&self, path: &str) -> Option<String>;
}

/// Limits applied while building a symbol index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    /// Maximum byte length of an individual source file.
    pub max_file_bytes: u64,
    /// Maximum number of indexing workers.
    pub max_workers: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 2 * 1024 * 1024,
            max_workers: 8,
        }
    }
}

/// Outcome of a cancellable symbol-index build.
#[derive(Debug)]
// Keep the specified public `Completed(SymbolIndex)` shape rather than adding
// allocation and changing the API solely to reduce the enum's stack size.
#[allow(clippy::large_enum_variant)]
pub enum IndexBuild {
    /// Every indexable path was scanned.
    Completed(SymbolIndex),
    /// Cancellation stopped the build before it could publish an index.
    Cancelled {
        /// Number of indexable files whose load was attempted.
        scanned_files: usize,
    },
    /// The source root was invalid or an indexing worker panicked.
    Failed {
        /// Human-readable reason the build could not complete.
        message: String,
    },
}

/// Outcome of a cancellable source-analysis build.
#[derive(Debug)]
pub enum AnalyzeBuild {
    /// Every analyzable path was scanned.
    Completed {
        /// Successfully loaded file analyses, sorted by path.
        files: Vec<FileAnalysis>,
        /// Number of analyzable files whose load was attempted.
        scanned_files: usize,
    },
    /// Cancellation stopped analysis before results could be published.
    Cancelled {
        /// Number of analyzable files whose load was attempted.
        scanned_files: usize,
    },
    /// The source root was invalid or an analysis worker panicked.
    Failed {
        /// Human-readable reason the analysis could not complete.
        message: String,
    },
}

/// Builds a symbol index by loading and parsing the supplied paths in parallel.
///
/// Unsupported, oversized, missing, and unreadable files are skipped without
/// failing the build. Unlike octorus, every successfully loaded supported file
/// gets an index entry even when it contains no symbols (deviation D2).
/// Consequently, the indexed file count equals successful loads, while
/// [`SymbolIndex::scanned_file_count`] additionally counts files whose load
/// failed after a successful probe.
///
/// Duplicate entries in `paths` collapse last-wins through the index's
/// upsert (divergence D1); octorus kept duplicate rows. `scanned_files`
/// still counts every input occurrence.
pub fn build_index(
    registry: &LanguageRegistry,
    loader: &dyn SourceLoader,
    paths: &[String],
    cancel: &dyn CancelSignal,
    options: &BuildOptions,
) -> IndexBuild {
    // Keep the symbols-only prefilter and worker here. Running full analysis
    // would change scanned accounting for import-only registrations and could
    // make an unrelated custom import extractor affect symbol-index builds.
    match drive_paths(
        registry,
        loader,
        paths,
        cancel,
        options,
        supports_symbols,
        analyze_symbols_only,
    ) {
        DriverBuild::Completed {
            files,
            scanned_files,
        } => {
            let files = files
                .into_iter()
                .map(|analysis| FileSymbols {
                    path: analysis.path,
                    content_hash: analysis.content_hash,
                    symbols: analysis.symbols,
                })
                .collect();
            let mut index = SymbolIndex::from_files(files, registry.generation());
            index.set_scanned_files(scanned_files);
            IndexBuild::Completed(index)
        }
        DriverBuild::Cancelled { scanned_files } => IndexBuild::Cancelled { scanned_files },
        DriverBuild::Failed { message } => IndexBuild::Failed { message },
        DriverBuild::Panicked => IndexBuild::Failed {
            message: "symbol indexing worker panicked; retry the build".to_owned(),
        },
    }
}

/// Analyzes symbols and imports for the supplied paths in parallel.
///
/// Paths are prefiltered when their registered language supports either
/// symbols or imports. Duplicate inputs are all scanned and retained; the
/// returned vector is stably sorted by path, preserving duplicate input order.
pub fn analyze_paths(
    registry: &LanguageRegistry,
    loader: &dyn SourceLoader,
    paths: &[String],
    cancel: &dyn CancelSignal,
    options: &BuildOptions,
) -> AnalyzeBuild {
    match drive_paths(
        registry,
        loader,
        paths,
        cancel,
        options,
        supports_analysis,
        analyze_source,
    ) {
        DriverBuild::Completed {
            files,
            scanned_files,
        } => AnalyzeBuild::Completed {
            files,
            scanned_files,
        },
        DriverBuild::Cancelled { scanned_files } => AnalyzeBuild::Cancelled { scanned_files },
        DriverBuild::Failed { message } => AnalyzeBuild::Failed { message },
        DriverBuild::Panicked => AnalyzeBuild::Failed {
            message: "source analysis worker panicked; retry the analysis".to_owned(),
        },
    }
}

type SupportPredicate = fn(&LanguageRegistry, &Path) -> bool;
type Analyzer = fn(&str, &str, u64, &mut ParserPool<'_>) -> FileAnalysis;

enum DriverBuild {
    Completed {
        files: Vec<FileAnalysis>,
        scanned_files: usize,
    },
    Cancelled {
        scanned_files: usize,
    },
    Failed {
        message: String,
    },
    Panicked,
}

struct ChunkOutcome {
    files: Vec<FileAnalysis>,
    scanned: usize,
    stopped_early: bool,
}

#[allow(clippy::too_many_arguments)]
fn drive_paths(
    registry: &LanguageRegistry,
    loader: &dyn SourceLoader,
    paths: &[String],
    cancel: &dyn CancelSignal,
    options: &BuildOptions,
    supports: SupportPredicate,
    analyzer: Analyzer,
) -> DriverBuild {
    if let Err(message) = loader.verify() {
        return DriverBuild::Failed { message };
    }

    if cancel.is_cancelled() {
        return DriverBuild::Cancelled { scanned_files: 0 };
    }

    let mut analyzable = Vec::new();
    for (position, path) in paths.iter().enumerate() {
        if position != 0 && position % PREFILTER_CANCEL_POLL_INTERVAL == 0 && cancel.is_cancelled()
        {
            return DriverBuild::Cancelled { scanned_files: 0 };
        }
        if supports(registry, Path::new(path))
            && loader
                .probe(path)
                .is_some_and(|length| length <= options.max_file_bytes)
        {
            analyzable.push(path);
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .clamp(1, options.max_workers.max(1))
        .min(analyzable.len().max(1));
    let chunk_size = analyzable.len().div_ceil(workers).max(1);

    let outcomes: Vec<std::thread::Result<ChunkOutcome>> = std::thread::scope(|scope| {
        let handles: Vec<_> = analyzable
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || analyze_chunk(registry, loader, chunk, cancel, analyzer))
            })
            .collect();
        handles.into_iter().map(|handle| handle.join()).collect()
    });

    let mut files = Vec::new();
    let mut scanned_files = 0;
    let mut stopped_early = false;
    for outcome in outcomes {
        let Ok(outcome) = outcome else {
            return DriverBuild::Panicked;
        };
        files.extend(outcome.files);
        scanned_files += outcome.scanned;
        stopped_early |= outcome.stopped_early;
    }

    if stopped_early {
        return DriverBuild::Cancelled { scanned_files };
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    DriverBuild::Completed {
        files,
        scanned_files,
    }
}

fn analyze_chunk(
    registry: &LanguageRegistry,
    loader: &dyn SourceLoader,
    paths: &[&String],
    cancel: &dyn CancelSignal,
    analyzer: Analyzer,
) -> ChunkOutcome {
    let mut pool = ParserPool::new(registry);
    let mut outcome = ChunkOutcome {
        files: Vec::new(),
        scanned: 0,
        stopped_early: false,
    };

    for path in paths {
        if cancel.is_cancelled() {
            outcome.stopped_early = true;
            return outcome;
        }
        outcome.scanned += 1;
        let Some(source) = loader.load(path) else {
            continue;
        };
        let content_hash = xxh3_64(source.as_bytes());
        outcome
            .files
            .push(analyzer(&source, path, content_hash, &mut pool));
    }

    outcome
}

fn supports_symbols(registry: &LanguageRegistry, path: &Path) -> bool {
    registry.supports_symbols(path)
}

fn supports_analysis(registry: &LanguageRegistry, path: &Path) -> bool {
    registry.supports_symbols(path) || registry.supports_imports(path)
}

fn analyze_symbols_only(
    source: &str,
    path: &str,
    content_hash: u64,
    pool: &mut ParserPool<'_>,
) -> FileAnalysis {
    FileAnalysis {
        path: CompactString::from(path),
        content_hash,
        language: None,
        symbols: extract_symbols(source, path, pool),
        imports: Vec::new(),
        has_opaque_imports: false,
    }
}
