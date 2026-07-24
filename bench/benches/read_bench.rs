//! COMPONENT microbenchmark: the in-process warm `read()` (cache serve + one
//! alloc/copy, UTF-8 validity cached) vs `std::fs::read_to_string`.
//!
//! This is NOT a `cat` comparison and must not be read as one: it excludes the
//! socket round-trip, msgpack encode/decode, process spawn, and stdout write
//! that the shipped CLI `read` pays, and `std::fs::read_to_string` itself both
//! omits work `cat` does (spawn, stdout write) and adds work `cat` skips (UTF-8
//! validation). It measures the *native/napi* read primitive only. For the
//! end-to-end `cat` comparison see `bench/harness/compare.sh` (where the CLI
//! `read` currently LOSES to `cat` — it is IPC-bound).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use hearth_bench::{bench_engine, file_text};
use hearth_proto::ReadParams;
use hearth_tools::read;
use std::hint::black_box;

fn bench_read(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let eng = bench_engine(dir.path());
    let sizes = [("small_100", 100usize), ("medium_2k", 2000), ("large_20k", 20000)];

    let mut group = c.benchmark_group("read");
    for (name, lines) in sizes {
        let path = dir.path().join(format!("{name}.rs"));
        let content = file_text(42, lines);
        std::fs::write(&path, &content).unwrap();
        let ps = path.display().to_string();
        group.throughput(Throughput::Bytes(content.len() as u64));

        let params = ReadParams { path: ps.clone(), offset: None, limit: None, line_numbers: false };
        let _ = read(&eng, &params).unwrap(); // warm the cache

        group.bench_with_input(BenchmarkId::new("hearth_warm", name), &params, |b, params| {
            b.iter(|| {
                let r = read(&eng, params).unwrap();
                black_box(r.byte_len)
            });
        });
        group.bench_with_input(BenchmarkId::new("std_fs", name), &path, |b, path| {
            b.iter(|| {
                let s = std::fs::read_to_string(path).unwrap();
                black_box(s.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_read);
criterion_main!(benches);
