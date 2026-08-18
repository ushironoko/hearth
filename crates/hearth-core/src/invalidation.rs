//! A bounded journal of cache invalidations for derived per-path state.
//!
//! The engine's caches already invalidate themselves eagerly. This log lets
//! other per-engine state consume the same invalidations lazily, without
//! coupling that state to the watcher or individual mutation entry points.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const INVALIDATION_CAPACITY: usize = 4096;

/// A bounded, revisioned log of paths affected by cache invalidation.
///
/// Consumers remember the revision returned by [`InvalidationLog::since`].
/// If entries are evicted or an invalidation cannot enumerate individual
/// paths, the next stale consumer receives a full-discard signal.
pub struct InvalidationLog {
    revision: AtomicU64,
    inner: Mutex<InvalidationInner>,
}

struct InvalidationInner {
    entries: VecDeque<(u64, PathBuf)>,
    evicted_through: u64,
}

/// The invalidations recorded after a consumer's previous revision.
#[derive(Debug, Eq, PartialEq)]
pub struct InvalidationDelta {
    /// The latest log revision, to use as the consumer's next `last_seen`.
    pub revision: u64,
    /// Changed paths, or `None` when the consumer must discard all derived
    /// state because part of the invalidation history is not enumerable.
    pub paths: Option<Vec<PathBuf>>,
}

impl InvalidationLog {
    /// Creates an empty invalidation log.
    pub fn new() -> Self {
        Self {
            revision: AtomicU64::new(0),
            inner: Mutex::new(InvalidationInner {
                entries: VecDeque::with_capacity(INVALIDATION_CAPACITY),
                evicted_through: 0,
            }),
        }
    }

    /// Records one path whose cached or derived state may now be stale.
    ///
    /// Recording is amortized O(1). When the bounded ring is full, its oldest
    /// revision is marked non-enumerable so lagging consumers can fall back to
    /// discarding all derived state.
    pub fn record(&self, path: &Path) {
        let mut inner = self.inner.lock();
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        if inner.entries.len() == INVALIDATION_CAPACITY {
            let (evicted_revision, _) = inner
                .entries
                .pop_front()
                .expect("a full ring has an oldest entry");
            inner.evicted_through = evicted_revision;
        }
        inner.entries.push_back((revision, path.to_path_buf()));
    }

    /// Records an invalidation that cannot enumerate all affected paths.
    ///
    /// Prefix, root, and full-cache invalidations use this marker. It makes all
    /// older consumer revisions require a full discard and clears superseded
    /// path entries from the ring.
    pub fn record_wipe(&self) {
        let mut inner = self.inner.lock();
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        inner.entries.clear();
        inner.evicted_through = revision;
    }

    /// Returns every enumerable invalidation newer than `last_seen`.
    ///
    /// A `None` path list means some newer invalidation is no longer
    /// enumerable and the caller must discard all derived state. The returned
    /// revision is current even in that case and becomes the caller's next
    /// `last_seen`.
    pub fn since(&self, last_seen: u64) -> InvalidationDelta {
        let inner = self.inner.lock();
        let revision = self.revision.load(Ordering::Relaxed);
        let paths = if last_seen < inner.evicted_through {
            None
        } else {
            Some(
                inner
                    .entries
                    .iter()
                    .filter(|(entry_revision, _)| *entry_revision > last_seen)
                    .map(|(_, path)| path.clone())
                    .collect(),
            )
        };
        InvalidationDelta { revision, paths }
    }
}

impl Default for InvalidationLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{INVALIDATION_CAPACITY, InvalidationLog};
    use crate::{Engine, EngineConfig};
    use hearth_proto::CacheScope;
    use std::path::{Path, PathBuf};

    #[test]
    fn record_and_since_preserve_order() {
        let log = InvalidationLog::new();
        let paths = [
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("a.rs"),
        ];

        for path in &paths {
            log.record(path);
        }

        let delta = log.since(0);
        assert_eq!(delta.revision, 3);
        assert_eq!(delta.paths, Some(paths.into()));
    }

    #[test]
    fn revisions_increase_across_records_and_wipes() {
        let log = InvalidationLog::new();
        let fresh = log.since(0);
        assert_eq!(fresh.revision, 0);
        assert_eq!(fresh.paths, Some(Vec::new()));

        log.record(Path::new("a.rs"));
        let first = log.since(0).revision;
        log.record_wipe();
        let second = log.since(first).revision;
        log.record(Path::new("b.rs"));
        let third = log.since(second).revision;

        assert!(first < second);
        assert!(second < third);
    }

    #[test]
    fn overflow_distinguishes_stale_and_retained_consumers() {
        let log = InvalidationLog::new();
        let extra = 7;
        for index in 0..(INVALIDATION_CAPACITY + extra) {
            log.record(Path::new(&format!("{index}.rs")));
        }

        let stale = log.since(extra as u64 - 1);
        assert_eq!(stale.revision, (INVALIDATION_CAPACITY + extra) as u64);
        assert_eq!(stale.paths, None);

        let boundary = log.since(extra as u64);
        assert_eq!(
            boundary.paths.as_ref().map(Vec::len),
            Some(INVALIDATION_CAPACITY),
            "last_seen == evicted_through remains fully enumerable"
        );

        let first_retained_revision = extra as u64 + 1;
        let retained = log.since(first_retained_revision);
        let paths = retained
            .paths
            .expect("retained revisions remain enumerable");
        assert_eq!(paths.len(), INVALIDATION_CAPACITY - 1);
        assert_eq!(
            paths.first(),
            Some(&PathBuf::from(format!("{}.rs", extra + 1)))
        );
        assert_eq!(
            paths.last(),
            Some(&PathBuf::from(format!(
                "{}.rs",
                INVALIDATION_CAPACITY + extra - 1
            )))
        );
    }

    #[test]
    fn latest_revision_returns_an_empty_delta() {
        let log = InvalidationLog::new();
        log.record(Path::new("a.rs"));
        let revision = log.since(0).revision;

        assert_eq!(log.since(revision).paths, Some(Vec::new()));
    }

    #[test]
    fn wipe_requires_stale_consumers_to_discard_everything() {
        let log = InvalidationLog::new();
        log.record(Path::new("a.rs"));
        let before_wipe = log.since(0).revision;
        log.record_wipe();

        let stale = log.since(before_wipe);
        assert_eq!(stale.paths, None);
        assert_eq!(log.since(stale.revision).paths, Some(Vec::new()));
    }

    #[test]
    fn engine_records_path_and_wipe_entry_points() {
        let engine = Engine::new(EngineConfig {
            enable_watch: false,
            enable_optimizer: false,
            ..EngineConfig::default()
        });
        let path = Path::new("src/lib.rs");
        let root = Path::new("src");
        let resolved = engine.resolve_path(path);

        engine.invalidate_path(path);
        let invalidated = engine.invalidations().since(0);
        assert_eq!(invalidated.paths, Some(vec![resolved.clone()]));

        engine.note_mutation(path, false);
        let mutated = engine.invalidations().since(invalidated.revision);
        assert_eq!(mutated.paths, Some(vec![resolved.clone()]));

        engine.invalidate(path, false, CacheScope::All);
        let non_recursive = engine.invalidations().since(mutated.revision);
        assert_eq!(non_recursive.paths, Some(vec![resolved]));

        engine.clear_caches();
        let cleared = engine.invalidations().since(non_recursive.revision);
        assert_eq!(cleared.paths, None);

        engine.invalidate_root(root);
        let root_invalidated = engine.invalidations().since(cleared.revision);
        assert_eq!(root_invalidated.paths, None);

        engine.invalidate(path, true, CacheScope::All);
        let recursive = engine.invalidations().since(root_invalidated.revision);
        assert_eq!(recursive.paths, None);
    }
}
