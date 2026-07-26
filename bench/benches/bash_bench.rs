//! Warm-shell pool vs spawn-per-command for a trivial command (in-process, so
//! there is no CLI-startup masking — this isolates the per-command shell cost).

use criterion::{criterion_group, criterion_main, Criterion};
use hearth_bench::bench_engine;
use hearth_core::{Engine, EngineConfig};
use hearth_proto::BashParams;
use hearth_tools::bash;
use std::hint::black_box;

fn warm_engine(cwd: &std::path::Path) -> Engine {
    Engine::new(EngineConfig {
        default_cwd: cwd.to_path_buf(),
        enable_optimizer: false,
        warm_shell: true,
        ..EngineConfig::default()
    })
}

fn bench_bash(c: &mut Criterion) {
    let tmp = tempfile::tempdir().unwrap();
    let spawn_eng = bench_engine(tmp.path()); // warm_shell = false
    let warm_eng = warm_engine(tmp.path());
    let p = BashParams { cwd: Some(tmp.path().display().to_string()), timeout_ms: Some(5000), env: vec![], ..BashParams::new("true") };
    let _ = bash(&warm_eng, &p); // prime the pool

    let mut g = c.benchmark_group("bash_true");
    g.bench_function("warm_shell", |b| {
        b.iter(|| black_box(bash(&warm_eng, &p).unwrap().exit_code));
    });
    g.bench_function("spawn", |b| {
        b.iter(|| black_box(bash(&spawn_eng, &p).unwrap().exit_code));
    });
    g.finish();
}

criterion_group!(benches, bench_bash);
criterion_main!(benches);
