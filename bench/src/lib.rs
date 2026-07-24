//! Benchmark helpers: deterministic corpus generation and engine construction.
//!
//! The corpus is fully deterministic (a tiny hash-based PRNG seeded by indices)
//! so criterion runs and the CLI hyperfine harness search identical trees.

use hearth_core::{Engine, EngineConfig};
use std::path::{Path, PathBuf};

/// A deterministic splitmix64 step — no global RNG, reproducible across runs.
#[inline]
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

const WORDS: &[&str] = &[
    "engine", "cache", "token", "buffer", "index", "walk", "shard", "arena",
    "vector", "matcher", "region", "handle", "session", "worker", "signal",
    "kernel", "packet", "stream", "cursor", "anchor", "module", "syntax",
];

/// Generate one file's UTF-8 text with `lines` lines. Some lines carry known
/// tokens so searches have predictable hit counts:
/// * every 37th line contains `TODO_MATCH`
/// * every 11th line contains `fn function_<n>`
pub fn file_text(seed: u64, lines: usize) -> String {
    let mut out = String::with_capacity(lines * 48);
    for l in 0..lines {
        let r = mix(seed ^ (l as u64).wrapping_mul(0x100000001B3));
        let w1 = WORDS[(r % WORDS.len() as u64) as usize];
        let w2 = WORDS[((r >> 8) % WORDS.len() as u64) as usize];
        let n = r % 1000;
        if l % 37 == 0 {
            out.push_str("// TODO_MATCH revisit this path\n");
        } else if l % 11 == 0 {
            out.push_str(&format!("fn function_{n}() {{ let {w1} = {w2}; }}\n"));
        } else {
            out.push_str(&format!("    {w1}_{n} = {w2}({n});\n"));
        }
    }
    out
}

/// Write a corpus of `num_files` files spread across `dirs` subdirectories,
/// each with `lines_per_file` lines. Returns the paths written. Idempotent for a
/// given (root, params): re-running overwrites the same content.
pub fn gen_corpus(root: &Path, num_files: usize, dirs: usize, lines_per_file: usize) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(num_files);
    for d in 0..dirs {
        std::fs::create_dir_all(root.join(format!("d{d:03}"))).unwrap();
    }
    for i in 0..num_files {
        let d = i % dirs;
        let path = root.join(format!("d{d:03}")).join(format!("f{i:05}.rs"));
        std::fs::write(&path, file_text(i as u64, lines_per_file)).unwrap();
        paths.push(path);
    }
    paths
}

/// Like [`gen_corpus`] but with a heavy-tailed size distribution: most files
/// are small, ~1 in 8 are medium, ~1 in 64 are large. `lines` sets the small
/// baseline. Deterministic per index.
pub fn gen_corpus_skewed(root: &Path, num_files: usize, dirs: usize, lines: usize) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(num_files);
    for d in 0..dirs {
        std::fs::create_dir_all(root.join(format!("d{d:03}"))).unwrap();
    }
    for i in 0..num_files {
        let d = i % dirs;
        let n = if i % 64 == 0 {
            lines * 10
        } else if i % 8 == 0 {
            lines * 3
        } else {
            lines
        };
        let path = root.join(format!("d{d:03}")).join(format!("f{i:05}.rs"));
        std::fs::write(&path, file_text(i as u64, n)).unwrap();
        paths.push(path);
    }
    paths
}

/// A warm engine (optimizer/watch off for benchmark stability).
pub fn bench_engine(cwd: &Path) -> Engine {
    let mut cfg = EngineConfig::default();
    cfg.default_cwd = cwd.to_path_buf();
    cfg.enable_optimizer = false;
    cfg.enable_watch = false;
    Engine::new(cfg)
}

/// A warm engine in `trust_cache` mode: warm hits skip the freshness stat
/// (no fs-watcher; single-writer assumption).
pub fn bench_engine_trusted(cwd: &Path) -> Engine {
    let mut cfg = EngineConfig::default();
    cfg.default_cwd = cwd.to_path_buf();
    cfg.enable_optimizer = false;
    cfg.enable_watch = false;
    cfg.trust_cache = true;
    Engine::new(cfg)
}
