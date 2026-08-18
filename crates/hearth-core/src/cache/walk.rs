//! The shared directory-walk cache.
//!
//! Enumerating a tree — honoring `.gitignore`, hidden rules, and symlink policy
//! — is the cold cost a one-shot `rg` pays on *every* invocation. Here the file
//! list is walked once (in parallel, ripgrep-style) and cached per
//! `(root, ignore-config)`. Later searches over the same tree skip the walk
//! entirely and just re-filter the cached list by glob in memory.

use crate::CancelToken;
use dashmap::DashMap;
use hearth_proto::ToolResult;
use ignore::{WalkBuilder, WalkState};
use parking_lot::Mutex;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// Why a bounded walk could not produce a cacheable complete snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkFailure {
    /// One of the entry-count, file-count, path-byte, or ignore-byte caps fired.
    BudgetExceeded,
    /// Traversal or ignore-file processing failed. `kind` preserves stable
    /// permission/not-found/I/O classification for tool adapters.
    Io {
        kind: std::io::ErrorKind,
        path: PathBuf,
        message: String,
    },
}

impl WalkFailure {
    fn io(path: &Path, error: &std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }
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
    /// Present only when traversal failed. Failed snapshots are never cached.
    pub failure: Option<WalkFailure>,
    /// Backward-compatible completeness bit. New callers should inspect
    /// [`Self::failure`] for a typed cause when this is false.
    #[deprecated(note = "inspect WalkEntry::failure for the typed cause")]
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
        self.get_cancellable(root, opts, &CancelToken::none())
            .expect("a non-cancellable walk cannot be cancelled")
    }

    /// As [`Self::get`], but cancellation can settle while waiting for the
    /// global cold-build lock and is re-checked before starting a walk.
    pub fn get_cancellable(
        &self,
        root: &Path,
        opts: WalkKey,
        cancel: &CancelToken,
    ) -> ToolResult<(Arc<WalkEntry>, bool)> {
        cancel.check()?;
        let key = CacheKey {
            root: root.to_path_buf(),
            opts,
        };
        if let Some(entry) = self.map.get(&key) {
            self.touch(entry.value());
            crate::profiler::count("cache.walk.hit", 1);
            return Ok((Arc::clone(entry.value()), true));
        }
        crate::profiler::count("cache.walk.miss", 1);
        let _build = if cancel.is_live() {
            loop {
                cancel.check()?;
                if let Some(guard) = self.build.try_lock_for(Duration::from_millis(10)) {
                    break guard;
                }
            }
        } else {
            self.build.lock()
        };
        cancel.check()?;
        if let Some(entry) = self.map.get(&key) {
            self.touch(entry.value());
            return Ok((Arc::clone(entry.value()), true));
        }
        let generation = self.generation.load(Ordering::Acquire);
        let (files, directories, symlinks, failure) = self.build(root, opts);
        let retained_path_bytes = files
            .iter()
            .chain(&directories)
            .chain(&symlinks)
            .map(|path| path.as_os_str().len())
            .sum();
        let complete = failure.is_none();
        #[allow(deprecated)]
        let entry = Arc::new(WalkEntry {
            root: root.to_path_buf(),
            files: Arc::new(files),
            directories: Arc::new(directories),
            symlinks: Arc::new(symlinks),
            failure,
            complete,
            retained_path_bytes,
            last_access: AtomicU64::new(0),
        });
        self.touch(&entry);

        let _mutation = self.mutation.lock();
        if self.generation.load(Ordering::Acquire) != generation {
            return Ok((entry, false));
        }
        if self.max_entries == 0 || entry.failure.is_some() {
            return Ok((entry, false));
        }
        if let Some(existing) = self.map.get(&key) {
            self.touch(existing.value());
            return Ok((Arc::clone(existing.value()), true));
        }
        self.map.insert(key, Arc::clone(&entry));
        self.evict_locked();
        Ok((entry, false))
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
    ) -> (
        Vec<PathBuf>,
        Vec<PathBuf>,
        Vec<PathBuf>,
        Option<WalkFailure>,
    ) {
        if let Err(failure) = self.preflight(root, opts) {
            return (Vec::new(), Vec::new(), Vec::new(), Some(failure));
        }
        // A second preflight immediately before the real walk narrows mutation
        // races. Correctness under an actively hostile concurrent filesystem is
        // not claimed; the per-UID daemon is not a sandbox boundary.
        if let Err(failure) = self.preflight(root, opts) {
            return (Vec::new(), Vec::new(), Vec::new(), Some(failure));
        }
        let mut builder = self.configured_builder(root, opts, opts.follow_symlinks);

        struct Sink {
            files: Vec<PathBuf>,
            directories: Vec<PathBuf>,
            symlinks: Vec<PathBuf>,
            dangling_errors: Vec<PathBuf>,
            failures: Vec<WalkFailure>,
            path_bytes: usize,
            exhausted: bool,
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
            failures: Vec::new(),
            path_bytes: 0,
            exhausted: false,
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
                        sink.lock().failures.extend(ignore_failures(&error, root));
                        return WalkState::Continue;
                    }
                };
                if let Some(error) = entry.error() {
                    sink.lock()
                        .failures
                        .extend(ignore_failures(error, entry.path()));
                }
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
        if !sink.dangling_errors.is_empty() {
            sink.dangling_errors.sort_unstable();
            sink.dangling_errors.dedup();
            match self.admitted_dangling_symlinks(root, opts, &sink.dangling_errors) {
                Ok(admitted) => {
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
                Err(failure) => sink.failures.push(failure),
            }
        }
        sink.files.sort_unstable();
        sink.directories.sort_unstable();
        sink.symlinks.sort_unstable();
        let Sink {
            files,
            directories,
            symlinks,
            failures,
            exhausted: sink_exhausted,
            ..
        } = sink;
        let failure = failures.into_iter().min_by(walk_failure_order).or_else(|| {
            (sink_exhausted || exhausted.load(Ordering::Relaxed))
                .then_some(WalkFailure::BudgetExceeded)
        });
        (files, directories, symlinks, failure)
    }

    /// Apply the configured hidden/ignore policy directly to dangling-link
    /// candidates. A traversal with following disabled cannot reach candidates
    /// below a followed directory alias; incremental matchers can classify the
    /// root-relative spelling without traversing it.
    fn admitted_dangling_symlinks(
        &self,
        root: &Path,
        opts: WalkKey,
        candidates: &[PathBuf],
    ) -> Result<Vec<PathBuf>, WalkFailure> {
        let builder = self.configured_builder(root, opts, false);
        let mut matchers = builder.build_matchers();
        let Some(mut matcher) = matchers.pop() else {
            return Err(WalkFailure::Io {
                kind: std::io::ErrorKind::InvalidData,
                path: root.to_path_buf(),
                message: "walk builder did not produce a root ignore matcher".to_string(),
            });
        };
        let mut admitted = Vec::new();
        let mut path_bytes = 0usize;
        for (visited, candidate) in candidates.iter().enumerate() {
            path_bytes = path_bytes.saturating_add(candidate.as_os_str().len());
            if visited >= self.max_visited_entries || path_bytes > self.max_path_bytes {
                return Err(WalkFailure::BudgetExceeded);
            }
            let relative = candidate.strip_prefix(root).map_err(|_| WalkFailure::Io {
                kind: std::io::ErrorKind::InvalidData,
                path: candidate.clone(),
                message: "dangling candidate is outside its walk root".to_string(),
            })?;
            let (matched, error) = matcher.matched_with_errors(relative, false);
            if let Some(error) = error {
                return Err(primary_ignore_failure(&error, candidate));
            }
            if !matched.is_ignore() {
                admitted.push(candidate.clone());
            }
        }
        Ok(admitted)
    }

    fn preflight(&self, root: &Path, opts: WalkKey) -> Result<(), WalkFailure> {
        const MAX_IGNORE_BYTES: u64 = 16 * 1024 * 1024;
        let mut stack = vec![root.to_path_buf()];
        let mut visited = 0usize;
        let mut path_bytes = root.as_os_str().len();
        let mut ignore_bytes = 0u64;
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|error| WalkFailure::io(&dir, &error))?;
            for entry in entries {
                let entry = entry.map_err(|error| WalkFailure::io(&dir, &error))?;
                visited = visited.saturating_add(1);
                let path = entry.path();
                path_bytes = path_bytes.saturating_add(path.as_os_str().len());
                if visited > self.max_visited_entries || path_bytes > self.max_path_bytes {
                    return Err(WalkFailure::BudgetExceeded);
                }
                let file_type = entry
                    .file_type()
                    .map_err(|error| WalkFailure::io(&path, &error))?;
                let name = entry.file_name();
                if opts.respect_gitignore && matches!(name.to_str(), Some(".ignore" | ".rgignore"))
                {
                    let metadata = std::fs::symlink_metadata(&path)
                        .map_err(|error| WalkFailure::io(&path, &error))?;
                    if !metadata.file_type().is_file() {
                        return Err(WalkFailure::Io {
                            kind: std::io::ErrorKind::InvalidData,
                            path,
                            message: "ignore path must be a regular file".to_string(),
                        });
                    }
                    if metadata.len() > MAX_IGNORE_BYTES.saturating_sub(ignore_bytes) {
                        return Err(WalkFailure::BudgetExceeded);
                    }
                    let mut file = std::fs::File::open(&path)
                        .map_err(|error| WalkFailure::io(&path, &error))?;
                    let mut buffer = [0u8; 8192];
                    loop {
                        let read = file
                            .read(&mut buffer)
                            .map_err(|error| WalkFailure::io(&path, &error))?;
                        if read == 0 {
                            break;
                        }
                        ignore_bytes = ignore_bytes.saturating_add(read as u64);
                        if ignore_bytes > MAX_IGNORE_BYTES {
                            return Err(WalkFailure::BudgetExceeded);
                        }
                    }
                }
                let followed_dir = if opts.follow_symlinks && file_type.is_symlink() {
                    match path.metadata() {
                        Ok(metadata) => metadata.is_dir(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => return Err(WalkFailure::io(&path, &error)),
                    }
                } else {
                    false
                };
                if file_type.is_dir() || followed_dir {
                    stack.push(path);
                }
            }
        }
        Ok(())
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

fn walk_failure_order(left: &WalkFailure, right: &WalkFailure) -> std::cmp::Ordering {
    match (left, right) {
        (
            WalkFailure::Io {
                path: left_path,
                message: left_message,
                ..
            },
            WalkFailure::Io {
                path: right_path,
                message: right_message,
                ..
            },
        ) => left_path
            .cmp(right_path)
            .then_with(|| left_message.cmp(right_message)),
        (WalkFailure::Io { .. }, WalkFailure::BudgetExceeded) => std::cmp::Ordering::Less,
        (WalkFailure::BudgetExceeded, WalkFailure::Io { .. }) => std::cmp::Ordering::Greater,
        (WalkFailure::BudgetExceeded, WalkFailure::BudgetExceeded) => std::cmp::Ordering::Equal,
    }
}

fn primary_ignore_failure(error: &ignore::Error, fallback: &Path) -> WalkFailure {
    ignore_failures(error, fallback)
        .into_iter()
        .min_by(walk_failure_order)
        .expect("every ignore error has at least one leaf")
}

fn ignore_failures(error: &ignore::Error, fallback: &Path) -> Vec<WalkFailure> {
    fn collect(error: &ignore::Error, path: &Path, context: &str, failures: &mut Vec<WalkFailure>) {
        match error {
            ignore::Error::Partial(errors) => {
                for error in errors {
                    collect(error, path, &error.to_string(), failures);
                }
            }
            ignore::Error::WithPath { path, err } => collect(err, path, context, failures),
            ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
                collect(err, path, context, failures);
            }
            ignore::Error::Loop { child, .. } => failures.push(WalkFailure::Io {
                kind: std::io::ErrorKind::Other,
                path: child.clone(),
                message: context.to_string(),
            }),
            ignore::Error::Io(error) => failures.push(WalkFailure::Io {
                kind: error.kind(),
                path: path.to_path_buf(),
                message: context.to_string(),
            }),
            ignore::Error::Glob { .. }
            | ignore::Error::UnrecognizedFileType(_)
            | ignore::Error::InvalidDefinition => failures.push(WalkFailure::Io {
                kind: std::io::ErrorKind::InvalidData,
                path: path.to_path_buf(),
                message: context.to_string(),
            }),
        }
    }

    let mut failures = Vec::new();
    collect(error, fallback, &error.to_string(), &mut failures);
    failures
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
        assert_eq!(entry.failure, Some(WalkFailure::BudgetExceeded));
        #[allow(deprecated)]
        {
            assert!(!entry.complete);
        }
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

    #[test]
    fn malformed_custom_ignore_is_typed_and_never_cached() {
        let dir = tempfile::tempdir().unwrap();
        let ignore = dir.path().join(".rgignore");
        std::fs::write(&ignore, "[z-a]\n").unwrap();
        std::fs::write(dir.path().join("file.rs"), b"x").unwrap();
        let cache = WalkCache::new(1);
        let opts = WalkKey {
            respect_gitignore: true,
            ..key()
        };

        let (failed, hit) = cache.get(dir.path(), opts);
        assert!(!hit);
        assert!(
            matches!(
                failed.failure.as_ref(),
                Some(WalkFailure::Io {
                    kind: std::io::ErrorKind::InvalidData,
                    path,
                    ..
                }) if path == &ignore
            ),
            "unexpected failure: {:?}",
            failed.failure
        );
        assert!(cache.is_empty(), "an errored snapshot must not be retained");

        std::fs::write(&ignore, "*.tmp\n").unwrap();
        let (recovered, hit) = cache.get(dir.path(), opts);
        assert!(!hit, "recovery must rebuild rather than reuse a failure");
        assert_eq!(recovered.failure, None);
        assert!(cache.get(dir.path(), opts).1);
    }

    #[test]
    fn disabled_ignore_policy_does_not_validate_ignore_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".ignore")).unwrap();
        std::fs::write(dir.path().join("file.rs"), b"x").unwrap();
        let cache = WalkCache::new(1);

        let (disabled, _) = cache.get(dir.path(), key());
        assert_eq!(disabled.failure, None);
        assert!(disabled.directories.contains(&dir.path().join(".ignore")));

        let enabled_key = WalkKey {
            respect_gitignore: true,
            ..key()
        };
        let (enabled, _) = cache.get(dir.path(), enabled_key);
        assert!(matches!(
            enabled.failure.as_ref(),
            Some(WalkFailure::Io {
                kind: std::io::ErrorKind::InvalidData,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_enabled_ignore_is_a_permission_failure_when_enforced_by_the_os() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let ignore = dir.path().join(".ignore");
        std::fs::write(&ignore, "*.tmp\n").unwrap();
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o000)).unwrap();
        let os_enforces_mode = std::fs::File::open(&ignore).is_err();
        let cache = WalkCache::new(1);
        let opts = WalkKey {
            respect_gitignore: true,
            ..key()
        };
        let (entry, _) = cache.get(dir.path(), opts);
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o600)).unwrap();

        if os_enforces_mode {
            assert!(matches!(
                entry.failure.as_ref(),
                Some(WalkFailure::Io {
                    kind: std::io::ErrorKind::PermissionDenied,
                    path,
                    ..
                }) if path == &ignore
            ));
            assert!(cache.is_empty());
        }
    }

    #[test]
    fn partial_ignore_errors_keep_each_leaf_kind_with_its_path() {
        let first = PathBuf::from("a.ignore");
        let second = PathBuf::from("z.ignore");
        let error = ignore::Error::Partial(vec![
            ignore::Error::WithPath {
                path: second,
                err: Box::new(ignore::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))),
            },
            ignore::Error::WithPath {
                path: first.clone(),
                err: Box::new(ignore::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "gone",
                ))),
            },
        ]);

        assert!(matches!(
            primary_ignore_failure(&error, Path::new("fallback")),
            WalkFailure::Io {
                kind: std::io::ErrorKind::NotFound,
                path,
                ..
            } if path == first
        ));
    }

    #[test]
    fn cancellation_settles_while_the_global_build_lock_is_held() {
        use hearth_proto::ErrorKind;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        let cache = Arc::new(WalkCache::new(1));
        let build_guard = cache.build.lock();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker_cache = Arc::clone(&cache);
        let root = dir.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let error = worker_cache
                .get_cancellable(&root, key(), &worker_cancel)
                .err()
                .expect("a cancelled waiter must fail");
            done_tx.send(error.kind).unwrap();
        });

        started_rx.recv().unwrap();
        for _ in 0..100 {
            std::thread::yield_now();
        }
        cancel.cancel();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ErrorKind::Cancelled,
            "the waiter must settle before the held build lock is released"
        );
        drop(build_guard);
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn followed_alias_admits_nested_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink("missing", target.join("dangling")).unwrap();
        symlink("target", dir.path().join("alias")).unwrap();
        let cache = WalkCache::new(1);
        let opts = WalkKey {
            follow_symlinks: true,
            ..key()
        };

        let (entry, _) = cache.get(dir.path(), opts);

        assert_eq!(entry.failure, None);
        assert!(entry.symlinks.contains(&dir.path().join("alias/dangling")));
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
