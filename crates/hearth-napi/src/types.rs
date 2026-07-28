//! The typed N-API boundary.
//!
//! Every param and result is a concrete `#[napi(object)]` struct rather than a
//! `serde_json::Value`, so the generated `index.d.ts` describes the real shape
//! and a TypeScript caller gets checked field names instead of `any`.
//!
//! Counts and byte lengths are `i64` because napi-rs maps `i64` to a JS
//! `number` (and `u64` to a `bigint`, which would force every consumer to deal
//! with BigInt arithmetic for a line count). JS numbers are exact to 2^53,
//! comfortably past any real file.
//!
//! Optional fields are `Option<T>`, so they surface as `field?: T` and a caller
//! writes the minimal object; the `From` impls apply the same defaults the
//! native API uses.

use hearth_proto as proto;
use napi_derive::napi;
use std::collections::HashMap;

/// Saturating `u64` → `i64`, so a pathological value clamps rather than wraps
/// into a negative count.
fn as_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

// ---------------------------------------------------------------------------
// enums
// ---------------------------------------------------------------------------

/// How a line window is cut out of a file.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum LineWindowMode {
    /// `cat`-style: the window keeps the newline that ends its last line, and a
    /// trailing newline adds no phantom line to `totalLines`.
    Slice,
    /// Newline-split style: the file is split on newlines and the selected
    /// elements are re-joined, so a window never ends with a newline and a
    /// trailing newline yields a final empty element.
    SplitLines,
}

/// How `write` puts bytes on disk.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum WriteMode {
    /// Crash-safe temp file + rename. Replaces the target inode.
    Atomic,
    /// Truncate and rewrite the existing inode, like `fs.writeFile`.
    InPlace,
}

/// How `editBatch` treats a whitespace-only `oldText` — one whose
/// fuzzy-normalized form is empty while the text itself is not.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum WhitespaceOnlyTargetPolicy {
    /// Reject the edit as invalid input (the default).
    Reject,
    /// Allow it only when the LF-normalized `oldText` equals the entire
    /// BOM-stripped, LF-normalized file content; matching is exact, never
    /// normalized. Empty `oldText` stays invalid regardless.
    ExactFile,
}

/// Output shape for `grep`.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum GrepMode {
    FilesWithMatches,
    Content,
    Count,
}

/// How a command reaches the shell process.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum ShellTransport {
    /// Appended as a final argv entry, e.g. `bash -c '<command>'`.
    Arg,
    /// Written to the shell's stdin, e.g. `bash -s`.
    Stdin,
}

/// Which pipe a streamed chunk came from.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum BashChannel {
    Stdout,
    Stderr,
}

/// What one row of a diff hunk does.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum DiffOp {
    Equal,
    Insert,
    Delete,
}

/// Which caches an invalidation should drop.
#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum CacheScope {
    Files,
    Walks,
    All,
}

impl From<LineWindowMode> for proto::LineWindowMode {
    fn from(v: LineWindowMode) -> Self {
        match v {
            LineWindowMode::Slice => proto::LineWindowMode::Slice,
            LineWindowMode::SplitLines => proto::LineWindowMode::SplitLines,
        }
    }
}

impl From<WriteMode> for proto::WriteMode {
    fn from(v: WriteMode) -> Self {
        match v {
            WriteMode::Atomic => proto::WriteMode::Atomic,
            WriteMode::InPlace => proto::WriteMode::InPlace,
        }
    }
}

impl From<WhitespaceOnlyTargetPolicy> for proto::WhitespaceOnlyTargetPolicy {
    fn from(v: WhitespaceOnlyTargetPolicy) -> Self {
        match v {
            WhitespaceOnlyTargetPolicy::Reject => proto::WhitespaceOnlyTargetPolicy::Reject,
            WhitespaceOnlyTargetPolicy::ExactFile => proto::WhitespaceOnlyTargetPolicy::ExactFile,
        }
    }
}

impl From<GrepMode> for proto::GrepMode {
    fn from(v: GrepMode) -> Self {
        match v {
            GrepMode::FilesWithMatches => proto::GrepMode::FilesWithMatches,
            GrepMode::Content => proto::GrepMode::Content,
            GrepMode::Count => proto::GrepMode::Count,
        }
    }
}

impl From<ShellTransport> for proto::ShellTransport {
    fn from(v: ShellTransport) -> Self {
        match v {
            ShellTransport::Arg => proto::ShellTransport::Arg,
            ShellTransport::Stdin => proto::ShellTransport::Stdin,
        }
    }
}

impl From<proto::BashChannel> for BashChannel {
    fn from(v: proto::BashChannel) -> Self {
        match v {
            proto::BashChannel::Stdout => BashChannel::Stdout,
            proto::BashChannel::Stderr => BashChannel::Stderr,
        }
    }
}

impl From<proto::DiffOp> for DiffOp {
    fn from(v: proto::DiffOp) -> Self {
        match v {
            proto::DiffOp::Equal => DiffOp::Equal,
            proto::DiffOp::Insert => DiffOp::Insert,
            proto::DiffOp::Delete => DiffOp::Delete,
        }
    }
}

impl From<CacheScope> for proto::CacheScope {
    fn from(v: CacheScope) -> Self {
        match v {
            CacheScope::Files => proto::CacheScope::Files,
            CacheScope::Walks => proto::CacheScope::Walks,
            CacheScope::All => proto::CacheScope::All,
        }
    }
}

// ---------------------------------------------------------------------------
// engine
// ---------------------------------------------------------------------------

/// Construction options for a [`crate::HearthEngine`].
#[napi(object)]
#[derive(Default)]
pub struct EngineOptions {
    /// Working directory relative paths resolve against, and `bash`'s default.
    pub cwd: Option<String>,
    /// Threads used by the parallel directory walker and by `grep`.
    pub walk_threads: Option<u32>,
    /// Watch searched roots and invalidate caches from filesystem events.
    pub enable_watch: Option<bool>,
    /// Run the background cache-tuning loop.
    pub enable_optimizer: Option<bool>,
    /// Skip the per-hit freshness `stat` on warm reads. Single-writer fast path:
    /// changes made outside Hearth stay cached until explicitly invalidated.
    pub trust_cache: Option<bool>,
    /// Use the pooled warm shell for `bash`.
    pub warm_shell: Option<bool>,
    /// Hard cap on the number of files kept warm.
    pub max_cached_files: Option<u32>,
    /// Default `bash` timeout in milliseconds.
    pub bash_timeout_ms: Option<i64>,
    /// Default shell for `bash`. Per-call `shell` overrides this.
    pub shell: Option<ShellSpec>,
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct ReadParams {
    /// Path to read. Relative paths resolve against the engine's cwd.
    pub path: String,
    /// 1-based line to start from.
    pub offset: Option<i64>,
    /// Maximum number of lines to return.
    pub limit: Option<i64>,
    /// Prefix each returned line with its number, like `cat -n`.
    pub line_numbers: Option<bool>,
    /// How the window is cut. Defaults to `slice`.
    pub line_mode: Option<LineWindowMode>,
}

impl From<ReadParams> for proto::ReadParams {
    fn from(p: ReadParams) -> Self {
        Self {
            path: p.path,
            offset: p.offset.map(|v| v.max(0) as u64),
            limit: p.limit.map(|v| v.max(0) as u64),
            line_numbers: p.line_numbers.unwrap_or(false),
            line_mode: p.line_mode.map(Into::into).unwrap_or_default(),
        }
    }
}

#[napi(object)]
pub struct ReadResult {
    pub content: String,
    /// Total lines in the file, counted per `lineMode`.
    pub total_lines: i64,
    pub returned_lines: i64,
    pub byte_len: i64,
    /// True when `offset`/`limit` clipped the content.
    pub truncated: bool,
    /// True when the file looks binary; `content` is then empty.
    pub binary: bool,
    /// True when this came from the warm cache.
    pub cache_hit: bool,
    /// Whether the underlying file ends with a newline.
    pub ends_with_newline: bool,
}

impl From<proto::ReadResult> for ReadResult {
    fn from(r: proto::ReadResult) -> Self {
        Self {
            content: r.content,
            total_lines: as_i64(r.total_lines),
            returned_lines: as_i64(r.returned_lines),
            byte_len: as_i64(r.byte_len),
            truncated: r.truncated,
            binary: r.binary,
            cache_hit: r.cache_hit,
            ends_with_newline: r.ends_with_newline,
        }
    }
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct WriteParams {
    pub path: String,
    pub content: String,
    /// Create missing parent directories. Defaults to true.
    pub create_dirs: Option<bool>,
    /// Defaults to `atomic`.
    pub mode: Option<WriteMode>,
    /// Write through a symlink instead of replacing it. Defaults to true.
    pub follow_symlinks: Option<bool>,
}

impl From<WriteParams> for proto::WriteParams {
    fn from(p: WriteParams) -> Self {
        Self {
            path: p.path,
            content: p.content,
            create_dirs: p.create_dirs.unwrap_or(true),
            mode: p.mode.map(Into::into).unwrap_or_default(),
            follow_symlinks: p.follow_symlinks.unwrap_or(true),
        }
    }
}

#[napi(object)]
pub struct WriteResult {
    pub bytes_written: i64,
    /// True when the file already existed.
    pub existed: bool,
    /// The path actually written, after symlink resolution.
    pub path: String,
    pub followed_symlink: bool,
}

impl From<proto::WriteResult> for WriteResult {
    fn from(r: proto::WriteResult) -> Self {
        Self {
            bytes_written: as_i64(r.bytes_written),
            existed: r.existed,
            path: r.path,
            followed_symlink: r.followed_symlink,
        }
    }
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct EditParams {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

impl From<EditParams> for proto::EditParams {
    fn from(p: EditParams) -> Self {
        Self {
            path: p.path,
            old_string: p.old_string,
            new_string: p.new_string,
            replace_all: p.replace_all.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct EditResult {
    pub replacements: i64,
    pub byte_len: i64,
}

impl From<proto::EditResult> for EditResult {
    fn from(r: proto::EditResult) -> Self {
        Self { replacements: as_i64(r.replacements), byte_len: as_i64(r.byte_len) }
    }
}

/// One targeted replacement. `oldText` must be unique in the original file and
/// must not overlap another edit's target.
#[napi(object)]
pub struct EditReplacement {
    pub old_text: String,
    pub new_text: String,
}

impl From<EditReplacement> for proto::EditReplacement {
    fn from(e: EditReplacement) -> Self {
        Self { old_text: e.old_text, new_text: e.new_text }
    }
}

#[napi(object)]
pub struct EditBatchParams {
    pub path: String,
    /// At least one replacement. Each is matched against the original file, not
    /// against the result of an earlier edit in the same call.
    pub edits: Vec<EditReplacement>,
    /// Unchanged lines kept around each change in `hunks`. Defaults to 4.
    pub diff_context: Option<u32>,
    /// Skip diff computation; `hunks` comes back empty.
    pub skip_diff: Option<bool>,
    /// Also return the full post-edit content.
    pub return_content: Option<bool>,
    /// Also return the exact pre-edit text (BOM and line endings intact),
    /// captured while the mutation lock is held.
    pub return_original_content: Option<bool>,
    /// How a whitespace-only `oldText` is treated. Defaults to `reject`.
    pub whitespace_only_target_policy: Option<WhitespaceOnlyTargetPolicy>,
    /// Defaults to `atomic`.
    pub mode: Option<WriteMode>,
    /// Edit a symlink's target instead of replacing the link. Defaults to true.
    pub follow_symlinks: Option<bool>,
}

impl From<EditBatchParams> for proto::EditBatchParams {
    fn from(p: EditBatchParams) -> Self {
        let defaults = proto::EditBatchParams::new(String::new(), Vec::new());
        Self {
            path: p.path,
            edits: p.edits.into_iter().map(Into::into).collect(),
            diff_context: p.diff_context.unwrap_or(defaults.diff_context),
            skip_diff: p.skip_diff.unwrap_or(false),
            return_content: p.return_content.unwrap_or(false),
            return_original_content: p.return_original_content.unwrap_or(false),
            whitespace_only_target_policy: p
                .whitespace_only_target_policy
                .map(Into::into)
                .unwrap_or_default(),
            mode: p.mode.map(Into::into).unwrap_or_default(),
            follow_symlinks: p.follow_symlinks.unwrap_or(true),
        }
    }
}

/// One line of a diff hunk, without its terminator.
#[napi(object)]
pub struct DiffRow {
    pub op: DiffOp,
    /// 1-based line in the pre-edit content; absent for inserts.
    pub old_line: Option<i64>,
    /// 1-based line in the post-edit content; absent for deletes.
    pub new_line: Option<i64>,
    pub text: String,
}

impl From<proto::DiffRow> for DiffRow {
    fn from(r: proto::DiffRow) -> Self {
        Self {
            op: r.op.into(),
            old_line: r.old_line.map(as_i64),
            new_line: r.new_line.map(as_i64),
            text: r.text,
        }
    }
}

/// A changed region plus its context. The gap between one hunk's end and the
/// next hunk's start is exactly the number of unchanged lines a renderer elides.
#[napi(object)]
pub struct DiffHunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub rows: Vec<DiffRow>,
}

impl From<proto::DiffHunk> for DiffHunk {
    fn from(h: proto::DiffHunk) -> Self {
        Self {
            old_start: as_i64(h.old_start),
            old_lines: as_i64(h.old_lines),
            new_start: as_i64(h.new_start),
            new_lines: as_i64(h.new_lines),
            rows: h.rows.into_iter().map(Into::into).collect(),
        }
    }
}

#[napi(object)]
pub struct EditBatchResult {
    /// The path actually written, after symlink resolution.
    pub path: String,
    pub replacements: i64,
    pub byte_len: i64,
    /// True when at least one edit only matched after normalization.
    pub used_normalized_fallback: bool,
    /// True when the file started with a UTF-8 BOM (preserved).
    pub had_bom: bool,
    /// True when the file's line endings were CRLF (preserved).
    pub crlf: bool,
    /// Line count of the pre-edit content, counting the empty element after a
    /// trailing newline (the count a newline split yields).
    pub old_line_count: i64,
    /// Line count of the post-edit content, counted the same way.
    pub new_line_count: i64,
    /// 1-based line in the post-edit content where the first change appears.
    pub first_changed_line: Option<i64>,
    pub hunks: Vec<DiffHunk>,
    /// Present only when `returnContent` was set.
    pub content: Option<String>,
    /// Present only when `returnOriginalContent` was set: the exact pre-edit
    /// text, BOM and line endings intact, captured under the same mutation
    /// lock as the write. Only writers going through this engine are
    /// serialized by that lock.
    pub original_content: Option<String>,
}

impl From<proto::EditBatchResult> for EditBatchResult {
    fn from(r: proto::EditBatchResult) -> Self {
        Self {
            path: r.path,
            replacements: as_i64(r.replacements),
            byte_len: as_i64(r.byte_len),
            used_normalized_fallback: r.used_normalized_fallback,
            had_bom: r.had_bom,
            crlf: r.crlf,
            old_line_count: as_i64(r.old_line_count),
            new_line_count: as_i64(r.new_line_count),
            first_changed_line: r.first_changed_line.map(as_i64),
            hunks: r.hunks.into_iter().map(Into::into).collect(),
            content: r.content,
            original_content: r.original_content,
        }
    }
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

/// The shell a `bash` call runs under.
#[napi(object)]
#[derive(Clone)]
pub struct ShellSpec {
    /// Executable to spawn, e.g. `/bin/bash`.
    pub program: String,
    /// Arguments before the command, e.g. `["-c"]`.
    pub args: Option<Vec<String>>,
    /// How the command is handed over. Defaults to `arg`.
    pub transport: Option<ShellTransport>,
}

impl From<ShellSpec> for proto::ShellSpec {
    fn from(s: ShellSpec) -> Self {
        Self {
            program: s.program,
            args: s.args.unwrap_or_default(),
            transport: s.transport.map(Into::into).unwrap_or_default(),
        }
    }
}

#[napi(object)]
pub struct BashParams {
    pub command: String,
    /// Working directory. Defaults to the engine's cwd.
    pub cwd: Option<String>,
    /// Hard timeout in milliseconds. Defaults to the engine's.
    pub timeout_ms: Option<i64>,
    /// Extra environment variables for this command only.
    pub env: Option<HashMap<String, String>>,
    /// Start from an empty environment instead of inheriting. Ignored in
    /// warm-shell mode, which always extends the pooled shell's environment.
    pub env_clear: Option<bool>,
    /// Shell to run under. Defaults to the engine's, then `/bin/sh -c`.
    pub shell: Option<ShellSpec>,
    /// Accumulate the full output into the result. Defaults to true; turn it
    /// off when only consuming the stream.
    pub collect_output: Option<bool>,
}

impl From<BashParams> for proto::BashParams {
    fn from(p: BashParams) -> Self {
        Self {
            command: p.command,
            cwd: p.cwd,
            timeout_ms: p.timeout_ms.map(|v| v.max(0) as u64),
            env: p.env.map(|e| e.into_iter().collect()).unwrap_or_default(),
            env_clear: p.env_clear.unwrap_or(false),
            shell: p.shell.map(Into::into),
            collect_output: p.collect_output.unwrap_or(true),
        }
    }
}

/// One streamed slice of command output. `seq` is a single monotonic counter
/// shared by both channels, so replaying chunks in `seq` order reproduces the
/// order Hearth observed the bytes in.
#[napi(object)]
pub struct BashChunk {
    pub seq: i64,
    pub channel: BashChannel,
    /// UTF-8 text. Multi-byte sequences are never split across chunks.
    pub text: String,
}

impl From<proto::BashChunk> for BashChunk {
    fn from(c: proto::BashChunk) -> Self {
        Self { seq: as_i64(c.seq), channel: c.channel.into(), text: c.text }
    }
}

#[napi(object)]
pub struct BashResult {
    /// Everything written to stdout; empty when `collectOutput` was off.
    pub stdout: String,
    /// Everything written to stderr; empty when `collectOutput` was off.
    pub stderr: String,
    /// Exit code, or -1 when the command was signalled.
    pub exit_code: i32,
    /// The signal that killed the process, when it was signalled.
    pub signal: Option<i32>,
    /// True when the command was killed for exceeding its timeout.
    pub timed_out: bool,
    /// True when the caller's `AbortSignal` killed it. The partial output is
    /// still returned rather than thrown away.
    pub aborted: bool,
    pub duration_us: i64,
    /// How many chunks were delivered to the stream callback.
    pub chunks: i64,
}

impl From<proto::BashResult> for BashResult {
    fn from(r: proto::BashResult) -> Self {
        Self {
            stdout: r.stdout,
            stderr: r.stderr,
            exit_code: r.exit_code,
            signal: r.signal,
            timed_out: r.timed_out,
            aborted: r.aborted,
            duration_us: as_i64(r.duration_us),
            chunks: as_i64(r.chunks),
        }
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct GrepParams {
    /// Regular expression, or a literal when `fixedStrings` is set.
    pub pattern: String,
    /// Root to search. A file searches just that file.
    pub path: String,
    /// Defaults to `filesWithMatches`.
    pub mode: Option<GrepMode>,
    /// Restrict to paths matching any of these globs.
    pub globs: Option<Vec<String>>,
    pub case_insensitive: Option<bool>,
    /// Case-insensitive unless the pattern contains an uppercase letter.
    pub smart_case: Option<bool>,
    pub fixed_strings: Option<bool>,
    pub multiline: Option<bool>,
    /// Context lines before each match (`content` mode only).
    pub before_context: Option<u32>,
    /// Context lines after each match (`content` mode only).
    pub after_context: Option<u32>,
    /// Stop after this many matches per file.
    pub max_count: Option<i64>,
    /// Stop after this many matches across all files. The kept matches are the
    /// first `maxTotalCount` in path order, independent of scheduling.
    pub max_total_count: Option<i64>,
    /// Search hidden files and directories.
    pub hidden: Option<bool>,
    /// Honour `.gitignore`/`.ignore`. Defaults to true.
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
}

impl From<GrepParams> for proto::GrepParams {
    fn from(p: GrepParams) -> Self {
        Self {
            pattern: p.pattern,
            path: p.path,
            mode: p.mode.map(Into::into).unwrap_or_default(),
            globs: p.globs.unwrap_or_default(),
            case_insensitive: p.case_insensitive.unwrap_or(false),
            smart_case: p.smart_case.unwrap_or(false),
            fixed_strings: p.fixed_strings.unwrap_or(false),
            multiline: p.multiline.unwrap_or(false),
            before_context: p.before_context.unwrap_or(0),
            after_context: p.after_context.unwrap_or(0),
            max_count: p.max_count.map(|v| v.max(0) as u64),
            max_total_count: p.max_total_count.map(|v| v.max(0) as u64),
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GrepLine {
    pub line_number: i64,
    /// The line text, without its terminator.
    pub text: String,
    /// True for a match, false for a context line.
    pub is_match: bool,
}

impl From<proto::GrepLine> for GrepLine {
    fn from(l: proto::GrepLine) -> Self {
        Self { line_number: as_i64(l.line_number), text: l.text, is_match: l.is_match }
    }
}

#[napi(object)]
pub struct FileMatches {
    pub path: String,
    pub match_count: i64,
    /// Matching and context lines; empty outside `content` mode.
    pub lines: Vec<GrepLine>,
}

impl From<proto::FileMatches> for FileMatches {
    fn from(f: proto::FileMatches) -> Self {
        Self {
            path: f.path,
            match_count: as_i64(f.match_count),
            lines: f.lines.into_iter().map(Into::into).collect(),
        }
    }
}

#[napi(object)]
pub struct GrepResult {
    /// Per-file results, sorted by path.
    pub files: Vec<FileMatches>,
    pub total_matches: i64,
    pub files_searched: i64,
    pub walk_cache_hit: bool,
    /// True when `maxTotalCount` capped the result; more matches exist.
    pub limit_reached: bool,
    /// The resolved search root.
    pub root: String,
    pub root_is_dir: bool,
}

impl From<proto::GrepResult> for GrepResult {
    fn from(r: proto::GrepResult) -> Self {
        Self {
            files: r.files.into_iter().map(Into::into).collect(),
            total_matches: as_i64(r.total_matches),
            files_searched: as_i64(r.files_searched),
            walk_cache_hit: r.walk_cache_hit,
            limit_reached: r.limit_reached,
            root: r.root,
            root_is_dir: r.root_is_dir,
        }
    }
}

// ---------------------------------------------------------------------------
// cache invalidation
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct InvalidateResult {
    pub files_invalidated: i64,
    pub walks_invalidated: i64,
}

impl From<proto::InvalidateResult> for InvalidateResult {
    fn from(r: proto::InvalidateResult) -> Self {
        Self {
            files_invalidated: as_i64(r.files_invalidated),
            walks_invalidated: as_i64(r.walks_invalidated),
        }
    }
}
