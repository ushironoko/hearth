//! Ablates the compounded warm-`grep` win into directory-walk-cache and
//! file-content-cache contributions by searching the same 2,000-file corpus
//! with both caches cold, either cache warm, or both caches warm.

use criterion::{Criterion, criterion_group, criterion_main};
use hearth_bench::{bench_engine, gen_corpus};
use hearth_proto::{GrepMode, GrepParams};
use hearth_tools::grep;
use std::hint::black_box;
use std::path::Path;

fn params(root: &Path) -> GrepParams {
    GrepParams {
        pattern: "TODO_MATCH".into(),
        path: root.display().to_string(),
        mode: GrepMode::FilesWithMatches,
        globs: vec![],
        ..Default::default()
    }
}

fn bench_grep_ablation(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    gen_corpus(root, 2000, 32, 200);
    let p = params(root);

    let walk_only_engine = bench_engine(root);
    let _ = grep(&walk_only_engine, &p).unwrap();

    let content_only_engine = bench_engine(root);
    let _ = grep(&content_only_engine, &p).unwrap();

    let both_warm_engine = bench_engine(root);
    let _ = grep(&both_warm_engine, &p).unwrap();

    let mut group = c.benchmark_group("grep_ablation_2000files");
    group.sample_size(30);

    // Fresh-engine construction is included by design: a one-shot process pays it.
    group.bench_function("all_cold", |b| {
        b.iter(|| {
            let engine = bench_engine(root);
            black_box(grep(&engine, &p).unwrap().files.len())
        });
    });

    group.bench_function("walk_only", |b| {
        b.iter(|| {
            walk_only_engine.files().clear();
            black_box(grep(&walk_only_engine, &p).unwrap().files.len())
        });
    });

    group.bench_function("content_only", |b| {
        b.iter(|| {
            content_only_engine.walks().clear();
            black_box(grep(&content_only_engine, &p).unwrap().files.len())
        });
    });

    group.bench_function("both_warm", |b| {
        b.iter(|| black_box(grep(&both_warm_engine, &p).unwrap().files.len()));
    });

    group.finish();
}

criterion_group!(benches, bench_grep_ablation);
criterion_main!(benches);
