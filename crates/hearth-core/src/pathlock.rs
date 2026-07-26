//! Per-path serialization for file mutations.
//!
//! Two concurrent `edit`s of the same file each read the original, compute a
//! new whole-file image, and write it — so without serialization the second
//! write silently discards the first. This registry gives every mutation an
//! exclusive lock on its target for the whole read-modify-write, and holds it
//! until the bytes are committed *and* the cache is refreshed.
//!
//! Keys are **canonical**, so `dir/file`, `./dir/file` and `link-to-dir/file`
//! all serialize against each other. A path that does not exist yet is keyed by
//! its canonical parent plus its file name, so the create-then-edit race is
//! covered too.
//!
//! Entries are dropped once nobody holds them, so a long-lived engine that
//! edits many files does not accumulate one mutex per file ever touched.

use parking_lot::lock_api::ArcMutexGuard;
use parking_lot::{Mutex, RawMutex};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The canonical key a mutation of `path` serializes on.
///
/// Falls back gracefully: a missing file keys on `canonical(parent)/name`, and
/// a missing parent keys on the path itself. Two callers only ever disagree
/// when the filesystem changed shape between their calls, in which case they
/// were not racing on the same inode anyway.
pub fn mutation_key(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            match std::fs::canonicalize(parent) {
                Ok(real_parent) => real_parent.join(name),
                Err(_) => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

/// A registry of per-path mutexes, held as a per-engine extension.
#[derive(Default)]
pub struct PathLocks {
    map: Mutex<FxHashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl PathLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the exclusive mutation lock for `key`, blocking until it is free.
    ///
    /// `key` should come from [`mutation_key`]. The returned guard releases on
    /// drop — including on unwind, so a panicking tool cannot wedge the path.
    pub fn lock(self: &Arc<Self>, key: PathBuf) -> PathGuard {
        // Clone the entry's Arc while holding the registry lock, so a concurrent
        // `release` can never observe a count of 1 and remove an entry that this
        // thread is about to lock.
        let entry = {
            let mut map = self.map.lock();
            Arc::clone(map.entry(key.clone()).or_default())
        };
        let guard = entry.lock_arc();
        PathGuard { locks: Arc::clone(self), key, guard: Some(guard) }
    }

    /// Number of live entries. Test/observability only.
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop `key`'s entry if this was the last holder.
    ///
    /// Safe because the count is read while holding the registry lock and the
    /// caller has already dropped both its guard and its `Arc`: a count of 1
    /// therefore means the map itself is the only owner, and no other thread
    /// can be between "clone the entry" and "lock it" — that window is also
    /// under the registry lock.
    fn release(&self, key: &Path) {
        let mut map = self.map.lock();
        if let Some(entry) = map.get(key)
            && Arc::strong_count(entry) == 1 {
            map.remove(key);
        }
    }
}

/// An owned exclusive lock on one path's mutations.
pub struct PathGuard {
    locks: Arc<PathLocks>,
    key: PathBuf,
    // `Option` so `Drop` can release the mutex *before* the registry cleanup.
    guard: Option<ArcMutexGuard<RawMutex, ()>>,
}

impl PathGuard {
    /// The canonical path this guard serializes.
    pub fn key(&self) -> &Path {
        &self.key
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // Release the mutex and our Arc clone first, then let the registry
        // garbage-collect the entry if we were the last holder.
        drop(self.guard.take());
        self.locks.release(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn same_path_serializes_and_entries_are_reclaimed() {
        let locks = Arc::new(PathLocks::new());
        let key = PathBuf::from("/tmp/hearth-pathlock-test");
        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let locks = Arc::clone(&locks);
            let key = key.clone();
            let inside = Arc::clone(&inside);
            let max_seen = Arc::clone(&max_seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let _g = locks.lock(key.clone());
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(n, Ordering::SeqCst);
                    std::hint::spin_loop();
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "mutations must be serialized");
        assert!(locks.is_empty(), "entries must be reclaimed once unused");
    }

    #[test]
    fn different_paths_do_not_block_each_other() {
        let locks = Arc::new(PathLocks::new());
        let a = locks.lock(PathBuf::from("/tmp/a"));
        // Would deadlock if the registry used one global lock.
        let b = locks.lock(PathBuf::from("/tmp/b"));
        assert_eq!(locks.len(), 2);
        drop(a);
        drop(b);
        assert!(locks.is_empty());
    }

    #[test]
    fn mutation_key_unifies_symlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let file = real.join("f.txt");
        std::fs::write(&file, b"x").unwrap();

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(mutation_key(&file), mutation_key(&link.join("f.txt")));

        // A file that does not exist yet still keys through its canonical parent.
        let missing_direct = real.join("new.txt");
        let missing_alias = link.join("new.txt");
        assert_eq!(mutation_key(&missing_direct), mutation_key(&missing_alias));
    }
}
