//! The shared directory-walk cache.
//!
//! Enumerating a tree — honoring `.gitignore`, hidden rules, and symlink policy
//! — is the cold cost a one-shot `rg` pays on *every* invocation. Here the file
//! list is walked once (in parallel, ripgrep-style) and cached per
//! `(root, ignore-config)`. Later searches over the same tree skip the walk
//! entirely and just re-filter the cached list by glob in memory.

use dashmap::DashMap;
use ignore::{WalkBuilder, WalkState};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The walk-affecting knobs. Globs are *not* here: they post-filter the cached
/// list, so one walk serves every glob over the same tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WalkKey {
    pub respect_gitignore: bool,
    pub hidden: bool,
    pub follow_symlinks: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    root: PathBuf,
    opts: WalkKey,
}

/// A cached enumeration of the files under one root.
pub struct WalkEntry {
    pub root: PathBuf,
    pub files: Arc<Vec<PathBuf>>,
    last_access: AtomicU64,
}

pub struct WalkCache {
    map: DashMap<CacheKey, Arc<WalkEntry>>,
    threads: usize,
    max_entries: usize,
    max_files: usize,
    max_path_bytes: usize,
    clock: AtomicU64,
    /// Serializes insert/evict and invalidation so the count cap is restored
    /// synchronously before a mutation returns.
    mutation: Mutex<()>,
}

impl WalkCache {
    pub fn new(threads: usize) -> Self {
        Self::with_limit(threads, usize::MAX)
    }

    pub fn with_limit(threads: usize, max_entries: usize) -> Self {
        Self::with_limits(threads, max_entries, usize::MAX, usize::MAX)
    }

    pub fn with_limits(
        threads: usize,
        max_entries: usize,
        max_files: usize,
        max_path_bytes: usize,
    ) -> Self {
        Self {
            map: DashMap::new(),
            threads: threads.max(1),
            max_entries,
            max_files,
            max_path_bytes,
            clock: AtomicU64::new(1),
            mutation: Mutex::new(()),
        }
    }

    fn touch(&self, entry: &WalkEntry) {
        entry.last_access.store(
            self.clock.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// Get (or build) the file list for `root` under `opts`. `hit` reports
    /// whether the cached walk was reused.
    pub fn get(&self, root: &Path, opts: WalkKey) -> (Arc<WalkEntry>, bool) {
        let key = CacheKey {
            root: root.to_path_buf(),
            opts,
        };
        if let Some(entry) = self.map.get(&key) {
            self.touch(entry.value());
            crate::profiler::count("cache.walk.hit", 1);
            return (Arc::clone(entry.value()), true);
        }
        crate::profiler::count("cache.walk.miss", 1);
        let files = self.build(root, opts);
        let entry = Arc::new(WalkEntry {
            root: root.to_path_buf(),
            files: Arc::new(files),
            last_access: AtomicU64::new(0),
        });
        self.touch(&entry);

        let _mutation = self.mutation.lock();
        if self.max_entries == 0 {
            return (entry, false);
        }
        if let Some(existing) = self.map.get(&key) {
            self.touch(existing.value());
            return (Arc::clone(existing.value()), true);
        }
        self.map.insert(key, Arc::clone(&entry));
        self.evict_locked();
        (entry, false)
    }

    fn evict_locked(&self) {
        while self.map.len() > self.max_entries {
            let victim = self
                .map
                .iter()
                .map(|entry| {
                    (
                        entry.key().clone(),
                        Arc::clone(entry.value()),
                        entry.value().last_access.load(Ordering::Relaxed),
                    )
                })
                .min_by(|left, right| {
                    left.2
                        .cmp(&right.2)
                        .then_with(|| left.0.root.cmp(&right.0.root))
                });
            let Some((key, identity, _)) = victim else {
                break;
            };
            self.map
                .remove_if(&key, |_, current| Arc::ptr_eq(current, &identity));
        }
    }

    fn build(&self, root: &Path, opts: WalkKey) -> Vec<PathBuf> {
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(!opts.hidden)
            .git_ignore(opts.respect_gitignore)
            .git_global(opts.respect_gitignore)
            .git_exclude(opts.respect_gitignore)
            .ignore(opts.respect_gitignore)
            .parents(opts.respect_gitignore)
            .follow_links(opts.follow_symlinks)
            .threads(self.threads);

        struct Sink {
            files: Vec<PathBuf>,
            path_bytes: usize,
        }
        let sink = Mutex::new(Sink {
            files: Vec::new(),
            path_bytes: 0,
        });
        builder.build_parallel().run(|| {
            Box::new(|result| {
                if let Ok(entry) = result
                    && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                {
                    let path = entry.into_path();
                    let path_bytes = path.as_os_str().len();
                    let mut sink = sink.lock();
                    if sink.files.len() >= self.max_files
                        || path_bytes > self.max_path_bytes.saturating_sub(sink.path_bytes)
                    {
                        return WalkState::Quit;
                    }
                    sink.path_bytes += path_bytes;
                    sink.files.push(path);
                }
                WalkState::Continue
            })
        });
        // The parallel walk finishes in thread-completion order. Sorting here
        // once makes every consumer deterministic — and lets `grep` treat the
        // index order as path order when it applies a global match limit.
        let mut sink = sink.into_inner();
        sink.files.sort_unstable();
        sink.files
    }

    /// Invalidate every cached walk that overlaps `path` — both walks rooted at
    /// an ancestor of `path` (whose file list may now be wrong) and walks rooted
    /// beneath it (which `path` may have just replaced wholesale). Returns the
    /// number of entries dropped.
    pub fn invalidate_under(&self, path: &Path) -> usize {
        let _mutation = self.mutation.lock();
        let before = self.map.len();
        self.map
            .retain(|k, _| !path.starts_with(&k.root) && !k.root.starts_with(path));
        before.saturating_sub(self.map.len())
    }

    /// Drop every cached walk. Returns the number of entries removed.
    pub fn clear(&self) -> usize {
        let _mutation = self.mutation.lock();
        let n = self.map.len();
        self.map.clear();
        n
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WalkKey {
        WalkKey {
            respect_gitignore: false,
            hidden: true,
            follow_symlinks: false,
        }
    }

    #[test]
    fn entry_cap_is_synchronous_and_lru() {
        let dir = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..3)
            .map(|i| {
                let root = dir.path().join(format!("r{i}"));
                std::fs::create_dir(&root).unwrap();
                std::fs::write(root.join("file"), b"x").unwrap();
                root
            })
            .collect();
        let cache = WalkCache::with_limit(1, 2);

        cache.get(&roots[0], key());
        cache.get(&roots[1], key());
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&roots[0], key()).1, "root zero should become MRU");
        cache.get(&roots[2], key());
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&roots[0], key()).1, "MRU entry must survive");
        assert!(!cache.get(&roots[1], key()).1, "LRU entry must be evicted");
    }

    #[test]
    fn zero_limit_returns_walk_without_retention() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        let cache = WalkCache::with_limit(1, 0);
        let (entry, hit) = cache.get(dir.path(), key());
        assert!(!hit);
        assert_eq!(entry.files.len(), 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn file_and_path_byte_caps_apply_during_walk() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["aaaa", "bbbb", "cccc"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let one_path = dir.path().join("aaaa").as_os_str().len();
        let cache = WalkCache::with_limits(1, 1, 2, one_path * 2);
        let (entry, _) = cache.get(dir.path(), key());
        assert!(entry.files.len() <= 2);
        assert!(
            entry
                .files
                .iter()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
                <= one_path * 2
        );
    }
}
