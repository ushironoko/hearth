//! The shared file-content cache — the single resource `read`, `edit`, and
//! `grep` all draw from.
//!
//! A warm hit costs one `stat` (for coherence) plus an `Arc` clone: no re-read,
//! no re-parse, and the line index / content hash stay memoized across calls.
//! That is the resident-server advantage a one-shot `cat`/`rg` cannot have.
//!
//! Content is stored as **owned bytes** (`Arc<[u8]>`), not `mmap`, so an
//! external truncation can never `SIGBUS` a held mapping. `mmap` is reserved as
//! a transient, opt-in fast path for very large files inside `grep`.

use crate::line_index::LineIndex;
use crate::singleflight::SingleFlight;
use dashmap::DashMap;
use hearth_proto::ToolError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;
use xxhash_rust::xxh3::xxh3_64;

/// One cached file: its bytes plus lazily-computed derived data.
pub struct FileEntry {
    pub path: PathBuf,
    data: Arc<[u8]>,
    pub size: u64,
    pub mtime_ns: i128,
    hash: OnceLock<u64>,
    line_index: OnceLock<Arc<LineIndex>>,
    valid_utf8: OnceLock<bool>,
    /// Monotonic access stamp for LRU eviction (0 = never touched).
    last_access: AtomicU64,
}

impl FileEntry {
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Cheap clone of the backing bytes (shared, not copied).
    #[inline]
    pub fn arc_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.data)
    }

    /// The line index, built once and cached.
    pub fn line_index(&self) -> &LineIndex {
        self.line_index.get_or_init(|| Arc::new(LineIndex::new(&self.data)))
    }

    /// xxh3 content fingerprint, computed once.
    pub fn content_hash(&self) -> u64 {
        *self.hash.get_or_init(|| xxh3_64(&self.data))
    }

    /// Best-effort binary detection: a NUL byte in the first 8 KiB.
    pub fn is_binary(&self) -> bool {
        let head = &self.data[..self.data.len().min(8192)];
        memchr::memchr(0, head).is_some()
    }

    /// Whether the content is valid UTF-8, validated once and cached. This is
    /// the key to a fast warm read: after the first check, later reads skip the
    /// validation pass entirely.
    pub fn is_valid_utf8(&self) -> bool {
        *self.valid_utf8.get_or_init(|| std::str::from_utf8(&self.data).is_ok())
    }

    /// The content as `&str` without copying, when valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        if self.is_valid_utf8() {
            // SAFETY: validated and cached by `is_valid_utf8`.
            Some(unsafe { std::str::from_utf8_unchecked(&self.data) })
        } else {
            None
        }
    }

    /// An owned `String` copy of the content. For valid UTF-8 this skips
    /// re-validation (one alloc + copy), which is what lets a warm read beat a
    /// fresh `read_to_string` (copy **and** validate).
    pub fn to_text(&self) -> String {
        if self.is_valid_utf8() {
            // SAFETY: validated and cached above.
            unsafe { String::from_utf8_unchecked(self.data.to_vec()) }
        } else {
            String::from_utf8_lossy(&self.data).into_owned()
        }
    }

    /// The content as UTF-8 text, lossily replacing invalid sequences.
    pub fn text_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.data)
    }
}

/// A cache of file contents keyed by absolute path, validated by `(mtime, size)`.
pub struct FileCache {
    map: DashMap<PathBuf, Arc<FileEntry>>,
    loads: SingleFlight<PathBuf, Result<Arc<FileEntry>, ToolError>>,
    /// Total bytes currently held (maintained on insert/remove — O(1) to read).
    total_bytes: AtomicU64,
    /// Monotonic clock stamped onto entries on access, for LRU eviction.
    clock: AtomicU64,
    /// Always-on hit/miss counters (independent of the profiler) so the
    /// self-optimizer always has data to act on.
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            map: DashMap::new(),
            loads: SingleFlight::new(),
            total_bytes: AtomicU64::new(0),
            clock: AtomicU64::new(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Cumulative `(hits, misses)` since construction (always-on).
    pub fn cache_stats(&self) -> (u64, u64) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }

    /// Stamp an entry as just-accessed (for LRU).
    #[inline]
    fn touch(&self, entry: &FileEntry) {
        entry.last_access.store(self.clock.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Saturating subtract from `total_bytes` (never underflow-wraps, so a
    /// racing miscount can't blow the counter up to ~u64::MAX and trigger a
    /// runaway eviction).
    fn sub_bytes(&self, n: u64) {
        let mut cur = self.total_bytes.load(Ordering::Relaxed);
        loop {
            let new = cur.saturating_sub(n);
            match self.total_bytes.compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Insert an entry, keeping `total_bytes` correct across replacement.
    fn insert_accounted(&self, key: PathBuf, entry: Arc<FileEntry>) {
        let new_size = entry.size;
        self.touch(&entry);
        self.total_bytes.fetch_add(new_size, Ordering::Relaxed);
        if let Some(old) = self.map.insert(key, entry) {
            self.sub_bytes(old.size);
        }
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Fetch a file, serving from cache when the on-disk `(mtime, size)` is
    /// unchanged. `hit` reports whether the warm path was taken.
    pub fn get(&self, path: &Path) -> Result<(Arc<FileEntry>, bool), ToolError> {
        match self.get_bounded(path, u64::MAX)? {
            Some(pair) => Ok(pair),
            // Unreachable with u64::MAX, but keep it total.
            None => Err(ToolError::internal("file exceeds cache bound")),
        }
    }

    /// As [`get`](Self::get), but skips the freshness `stat` on a hit when
    /// `trust` is set (see [`get_bounded_trusting`](Self::get_bounded_trusting)).
    pub fn get_trusting(&self, path: &Path, trust: bool) -> Result<(Arc<FileEntry>, bool), ToolError> {
        match self.get_bounded_trusting(path, u64::MAX, trust)? {
            Some(pair) => Ok(pair),
            None => Err(ToolError::internal("file exceeds cache bound")),
        }
    }

    /// Like [`get`](Self::get) but refuses to cache files larger than
    /// `max_bytes`, returning `Ok(None)` instead. `grep` uses this so a single
    /// search over a huge tree never pulls oversize files into the warm cache;
    /// the caller streams those directly instead.
    pub fn get_bounded(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<(Arc<FileEntry>, bool)>, ToolError> {
        self.get_bounded_trusting(path, max_bytes, false)
    }

    /// As [`get_bounded`](Self::get_bounded), but when `trust` is set a cache
    /// hit is served **without a freshness `stat`** — the caller has vouched
    /// (via a healthy watcher) that stale entries are invalidated out of band.
    /// A miss still stats + reads normally.
    pub fn get_bounded_trusting(
        &self,
        path: &Path,
        max_bytes: u64,
        trust: bool,
    ) -> Result<Option<(Arc<FileEntry>, bool)>, ToolError> {
        if trust {
            if let Some(entry) = self.map.get(path) {
                crate::profiler::count("cache.file.hit_trusted", 1);
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.touch(entry.value());
                return Ok(Some((Arc::clone(entry.value()), true)));
            }
        }
        let meta = std::fs::metadata(path).map_err(|e| map_io(e, path))?;
        if !meta.is_file() {
            return Err(ToolError::invalid(format!("not a regular file: {}", path.display()))
                .with_path(path.display().to_string()));
        }
        let size = meta.len();
        let mtime_ns = mtime_nanos(&meta);

        if let Some(entry) = self.map.get(path) {
            if entry.size == size && entry.mtime_ns == mtime_ns {
                crate::profiler::count("cache.file.hit", 1);
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.touch(entry.value());
                return Ok(Some((Arc::clone(entry.value()), true)));
            }
        }

        if size > max_bytes {
            return Ok(None);
        }

        crate::profiler::count("cache.file.miss", 1);
        self.misses.fetch_add(1, Ordering::Relaxed);
        let key = path.to_path_buf();
        let loaded = self.loads.run(key.clone(), || {
            let bytes = std::fs::read(&key).map_err(|e| map_io(e, &key))?;
            let entry = Arc::new(FileEntry {
                path: key.clone(),
                data: Arc::from(bytes.into_boxed_slice()),
                size,
                mtime_ns,
                hash: OnceLock::new(),
                line_index: OnceLock::new(),
                valid_utf8: OnceLock::new(),
                last_access: AtomicU64::new(0),
            });
            Ok(entry)
        });
        let entry = loaded?;
        self.insert_accounted(key, Arc::clone(&entry));
        Ok(Some((entry, false)))
    }

    /// Update the cache to reflect a freshly-written file without re-reading it
    /// from disk. Called by `write`/`edit` right after they persist bytes.
    pub fn put_written(&self, path: &Path, bytes: Arc<[u8]>, size: u64, mtime_ns: i128) {
        let entry = Arc::new(FileEntry {
            path: path.to_path_buf(),
            data: bytes,
            size,
            mtime_ns,
            hash: OnceLock::new(),
            line_index: OnceLock::new(),
            valid_utf8: OnceLock::new(),
            last_access: AtomicU64::new(0),
        });
        self.insert_accounted(path.to_path_buf(), entry);
    }

    /// Total bytes held across all entries (O(1)).
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Evict least-recently-used entries until the cache holds at most
    /// `byte_budget` bytes. Returns `(entries_evicted, bytes_freed)`. Runs off
    /// the hot path (the background optimizer calls it).
    pub fn evict_to_bytes(&self, byte_budget: u64) -> (usize, u64) {
        if self.total_bytes() <= byte_budget {
            return (0, 0);
        }
        // Snapshot each entry's *identity* (the Arc) + recency, oldest first, so
        // we only ever remove the exact entry we costed — never a replacement.
        let mut items: Vec<(PathBuf, Arc<FileEntry>, u64)> = self
            .map
            .iter()
            .map(|e| {
                (e.key().clone(), Arc::clone(e.value()), e.value().last_access.load(Ordering::Relaxed))
            })
            .collect();
        items.sort_by_key(|(_, _, la)| *la);

        let mut freed = 0u64;
        let mut evicted = 0usize;
        for (path, arc, _) in items {
            // `total_bytes` already reflects prior removals — test it directly
            // (do NOT also subtract `freed`, or eviction stops one entry short).
            if self.total_bytes() <= byte_budget {
                break;
            }
            // Remove only if this exact entry is still present (not replaced by a
            // concurrent write); subtract the actually-removed size.
            if let Some((_, removed)) = self.map.remove_if(&path, |_, v| Arc::ptr_eq(v, &arc)) {
                self.sub_bytes(removed.size);
                freed += removed.size;
                evicted += 1;
            }
        }
        (evicted, freed)
    }

    /// Legacy entry-count bound (kept for callers that want a hard cap).
    pub fn prune(&self, max_entries: usize) -> usize {
        let len = self.map.len();
        if len <= max_entries {
            return 0;
        }
        let mut to_drop = len - max_entries;
        let mut dropped = 0;
        self.map.retain(|_, v| {
            if to_drop > 0 {
                to_drop -= 1;
                dropped += 1;
                self.sub_bytes(v.size);
                false
            } else {
                true
            }
        });
        dropped
    }

    /// Drop a single path (called by fs-watch invalidation).
    pub fn invalidate(&self, path: &Path) {
        if let Some((_, entry)) = self.map.remove(path) {
            self.sub_bytes(entry.size);
        }
    }

    /// Drop everything.
    pub fn clear(&self) {
        self.map.clear();
        self.total_bytes.store(0, Ordering::Relaxed);
    }
}

/// Nanoseconds since the Unix epoch for the file's mtime (signed for pre-epoch).
fn mtime_nanos(meta: &std::fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

fn map_io(e: std::io::Error, path: &Path) -> ToolError {
    let mut err = ToolError::from(e);
    err.path = Some(path.display().to_string());
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, size: usize) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![b'a'; size]).unwrap();
        p
    }

    #[test]
    fn byte_accounting_and_lru_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new();
        let a = write_file(dir.path(), "a", 1000);
        let b = write_file(dir.path(), "b", 1000);
        let c = write_file(dir.path(), "c", 1000);

        cache.get(&a).unwrap();
        cache.get(&b).unwrap();
        cache.get(&c).unwrap();
        assert_eq!(cache.total_bytes(), 3000);

        // Re-access a and b so c becomes least-recently-used.
        cache.get(&a).unwrap();
        cache.get(&b).unwrap();

        // Budget of 2000 → exactly one eviction, and it must be c (the LRU).
        let (evicted, freed) = cache.evict_to_bytes(2000);
        assert_eq!(evicted, 1);
        assert_eq!(freed, 1000);
        assert!(cache.total_bytes() <= 2000);

        // a and b remain cached (trusted hit); c was evicted (re-read → miss).
        assert!(cache.get_trusting(&a, true).unwrap().1, "a should still be warm");
        assert!(!cache.get_trusting(&c, true).unwrap().1, "c should have been evicted");

        // Tighter budget must actually reach it (regression for the freed
        // double-count that stopped eviction one entry short).
        cache.get(&a).unwrap();
        cache.get(&b).unwrap();
        cache.get(&c).unwrap();
        assert_eq!(cache.total_bytes(), 3000);
        cache.evict_to_bytes(1000);
        assert!(cache.total_bytes() <= 1000, "budget must be reached, not one short");
    }

    #[test]
    fn total_bytes_consistent_under_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::new());
        let paths: Vec<PathBuf> =
            (0..16).map(|i| write_file(dir.path(), &format!("f{i}"), 100 + i * 10)).collect();

        let mut handles = Vec::new();
        for t in 0..8u64 {
            let cache = Arc::clone(&cache);
            let paths = paths.clone();
            handles.push(std::thread::spawn(move || {
                for round in 0..300u64 {
                    let p = &paths[((t * 7 + round) as usize) % paths.len()];
                    let _ = cache.get(p);
                    if round % 5 == 0 {
                        cache.invalidate(p);
                    }
                    if round % 11 == 0 {
                        let b: Arc<[u8]> = Arc::from(vec![b'z'; 50].into_boxed_slice());
                        cache.put_written(p, b, 50, round as i128);
                    }
                    if round % 13 == 0 {
                        cache.evict_to_bytes(400);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // After all mutation stops, total_bytes must equal the real sum — no
        // drift, no underflow-wrap.
        let actual: u64 = cache.map.iter().map(|e| e.value().size).sum();
        assert_eq!(cache.total_bytes(), actual, "total_bytes drifted from the true sum");
    }

    #[test]
    fn total_bytes_tracks_replacement_and_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new();
        let a = write_file(dir.path(), "a", 500);
        cache.get(&a).unwrap();
        assert_eq!(cache.total_bytes(), 500);

        // put_written replaces the entry with a different size.
        let bytes: Arc<[u8]> = Arc::from(vec![b'x'; 800].into_boxed_slice());
        cache.put_written(&a, bytes, 800, 12345);
        assert_eq!(cache.total_bytes(), 800);

        cache.invalidate(&a);
        assert_eq!(cache.total_bytes(), 0);
    }
}
