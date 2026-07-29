//! Shared fixtures for the contract suites.
//!
//! Every test binary compiles this whole module, so helpers used by only some
//! of them are expected to look dead here.
#![allow(dead_code)]

use hearth_core::{Engine, EngineConfig};
use std::path::Path;

/// A quiet engine: no optimizer thread, no watcher, correctness-first caching.
pub fn engine(cwd: &Path) -> Engine {
    Engine::new(EngineConfig {
        default_cwd: cwd.to_path_buf(),
        enable_optimizer: false,
        enable_watch: false,
        ..EngineConfig::default()
    })
}

/// As [`engine`], with recursive filesystem watching enabled.
pub fn watching_engine(cwd: &Path) -> Engine {
    Engine::new(EngineConfig {
        default_cwd: cwd.to_path_buf(),
        enable_optimizer: false,
        enable_watch: true,
        ..EngineConfig::default()
    })
}

/// As [`engine`], with the `trustCache` fast path on.
pub fn trusting_engine(cwd: &Path) -> Engine {
    Engine::new(EngineConfig {
        default_cwd: cwd.to_path_buf(),
        enable_optimizer: false,
        enable_watch: false,
        trust_cache: true,
        ..EngineConfig::default()
    })
}

/// As [`engine`], with the warm-shell pool on.
pub fn warm_engine(cwd: &Path) -> Engine {
    Engine::new(EngineConfig {
        default_cwd: cwd.to_path_buf(),
        enable_optimizer: false,
        enable_watch: false,
        warm_shell: true,
        ..EngineConfig::default()
    })
}

/// An absolute path inside `dir`, as the string the tools take.
pub fn abs(dir: &Path, name: &str) -> String {
    dir.join(name).display().to_string()
}

/// Write a fixture file straight to disk, bypassing Hearth.
pub fn seed(dir: &Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    path.display().to_string()
}
