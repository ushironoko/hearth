//! Best-effort filesystem watching that proactively invalidates the caches.
//!
//! Correctness never depends on this in the default configuration: [`FileCache`]
//! still stat-validates every hit. But when the engine is put in `trust_watch`
//! mode, warm reads/greps skip even that stat and rely on the watcher to have
//! invalidated stale entries — so the watcher tracks a `healthy` flag and any
//! backend error flips the engine back to strict stat validation.

use crate::cache::{FileCache, WalkCache};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Owns the OS watcher; dropping it stops the background thread.
pub struct WatchHandle {
    watcher: RecommendedWatcher,
    healthy: Arc<AtomicBool>,
}

impl WatchHandle {
    /// Start watching `root` recursively, routing events to the caches.
    pub fn start(
        root: &Path,
        files: Arc<FileCache>,
        walks: Arc<WalkCache>,
    ) -> Result<Self, notify::Error> {
        let healthy = Arc::new(AtomicBool::new(true));
        let cb_healthy = Arc::clone(&healthy);
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let event = match res {
                Ok(e) => e,
                Err(_) => {
                    // A backend error means we may miss invalidations — no longer
                    // safe to skip stats. Fall back to strict mode permanently.
                    cb_healthy.store(false, Ordering::Relaxed);
                    return;
                }
            };
            use notify::event::ModifyKind;
            // Only invalidate cached content on real content/structural changes.
            // Metadata-only events (e.g. atime bumps caused by our own reads)
            // must NOT invalidate, or a read→atime→invalidate→re-read feedback
            // loop makes the warm cache defeat itself.
            let content_change = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Modify(ModifyKind::Any)
            );
            let structural = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
            );
            for path in &event.paths {
                if content_change {
                    files.invalidate(path);
                }
                if structural {
                    walks.invalidate_under(path);
                }
            }
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Self { watcher, healthy })
    }

    /// Add another root to the same watcher.
    pub fn add_root(&mut self, root: &Path) -> Result<(), notify::Error> {
        self.watcher.watch(root, RecursiveMode::Recursive)
    }

    /// Whether the watcher has seen no backend errors (safe to skip stats).
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}
