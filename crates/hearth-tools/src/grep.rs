//! The `grep` tool.
//!
//! Uses the same core engine as ripgrep (`grep-searcher` + `grep-regex`). The
//! warm-path advantage over a one-shot `rg` is **composite**, not walk-only:
//!  1. the **walk cache** reuses the directory traversal + `.gitignore` parse;
//!  2. `get_bounded` searches **cached file bytes** (`search_slice`) for files
//!     ≤ 4 MiB, so a repeated search does zero `open()`/`read()` syscalls —
//!     only one `stat` per file for coherence;
//!  3. the OS page cache further warms both.
//! Files above the cap fall back to `grep-searcher`'s own IO (`search_path`).
//! Per-file search is parallelised across worker threads.

use crate::util::resolve_path;
use dashmap::DashMap;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use hearth_core::cache::WalkKey;
use hearth_core::{profile, Engine};
use hearth_proto::{
    FileMatches, GrepLine, GrepMode, GrepParams, GrepResult, ToolError, ToolResult,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn grep(engine: &Engine, params: &GrepParams) -> ToolResult<GrepResult> {
    profile!("tool.grep", {
        // Compiled regex + glob sets are cached on the engine, so a repeated
        // pattern is never recompiled.
        let cache = engine.extension::<MatcherCache>();
        let matcher = cache.matcher(params)?;
        let glob_filter = cache.glob_filter(&params.globs)?;

        let root = resolve_path(engine, &params.path);
        let meta = std::fs::metadata(&root)
            .map_err(|_| ToolError::not_found(root.display().to_string()))?;

        // Resolve the target set as a shared slice + the indices passing the
        // glob filter — no per-file PathBuf clones (the walk's Arc is reused).
        let (all_files, indices, walk_hit): (Arc<Vec<PathBuf>>, Vec<usize>, bool) =
            if meta.is_file() {
                (Arc::new(vec![root.clone()]), vec![0], false)
            } else {
                let key = WalkKey {
                    respect_gitignore: params.respect_gitignore,
                    hidden: params.hidden,
                    follow_symlinks: params.follow_symlinks,
                };
                engine.watch_root(&root);
                let (entry, hit) = engine.walks().get(&root, key);
                let files = Arc::clone(&entry.files);
                let idx: Vec<usize> =
                    (0..files.len()).filter(|&i| glob_filter.is_match(&files[i])).collect();
                (files, idx, hit)
            };

        // If a healthy watcher covers this root and trust_watch is on, warm
        // hits skip the per-file freshness stat.
        let trust = engine.stat_free(&root);
        let files_searched = indices.len() as u64;
        let threads = engine.config().walk_threads.min(indices.len().max(1));
        let (tx, rx) = crossbeam_channel::unbounded::<usize>();
        for i in &indices {
            let _ = tx.send(*i);
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
                    let params = &params;
                    let engine_ref = engine;
                    scope.spawn(move || {
                        let mut searcher = build_searcher(params);
                        let mut local: Vec<FileMatches> = Vec::new();
                        while let Ok(i) = rx.recv() {
                            if let Some(fm) = search_one(
                                &mut searcher,
                                &matcher,
                                engine_ref,
                                &all_files[i],
                                params,
                                trust,
                            ) {
                                local.push(fm);
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

        files.sort_by(|a, b| a.path.cmp(&b.path));
        let total_matches: u64 = files.iter().map(|f| f.match_count).sum();

        hearth_core::profiler::count("tool.grep.files_searched", files_searched);
        hearth_core::profiler::count("tool.grep.matches", total_matches);

        Ok(GrepResult { files, total_matches, files_searched, walk_cache_hit: walk_hit })
    })
}

/// The compiled-matcher cache — a per-engine extension keyed by pattern + flags
/// (regex) and by the glob list (glob sets). A repeated grep pattern is compiled
/// exactly once for the engine's lifetime.
#[derive(Default)]
pub struct MatcherCache {
    regex: DashMap<RegexKey, Arc<RegexMatcher>>,
    globs: DashMap<Vec<String>, Arc<GlobFilter>>,
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
        self.regex.insert(key, Arc::clone(&m));
        Ok(m)
    }

    fn glob_filter(&self, globs: &[String]) -> ToolResult<Arc<GlobFilter>> {
        if let Some(g) = self.globs.get(globs) {
            return Ok(Arc::clone(g.value()));
        }
        let g = Arc::new(GlobFilter::new(globs)?);
        self.globs.insert(globs.to_vec(), Arc::clone(&g));
        Ok(g)
    }
}

fn build_matcher(params: &GrepParams) -> ToolResult<RegexMatcher> {
    let pattern = if params.fixed_strings {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(params.case_insensitive).case_smart(params.smart_case);
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
        .build()
}

/// Files at or below this size are searched from (and pulled into) the shared
/// cache; larger files are streamed via `search_path` so one search over a huge
/// tree never floods the warm cache.
const MAX_GREP_CACHE_BYTES: u64 = 4 * 1024 * 1024;

fn search_one(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    engine: &Engine,
    path: &Path,
    params: &GrepParams,
    trust: bool,
) -> Option<FileMatches> {
    let mut sink = CollectSink {
        mode: params.mode,
        max_count: params.max_count,
        match_count: 0,
        blob: Vec::new(),
        spans: Vec::new(),
        found: false,
    };
    // Fast path: search the cached bytes directly (no open()/read() syscalls on
    // a warm file, and no freshness stat when `trust` is set). Oversize or
    // uncacheable files fall back to grep-searcher's own IO. Unreadable/binary
    // files are silently skipped, matching rg.
    let searched_ok = match engine.files().get_bounded_trusting(path, MAX_GREP_CACHE_BYTES, trust) {
        Ok(Some((entry, _hit))) => searcher.search_slice(matcher, entry.bytes(), &mut sink).is_ok(),
        _ => searcher.search_path(matcher, path, &mut sink).is_ok(),
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
struct CollectSink {
    mode: GrepMode,
    max_count: Option<u64>,
    match_count: u64,
    blob: Vec<u8>,
    spans: Vec<LineSpan>,
    found: bool,
}

impl CollectSink {
    #[inline]
    fn push_line(&mut self, line_number: u64, bytes: &[u8], is_match: bool) {
        let text = trim_eol(bytes);
        let start = self.blob.len() as u32;
        self.blob.extend_from_slice(text);
        self.spans.push(LineSpan { line_number, start, len: text.len() as u32, is_match });
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

impl Sink for CollectSink {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> std::io::Result<bool> {
        self.found = true;
        // In FilesWithMatches mode a single hit is enough.
        if self.mode == GrepMode::FilesWithMatches {
            self.match_count = 1;
            return Ok(false);
        }
        self.match_count += 1;
        if self.mode == GrepMode::Content {
            let line_number = mat.line_number().unwrap_or(0);
            self.push_line(line_number, mat.bytes(), true);
        }
        if let Some(mc) = self.max_count {
            if self.match_count >= mc {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn context(&mut self, _s: &Searcher, ctx: &SinkContext<'_>) -> std::io::Result<bool> {
        if self.mode == GrepMode::Content {
            let line_number = ctx.line_number().unwrap_or(0);
            self.push_line(line_number, ctx.bytes(), false);
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
        self.sets.iter().any(|m| {
            m.is_match(path) || name.map(|n| m.is_match(n)).unwrap_or(false)
        })
    }
}
