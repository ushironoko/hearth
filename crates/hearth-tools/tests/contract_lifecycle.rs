//! Engine lifecycle: one long-lived engine per process, and a clean shutdown
//! that leaves nothing running behind it.

mod common;

use common::{engine, seed, warm_engine};
use hearth_core::{Engine, EngineConfig};
use hearth_proto::*;
use hearth_tools::{bash, grep, read, write};
use std::time::{Duration, Instant};

/// The pooled shell's own pid: `$$` keeps the main shell's pid even inside the
/// per-command subshell.
fn warm_shell_pid(eng: &Engine) -> i32 {
    let r = bash(eng, &BashParams::new("printf '%s' \"$$\"")).unwrap();
    r.stdout.trim().parse().expect("shell pid")
}

fn wait_for_exit(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        // SAFETY: signal 0 only probes for the process's existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn dropping_an_engine_kills_its_pooled_shells() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());

    let pid = warm_shell_pid(&eng);
    // The same shell is reused, which is the point of the pool.
    assert_eq!(warm_shell_pid(&eng), pid);

    drop(eng);
    assert!(
        wait_for_exit(pid, Duration::from_secs(5)),
        "a dropped engine must not leave its shells running"
    );
}

#[test]
fn engines_can_be_created_and_dropped_repeatedly_without_accumulating_shells() {
    let dir = tempfile::tempdir().unwrap();
    let mut pids = Vec::new();
    for _ in 0..8 {
        let eng = warm_engine(dir.path());
        pids.push(warm_shell_pid(&eng));
        // Exercise the optimizer thread too, which starts with the engine.
        drop(eng);
    }
    for pid in pids {
        assert!(wait_for_exit(pid, Duration::from_secs(5)), "shell {pid} outlived its engine");
    }
}

#[test]
fn one_engine_serves_every_tool_and_stays_coherent_across_them() {
    let dir = tempfile::tempdir().unwrap();
    let eng = Engine::new(EngineConfig {
        default_cwd: dir.path().to_path_buf(),
        enable_optimizer: true,
        warm_shell: true,
        trust_cache: true,
        optimizer_interval_ms: 100,
        ..EngineConfig::default()
    });

    let path = dir.path().join("src.rs").display().to_string();
    write(&eng, &WriteParams::new(&path, "fn marker() {}\n")).unwrap();
    assert_eq!(read(&eng, &ReadParams::new(&path)).unwrap().content, "fn marker() {}\n");
    assert_eq!(
        grep(&eng, &GrepParams::new("marker", dir.path().display().to_string()))
            .unwrap()
            .files
            .len(),
        1
    );
    assert_eq!(bash(&eng, &BashParams::new("printf ok")).unwrap().stdout, "ok");

    // Let the optimizer tick at least once while the engine is live.
    std::thread::sleep(Duration::from_millis(250));
    assert!(eng.cache_report().contains("hit_rate"));
    drop(eng);
}

#[test]
fn a_relative_path_resolves_against_the_engine_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    seed(dir.path(), "rel.txt", "content\n");

    let r = read(&eng, &ReadParams::new("rel.txt")).unwrap();
    assert_eq!(r.content, "content\n");
}
