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
}

pub struct WalkCache {
    map: DashMap<CacheKey, Arc<WalkEntry>>,
    threads: usize,
}

impl WalkCache {
    pub fn new(threads: usize) -> Self {
        Self { map: DashMap::new(), threads: threads.max(1) }
    }

    /// Get (or build) the file list for `root` under `opts`. `hit` reports
    /// whether the cached walk was reused.
    pub fn get(&self, root: &Path, opts: WalkKey) -> (Arc<WalkEntry>, bool) {
        let key = CacheKey { root: root.to_path_buf(), opts };
        if let Some(entry) = self.map.get(&key) {
            crate::profiler::count("cache.walk.hit", 1);
            return (Arc::clone(entry.value()), true);
        }
        crate::profiler::count("cache.walk.miss", 1);
        let files = self.build(root, opts);
        let entry = Arc::new(WalkEntry { root: root.to_path_buf(), files: Arc::new(files) });
        self.map.insert(key, Arc::clone(&entry));
        (entry, false)
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

        let sink = Mutex::new(Vec::new());
        builder.build_parallel().run(|| {
            Box::new(|result| {
                if let Ok(entry) = result {
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        sink.lock().push(entry.into_path());
                    }
                }
                WalkState::Continue
            })
        });
        sink.into_inner()
    }

    /// Invalidate every cached walk whose root contains (or equals) `path`.
    /// Called when the fs-watcher sees a structural change (create/remove/rename).
    pub fn invalidate_under(&self, path: &Path) {
        self.map.retain(|k, _| !path.starts_with(&k.root) && !k.root.starts_with(path));
    }

    pub fn clear(&self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}
