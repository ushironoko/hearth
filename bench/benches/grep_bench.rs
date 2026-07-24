//! Warm `grep` (walk cached) vs cold `grep` (fresh engine) vs a hand-rolled
//! `ignore`-walk + `grep-searcher` baseline (the same engine ripgrep uses).
//!
//! The warm case is the resident-server advantage: the tree walk and
//! `.gitignore` parsing happen once and are reused.

use criterion::{criterion_group, criterion_main, Criterion};
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use hearth_bench::{bench_engine, bench_engine_trusted, gen_corpus};
use hearth_core::Engine;
use hearth_proto::{GrepMode, GrepParams};
use hearth_tools::grep;
use ignore::WalkBuilder;
use std::hint::black_box;
use std::path::Path;

fn params(root: &Path) -> GrepParams {
    GrepParams {
        pattern: "TODO_MATCH".into(),
        path: root.display().to_string(),
        mode: GrepMode::FilesWithMatches,
        globs: vec![],
        case_insensitive: false,
        smart_case: false,
        fixed_strings: false,
        multiline: false,
        before_context: 0,
        after_context: 0,
        max_count: None,
        hidden: false,
        respect_gitignore: true,
        follow_symlinks: false,
    }
}

/// A minimal ripgrep-equivalent: walk with `ignore`, search each file with
/// `grep-searcher`. Rebuilds the walk every call, like a one-shot `rg`.
fn baseline(root: &Path, threads: usize) -> u64 {
    let matcher = RegexMatcher::new("TODO_MATCH").unwrap();
    let count = std::sync::atomic::AtomicU64::new(0);
    let (tx, rx) = crossbeam_channel::unbounded::<std::path::PathBuf>();
    // walk
    let sink = parking_lot::Mutex::new(Vec::new());
    WalkBuilder::new(root).threads(threads).build_parallel().run(|| {
        Box::new(|res| {
            if let Ok(e) = res {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    sink.lock().push(e.into_path());
                }
            }
            ignore::WalkState::Continue
        })
    });
    for p in sink.into_inner() {
        let _ = tx.send(p);
    }
    drop(tx);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let rx = rx.clone();
            let matcher = &matcher;
            let count = &count;
            s.spawn(move || {
                let mut searcher = SearcherBuilder::new().build();
                while let Ok(path) = rx.recv() {
                    let mut hit = false;
                    let _ = searcher.search_path(matcher, &path, UTF8(|_, _| {
                        hit = true;
                        Ok(false)
                    }));
                    if hit {
                        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    });
    count.load(std::sync::atomic::Ordering::Relaxed)
}

fn bench_grep(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // ~2000 files across 32 dirs, 200 lines each (~a mid-size repo).
    gen_corpus(root, 2000, 32, 200);
    let threads = num_cpus::get();

    let warm_engine = bench_engine(root);
    let p = params(root);
    let _ = grep(&warm_engine, &p).unwrap(); // warm the walk cache

    let mut group = c.benchmark_group("grep_2000files");
    group.sample_size(30);

    group.bench_function("hearth_warm", |b| {
        b.iter(|| black_box(grep(&warm_engine, &p).unwrap().files.len()));
    });

    // Content mode (many matches) — exercises the arena sink's per-line path.
    let content_p = GrepParams { pattern: "function_".into(), mode: GrepMode::Content, ..p.clone() };
    let _ = grep(&warm_engine, &content_p).unwrap();
    group.bench_function("hearth_warm_content", |b| {
        b.iter(|| black_box(grep(&warm_engine, &content_p).unwrap().total_matches));
    });

    // Stat-free warm mode (trust_watch): warm hits skip the per-file freshness stat.
    let trusted_engine = bench_engine_trusted(root);
    let _ = grep(&trusted_engine, &p).unwrap(); // warm + start watcher + record root
    group.bench_function("hearth_warm_trusted", |b| {
        b.iter(|| black_box(grep(&trusted_engine, &p).unwrap().files.len()));
    });
    group.bench_function("hearth_cold", |b| {
        b.iter(|| {
            let eng = Engine::new({
                let mut cfg = hearth_core::EngineConfig::default();
                cfg.default_cwd = root.to_path_buf();
                cfg.enable_optimizer = false;
                cfg
            });
            black_box(grep(&eng, &p).unwrap().files.len())
        });
    });
    group.bench_function("ignore_baseline", |b| {
        b.iter(|| black_box(baseline(root, threads)));
    });
    group.finish();
}

criterion_group!(benches, bench_grep);
criterion_main!(benches);
