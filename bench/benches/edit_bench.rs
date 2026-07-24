//! Real warm `edit` (cached read + atomic write) vs a naive disk-read + atomic
//! write baseline. Each iteration edits there-and-back so the file always has a
//! match and stays warm in the cache.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use hearth_bench::{bench_engine, file_text};
use hearth_proto::EditParams;
use hearth_tools::edit;
use std::hint::black_box;
use std::path::Path;

/// Naive baseline: read from disk, replace, write atomically (temp + rename).
fn baseline_edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    let new = text.replace(from, to);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, new.as_bytes()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn bench_edit(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let eng = bench_engine(dir.path());
    let sizes = [("medium_2k", 2000usize), ("large_20k", 20000)];

    let mut group = c.benchmark_group("edit");
    for (name, lines) in sizes {
        let path = dir.path().join(format!("{name}.rs"));
        std::fs::write(&path, file_text(7, lines)).unwrap();
        let ps = path.display().to_string();

        let there = EditParams { path: ps.clone(), old_string: "engine".into(), new_string: "ENGINE".into(), replace_all: true };
        let back = EditParams { path: ps.clone(), old_string: "ENGINE".into(), new_string: "engine".into(), replace_all: true };
        // Warm the cache.
        edit(&eng, &there).unwrap();
        edit(&eng, &back).unwrap();

        group.bench_with_input(BenchmarkId::new("hearth_warm", name), &(&there, &back), |b, (there, back)| {
            b.iter(|| {
                edit(&eng, there).unwrap();
                black_box(edit(&eng, back).unwrap().replacements)
            });
        });
        group.bench_with_input(BenchmarkId::new("disk_baseline", name), &path, |b, path| {
            b.iter(|| {
                baseline_edit(path, "engine", "ENGINE");
                baseline_edit(path, "ENGINE", "engine");
                black_box(())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_edit);
criterion_main!(benches);
