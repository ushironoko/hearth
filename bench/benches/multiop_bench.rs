//! Isolates the resident server's cross-operation amortization by reusing its
//! directory-walk and file-content caches across a realistic read -> grep ->
//! edit sequence on the same 2,000-file tree.

use criterion::{Criterion, criterion_group, criterion_main};
use hearth_bench::{bench_engine, gen_corpus};
use hearth_core::Engine;
use hearth_proto::{EditParams, GrepMode, GrepParams, ReadParams};
use hearth_tools::{edit, grep, read};
use std::hint::black_box;

fn bench_agent_multiop(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    gen_corpus(root, 2000, 32, 200);

    let target = root.join("d000").join("f00000.rs").display().to_string();
    let read_params = ReadParams { offset: None, limit: None, line_numbers: false, ..ReadParams::new(target.clone()) };
    let grep_params = GrepParams {
        pattern: "TODO_MATCH".into(),
        path: root.display().to_string(),
        mode: GrepMode::FilesWithMatches,
        globs: vec![],
        ..Default::default()
    };
    let edit_there = EditParams {
        path: target.clone(),
        old_string: "engine".into(),
        new_string: "ENGINE".into(),
        replace_all: true,
    };
    let edit_back = EditParams {
        path: target,
        old_string: "ENGINE".into(),
        new_string: "engine".into(),
        replace_all: true,
    };

    let sequence = |engine: &Engine| {
        black_box(read(engine, &read_params).unwrap().byte_len);
        black_box(grep(engine, &grep_params).unwrap().files.len());
        edit(engine, &edit_there).unwrap();
        black_box(edit(engine, &edit_back).unwrap().replacements);
    };

    let warm_engine = bench_engine(root);
    sequence(&warm_engine);

    let mut group = c.benchmark_group("agent_multiop_2000files");
    group.sample_size(30);

    group.bench_function("warm_resident", |b| {
        b.iter(|| sequence(&warm_engine));
    });

    group.bench_function("cold_per_iter", |b| {
        b.iter(|| {
            let engine = bench_engine(root);
            sequence(&engine);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_agent_multiop);
criterion_main!(benches);
