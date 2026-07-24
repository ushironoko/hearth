//! The sharded, low-overhead timing profiler.
//!
//! Design (adapted from `vize_carton`):
//! * one relaxed atomic gate — disabled cost is a single load;
//! * a thread-local span stack computes self/child time without global locks;
//! * the global store is sharded by hashed `&'static str` name to avoid
//!   contention across worker threads;
//! * locks are poison-tolerant so a panicking task never bricks the profiler;
//! * `&'static str` keys mean no key allocation on the hot path.

use super::allocation;
use super::metrics::{Counter, Metrics};
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SHARDS: usize = 32;

struct Frame {
    start: Instant,
    child: Duration,
}

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// The profiler: a sharded map of timing metrics plus counters.
pub struct Profiler {
    enabled: AtomicBool,
    metrics: [RwLock<FxHashMap<&'static str, Metrics>>; SHARDS],
    counters: [RwLock<FxHashMap<&'static str, Counter>>; SHARDS],
}

impl Default for Profiler {
    fn default() -> Self {
        Self::new()
    }
}

impl Profiler {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            metrics: std::array::from_fn(|_| RwLock::new(FxHashMap::default())),
            counters: std::array::from_fn(|_| RwLock::new(FxHashMap::default())),
        }
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Enable timing + allocation tracking together.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
        allocation::reset_counters();
        allocation::set_tracking_enabled(true);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        allocation::set_tracking_enabled(false);
    }

    /// Begin a scoped span. The returned guard records the sample on drop.
    #[inline]
    pub fn span(&self, name: &'static str) -> ProfileGuard<'_> {
        if self.is_enabled() {
            let _pause = allocation::pause_tracking();
            STACK.with(|s| s.borrow_mut().push(Frame { start: Instant::now(), child: Duration::ZERO }));
        }
        ProfileGuard { profiler: self, name, active: self.is_enabled() }
    }

    /// Record a raw duration sample for `name`.
    pub fn record(&self, name: &'static str, total: Duration, child: Duration) {
        let _pause = allocation::pause_tracking();
        let shard = &self.metrics[shard_index(name)];
        let mut map = shard.write();
        map.entry(name).or_default().record(total, child);
    }

    /// Record a value into a named counter (bytes, hits, …).
    pub fn count(&self, name: &'static str, value: u64) {
        if !self.is_enabled() {
            return;
        }
        let _pause = allocation::pause_tracking();
        let shard = &self.counters[shard_index(name)];
        let mut map = shard.write();
        map.entry(name).or_default().record(value);
    }

    /// Merge every shard into one sorted snapshot (by total time desc).
    pub fn timing_snapshot(&self) -> Vec<(&'static str, Metrics)> {
        let _pause = allocation::pause_tracking();
        let mut merged: FxHashMap<&'static str, Metrics> = FxHashMap::default();
        for shard in &self.metrics {
            for (k, v) in shard.read().iter() {
                merged.entry(k).or_default().merge(v);
            }
        }
        let mut out: Vec<_> = merged.into_iter().collect();
        out.sort_by(|a, b| b.1.total.cmp(&a.1.total));
        out
    }

    /// Merge every counter shard into one sorted snapshot (by name).
    pub fn counter_snapshot(&self) -> Vec<(&'static str, Counter)> {
        let _pause = allocation::pause_tracking();
        let mut merged: FxHashMap<&'static str, Counter> = FxHashMap::default();
        for shard in &self.counters {
            for (k, v) in shard.read().iter() {
                merged.entry(k).or_default().merge(v);
            }
        }
        let mut out: Vec<_> = merged.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// Look up the total of a single counter, or 0.
    pub fn counter_total(&self, name: &'static str) -> u64 {
        let shard = &self.counters[shard_index(name)];
        shard.read().get(name).map(|c| c.total).unwrap_or(0)
    }

    pub fn clear(&self) {
        let _pause = allocation::pause_tracking();
        for shard in &self.metrics {
            shard.write().clear();
        }
        for shard in &self.counters {
            shard.write().clear();
        }
        allocation::reset_counters();
    }
}

/// RAII guard returned by [`Profiler::span`].
pub struct ProfileGuard<'a> {
    profiler: &'a Profiler,
    name: &'static str,
    active: bool,
}

impl Drop for ProfileGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _pause = allocation::pause_tracking();
        let (total, child) = STACK.with(|s| {
            let mut stack = s.borrow_mut();
            match stack.pop() {
                Some(frame) => (frame.start.elapsed(), frame.child),
                None => (Duration::ZERO, Duration::ZERO),
            }
        });
        // Attribute this span's total time to the parent's child bucket.
        STACK.with(|s| {
            if let Some(parent) = s.borrow_mut().last_mut() {
                parent.child = parent.child.saturating_add(total);
            }
        });
        self.profiler.record(self.name, total, child);
    }
}

/// FNV-1a hash of the static name, masked to the shard count (power of two).
#[inline]
fn shard_index(name: &str) -> usize {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in name.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash as usize) & (SHARDS - 1)
}
