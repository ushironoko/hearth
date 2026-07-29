//! Low-selectivity verified-rdeps benchmark.
//!
//! The default corpus has 1,000 TypeScript files. Most mention the target stem
//! in a comment, while only a few import it, forcing the grep backstop to feed
//! many candidates through the bounded repair loop.
//!
//! The 200k-file scale is opt-in because generating and repeatedly scanning it
//! is intentionally expensive:
//! `HEARTH_BENCH_RDEPS_FILES=200000 cargo bench -p hearth-bench --bench graph_rdeps_bench`
//!
//! Measurement status: criterion smoke (`--test`) has been run at the 1k
//! default only; the 200k scale has NOT been executed — no numbers at that
//! scale are recorded anywhere in this repository.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use hearth_bench::bench_engine;
use hearth_proto::{GraphOp, GraphOutput, GraphParams};
use hearth_tools::graph;
use std::hint::black_box;
use std::path::{Path, PathBuf};

const DEFAULT_FILE_COUNT: usize = 1_000;
const TARGET_STEM: &str = "needle_anchor";
const ACTUAL_IMPORTERS: usize = 8;

fn requested_file_count() -> usize {
    std::env::var("HEARTH_BENCH_RDEPS_FILES").map_or(DEFAULT_FILE_COUNT, |value| {
        value
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("invalid HEARTH_BENCH_RDEPS_FILES={value:?}: {error}"))
            .max(2)
    })
}

fn gen_rdeps_corpus(root: &Path, file_count: usize) -> PathBuf {
    let target = root.join(format!("{TARGET_STEM}.ts"));
    std::fs::write(&target, "export const anchor = true;\n").unwrap();

    let candidate_count = file_count - 1;
    let directory_count = file_count.div_ceil(1_000).clamp(16, 256);
    let importer_stride = candidate_count.div_ceil(ACTUAL_IMPORTERS).max(1);
    for directory in 0..directory_count {
        std::fs::create_dir_all(root.join(format!("d{directory:03}"))).unwrap();
    }

    for index in 0..candidate_count {
        let directory = index % directory_count;
        let path = root
            .join(format!("d{directory:03}"))
            .join(format!("candidate_{index:06}.ts"));
        let source = if index.is_multiple_of(importer_stride) {
            format!(
                "import {{ anchor }} from \"../{TARGET_STEM}\";\n\
                 export const value_{index} = anchor;\n"
            )
        } else if !index.is_multiple_of(20) {
            format!(
                "// {TARGET_STEM} is common text, not an import\n\
                 export const value_{index} = {index};\n"
            )
        } else {
            format!("export const value_{index} = {index};\n")
        };
        std::fs::write(path, source).unwrap();
    }

    target
}

fn rdeps_params(root: &Path, target: &Path) -> GraphParams {
    let target = target.display().to_string();
    let mut params = GraphParams::new(
        root.display().to_string(),
        GraphOp::Rdeps {
            path: target.clone(),
            depth: 1,
            verify: true,
        },
    );
    // Keep the initial sweep deliberately incomplete. Verification must grep
    // the full root, then repair or reject the low-selectivity candidates.
    params.files = vec![target];
    params
}

fn observed_result(result: hearth_proto::GraphResult) -> (usize, bool, bool) {
    let GraphOutput::Rdeps(rdeps) = result.output else {
        unreachable!("rdeps query returned a different graph payload");
    };
    (
        rdeps.importers.len(),
        rdeps.verified,
        result.meta.repair_truncated,
    )
}

fn bench_graph_rdeps(c: &mut Criterion) {
    let file_count = requested_file_count();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = gen_rdeps_corpus(root, file_count);
    let params = rdeps_params(root, &target);
    let warm_engine = bench_engine(root);
    black_box(graph(&warm_engine, &params).unwrap());

    let mut group = c.benchmark_group(format!("graph_rdeps_low_selectivity_{file_count}files"));
    group.sample_size(10);

    group.bench_function("cold_first_rdeps", |b| {
        b.iter_batched(
            || bench_engine(root),
            |engine| {
                let result = graph(&engine, &params).unwrap();
                black_box(observed_result(result))
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("warm_repeat_rdeps", |b| {
        b.iter(|| {
            let result = graph(&warm_engine, &params).unwrap();
            black_box(observed_result(result))
        });
    });

    group.finish();
}

criterion_group!(benches, bench_graph_rdeps);
criterion_main!(benches);
