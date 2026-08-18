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
use crate::pathlock::mutation_key;
use crate::singleflight::SingleFlight;
use dashmap::DashMap;
use hearth_proto::ToolError;
use parking_lot::Mutex;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;
use xxhash_rust::xxh3::xxh3_64;

/// One cached file: its bytes plus lazily-computed derived data.
pub struct FileEntry {
    pub path: PathBuf,
    /// Canonical filesystem identity used to invalidate every cached symlink
    /// spelling of the same file after a Hearth-owned mutation.
    identity: PathBuf,
    data: Arc<[u8]>,
    pub size: u64,
    pub mtime_ns: i128,
    /// Source bytes plus the full reserved upper bound for the lazy line index.
    /// `None` means the bound overflowed `u64`, so this entry is never retained.
    accounted_bytes: Option<u64>,
    hash: OnceLock<u64>,
    line_index: OnceLock<LineIndex>,
    valid_utf8: OnceLock<bool>,
    /// Monotonic access stamp for LRU eviction (0 = never touched).
    last_access: AtomicU64,
}

impl FileEntry {
    fn new(path: PathBuf, data: Arc<[u8]>, size: u64, mtime_ns: i128) -> Self {
        let source_bytes = u64::try_from(data.len()).ok();
        let accounted_bytes = source_bytes.and_then(|source_bytes| {
            source_bytes.checked_add(LineIndex::max_heap_bytes(data.len())?)
        });
        let identity = mutation_key(&path);
        Self {
            path,
            identity,
            data,
            size,
            mtime_ns,
            accounted_bytes,
            hash: OnceLock::new(),
            line_index: OnceLock::new(),
            valid_utf8: OnceLock::new(),
            last_access: AtomicU64::new(0),
        }
    }

    fn retained_bytes(&self) -> u64 {
        self.accounted_bytes
            .expect("retained file entry must have representable accounting")
    }

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
        self.line_index.get_or_init(|| LineIndex::new(&self.data))
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
        *self
            .valid_utf8
            .get_or_init(|| std::str::from_utf8(&self.data).is_ok())
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

#[derive(Debug, Default)]
struct CacheState {
    /// Exact accounted bytes for entries currently present in `map`.
    /// `u128` also represents the permitted one-object insertion overshoot.
    total_bytes: u128,
}

type EvictionCandidate = (PathBuf, Arc<FileEntry>, u64);

/// A cache of file contents keyed by absolute path, validated by `(mtime, size)`.
pub struct FileCache {
    map: DashMap<PathBuf, Arc<FileEntry>>,
    loads: SingleFlight<PathBuf, Result<Arc<FileEntry>, ToolError>>,
    /// Serializes every map mutation with accounting and hard-cap enforcement.
    ///
    /// Invariant whenever this mutex is unlocked: `state.total_bytes` equals the
    /// sum of `FileEntry::accounted_bytes` in `map`, and both configured hard
    /// caps hold. An insertion may add only its one candidate while holding the
    /// mutex, then synchronously evicts before unlocking.
    state: Mutex<CacheState>,
    max_bytes: u64,
    max_entries: usize,
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
    /// Build an effectively unbounded standalone cache. Engines use
    /// [`with_limits`](Self::with_limits) so configuration caps are always live.
    pub fn new() -> Self {
        Self::with_limits(u64::MAX, usize::MAX)
    }

    /// Build a cache with immutable hard resident-byte and entry-count caps.
    pub fn with_limits(max_bytes: u64, max_entries: usize) -> Self {
        Self {
            map: DashMap::new(),
            loads: SingleFlight::new(),
            state: Mutex::new(CacheState::default()),
            max_bytes,
            max_entries,
            clock: AtomicU64::new(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Cumulative `(hits, misses)` since construction (always-on).
    pub fn cache_stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Stamp an entry as just-accessed (for LRU).
    #[inline]
    fn touch(&self, entry: &FileEntry) {
        entry.last_access.store(
            self.clock.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// Insert, replace, account, and synchronously restore both hard caps as one
    /// serialized cache mutation. The returned object is usable regardless of
    /// whether it is retained.
    fn insert_accounted(&self, key: PathBuf, entry: Arc<FileEntry>) {
        self.touch(&entry);
        let mut state = self.state.lock();

        let Some(new_bytes) = entry.accounted_bytes else {
            self.remove_key_locked(&mut state, &key);
            return;
        };
        if new_bytes > self.max_bytes || self.max_entries == 0 {
            // An individually oversized replacement must also invalidate the
            // prior value; retaining it would be stale in trust-cache mode.
            self.remove_key_locked(&mut state, &key);
            return;
        }

        if let Some(old) = self.map.insert(key, entry) {
            Self::subtract_locked(&mut state, old.retained_bytes());
        }
        state.total_bytes += u128::from(new_bytes);

        // The map may exceed either cap by this insertion alone while the gate
        // is held. Eviction completes before any other mutation or accounting
        // observer can proceed.
        self.enforce_limits_locked(&mut state, self.max_bytes, self.max_entries);
        debug_assert_eq!(state.total_bytes, self.accounted_map_bytes());
        debug_assert!(state.total_bytes <= u128::from(self.max_bytes));
        debug_assert!(self.map.len() <= self.max_entries);
    }

    fn subtract_locked(state: &mut CacheState, bytes: u64) {
        state.total_bytes = state
            .total_bytes
            .checked_sub(u128::from(bytes))
            .expect("file cache accounting invariant violated");
    }

    fn remove_key_locked(&self, state: &mut CacheState, path: &Path) -> bool {
        if let Some((_, removed)) = self.map.remove(path) {
            Self::subtract_locked(state, removed.retained_bytes());
            true
        } else {
            false
        }
    }

    fn remove_identity_locked(
        &self,
        state: &mut CacheState,
        path: &Path,
        expected: &Arc<FileEntry>,
    ) -> Option<Arc<FileEntry>> {
        let (_, removed) = self
            .map
            .remove_if(path, |_, current| Arc::ptr_eq(current, expected))?;
        Self::subtract_locked(state, removed.retained_bytes());
        Some(removed)
    }

    fn accounted_map_bytes(&self) -> u128 {
        self.map
            .iter()
            .map(|entry| u128::from(entry.value().retained_bytes()))
            .sum()
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        let _state = self.state.lock();
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    pub fn get_trusting(
        &self,
        path: &Path,
        trust: bool,
    ) -> Result<(Arc<FileEntry>, bool), ToolError> {
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
        if trust && let Some(entry) = self.map.get(path) {
            if entry.bytes().len() as u64 > max_bytes {
                return Ok(None);
            }
            crate::profiler::count("cache.file.hit_trusted", 1);
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.touch(entry.value());
            return Ok(Some((Arc::clone(entry.value()), true)));
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
        let mut file = options.open(path).map_err(|e| map_io(e, path))?;
        let meta = file.metadata().map_err(|e| map_io(e, path))?;
        if !meta.is_file() {
            return Err(
                ToolError::invalid(format!("not a regular file: {}", path.display()))
                    .with_path(path.display().to_string()),
            );
        }
        let size = meta.len();
        let mtime_ns = mtime_nanos(&meta);

        if let Some(entry) = self.map.get(path)
            && entry.size == size
            && entry.mtime_ns == mtime_ns
        {
            if entry.bytes().len() as u64 > max_bytes {
                return Ok(None);
            }
            crate::profiler::count("cache.file.hit", 1);
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.touch(entry.value());
            return Ok(Some((Arc::clone(entry.value()), true)));
        }

        if size > max_bytes {
            return Ok(None);
        }

        crate::profiler::count("cache.file.miss", 1);
        self.misses.fetch_add(1, Ordering::Relaxed);
        let key = path.to_path_buf();
        let loaded = self.loads.run(key.clone(), || {
            let max_read = max_bytes.saturating_add(1);
            let mut bytes = Vec::with_capacity(usize::try_from(size.min(max_bytes)).unwrap_or(0));
            file.by_ref()
                .take(max_read)
                .read_to_end(&mut bytes)
                .map_err(|e| map_io(e, &key))?;
            if bytes.len() as u64 > max_bytes {
                return Err(ToolError::invalid("file exceeds byte limit"));
            }
            let after = file.metadata().map_err(|e| map_io(e, &key))?;
            let final_size = after.len();
            if final_size != bytes.len() as u64 {
                return Err(ToolError::invalid("file changed while being read"));
            }
            let final_mtime_ns = mtime_nanos(&after);
            let entry = Arc::new(FileEntry::new(
                key.clone(),
                Arc::from(bytes.into_boxed_slice()),
                final_size,
                final_mtime_ns,
            ));
            Ok(entry)
        });
        let entry = loaded?;
        self.insert_accounted(key, Arc::clone(&entry));
        Ok(Some((entry, false)))
    }

    /// Update the cache to reflect a freshly-written file without re-reading it
    /// from disk. Called by `write`/`edit` right after they persist bytes.
    pub fn put_written(&self, path: &Path, bytes: Arc<[u8]>, size: u64, mtime_ns: i128) {
        let entry = Arc::new(FileEntry::new(path.to_path_buf(), bytes, size, mtime_ns));
        self.insert_accounted(path.to_path_buf(), entry);
    }

    /// Accounted bytes held across all entries.
    ///
    /// This includes source bytes plus each entry's full possible line-index
    /// allocation, whether or not that lazy index has been built yet.
    pub fn total_bytes(&self) -> u64 {
        let state = self.state.lock();
        u64::try_from(state.total_bytes)
            .expect("settled file cache accounting must fit its u64 hard cap")
    }

    fn lru_snapshot(&self) -> Vec<EvictionCandidate> {
        // Snapshot each entry's *identity* (the Arc) + recency, oldest first, so
        // we only ever remove the exact entry we costed — never a replacement.
        let mut items: Vec<EvictionCandidate> = self
            .map
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    Arc::clone(e.value()),
                    e.value().last_access.load(Ordering::Relaxed),
                )
            })
            .collect();
        items.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        items
    }

    fn over_limits(&self, state: &CacheState, byte_budget: u64, max_entries: usize) -> bool {
        state.total_bytes > u128::from(byte_budget) || self.map.len() > max_entries
    }

    fn evict_snapshot_locked(
        &self,
        state: &mut CacheState,
        items: Vec<EvictionCandidate>,
        byte_budget: u64,
        max_entries: usize,
    ) -> (usize, u128) {
        let mut evicted = 0usize;
        let mut freed = 0u128;
        for (path, arc, _) in items {
            if !self.over_limits(state, byte_budget, max_entries) {
                break;
            }
            // The snapshot can be stale. Charge only a successful conditional
            // removal of the exact Arc that was costed.
            if let Some(removed) = self.remove_identity_locked(state, &path, &arc) {
                freed += u128::from(removed.retained_bytes());
                evicted += 1;
            }
        }
        (evicted, freed)
    }

    fn enforce_limits_locked(
        &self,
        state: &mut CacheState,
        byte_budget: u64,
        max_entries: usize,
    ) -> (usize, u128) {
        let mut evicted = 0usize;
        let mut freed = 0u128;
        while self.over_limits(state, byte_budget, max_entries) {
            let items = self.lru_snapshot();
            let (round_evicted, round_freed) =
                self.evict_snapshot_locked(state, items, byte_budget, max_entries);
            assert!(
                round_evicted > 0,
                "file cache limits cannot be restored from current accounting"
            );
            evicted += round_evicted;
            freed += round_freed;
        }
        (evicted, freed)
    }

    fn evict_to_limits_with_first_snapshot_hook<F>(
        &self,
        byte_budget: u64,
        max_entries: usize,
        after_snapshot: F,
    ) -> (usize, u64)
    where
        F: FnOnce(),
    {
        {
            let state = self.state.lock();
            if !self.over_limits(&state, byte_budget, max_entries) {
                return (0, 0);
            }
        }
        // Deliberately snapshot before taking the mutation gate: optimizer work
        // does not block inserts while collecting candidates. Arc identity on
        // removal makes any intervening replacement safe.
        let first_items = self.lru_snapshot();
        after_snapshot();

        let mut state = self.state.lock();
        let (mut evicted, mut freed) =
            self.evict_snapshot_locked(&mut state, first_items, byte_budget, max_entries);
        let (retry_evicted, retry_freed) =
            self.enforce_limits_locked(&mut state, byte_budget, max_entries);
        evicted += retry_evicted;
        freed += retry_freed;
        debug_assert_eq!(state.total_bytes, self.accounted_map_bytes());
        (
            evicted,
            u64::try_from(freed).expect("settled file-cache eviction fits u64"),
        )
    }

    /// Evict least-recently-used entries until accounted cache bytes are at
    /// most `byte_budget`. Returns `(entries_evicted, accounted_bytes_freed)`.
    /// Runs off the hot path (the background optimizer calls it).
    pub fn evict_to_bytes(&self, byte_budget: u64) -> (usize, u64) {
        self.evict_to_limits_with_first_snapshot_hook(byte_budget, usize::MAX, || {})
    }

    /// Legacy entry-count bound (kept for callers that want a tighter cap).
    pub fn prune(&self, max_entries: usize) -> usize {
        self.evict_to_limits_with_first_snapshot_hook(u64::MAX, max_entries, || {})
            .0
    }

    /// Drop a single path and every cached symlink spelling with the same
    /// canonical filesystem identity. Returns whether any entry was removed.
    pub fn invalidate(&self, path: &Path) -> bool {
        self.invalidate_aliases(path) > 0
    }

    /// As [`Self::invalidate`], returning the exact number of alias entries
    /// removed for public invalidation accounting.
    pub fn invalidate_aliases(&self, path: &Path) -> usize {
        let identity = mutation_key(path);
        let mut state = self.state.lock();
        let victims: Vec<PathBuf> = self
            .map
            .iter()
            .filter(|entry| entry.key() == path || entry.value().identity == identity)
            .map(|entry| entry.key().clone())
            .collect();
        let mut removed = 0;
        for victim in victims {
            if self.remove_key_locked(&mut state, &victim) {
                removed += 1;
            }
        }
        removed
    }

    /// Drop `root`, every entry beneath it, and equivalent cached alias paths.
    ///
    /// Cost is proportional to the number of *cached* entries, not to the size
    /// of the tree on disk, so this stays bounded by the cache's own entry cap
    /// even when pointed at a huge directory.
    pub fn invalidate_prefix(&self, root: &Path) -> usize {
        let identity = mutation_key(root);
        let mut state = self.state.lock();
        let victims: Vec<PathBuf> = self
            .map
            .iter()
            .filter(|entry| {
                entry.key().starts_with(root) || entry.value().identity.starts_with(&identity)
            })
            .map(|entry| entry.key().clone())
            .collect();
        let mut removed = 0;
        for victim in victims {
            if self.remove_key_locked(&mut state, &victim) {
                removed += 1;
            }
        }
        removed
    }

    /// Drop everything. Returns the number of entries removed.
    pub fn clear(&self) -> usize {
        let mut state = self.state.lock();
        let n = self.map.len();
        self.map.clear();
        state.total_bytes = 0;
        n
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
    use std::sync::Barrier;

    fn write_file(dir: &Path, name: &str, size: usize) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![b'a'; size]).unwrap();
        p
    }

    fn accounted(size: usize) -> u64 {
        size as u64 + LineIndex::max_heap_bytes(size).unwrap()
    }

    fn bytes(size: usize, value: u8) -> Arc<[u8]> {
        Arc::from(vec![value; size].into_boxed_slice())
    }

    fn assert_settled(cache: &FileCache) {
        let state = cache.state.lock();
        assert_eq!(state.total_bytes, cache.accounted_map_bytes());
        assert!(state.total_bytes <= u128::from(cache.max_bytes));
        assert!(cache.map.len() <= cache.max_entries);
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
        let one = accounted(1000);
        assert_eq!(cache.total_bytes(), 3 * one);

        // Re-access a and b so c becomes least-recently-used.
        cache.get(&a).unwrap();
        cache.get(&b).unwrap();

        // A two-entry budget evicts exactly c, the LRU.
        let (evicted, freed) = cache.evict_to_bytes(2 * one);
        assert_eq!(evicted, 1);
        assert_eq!(freed, one);
        assert_eq!(cache.total_bytes(), 2 * one);

        // a and b remain cached (trusted hit); c was evicted (re-read → miss).
        assert!(
            cache.get_trusting(&a, true).unwrap().1,
            "a should still be warm"
        );
        assert!(
            !cache.get_trusting(&c, true).unwrap().1,
            "c should have been evicted"
        );

        // Tighter budget must actually reach it (regression for the freed
        // double-count that stopped eviction one entry short).
        cache.get(&a).unwrap();
        cache.get(&b).unwrap();
        cache.get(&c).unwrap();
        assert_eq!(cache.total_bytes(), 3 * one);
        cache.evict_to_bytes(one);
        assert_eq!(cache.total_bytes(), one, "budget must be reached exactly");
    }

    #[test]
    fn byte_cap_is_synchronous_on_every_insertion() {
        let cache = FileCache::with_limits(2 * accounted(64), 10);
        for i in 0..3 {
            cache.put_written(
                Path::new(match i {
                    0 => "/a",
                    1 => "/b",
                    _ => "/c",
                }),
                bytes(64, b'a' + i),
                64,
                i128::from(i),
            );
            assert_settled(&cache);
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_bytes(), 2 * accounted(64));
    }

    #[test]
    fn entry_cap_is_synchronous_on_every_insertion() {
        let cache = FileCache::with_limits(10 * accounted(64), 2);
        for i in 0..3 {
            let path = PathBuf::from(format!("/{i}"));
            cache.put_written(&path, bytes(64, b'x'), 64, i128::from(i));
            assert_settled(&cache);
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_bytes(), 2 * accounted(64));
    }

    #[test]
    fn hard_caps_and_accounting_remain_exact_under_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let one = accounted(64);
        let cache = Arc::new(FileCache::with_limits(4 * one, 4));
        let paths: Vec<PathBuf> = (0..12).map(|i| dir.path().join(format!("f{i}"))).collect();
        let start = Arc::new(Barrier::new(8));

        let mut handles = Vec::new();
        for t in 0..8usize {
            let cache = Arc::clone(&cache);
            let paths = paths.clone();
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                for round in 0..200usize {
                    let p = &paths[(t * 7 + round) % paths.len()];
                    cache.put_written(p, bytes(64, t as u8), 64, round as i128);
                    assert_settled(&cache);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_settled(&cache);
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.total_bytes(), 4 * one);
    }

    #[test]
    fn total_bytes_tracks_replacement_and_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new();
        let a = write_file(dir.path(), "a", 500);
        cache.get(&a).unwrap();
        assert_eq!(cache.total_bytes(), accounted(500));

        // put_written replaces the entry with a different size.
        let bytes: Arc<[u8]> = Arc::from(vec![b'x'; 800].into_boxed_slice());
        cache.put_written(&a, bytes, 800, 12345);
        assert_eq!(cache.total_bytes(), accounted(800));

        cache.invalidate(&a);
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn individually_oversized_object_is_returned_but_never_retained() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "oversized", 65);
        let cache = FileCache::with_limits(accounted(65) - 1, 8);

        let (first, first_hit) = cache.get(&path).unwrap();
        assert!(!first_hit);
        assert_eq!(first.bytes(), &[b'a'; 65]);
        assert!(cache.is_empty());
        assert_eq!(cache.total_bytes(), 0);

        let (second, second_hit) = cache.get(&path).unwrap();
        assert!(!second_hit, "an oversized object must be read again");
        assert_eq!(second.bytes(), first.bytes());
        assert!(cache.is_empty());
    }

    #[test]
    fn oversized_replacement_drops_the_prior_cached_identity() {
        let path = Path::new("/replacement");
        let cache = FileCache::with_limits(accounted(32), 8);
        cache.put_written(path, bytes(16, b'a'), 16, 1);
        assert_eq!(cache.len(), 1);

        cache.put_written(path, bytes(33, b'b'), 33, 2);
        assert!(cache.is_empty());
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn line_index_worst_case_is_reserved_before_lazy_construction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines");
        let source = vec![b'\n'; 128];
        std::fs::write(&path, &source).unwrap();
        let reserved = accounted(source.len());
        let cache = FileCache::with_limits(reserved, 1);

        let (entry, _) = cache.get(&path).unwrap();
        assert_eq!(cache.total_bytes(), reserved);
        let index = entry.line_index();
        assert_eq!(
            index.heap_bytes(),
            LineIndex::max_heap_bytes(source.len()).unwrap()
        );
        assert_eq!(cache.total_bytes(), reserved);
        assert_settled(&cache);
    }

    #[test]
    fn stale_concurrent_eviction_never_removes_or_charges_replacement() {
        let one = accounted(64);
        let cache = Arc::new(FileCache::with_limits(3 * one, 3));
        let victim = PathBuf::from("/victim");
        let other = PathBuf::from("/other");
        cache.put_written(&victim, bytes(64, b'v'), 64, 1);
        cache.put_written(&other, bytes(64, b'o'), 64, 1);

        let snapshot_ready = Arc::new(Barrier::new(2));
        let replacement_ready = Arc::new(Barrier::new(2));
        let worker_cache = Arc::clone(&cache);
        let worker_snapshot = Arc::clone(&snapshot_ready);
        let worker_replacement = Arc::clone(&replacement_ready);
        let evictor = std::thread::spawn(move || {
            worker_cache.evict_to_limits_with_first_snapshot_hook(one, usize::MAX, || {
                worker_snapshot.wait();
                worker_replacement.wait();
            })
        });

        snapshot_ready.wait();
        cache.put_written(&victim, bytes(64, b'n'), 64, 2);
        let replacement = Arc::clone(cache.map.get(&victim).unwrap().value());
        replacement_ready.wait();

        assert_eq!(evictor.join().unwrap(), (1, one));
        let current = cache.map.get(&victim).unwrap();
        assert!(Arc::ptr_eq(current.value(), &replacement));
        assert_eq!(current.bytes(), &[b'n'; 64]);
        assert!(!cache.map.contains_key(&other));
        drop(current);
        assert_eq!(cache.total_bytes(), one);
        assert_settled(&cache);
    }
}
