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

/// A cached enumeration of the entries under one root.
pub struct WalkEntry {
    pub root: PathBuf,
    /// Target-regular files. This retains the pre-find classification used by
    /// grep and graph, including followed symlinks to regular files.
    pub files: Arc<Vec<PathBuf>>,
    /// Target-directories, excluding the root itself. A followed symlink to a
    /// directory appears here so path-oriented consumers can render `/`.
    pub directories: Arc<Vec<PathBuf>>,
    /// Symlinks whose target type was not followed/classified by the walker,
    /// including dangling links and all links when following is disabled.
    pub symlinks: Arc<Vec<PathBuf>>,
    /// False when the preflight or walker exhausted a hard work budget.
    pub complete: bool,
    retained_path_bytes: usize,
    last_access: AtomicU64,
}

pub struct WalkCache {
    map: DashMap<CacheKey, Arc<WalkEntry>>,
    threads: usize,
    max_entries: usize,
    max_files: usize,
    max_path_bytes: usize,
    max_resident_path_bytes: usize,
    max_visited_entries: usize,
    clock: AtomicU64,
    /// Serializes insert/evict and invalidation so the count cap is restored
    /// synchronously before a mutation returns.
    mutation: Mutex<()>,
    build: Mutex<()>,
    generation: AtomicU64,
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
            max_resident_path_bytes: max_path_bytes,
            max_visited_entries: max_files,
            clock: AtomicU64::new(1),
            mutation: Mutex::new(()),
            build: Mutex::new(()),
            generation: AtomicU64::new(0),
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
        let _build = self.build.lock();
        if let Some(entry) = self.map.get(&key) {
            self.touch(entry.value());
            return (Arc::clone(entry.value()), true);
        }
        let generation = self.generation.load(Ordering::Acquire);
        let (files, directories, symlinks, complete) = self.build(root, opts);
        let retained_path_bytes = files
            .iter()
            .chain(&directories)
            .chain(&symlinks)
            .map(|path| path.as_os_str().len())
            .sum();
        let entry = Arc::new(WalkEntry {
            root: root.to_path_buf(),
            files: Arc::new(files),
            directories: Arc::new(directories),
            symlinks: Arc::new(symlinks),
            complete,
            retained_path_bytes,
            last_access: AtomicU64::new(0),
        });
        self.touch(&entry);

        let _mutation = self.mutation.lock();
        if self.generation.load(Ordering::Acquire) != generation {
            return (entry, false);
        }
        if self.max_entries == 0 || !entry.complete {
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
        while self.map.len() > self.max_entries
            || self
                .map
                .iter()
                .map(|entry| entry.value().retained_path_bytes)
                .sum::<usize>()
                > self.max_resident_path_bytes
        {
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

    fn configured_builder(&self, root: &Path, opts: WalkKey, follow_links: bool) -> WalkBuilder {
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(!opts.hidden)
            // `ignore` resolves .gitignore through ancestor repositories even
            // with parents(false), which cannot be bounded to this root.
            .git_ignore(false)
            // Parent/global/exclude rule discovery reaches outside the bounded
            // root and cannot be pre-accounted safely. Root-local ignore files
            // remain supported and are covered by the preflight byte budget.
            .git_global(false)
            .git_exclude(false)
            .ignore(opts.respect_gitignore)
            .parents(false)
            .follow_links(follow_links)
            .threads(self.threads);
        if opts.respect_gitignore {
            builder.add_custom_ignore_filename(".rgignore");
        }
        builder
    }

    fn build(
        &self,
        root: &Path,
        opts: WalkKey,
    ) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>, bool) {
        if !self.preflight(root, opts) {
            return (Vec::new(), Vec::new(), Vec::new(), false);
        }
        // A second preflight immediately before the real walk narrows mutation
        // races. Correctness under an actively hostile concurrent filesystem is
        // not claimed; the per-UID daemon is not a sandbox boundary.
        if !self.preflight(root, opts) {
            return (Vec::new(), Vec::new(), Vec::new(), false);
        }
        let mut builder = self.configured_builder(root, opts, opts.follow_symlinks);

        struct Sink {
            files: Vec<PathBuf>,
            directories: Vec<PathBuf>,
            symlinks: Vec<PathBuf>,
            dangling_errors: Vec<PathBuf>,
            path_bytes: usize,
            exhausted: bool,
            io_failed: bool,
        }
        let visited = Arc::new(AtomicU64::new(0));
        let exhausted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let max_visited = self.max_visited_entries as u64;
        let filter_visited = Arc::clone(&visited);
        let filter_exhausted = Arc::clone(&exhausted);
        builder.filter_entry(move |_| {
            let admitted = filter_visited.fetch_add(1, Ordering::Relaxed) < max_visited;
            if !admitted {
                filter_exhausted.store(true, Ordering::Relaxed);
            }
            admitted
        });
        let sink = Mutex::new(Sink {
            files: Vec::new(),
            directories: Vec::new(),
            symlinks: Vec::new(),
            dangling_errors: Vec::new(),
            path_bytes: 0,
            exhausted: false,
            io_failed: false,
        });
        builder.build_parallel().run(|| {
            Box::new(|result| {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        // With link following enabled, walkdir reports a
                        // dangling link as NotFound before the ignore layer can
                        // classify it. Defer admission until a non-following
                        // walk with the same hidden/ignore policy can verify it.
                        if let Some(path) = dangling_symlink_path(&error)
                            && std::fs::symlink_metadata(path)
                                .is_ok_and(|meta| meta.file_type().is_symlink())
                        {
                            sink.lock().dangling_errors.push(path.to_path_buf());
                            return WalkState::Continue;
                        }
                        sink.lock().io_failed = true;
                        return WalkState::Continue;
                    }
                };
                // The walker yields the root at depth zero; it is traversal
                // context, never a find result.
                if entry.depth() == 0 {
                    return WalkState::Continue;
                }
                let file_type = entry.file_type();
                let is_file = file_type.is_some_and(|kind| kind.is_file());
                let is_dir = file_type.is_some_and(|kind| kind.is_dir());
                let is_symlink =
                    entry.path_is_symlink() || file_type.is_some_and(|kind| kind.is_symlink());
                let path = entry.into_path();
                let path_bytes = path.as_os_str().len();
                let mut sink = sink.lock();
                if (is_file && sink.files.len() >= self.max_files)
                    || path_bytes > self.max_path_bytes.saturating_sub(sink.path_bytes)
                {
                    sink.exhausted = true;
                    return WalkState::Quit;
                }
                if is_file {
                    // Do not move followed symlink-to-file entries out of this
                    // slice: grep and graph have always consumed them here.
                    sink.files.push(path);
                } else if is_dir {
                    sink.directories.push(path);
                } else if is_symlink {
                    sink.symlinks.push(path);
                } else {
                    // Sockets/devices are intentionally not search results and
                    // retain the existing file-walk behavior.
                    return WalkState::Continue;
                }
                sink.path_bytes += path_bytes;
                WalkState::Continue
            })
        });
        // The parallel walk finishes in thread-completion order. Sorting here
        // once makes every consumer deterministic — and lets `grep` treat the
        // index order as path order when it applies a global match limit.
        let mut sink = sink.into_inner();
        let mut dangling_scan_complete = true;
        if !sink.dangling_errors.is_empty() {
            sink.dangling_errors.sort_unstable();
            sink.dangling_errors.dedup();
            let (admitted, complete) =
                self.admitted_dangling_symlinks(root, opts, &sink.dangling_errors);
            dangling_scan_complete = complete;
            for path in admitted {
                let path_bytes = path.as_os_str().len();
                if path_bytes > self.max_path_bytes.saturating_sub(sink.path_bytes) {
                    sink.exhausted = true;
                    break;
                }
                sink.path_bytes += path_bytes;
                sink.symlinks.push(path);
            }
        }
        sink.files.sort_unstable();
        sink.directories.sort_unstable();
        sink.symlinks.sort_unstable();
        let complete = !sink.exhausted
            && !sink.io_failed
            && dangling_scan_complete
            && !exhausted.load(Ordering::Relaxed);
        (sink.files, sink.directories, sink.symlinks, complete)
    }

    /// Re-run only the rare dangling-link admission decision without following
    /// links. This is necessary because walkdir reports a followed dangling
    /// target as an error before `ignore` can apply hidden/custom rules.
    fn admitted_dangling_symlinks(
        &self,
        root: &Path,
        opts: WalkKey,
        candidates: &[PathBuf],
    ) -> (Vec<PathBuf>, bool) {
        let mut builder = self.configured_builder(root, opts, false);
        builder.threads(1);
        let mut admitted = Vec::new();
        let mut visited = 0usize;
        let mut path_bytes = 0usize;
        for result in builder.build() {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => return (admitted, false),
            };
            visited = visited.saturating_add(1);
            path_bytes = path_bytes.saturating_add(entry.path().as_os_str().len());
            if visited > self.max_visited_entries || path_bytes > self.max_path_bytes {
                return (admitted, false);
            }
            if entry.depth() > 0
                && entry.path_is_symlink()
                && candidates
                    .binary_search_by(|path| path.as_path().cmp(entry.path()))
                    .is_ok()
            {
                admitted.push(entry.into_path());
                if admitted.len() == candidates.len() {
                    break;
                }
            }
        }
        (admitted, true)
    }

    fn preflight(&self, root: &Path, opts: WalkKey) -> bool {
        const MAX_IGNORE_BYTES: u64 = 16 * 1024 * 1024;
        let mut stack = vec![root.to_path_buf()];
        let mut visited = 0usize;
        let mut path_bytes = root.as_os_str().len();
        let mut ignore_bytes = 0u64;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return false;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return false;
                };
                visited = visited.saturating_add(1);
                let path = entry.path();
                path_bytes = path_bytes.saturating_add(path.as_os_str().len());
                if visited > self.max_visited_entries || path_bytes > self.max_path_bytes {
                    return false;
                }
                let name = entry.file_name();
                if matches!(name.to_str(), Some(".ignore" | ".rgignore")) {
                    let metadata = std::fs::symlink_metadata(&path).ok();
                    if metadata
                        .as_ref()
                        .is_none_or(|meta| !meta.file_type().is_file())
                    {
                        return false;
                    }
                    ignore_bytes = ignore_bytes
                        .saturating_add(metadata.map_or(MAX_IGNORE_BYTES + 1, |meta| meta.len()));
                    if ignore_bytes > MAX_IGNORE_BYTES {
                        return false;
                    }
                }
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir())
                    || (opts.follow_symlinks && path.metadata().is_ok_and(|meta| meta.is_dir()));
                if is_dir {
                    stack.push(path);
                }
            }
        }
        true
    }

    /// Invalidate every cached walk that overlaps `path` — both walks rooted at
    /// an ancestor of `path` (whose file list may now be wrong) and walks rooted
    /// beneath it (which `path` may have just replaced wholesale). Returns the
    /// number of entries dropped.
    pub fn invalidate_under(&self, path: &Path) -> usize {
        let _mutation = self.mutation.lock();
        self.generation.fetch_add(1, Ordering::AcqRel);
        let before = self.map.len();
        self.map
            .retain(|k, _| !path.starts_with(&k.root) && !k.root.starts_with(path));
        before.saturating_sub(self.map.len())
    }

    /// Drop every cached walk. Returns the number of entries removed.
    pub fn clear(&self) -> usize {
        let _mutation = self.mutation.lock();
        self.generation.fetch_add(1, Ordering::AcqRel);
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

fn dangling_symlink_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, err }
            if err
                .io_error()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Some(path)
        }
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            dangling_symlink_path(err)
        }
        ignore::Error::Partial(errors) if errors.len() == 1 => dangling_symlink_path(&errors[0]),
        _ => None,
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
                .chain(entry.directories.iter())
                .chain(entry.symlinks.iter())
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
                <= one_path * 2
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_directories_and_unfollowed_symlinks_without_changing_files() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        symlink("file", dir.path().join("link-file")).unwrap();
        symlink("empty", dir.path().join("link-dir")).unwrap();
        symlink("missing", dir.path().join("dangling")).unwrap();
        let cache = WalkCache::new(1);

        let (entry, _) = cache.get(dir.path(), key());

        assert_eq!(entry.files, Arc::new(vec![dir.path().join("file")]));
        assert_eq!(entry.directories, Arc::new(vec![dir.path().join("empty")]));
        assert_eq!(
            entry.symlinks,
            Arc::new(vec![
                dir.path().join("dangling"),
                dir.path().join("link-dir"),
                dir.path().join("link-file"),
            ])
        );
    }
}
