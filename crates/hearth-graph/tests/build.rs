#![cfg(all(feature = "bundled-languages", feature = "fs"))]

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use hearth_graph::{
    BuildOptions, CancelSignal, FsLoader, IndexBuild, LanguageRegistry, NeverCancelled,
    SourceLoader, SymbolIndex, build_index,
};

/// Fires after it has been polled `limit` times.
struct PollCountCancel {
    limit: usize,
    polls: AtomicUsize,
}

impl CancelSignal for PollCountCancel {
    fn is_cancelled(&self) -> bool {
        self.polls.fetch_add(1, Ordering::SeqCst) >= self.limit
    }
}

#[derive(Clone, Default)]
struct FlagCancel {
    cancelled: Arc<AtomicBool>,
}

impl FlagCancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl CancelSignal for FlagCancel {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn completed_index(build: IndexBuild) -> SymbolIndex {
    match build {
        IndexBuild::Completed(index) => index,
        IndexBuild::Cancelled { scanned_files } => {
            panic!("build was cancelled after scanning {scanned_files} files")
        }
        IndexBuild::Failed { message } => panic!("build failed: {message}"),
    }
}

fn build_with_fs(
    root: &std::path::Path,
    paths: &[String],
    cancel: &dyn CancelSignal,
) -> IndexBuild {
    let registry = LanguageRegistry::bundled();
    let loader = FsLoader::new(root);
    build_index(&registry, &loader, paths, cancel, &BuildOptions::default())
}

#[test]
fn test_cancelled_build_stops_scanning_early() {
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().join("src");
    std::fs::create_dir(&source_dir).unwrap();
    let paths: Vec<_> = (0..1_000)
        .map(|index| {
            let path = format!("src/file_{index:04}.rs");
            std::fs::write(dir.path().join(&path), format!("pub fn f{index}() {{}}\n")).unwrap();
            path
        })
        .collect();

    let completed = build_with_fs(dir.path(), &paths, &NeverCancelled);
    let control_scanned = completed_index(completed).scanned_file_count();
    assert_eq!(control_scanned, 1_000);

    let cancel = PollCountCancel {
        limit: 50,
        polls: AtomicUsize::new(0),
    };
    match build_with_fs(dir.path(), &paths, &cancel) {
        IndexBuild::Cancelled { scanned_files } => {
            assert!(scanned_files <= 64, "scanned {scanned_files} files");
            assert!(scanned_files < control_scanned);
            assert!(scanned_files < 1_000);
        }
        IndexBuild::Completed(index) => panic!(
            "cancelled build completed after scanning {} files",
            index.scanned_file_count()
        ),
        IndexBuild::Failed { message } => {
            panic!("cancelled build failed instead of stopping early: {message}")
        }
    }
}

#[test]
fn test_cancelled_build_stops_the_metadata_prefilter() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..5_000)
        .map(|index| format!("notes/file_{index:04}.txt"))
        .collect();
    let cancel = PollCountCancel {
        limit: 1,
        polls: AtomicUsize::new(0),
    };

    match build_with_fs(dir.path(), &paths, &cancel) {
        IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
        IndexBuild::Completed(_) => panic!("metadata pre-filter ignored cancellation"),
        IndexBuild::Failed { message } => {
            panic!("metadata pre-filter failed instead of cancelling: {message}")
        }
    }
}

#[test]
fn test_precancelled_build_scans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.rs"), "pub fn present() {}\n").unwrap();
    let cancel = FlagCancel::default();
    cancel.cancel();

    match build_with_fs(dir.path(), &["file.rs".to_owned()], &cancel) {
        IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
        other => panic!("pre-cancelled build did not cancel: {other:?}"),
    }
}

#[test]
fn test_precancelled_build_with_no_indexable_paths_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = FlagCancel::default();
    cancel.cancel();
    // Below the pre-filter poll interval and non-indexable, so only the
    // pre-build cancellation check can stop this build.
    let paths = ["notes.txt".to_owned()];

    match build_with_fs(dir.path(), &paths, &cancel) {
        IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
        other => panic!("pre-cancelled empty build did not cancel: {other:?}"),
    }
}

#[test]
fn test_build_over_a_missing_root_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");

    match build_with_fs(&missing, &[], &NeverCancelled) {
        IndexBuild::Failed { message } => {
            assert!(
                message.contains(&missing.display().to_string()),
                "{message}"
            );
            assert!(message.contains("is unavailable"), "{message}");
        }
        other => panic!("missing root did not fail: {other:?}"),
    }
}

#[test]
fn test_a_root_that_is_a_file_fails_instead_of_indexing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_directory = dir.path().join("root.rs");
    std::fs::write(&not_a_directory, "pub fn a() {}\n").unwrap();

    match build_with_fs(
        &not_a_directory,
        &["src/lib.rs".to_owned()],
        &NeverCancelled,
    ) {
        IndexBuild::Failed { message } => {
            assert!(message.contains("is not a directory"), "{message}");
            assert!(
                message.contains(&not_a_directory.display().to_string()),
                "{message}"
            );
        }
        IndexBuild::Completed(index) => panic!(
            "a non-directory root degraded into an index with {} symbols",
            index.symbol_count()
        ),
        IndexBuild::Cancelled { .. } => panic!("nothing cancelled this build"),
    }
}

#[test]
fn test_completed_build_reports_every_walked_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("symbols.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(dir.path().join("comments.rs"), "// no symbols here\n").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "unsupported\n").unwrap();
    let paths = vec![
        "symbols.rs".to_owned(),
        "comments.rs".to_owned(),
        "notes.txt".to_owned(),
        "missing.rs".to_owned(),
    ];

    let index = completed_index(build_with_fs(dir.path(), &paths, &NeverCancelled));
    assert_eq!(index.scanned_file_count(), 2);
    // Deliberate deviation D2: octorus drops comments.rs and reports one
    // indexed file, while hearth remembers that it parsed an empty file.
    assert_eq!(index.paths().count(), 2);
    assert!(
        index
            .file_symbols("comments.rs")
            .is_some_and(<[hearth_graph::Symbol]>::is_empty)
    );
    assert!(index.file_symbols("notes.txt").is_none());
    assert!(index.file_symbols("missing.rs").is_none());
}

#[test]
fn test_build_indexes_supported_files_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(dir.path().join("src/b.ts"), "export function beta() {}\n").unwrap();
    std::fs::write(
        dir.path().join("src/c.vue"),
        "<script setup lang=\"ts\">\nexport function gamma() {}\n</script>\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not code\n").unwrap();
    let paths = vec![
        "src/a.rs".to_owned(),
        "src/b.ts".to_owned(),
        "src/c.vue".to_owned(),
        "notes.txt".to_owned(),
        "src/missing.rs".to_owned(),
    ];

    let index = completed_index(build_with_fs(dir.path(), &paths, &NeverCancelled));
    assert_eq!(index.paths().count(), 3);
    assert_eq!(index.definitions("alpha").len(), 1);
    assert_eq!(index.definitions("beta")[0].path, "src/b.ts");
    assert_eq!(index.definitions("gamma")[0].path, "src/c.vue");
}

struct PanickingLoader {
    marker: String,
    cancel: FlagCancel,
}

impl SourceLoader for PanickingLoader {
    fn verify(&self) -> Result<(), String> {
        Ok(())
    }

    fn probe(&self, _path: &str) -> Option<u64> {
        Some(32)
    }

    fn load(&self, path: &str) -> Option<String> {
        if path == self.marker {
            self.cancel.cancel();
            panic!("injected SourceLoader panic");
        }
        Some("pub fn ordinary() {}\n".to_owned())
    }
}

#[test]
fn test_build_fails_when_index_worker_panics_and_beats_cancellation() {
    let registry = LanguageRegistry::bundled();
    let cancel = FlagCancel::default();
    let marker = "virtual/panic.rs".to_owned();
    let loader = PanickingLoader {
        marker: marker.clone(),
        cancel: cancel.clone(),
    };
    let mut paths = vec![marker];
    paths.extend((0..63).map(|index| format!("virtual/ordinary_{index:02}.rs")));

    match build_index(
        &registry,
        &loader,
        &paths,
        &cancel,
        &BuildOptions::default(),
    ) {
        IndexBuild::Failed { message } => {
            assert!(
                message.contains("symbol indexing worker panicked"),
                "{message}"
            );
        }
        IndexBuild::Completed(index) => panic!(
            "worker panic degraded into a completed index with {} files",
            index.paths().count()
        ),
        IndexBuild::Cancelled { scanned_files } => {
            panic!("worker panic lost to cancellation after {scanned_files} files")
        }
    }
}

#[test]
fn test_build_skips_oversized_files() {
    let defaults = BuildOptions::default();
    assert_eq!(defaults.max_file_bytes, 2 * 1024 * 1024);

    let dir = tempfile::tempdir().unwrap();
    let huge = format!(
        "pub fn big() {{}}\n{}",
        "// filler filler filler filler\n".repeat(80_000)
    );
    assert!(huge.len() as u64 > defaults.max_file_bytes);
    std::fs::write(dir.path().join("big.rs"), huge).unwrap();
    std::fs::write(dir.path().join("small.rs"), "pub fn small() {}\n").unwrap();

    let registry = LanguageRegistry::bundled();
    let loader = FsLoader::new(dir.path());
    let index = completed_index(build_index(
        &registry,
        &loader,
        &["big.rs".to_owned(), "small.rs".to_owned()],
        &NeverCancelled,
        &defaults,
    ));
    assert_eq!(index.definitions("small").len(), 1);
    assert!(index.file_symbols("big.rs").is_none());

    std::fs::write(dir.path().join("tiny.rs"), "fn tiny(){}\n").unwrap();
    std::fs::write(
        dir.path().join("custom_too_large.rs"),
        "pub fn custom_too_large() {}\n",
    )
    .unwrap();
    let tiny_options = BuildOptions {
        max_file_bytes: 16,
        ..defaults
    };
    let index = completed_index(build_index(
        &registry,
        &loader,
        &["custom_too_large.rs".to_owned(), "tiny.rs".to_owned()],
        &NeverCancelled,
        &tiny_options,
    ));
    assert_eq!(index.definitions("tiny").len(), 1);
    assert!(index.file_symbols("custom_too_large.rs").is_none());
}

#[test]
fn test_build_with_no_paths() {
    let dir = tempfile::tempdir().unwrap();
    let index = completed_index(build_with_fs(dir.path(), &[], &NeverCancelled));
    assert_eq!(index.scanned_file_count(), 0);
    assert_eq!(index.paths().count(), 0);
}

#[test]
fn test_deterministic_path_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let paths: Vec<_> = (0..24)
        .rev()
        .map(|index| {
            let path = format!("src/file_{index:02}.rs");
            std::fs::write(
                dir.path().join(&path),
                format!("pub fn item_{index}() {{}}\n"),
            )
            .unwrap();
            path
        })
        .collect();
    let mut expected = paths.clone();
    expected.sort();

    let first = completed_index(build_with_fs(dir.path(), &paths, &NeverCancelled));
    let second = completed_index(build_with_fs(dir.path(), &paths, &NeverCancelled));
    let first_paths: Vec<_> = first.paths().map(str::to_owned).collect();
    let second_paths: Vec<_> = second.paths().map(str::to_owned).collect();

    assert_eq!(first_paths, expected);
    assert_eq!(second_paths, first_paths);
}

struct MemoryLoader {
    sources: HashMap<String, String>,
}

impl SourceLoader for MemoryLoader {
    fn verify(&self) -> Result<(), String> {
        Ok(())
    }

    fn probe(&self, path: &str) -> Option<u64> {
        self.sources.get(path).map(|source| source.len() as u64)
    }

    fn load(&self, path: &str) -> Option<String> {
        self.sources.get(path).cloned()
    }
}

#[test]
fn test_driver_uses_only_the_loader_seam() {
    let registry = LanguageRegistry::bundled();
    let loader = MemoryLoader {
        sources: HashMap::from([
            (
                "virtual/a.rs".to_owned(),
                "pub fn virtual_alpha() {}\n".to_owned(),
            ),
            (
                "virtual/b.ts".to_owned(),
                "export function virtualBeta() {}\n".to_owned(),
            ),
            (
                "virtual/notes.txt".to_owned(),
                "not source code\n".to_owned(),
            ),
        ]),
    };
    let paths = vec![
        "virtual/a.rs".to_owned(),
        "virtual/b.ts".to_owned(),
        "virtual/notes.txt".to_owned(),
        "virtual/missing.rs".to_owned(),
    ];

    let index = completed_index(build_index(
        &registry,
        &loader,
        &paths,
        &NeverCancelled,
        &BuildOptions::default(),
    ));
    assert_eq!(index.scanned_file_count(), 2);
    assert_eq!(index.paths().count(), 2);
    assert_eq!(index.definitions("virtual_alpha").len(), 1);
    assert_eq!(index.definitions("virtualBeta").len(), 1);
}

#[test]
fn test_empty_symbol_file_is_remembered_via_build() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("comments.rs"),
        "// successfully parsed, with no symbols\n",
    )
    .unwrap();

    let index = completed_index(build_with_fs(
        dir.path(),
        &["comments.rs".to_owned()],
        &NeverCancelled,
    ));
    let symbols = index
        .file_symbols("comments.rs")
        .expect("D2 keeps successfully loaded empty files");
    assert!(symbols.is_empty());
}
