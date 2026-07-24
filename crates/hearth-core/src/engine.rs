//! The resident engine: one cheap-to-clone handle bundling every shared
//! resource (file cache, walk cache, profiler, fs-watch) plus a background
//! self-optimization loop. The daemon, the CLI, and the napi addon each hold
//! one `Engine` for their whole lifetime; tools borrow it per call.

use crate::cache::{FileCache, WalkCache};
use crate::watch::WatchHandle;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::any::{Any, TypeId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Tunable engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Default working directory for `bash` and relative-path resolution.
    pub default_cwd: PathBuf,
    /// Default hard timeout for `bash` commands (ms).
    pub bash_timeout_ms: u64,
    /// Use the pooled warm-shell fast path for `bash` (opt-in). Default false:
    /// each command spawns a fresh `/bin/sh -c` (always correct). The warm pool
    /// avoids the per-command spawn but falls back to a fresh spawn on any
    /// protocol anomaly.
    pub warm_shell: bool,
    /// Threads used by the parallel directory walker.
    pub walk_threads: usize,
    /// Start an fs-watcher on searched roots to proactively invalidate caches.
    pub enable_watch: bool,
    /// Skip the per-hit freshness `stat` on warm reads/greps. This is a
    /// single-writer / bounded-staleness fast path: it assumes files are only
    /// modified *through* Hearth (whose `write`/`edit` refresh the cache) — an
    /// external edit would be served stale until evicted. No fs-watcher is used
    /// (on macOS FSEvents cannot distinguish an atime bump from a real write, so
    /// a watcher would invalidate the cache on Hearth's own reads). Default off:
    /// correctness-first. Opt in when Hearth owns the workspace.
    pub trust_cache: bool,
    /// Run the background self-optimization loop.
    pub enable_optimizer: bool,
    /// Hard safety cap on the number of files kept warm in the content cache.
    pub max_cached_files: usize,
    /// Lower/upper bounds for the adaptive cache byte budget.
    pub min_cache_bytes: u64,
    pub max_cache_bytes: u64,
    /// How often the optimizer runs (ms).
    pub optimizer_interval_ms: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let threads = num_cpus::get();
        Self {
            default_cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            bash_timeout_ms: 120_000,
            warm_shell: false,
            walk_threads: threads,
            enable_watch: false,
            trust_cache: false,
            enable_optimizer: true,
            max_cached_files: 65_536,
            min_cache_bytes: 64 * 1024 * 1024,
            max_cache_bytes: 1024 * 1024 * 1024,
            optimizer_interval_ms: 2000,
        }
    }
}

/// Live, atomically-tunable knobs the optimizer adjusts and tools read.
pub struct Tuning {
    /// Files at/above this size may use a transient mmap in `grep`.
    pub mmap_threshold: AtomicUsize,
    /// Current adaptive byte budget for the file cache (bytes). The optimizer
    /// raises it for warm, high-reuse workloads and lowers it when reuse is low.
    pub cache_byte_budget: AtomicU64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            mmap_threshold: AtomicUsize::new(256 * 1024),
            cache_byte_budget: AtomicU64::new(256 * 1024 * 1024),
        }
    }
}

struct EngineInner {
    config: EngineConfig,
    files: Arc<FileCache>,
    walks: Arc<WalkCache>,
    tuning: Arc<Tuning>,
    watch: Mutex<Option<WatchHandle>>,
    /// Type-erased per-engine extensions, so tools can stash long-lived state
    /// (e.g. a compiled-matcher cache) without `hearth-core` depending on their
    /// types.
    extensions: DashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    opt_stop: Arc<AtomicBool>,
    // Kept alive for the engine's lifetime; the thread exits when `opt_stop` is set.
    #[allow(dead_code)]
    opt_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        // Signal the optimizer to exit; it will notice within one interval.
        self.opt_stop.store(true, Ordering::Relaxed);
    }
}

/// A cheap-to-clone handle to the resident engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

impl Engine {
    /// Build an engine with the given configuration, starting the optimizer.
    pub fn new(config: EngineConfig) -> Self {
        let files = Arc::new(FileCache::new());
        let walks = Arc::new(WalkCache::new(config.walk_threads));
        let tuning = Arc::new(Tuning::default());
        // Normalize bounds so a misconfigured min>max can never panic `clamp`.
        let min_bytes = config.min_cache_bytes;
        let max_bytes = config.max_cache_bytes.max(min_bytes);
        // Start the adaptive budget at the floor; the optimizer grows it toward
        // the ceiling for warm, high-reuse workloads.
        tuning.cache_byte_budget.store(min_bytes, Ordering::Relaxed);
        let opt_stop = Arc::new(AtomicBool::new(false));

        let opt_thread = if config.enable_optimizer {
            let files = Arc::clone(&files);
            let tuning = Arc::clone(&tuning);
            let stop = Arc::clone(&opt_stop);
            let params = OptimizerParams {
                min_bytes,
                max_bytes,
                max_files: config.max_cached_files,
                interval: Duration::from_millis(config.optimizer_interval_ms.max(100)),
            };
            // Degrade gracefully if the thread can't spawn (don't panic Engine::new).
            std::thread::Builder::new()
                .name("hearth-optimizer".into())
                .spawn(move || optimizer_loop(files, tuning, stop, params))
                .ok()
        } else {
            None
        };

        Self {
            inner: Arc::new(EngineInner {
                config,
                files,
                walks,
                tuning,
                watch: Mutex::new(None),
                extensions: DashMap::new(),
                opt_stop,
                opt_thread: Mutex::new(opt_thread),
            }),
        }
    }

    /// Build an engine with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(EngineConfig::default())
    }

    #[inline]
    pub fn files(&self) -> &FileCache {
        &self.inner.files
    }

    #[inline]
    pub fn walks(&self) -> &WalkCache {
        &self.inner.walks
    }

    #[inline]
    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }

    #[inline]
    pub fn tuning(&self) -> &Tuning {
        &self.inner.tuning
    }

    /// Get (creating on first use) a per-engine extension of type `T`. Tools use
    /// this to keep long-lived state (like a compiled-matcher cache) that lives
    /// as long as the engine, without `hearth-core` knowing the concrete type.
    pub fn extension<T: Any + Send + Sync + Default>(&self) -> Arc<T> {
        let id = TypeId::of::<T>();
        if let Some(existing) = self.inner.extensions.get(&id) {
            return Arc::clone(existing.value())
                .downcast::<T>()
                .expect("extension registered under a mismatched TypeId");
        }
        let created: Arc<dyn Any + Send + Sync> = Arc::new(T::default());
        let entry = self.inner.extensions.entry(id).or_insert(created);
        Arc::clone(entry.value())
            .downcast::<T>()
            .expect("extension registered under a mismatched TypeId")
    }

    /// Begin watching `root` for changes (best effort; no-op if disabled or on
    /// error). Idempotent-ish: adds the root to the existing watcher.
    pub fn watch_root(&self, root: &Path) {
        if !self.inner.config.enable_watch {
            return;
        }
        let mut guard = self.inner.watch.lock();
        let started = match guard.as_mut() {
            Some(handle) => handle.add_root(root).is_ok(),
            None => match WatchHandle::start(
                root,
                Arc::clone(&self.inner.files),
                Arc::clone(&self.inner.walks),
            ) {
                Ok(handle) => {
                    *guard = Some(handle);
                    true
                }
                Err(_) => false,
            },
        };
        drop(guard);
        let _ = started;
    }

    /// Whether a warm hit may skip the freshness `stat` — true only in
    /// `trust_cache` mode (a single-writer / bounded-staleness opt-in). The safe
    /// default stats every hit.
    pub fn stat_free(&self, _path: &Path) -> bool {
        self.inner.config.trust_cache
    }

    /// Render the profiler report, prefixed with live cache/optimizer state.
    pub fn profiler_report(&self) -> String {
        format!("{}\n{}", self.cache_report(), crate::profiler::report())
    }

    /// A one-line snapshot of the adaptive cache/optimizer state.
    pub fn cache_report(&self) -> String {
        let (hits, misses) = self.inner.files.cache_stats();
        let total = hits + misses;
        let rate = if total > 0 { hits as f64 / total as f64 * 100.0 } else { 0.0 };
        format!(
            "── cache/optimizer ──\nfiles={} bytes={} budget={} hit_rate={:.1}% ({}/{}) walks={}",
            self.inner.files.len(),
            self.inner.files.total_bytes(),
            self.inner.tuning.cache_byte_budget.load(Ordering::Relaxed),
            rate,
            hits,
            total,
            self.inner.walks.len(),
        )
    }
}

struct OptimizerParams {
    min_bytes: u64,
    max_bytes: u64,
    max_files: usize,
    interval: Duration,
}

/// The self-optimization loop: an adaptive controller that, each tick, reads the
/// cache's windowed hit-rate and size, then retunes the cache byte budget with
/// hysteresis and enforces it via LRU eviction.
///
/// * high reuse + cache nearly full → **grow** the budget (a warm workload pays
///   back more warm memory), up to `max_bytes`;
/// * low reuse → **shrink** it (the cache is not earning its memory), down to
///   `min_bytes`.
///
/// The hit/miss signal is the cache's always-on counters, so this works whether
/// or not the timing profiler is enabled. Decisions are emitted as counters.
fn optimizer_loop(
    files: Arc<FileCache>,
    tuning: Arc<Tuning>,
    stop: Arc<AtomicBool>,
    params: OptimizerParams,
) {
    let OptimizerParams { min_bytes, max_bytes, max_files, interval } = params;
    let (mut prev_hits, mut prev_misses) = files.cache_stats();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let (hits, misses) = files.cache_stats();
        let dh = hits.saturating_sub(prev_hits);
        let dm = misses.saturating_sub(prev_misses);
        prev_hits = hits;
        prev_misses = misses;
        let window = dh + dm;
        let hit_rate = if window > 0 { dh as f64 / window as f64 } else { 1.0 };

        let cached = files.total_bytes();
        let mut budget = tuning.cache_byte_budget.load(Ordering::Relaxed);

        if window >= 8 {
            if hit_rate > 0.85 && (cached as f64) > 0.8 * (budget as f64) {
                // Saturating grow (+50%) so a tiny budget can still increase.
                budget = budget.saturating_add((budget / 2).max(1)).min(max_bytes);
            } else if hit_rate < 0.40 {
                budget = ((budget as f64 * 0.75) as u64).max(min_bytes);
            }
        }
        // `max_bytes >= min_bytes` is guaranteed by Engine::new, so this can't panic.
        budget = budget.clamp(min_bytes, max_bytes);
        tuning.cache_byte_budget.store(budget, Ordering::Relaxed);

        // Enforce the adaptive byte budget (LRU) plus a hard entry-count cap.
        let (evicted, freed) = files.evict_to_bytes(budget);
        let pruned = files.prune(max_files);

        crate::profiler::count("optimizer.byte_budget", budget);
        crate::profiler::count("optimizer.cached_bytes", cached);
        if evicted + pruned > 0 {
            crate::profiler::count("optimizer.evictions", (evicted + pruned) as u64);
            crate::profiler::count("optimizer.bytes_freed", freed);
        }
    }
}
