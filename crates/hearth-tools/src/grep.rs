//! The `grep` tool.
//!
//! Uses the same core engine as ripgrep (`grep-searcher` + `grep-regex`). The
//! warm-path advantage over a one-shot `rg` is **composite**, not walk-only:
//! 1. the **walk cache** reuses the directory traversal + `.gitignore` parse;
//! 2. `get_bounded` searches **cached file bytes** (`search_slice`) for files
//!    ≤ 4 MiB, so a repeated search does zero `open()`/`read()` syscalls —
//!    only one `stat` per file for coherence;
//! 3. the OS page cache further warms both.
//!
//! Files above the cap fall back to `grep-searcher`'s own IO (`search_path`).
//! Per-file search is parallelised across worker threads.

use crate::util::resolve_path;
use dashmap::DashMap;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use hearth_core::cache::WalkKey;
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{
    FileMatches, GrepLine, GrepMode, GrepParams, GrepResult, ToolError, ToolResult,
};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const MAX_PATTERN_BYTES: usize = 1024 * 1024;
const MAX_GLOBS: usize = 256;
const MAX_GLOB_BYTES: usize = 16 * 1024;
const MAX_CONTEXT_LINES: u32 = 10_000;
const MAX_MATCHES: u64 = 1_000_000;
const DEFAULT_MAX_MATCHES: u64 = 100_000;
const MAX_GREP_FILES: usize = 1_000_000;
const MAX_GREP_SEARCH_HEAP_BYTES: usize = 16 * 1024 * 1024;
const MAX_MATCHER_CACHE_ENTRIES: usize = 256;
const MAX_MATCHER_CACHE_KEY_BYTES: usize = 16 * 1024 * 1024;

pub fn grep(engine: &Engine, params: &GrepParams) -> ToolResult<GrepResult> {
    grep_cancellable(engine, params, &CancelToken::none())
}

/// As [`grep`], but stops scheduling and searching promptly when `cancel` is
/// latched. Every worker is joined before this returns, so no search thread
/// outlives the call.
pub fn grep_cancellable(
    engine: &Engine,
    params: &GrepParams,
    cancel: &CancelToken,
) -> ToolResult<GrepResult> {
    profile!("tool.grep", {
        cancel.check()?;
        validate_params(params)?;
        // Compiled regex + glob sets are cached on the engine, so a repeated
        // pattern is never recompiled.
        let cache = engine.extension::<MatcherCache>();
        let matcher = cache.matcher(params)?;
        let glob_filter = cache.glob_filter(&params.globs)?;

        let root = resolve_path(engine, &params.path);
        let meta = std::fs::metadata(&root)
            .map_err(|_| ToolError::not_found(root.display().to_string()))?;
        let root_is_dir = meta.is_dir();
        if !root_is_dir && !meta.is_file() {
            return Err(ToolError::invalid(
                "grep target must be a regular file or directory",
            ));
        }

        // Resolve the target set as a shared slice + the indices passing the
        // glob filter — no per-file PathBuf clones (the walk's Arc is reused).
        // The walk cache returns a path-sorted list, so index order *is* path
        // order, which is what makes the global limit deterministic.
        let (all_files, indices, walk_hit): (Arc<Vec<PathBuf>>, Vec<usize>, bool) = if !root_is_dir
        {
            (Arc::new(vec![root.clone()]), vec![0], false)
        } else {
            let key = WalkKey {
                respect_gitignore: params.respect_gitignore,
                hidden: params.hidden,
                follow_symlinks: params.follow_symlinks,
            };
            engine.watch_root(&root);
            let (entry, hit) = engine.walks().get(&root, key);
            if !entry.complete {
                return Err(ToolError::invalid("grep walk exceeded its work budget"));
            }
            if entry.files.len() > MAX_GREP_FILES {
                return Err(ToolError::invalid("grep file set exceeds 1000000 files"));
            }
            let files = Arc::clone(&entry.files);
            let idx: Vec<usize> = (0..files.len())
                .filter(|&i| glob_filter.is_match(&files[i]))
                .collect();
            (files, idx, hit)
        };

        // If a healthy watcher covers this root and trust_watch is on, warm
        // hits skip the per-file freshness stat.
        let trust = engine.stat_free(&root);
        let total_limit = params.max_total_count.unwrap_or(DEFAULT_MAX_MATCHES);
        let limiter = Some(Limiter::new(indices.len(), total_limit));
        let searched = AtomicU64::new(0);
        let result_bytes = AtomicUsize::new(0);
        let result_exhausted = AtomicBool::new(false);
        let threads = engine.config().walk_threads.min(indices.len().max(1));
        let (tx, rx) = crossbeam_channel::unbounded::<(usize, usize)>();
        for (slot, &i) in indices.iter().enumerate() {
            let _ = tx.send((slot, i));
        }
        drop(tx);

        // Each worker accumulates locally; results merge once at the end, so
        // there is no per-match shared mutex on the hot path.
        let mut files: Vec<FileMatches> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let rx = rx.clone();
                    let matcher = Arc::clone(&matcher);
                    let all_files = Arc::clone(&all_files);
                    let limiter = limiter.as_ref();
                    let searched = &searched;
                    let result_bytes = &result_bytes;
                    let result_exhausted = &result_exhausted;
                    let params = &params;
                    let engine_ref = engine;
                    scope.spawn(move || {
                        let mut searcher = build_searcher(params);
                        let mut local: Vec<FileMatches> = Vec::new();
                        while let Ok((slot, i)) = rx.recv() {
                            if cancel.is_cancelled() || result_exhausted.load(Ordering::Acquire) {
                                break;
                            }
                            // Stop once every file that could sort *before* an
                            // unstarted one has already produced enough matches.
                            if limiter.is_some_and(|l| l.should_stop()) {
                                break;
                            }
                            searched.fetch_add(1, Ordering::Relaxed);
                            let found = search_one(
                                &mut searcher,
                                &matcher,
                                engine_ref,
                                &all_files[i],
                                params,
                                trust,
                                cancel,
                            );
                            let count = found.as_ref().map(|f| f.match_count).unwrap_or(0);
                            if let Some(fm) = found {
                                let bytes = file_matches_bytes(&fm);
                                let reserved = result_bytes.fetch_update(
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                    |current| {
                                        current.checked_add(bytes).filter(|&next| {
                                            next <= engine_ref.config().max_grep_output_bytes
                                        })
                                    },
                                );
                                if reserved.is_ok() {
                                    local.push(fm);
                                } else {
                                    result_exhausted.store(true, Ordering::Release);
                                    break;
                                }
                            }
                            if let Some(l) = limiter {
                                l.complete(slot, count);
                            }
                        }
                        local
                    })
                })
                .collect();
            let mut merged = Vec::new();
            for h in handles {
                if let Ok(mut local) = h.join() {
                    merged.append(&mut local);
                }
            }
            merged
        });

        // Every worker has been joined by here, so cancellation can be reported
        // with the guarantee that nothing is still searching.
        cancel.check()?;

        files.sort_by(|a, b| a.path.cmp(&b.path));
        let limit_reached = apply_total_limit(&mut files, total_limit, params.after_context as u64);
        if result_exhausted.load(Ordering::Acquire) {
            return Err(ToolError::invalid("grep result exceeds global byte limit"));
        }
        let total_matches: u64 = files.iter().map(|f| f.match_count).sum();
        let files_searched = searched.load(Ordering::Relaxed);

        hearth_core::profiler::count("tool.grep.files_searched", files_searched);
        hearth_core::profiler::count("tool.grep.matches", total_matches);

        Ok(GrepResult {
            files,
            total_matches,
            files_searched,
            walk_cache_hit: walk_hit,
            limit_reached,
            root: root.display().to_string(),
            root_is_dir,
        })
    })
}

fn file_matches_bytes(file: &FileMatches) -> usize {
    file.path.len().saturating_add(
        file.lines
            .iter()
            .map(|line| line.text.len().saturating_add(size_of::<GrepLine>()))
            .sum::<usize>(),
    )
}

fn validate_params(params: &GrepParams) -> ToolResult<()> {
    if params.pattern.len() > MAX_PATTERN_BYTES {
        return Err(ToolError::invalid("grep pattern exceeds 1 MiB"));
    }
    if params.globs.len() > MAX_GLOBS {
        return Err(ToolError::invalid("grep accepts at most 256 globs"));
    }
    if params.globs.iter().any(|glob| glob.len() > MAX_GLOB_BYTES) {
        return Err(ToolError::invalid("grep glob exceeds 16 KiB"));
    }
    if params.before_context > MAX_CONTEXT_LINES || params.after_context > MAX_CONTEXT_LINES {
        return Err(ToolError::invalid("grep context exceeds 10000 lines"));
    }
    if params.max_count.is_some_and(|limit| limit > MAX_MATCHES)
        || params
            .max_total_count
            .is_some_and(|limit| limit > MAX_MATCHES)
    {
        return Err(ToolError::invalid("grep match limit exceeds 1000000"));
    }
    Ok(())
}

/// Keep only the first `limit` matches in path order, reporting whether the cap
/// was hit.
///
/// Truncation runs after the merge and after sorting, so which matches survive
/// never depends on how the parallel search interleaved. A partially kept file
/// retains the context lines that follow its last kept match, but not the
/// leading context of the first dropped one.
fn apply_total_limit(files: &mut Vec<FileMatches>, limit: u64, after_context: u64) -> bool {
    let mut remaining = limit;
    let mut keep = 0usize;
    for file in files.iter_mut() {
        if remaining == 0 {
            break;
        }
        if file.match_count > remaining {
            if !file.lines.is_empty() {
                let mut seen = 0u64;
                let mut cut = file.lines.len();
                let mut last_match_line = 0u64;
                for (i, line) in file.lines.iter().enumerate() {
                    if line.is_match {
                        if seen == remaining {
                            cut = i;
                            break;
                        }
                        seen += 1;
                        last_match_line = line.line_number;
                    }
                }
                file.lines.truncate(cut);
                // The rows just before a dropped match are *its* leading
                // context, not the kept match's trailing context.
                let keep_through = last_match_line + after_context;
                while file
                    .lines
                    .last()
                    .is_some_and(|l| l.line_number > keep_through)
                {
                    file.lines.pop();
                }
            }
            file.match_count = remaining;
        }
        remaining -= file.match_count;
        keep += 1;
    }
    let dropped_files = files.len() > keep;
    files.truncate(keep);
    let kept: u64 = files.iter().map(|f| f.match_count).sum();
    dropped_files || kept >= limit
}

/// Deterministic early stop for a globally limited search.
///
/// Tracks the contiguous prefix of *completed* files in path order. Once that
/// prefix alone holds enough matches, no file that has not started yet can
/// contribute to the first `limit` matches, so workers can stop pulling work.
/// Files that complete out of order are still accounted, they just cannot
/// trigger the stop until the gap ahead of them fills in.
struct Limiter {
    inner: Mutex<LimiterInner>,
    stop: AtomicBool,
    limit: u64,
}

struct LimiterInner {
    counts: Vec<Option<u64>>,
    next: usize,
    prefix_matches: u64,
}

impl Limiter {
    fn new(slots: usize, limit: u64) -> Self {
        Self {
            inner: Mutex::new(LimiterInner {
                counts: vec![None; slots],
                next: 0,
                prefix_matches: 0,
            }),
            stop: AtomicBool::new(false),
            limit,
        }
    }

    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn complete(&self, slot: usize, matches: u64) {
        let mut inner = self.inner.lock();
        inner.counts[slot] = Some(matches);
        while inner.next < inner.counts.len() {
            match inner.counts[inner.next] {
                Some(n) => {
                    inner.prefix_matches = inner.prefix_matches.saturating_add(n);
                    inner.next += 1;
                }
                None => break,
            }
        }
        if inner.prefix_matches >= self.limit {
            self.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// The compiled-matcher cache — a per-engine extension keyed by pattern + flags
/// (regex) and by the glob list (glob sets). A repeated grep pattern is compiled
/// exactly once for the engine's lifetime.
#[derive(Default)]
pub struct MatcherCache {
    regex: DashMap<RegexKey, Arc<RegexMatcher>>,
    globs: DashMap<Vec<String>, Arc<GlobFilter>>,
    mutation: Mutex<()>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RegexKey {
    pattern: String,
    case_insensitive: bool,
    smart_case: bool,
    fixed_strings: bool,
    multiline: bool,
}

impl MatcherCache {
    fn matcher(&self, params: &GrepParams) -> ToolResult<Arc<RegexMatcher>> {
        let key = RegexKey {
            pattern: params.pattern.clone(),
            case_insensitive: params.case_insensitive,
            smart_case: params.smart_case,
            fixed_strings: params.fixed_strings,
            multiline: params.multiline,
        };
        if let Some(m) = self.regex.get(&key) {
            return Ok(Arc::clone(m.value()));
        }
        let m = Arc::new(build_matcher(params)?);
        let _mutation = self.mutation.lock();
        if let Some(existing) = self.regex.get(&key) {
            return Ok(Arc::clone(existing.value()));
        }
        if self.regex.len() >= MAX_MATCHER_CACHE_ENTRIES
            || regex_key_bytes(&self.regex).saturating_add(key.pattern.len())
                > MAX_MATCHER_CACHE_KEY_BYTES
        {
            self.regex.clear();
        }
        self.regex.insert(key, Arc::clone(&m));
        Ok(m)
    }

    fn glob_filter(&self, globs: &[String]) -> ToolResult<Arc<GlobFilter>> {
        if let Some(g) = self.globs.get(globs) {
            return Ok(Arc::clone(g.value()));
        }
        let g = Arc::new(GlobFilter::new(globs)?);
        let _mutation = self.mutation.lock();
        if let Some(existing) = self.globs.get(globs) {
            return Ok(Arc::clone(existing.value()));
        }
        let key_bytes = globs.iter().map(String::len).sum::<usize>();
        if self.globs.len() >= MAX_MATCHER_CACHE_ENTRIES
            || glob_key_bytes(&self.globs).saturating_add(key_bytes) > MAX_MATCHER_CACHE_KEY_BYTES
        {
            self.globs.clear();
        }
        self.globs.insert(globs.to_vec(), Arc::clone(&g));
        Ok(g)
    }
}

fn regex_key_bytes(cache: &DashMap<RegexKey, Arc<RegexMatcher>>) -> usize {
    cache.iter().map(|entry| entry.key().pattern.len()).sum()
}

fn glob_key_bytes(cache: &DashMap<Vec<String>, Arc<GlobFilter>>) -> usize {
    cache
        .iter()
        .map(|entry| entry.key().iter().map(String::len).sum::<usize>())
        .sum()
}

fn build_matcher(params: &GrepParams) -> ToolResult<RegexMatcher> {
    let pattern = if params.fixed_strings {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(params.case_insensitive)
        .case_smart(params.smart_case);
    if params.multiline {
        b.multi_line(true).dot_matches_new_line(true);
    }
    b.build(&pattern)
        .map_err(|e| ToolError::invalid(format!("invalid pattern: {e}")))
}

fn build_searcher(params: &GrepParams) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .multi_line(params.multiline)
        .before_context(params.before_context as usize)
        .after_context(params.after_context as usize)
        .binary_detection(BinaryDetection::quit(0))
        .heap_limit(Some(MAX_GREP_SEARCH_HEAP_BYTES))
        .build()
}

/// Hard per-file search bound. Every accepted file is read through the cache's
/// nonblocking, same-FD bounded path and searched from an in-memory slice; no
/// pathname is reopened after validation.
const MAX_GREP_CACHE_BYTES: u64 = 16 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
fn search_one(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    engine: &Engine,
    path: &Path,
    params: &GrepParams,
    trust: bool,
    cancel: &CancelToken,
) -> Option<FileMatches> {
    let mut sink = CollectSink {
        mode: params.mode,
        max_count: params.max_count,
        match_count: 0,
        blob: Vec::new(),
        spans: Vec::new(),
        found: false,
        max_output_bytes: engine.config().max_grep_output_bytes,
        output_full: false,
        cancel,
    };
    // Search only bytes returned by the cache's bounded same-FD loader. This
    // avoids a validation/reopen race against FIFOs or device nodes. Oversize,
    // unreadable, and binary files are skipped.
    let searched_ok = match engine
        .files()
        .get_bounded_trusting(path, MAX_GREP_CACHE_BYTES, trust)
    {
        Ok(Some((entry, _hit))) => searcher
            .search_slice(matcher, entry.bytes(), &mut sink)
            .is_ok(),
        Ok(None) | Err(_) => false,
    };
    if !searched_ok || !sink.found {
        return None;
    }
    Some(FileMatches {
        path: path.display().to_string(),
        match_count: sink.match_count,
        lines: sink.materialize(),
    })
}

/// A span into [`CollectSink::blob`] describing one collected line.
struct LineSpan {
    line_number: u64,
    start: u32,
    len: u32,
    is_match: bool,
}

/// A sink that collects matches (and context) for `Content` mode with **no
/// per-line heap allocation on the search hot path**: line text is appended to a
/// single growing byte buffer (`blob`, an arena) and referenced by `spans`; the
/// owned `Vec<GrepLine>` is materialized once, after the search, in
/// [`materialize`](Self::materialize).
struct CollectSink<'a> {
    mode: GrepMode,
    max_count: Option<u64>,
    match_count: u64,
    blob: Vec<u8>,
    spans: Vec<LineSpan>,
    found: bool,
    max_output_bytes: usize,
    output_full: bool,
    cancel: &'a CancelToken,
}

impl CollectSink<'_> {
    #[inline]
    fn push_line(&mut self, line_number: u64, bytes: &[u8], is_match: bool) {
        if self.output_full {
            return;
        }
        let text = trim_eol(bytes);
        let retained = self
            .blob
            .len()
            .saturating_add(self.spans.len().saturating_mul(size_of::<LineSpan>()));
        let remaining = self.max_output_bytes.saturating_sub(retained);
        if text.len().saturating_add(size_of::<LineSpan>()) > remaining {
            self.output_full = true;
            return;
        }
        let Ok(start) = u32::try_from(self.blob.len()) else {
            self.output_full = true;
            return;
        };
        let Ok(len) = u32::try_from(text.len()) else {
            self.output_full = true;
            return;
        };
        self.blob.extend_from_slice(text);
        self.spans.push(LineSpan {
            line_number,
            start,
            len,
            is_match,
        });
    }

    /// Build the owned result once, slicing each line out of the arena blob.
    fn materialize(&self) -> Vec<GrepLine> {
        self.spans
            .iter()
            .map(|s| {
                let slice = &self.blob[s.start as usize..(s.start + s.len) as usize];
                GrepLine {
                    line_number: s.line_number,
                    text: String::from_utf8_lossy(slice).into_owned(),
                    is_match: s.is_match,
                }
            })
            .collect()
    }
}

impl Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> std::io::Result<bool> {
        // Abandon a single huge file promptly instead of only between files.
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        self.found = true;
        // In FilesWithMatches mode a single hit is enough.
        if self.mode == GrepMode::FilesWithMatches {
            self.match_count = 1;
            return Ok(false);
        }
        if self.mode == GrepMode::Content {
            let line_number = mat.line_number().unwrap_or(0);
            self.push_line(line_number, mat.bytes(), true);
            if self.output_full {
                return Ok(false);
            }
        }
        self.match_count += 1;
        if let Some(mc) = self.max_count
            && self.match_count >= mc
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn context(&mut self, _s: &Searcher, ctx: &SinkContext<'_>) -> std::io::Result<bool> {
        if self.mode == GrepMode::Content {
            let line_number = ctx.line_number().unwrap_or(0);
            self.push_line(line_number, ctx.bytes(), false);
            if self.output_full {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Strip a trailing `\n` / `\r\n` from a matched line.
fn trim_eol(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &bytes[..end]
}

/// Glob filtering: a path passes if there are no globs, or any glob matches its
/// full path or its file name (so `*.rs` matches `src/x.rs`).
struct GlobFilter {
    sets: Vec<globset::GlobMatcher>,
}

impl GlobFilter {
    fn new(globs: &[String]) -> ToolResult<Self> {
        let mut sets = Vec::with_capacity(globs.len());
        for g in globs {
            let glob = globset::Glob::new(g)
                .map_err(|e| ToolError::invalid(format!("invalid glob {g:?}: {e}")))?;
            sets.push(glob.compile_matcher());
        }
        Ok(Self { sets })
    }

    fn is_match(&self, path: &Path) -> bool {
        if self.sets.is_empty() {
            return true;
        }
        let name = path.file_name().map(Path::new);
        self.sets
            .iter()
            .any(|m| m.is_match(path) || name.map(|n| m.is_match(n)).unwrap_or(false))
    }
}
