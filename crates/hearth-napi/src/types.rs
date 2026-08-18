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
        Self {
            replacements: as_i64(r.replacements),
            byte_len: as_i64(r.byte_len),
        }
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
        Self {
            old_text: e.old_text,
            new_text: e.new_text,
        }
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
        Self {
            seq: as_i64(c.seq),
            channel: c.channel.into(),
            text: c.text,
        }
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
    /// True when output beyond the engine hard cap was drained and discarded.
    pub output_truncated: bool,
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
            output_truncated: r.output_truncated,
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
        Self {
            line_number: as_i64(l.line_number),
            text: l.text,
            is_match: l.is_match,
        }
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
    /// True when at least one selected file could not be searched completely.
    pub incomplete: bool,
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
            incomplete: r.incomplete,
            root: r.root,
            root_is_dir: r.root_is_dir,
        }
    }
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct FindParams {
    /// Smart-case glob pattern. Empty matches all; basename-only patterns
    /// match at every depth.
    pub pattern: String,
    /// Directory root. Defaults to the engine cwd.
    pub path: Option<String>,
    /// Maximum paths retained. Defaults to 1000; zero is valid.
    pub limit: Option<f64>,
    /// Include hidden entries. Defaults to true for pi compatibility.
    pub hidden: Option<bool>,
    /// Honor root-local `.ignore`/`.rgignore`. Defaults to true.
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    /// Root-relative path globs removed before matching/counting/limits.
    pub exclude_globs: Option<Vec<String>>,
}

impl TryFrom<FindParams> for proto::FindParams {
    type Error = proto::ToolError;

    fn try_from(p: FindParams) -> Result<Self, Self::Error> {
        let limit = match p.limit {
            Some(value)
                if !value.is_finite()
                    || value < 0.0
                    || value.fract() != 0.0
                    || value > 1_000_000.0 =>
            {
                return Err(proto::ToolError::invalid(
                    "find limit must be an integer from 0 through 1000000",
                ));
            }
            Some(value) => Some(value as u64),
            None => None,
        };
        Ok(Self {
            pattern: p.pattern,
            path: p.path.unwrap_or_else(|| ".".into()),
            limit,
            hidden: p.hidden.unwrap_or(true),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            exclude_globs: p.exclude_globs.unwrap_or_default(),
        })
    }
}

#[napi(object)]
pub struct FindResult {
    /// Search-root-relative POSIX paths; directories end in `/`.
    pub paths: Vec<String>,
    pub total_matches: i64,
    pub walk_cache_hit: bool,
    pub limit_reached: bool,
    /// Pi's 50 KiB presentation budget was crossed. `paths` includes the
    /// first complete crossing path so Pi emits its standard warning.
    pub output_limit_reached: bool,
    /// Absolute lexical root used by the walk cache.
    pub root: String,
}

impl From<proto::FindResult> for FindResult {
    fn from(r: proto::FindResult) -> Self {
        Self {
            paths: r.paths,
            total_matches: as_i64(r.total_matches),
            walk_cache_hit: r.walk_cache_hit,
            limit_reached: r.limit_reached,
            output_limit_reached: r.output_limit_reached,
            root: r.root,
        }
    }
}

// ---------------------------------------------------------------------------
// graph
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct GraphSymbolsParams {
    pub root: String,
    pub path: String,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphSymbolsParams> for proto::GraphParams {
    fn from(p: GraphSymbolsParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Symbols { path: p.path },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphOutlineParams {
    pub root: String,
    pub path: String,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphOutlineParams> for proto::GraphParams {
    fn from(p: GraphOutlineParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Outline { path: p.path },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphSearchParams {
    pub root: String,
    pub query: String,
    pub limit: Option<i64>,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphSearchParams> for proto::GraphParams {
    fn from(p: GraphSearchParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Search {
                query: p.query,
                // Mirrors the proto serde default.
                limit: p.limit.map(|v| v.max(0) as u64).unwrap_or(200),
            },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphDefinitionsParams {
    pub root: String,
    pub name: String,
    pub limit: Option<i64>,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphDefinitionsParams> for proto::GraphParams {
    fn from(p: GraphDefinitionsParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Definitions {
                name: p.name,
                // Mirrors the proto serde default.
                limit: p.limit.map(|v| v.max(0) as u64).unwrap_or(200),
            },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphDepsParams {
    pub root: String,
    pub path: String,
    pub depth: Option<u32>,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphDepsParams> for proto::GraphParams {
    fn from(p: GraphDepsParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Deps {
                path: p.path,
                // Mirrors the proto serde default.
                depth: p.depth.unwrap_or(1),
            },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphRdepsParams {
    pub root: String,
    pub path: String,
    pub depth: Option<u32>,
    pub verify: Option<bool>,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphRdepsParams> for proto::GraphParams {
    fn from(p: GraphRdepsParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Rdeps {
                path: p.path,
                // Mirrors the proto serde default.
                depth: p.depth.unwrap_or(1),
                verify: p.verify.unwrap_or(true),
            },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphNeighborhoodParams {
    pub root: String,
    pub path: String,
    pub depth: Option<u32>,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphNeighborhoodParams> for proto::GraphParams {
    fn from(p: GraphNeighborhoodParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Neighborhood {
                path: p.path,
                // Mirrors the proto serde default.
                depth: p.depth.unwrap_or(1),
            },
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(object)]
pub struct GraphStatusParams {
    pub root: String,
    pub hidden: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub follow_symlinks: Option<bool>,
    pub files: Option<Vec<String>>,
    pub max_stale_ms: Option<i64>,
    pub include_basis: Option<bool>,
}

impl From<GraphStatusParams> for proto::GraphParams {
    fn from(p: GraphStatusParams) -> Self {
        Self {
            root: p.root,
            op: proto::GraphOp::Status,
            hidden: p.hidden.unwrap_or(false),
            respect_gitignore: p.respect_gitignore.unwrap_or(true),
            follow_symlinks: p.follow_symlinks.unwrap_or(false),
            files: p.files.unwrap_or_default(),
            max_stale_ms: p.max_stale_ms.map(|v| v.max(0) as u64),
            include_basis: p.include_basis.unwrap_or(false),
        }
    }
}

#[napi(string_enum = "camelCase")]
#[derive(Clone, Copy)]
pub enum GraphGuarantee {
    Exact,
    Approximate,
}

impl From<proto::GraphGuarantee> for GraphGuarantee {
    fn from(v: proto::GraphGuarantee) -> Self {
        match v {
            proto::GraphGuarantee::Exact => GraphGuarantee::Exact,
            proto::GraphGuarantee::Approximate => GraphGuarantee::Approximate,
        }
    }
}

#[napi(object)]
pub struct GraphMeta {
    pub guarantee: GraphGuarantee,
    pub root: String,
    pub universe_files: i64,
    pub indexed_files: i64,
    pub unsupported_files: i64,
    pub oversize_files: i64,
    pub revalidated_files: i64,
    pub reindexed_files: i64,
    pub swept: bool,
    pub sweep_age_ms: i64,
    pub walk_cache_hit: bool,
    pub repair_truncated: bool,
}

impl From<proto::GraphMeta> for GraphMeta {
    fn from(m: proto::GraphMeta) -> Self {
        Self {
            guarantee: m.guarantee.into(),
            root: m.root,
            universe_files: as_i64(m.universe_files),
            indexed_files: as_i64(m.indexed_files),
            unsupported_files: as_i64(m.unsupported_files),
            oversize_files: as_i64(m.oversize_files),
            revalidated_files: as_i64(m.revalidated_files),
            reindexed_files: as_i64(m.reindexed_files),
            swept: m.swept,
            sweep_age_ms: as_i64(m.sweep_age_ms),
            walk_cache_hit: m.walk_cache_hit,
            repair_truncated: m.repair_truncated,
        }
    }
}

#[napi(object)]
pub struct GraphSymbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub node_id: String,
    pub line: i64,
    pub column: i64,
    pub end_line: Option<i64>,
    pub end_column: Option<i64>,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
    pub depth: u32,
}

impl From<proto::GraphSymbol> for GraphSymbol {
    fn from(s: proto::GraphSymbol) -> Self {
        Self {
            name: s.name,
            kind: s.kind,
            path: s.path,
            node_id: s.node_id,
            line: as_i64(s.line),
            column: as_i64(s.column),
            end_line: s.end_line.map(as_i64),
            end_column: s.end_column.map(as_i64),
            start_byte: s.start_byte.map(as_i64),
            end_byte: s.end_byte.map(as_i64),
            depth: s.depth,
        }
    }
}

#[napi(object)]
pub struct GraphNode {
    pub path: String,
    pub node_id: String,
    pub language: Option<String>,
    pub indexed: bool,
}

impl From<proto::GraphNode> for GraphNode {
    fn from(n: proto::GraphNode) -> Self {
        Self {
            path: n.path,
            node_id: n.node_id,
            language: n.language,
            indexed: n.indexed,
        }
    }
}

#[napi(object)]
pub struct GraphDepEdge {
    pub from: String,
    pub from_node_id: String,
    pub to: String,
    pub to_node_id: Option<String>,
    pub to_kind: String,
    pub specifier: String,
    pub kind: String,
    pub line: i64,
    pub guarantee: GraphGuarantee,
}

impl From<proto::GraphDepEdge> for GraphDepEdge {
    fn from(e: proto::GraphDepEdge) -> Self {
        Self {
            from: e.from,
            from_node_id: e.from_node_id,
            to: e.to,
            to_node_id: e.to_node_id,
            to_kind: e.to_kind,
            specifier: e.specifier,
            kind: e.kind,
            line: as_i64(e.line),
            guarantee: e.guarantee.into(),
        }
    }
}

#[napi(object)]
pub struct GraphUnresolvedImport {
    pub specifier: String,
    pub line: i64,
    pub reason: String,
}

impl From<proto::GraphUnresolvedImport> for GraphUnresolvedImport {
    fn from(i: proto::GraphUnresolvedImport) -> Self {
        Self {
            specifier: i.specifier,
            line: as_i64(i.line),
            reason: i.reason,
        }
    }
}

#[napi(object)]
pub struct GraphRdepEntry {
    pub node: GraphNode,
    pub specifier: Option<String>,
    pub line: i64,
    pub guarantee: GraphGuarantee,
}

impl From<proto::GraphRdepEntry> for GraphRdepEntry {
    fn from(e: proto::GraphRdepEntry) -> Self {
        Self {
            node: e.node.into(),
            specifier: e.specifier,
            line: as_i64(e.line),
            guarantee: e.guarantee.into(),
        }
    }
}

#[napi(object)]
pub struct GraphBasisEntry {
    pub path: String,
    pub content_hash_hex: String,
}

impl From<proto::GraphBasisEntry> for GraphBasisEntry {
    fn from(e: proto::GraphBasisEntry) -> Self {
        Self {
            path: e.path,
            content_hash_hex: e.content_hash_hex,
        }
    }
}

#[napi(object)]
pub struct GraphCoverage {
    pub analyzed: i64,
    pub stubs: i64,
    pub basis: Vec<GraphBasisEntry>,
}

impl From<proto::GraphCoverage> for GraphCoverage {
    fn from(c: proto::GraphCoverage) -> Self {
        Self {
            analyzed: as_i64(c.analyzed),
            stubs: as_i64(c.stubs),
            basis: c.basis.into_iter().map(Into::into).collect(),
        }
    }
}

#[napi(object)]
pub struct GraphSymbolsResult {
    pub path: String,
    pub node_id: String,
    pub symbols: Vec<GraphSymbol>,
    pub truncated: bool,
}

impl From<proto::GraphSymbolsResult> for GraphSymbolsResult {
    fn from(r: proto::GraphSymbolsResult) -> Self {
        Self {
            path: r.path,
            node_id: r.node_id,
            symbols: r.symbols.into_iter().map(Into::into).collect(),
            truncated: r.truncated,
        }
    }
}

#[napi(object)]
pub struct GraphOutlineResult {
    pub path: String,
    pub node_id: String,
    pub symbols: Vec<GraphSymbol>,
    pub truncated: bool,
}

impl From<proto::GraphOutlineResult> for GraphOutlineResult {
    fn from(r: proto::GraphOutlineResult) -> Self {
        Self {
            path: r.path,
            node_id: r.node_id,
            symbols: r.symbols.into_iter().map(Into::into).collect(),
            truncated: r.truncated,
        }
    }
}

#[napi(object)]
pub struct GraphSearchResult {
    pub symbols: Vec<GraphSymbol>,
    pub limit_reached: bool,
}

impl From<proto::GraphSearchResult> for GraphSearchResult {
    fn from(r: proto::GraphSearchResult) -> Self {
        Self {
            symbols: r.symbols.into_iter().map(Into::into).collect(),
            limit_reached: r.limit_reached,
        }
    }
}

#[napi(object)]
pub struct GraphDefinitionsResult {
    pub symbols: Vec<GraphSymbol>,
    pub limit_reached: bool,
}

impl From<proto::GraphDefinitionsResult> for GraphDefinitionsResult {
    fn from(r: proto::GraphDefinitionsResult) -> Self {
        Self {
            symbols: r.symbols.into_iter().map(Into::into).collect(),
            limit_reached: r.limit_reached,
        }
    }
}

#[napi(object)]
pub struct GraphDepsResult {
    pub node: GraphNode,
    pub edges: Vec<GraphDepEdge>,
    pub unresolved: Vec<GraphUnresolvedImport>,
    pub coverage: GraphCoverage,
}

impl From<proto::GraphDepsResult> for GraphDepsResult {
    fn from(r: proto::GraphDepsResult) -> Self {
        Self {
            node: r.node.into(),
            edges: r.edges.into_iter().map(Into::into).collect(),
            unresolved: r.unresolved.into_iter().map(Into::into).collect(),
            coverage: r.coverage.into(),
        }
    }
}

#[napi(object)]
pub struct GraphRdepsResult {
    pub node: GraphNode,
    pub importers: Vec<GraphRdepEntry>,
    pub verified: bool,
    pub coverage: GraphCoverage,
}

impl From<proto::GraphRdepsResult> for GraphRdepsResult {
    fn from(r: proto::GraphRdepsResult) -> Self {
        Self {
            node: r.node.into(),
            importers: r.importers.into_iter().map(Into::into).collect(),
            verified: r.verified,
            coverage: r.coverage.into(),
        }
    }
}

#[napi(object)]
pub struct GraphNeighborhoodResult {
    pub center: GraphNode,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphDepEdge>,
    pub coverage: GraphCoverage,
}

impl From<proto::GraphNeighborhoodResult> for GraphNeighborhoodResult {
    fn from(r: proto::GraphNeighborhoodResult) -> Self {
        Self {
            center: r.center.into(),
            nodes: r.nodes.into_iter().map(Into::into).collect(),
            edges: r.edges.into_iter().map(Into::into).collect(),
            coverage: r.coverage.into(),
        }
    }
}

#[napi(object)]
pub struct GraphLanguageStatus {
    pub language: String,
    pub files: i64,
    pub symbols: i64,
}

impl From<proto::GraphLanguageStatus> for GraphLanguageStatus {
    fn from(s: proto::GraphLanguageStatus) -> Self {
        Self {
            language: s.language,
            files: as_i64(s.files),
            symbols: as_i64(s.symbols),
        }
    }
}

#[napi(object)]
pub struct GraphStatusResult {
    pub built: bool,
    pub building: bool,
    pub universe_files: i64,
    pub indexed_files: i64,
    pub unsupported_files: i64,
    pub oversize_files: i64,
    pub pending_files: i64,
    pub stale_files: i64,
    pub failed_files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub components: i64,
    pub languages: Vec<GraphLanguageStatus>,
    pub last_sweep_ms_ago: Option<i64>,
    pub build_duration_us: Option<i64>,
}

impl From<proto::GraphStatusResult> for GraphStatusResult {
    fn from(r: proto::GraphStatusResult) -> Self {
        Self {
            built: r.built,
            building: r.building,
            universe_files: as_i64(r.universe_files),
            indexed_files: as_i64(r.indexed_files),
            unsupported_files: as_i64(r.unsupported_files),
            oversize_files: as_i64(r.oversize_files),
            pending_files: as_i64(r.pending_files),
            stale_files: as_i64(r.stale_files),
            failed_files: as_i64(r.failed_files),
            symbols: as_i64(r.symbols),
            edges: as_i64(r.edges),
            components: as_i64(r.components),
            languages: r.languages.into_iter().map(Into::into).collect(),
            last_sweep_ms_ago: r.last_sweep_ms_ago.map(as_i64),
            build_duration_us: r.build_duration_us.map(as_i64),
        }
    }
}

/// Exactly one output field is set, matching the requested graph operation.
#[napi(object)]
pub struct GraphResult {
    pub meta: GraphMeta,
    pub symbols: Option<GraphSymbolsResult>,
    pub outline: Option<GraphOutlineResult>,
    pub search: Option<GraphSearchResult>,
    pub definitions: Option<GraphDefinitionsResult>,
    pub deps: Option<GraphDepsResult>,
    pub rdeps: Option<GraphRdepsResult>,
    pub neighborhood: Option<GraphNeighborhoodResult>,
    pub status: Option<GraphStatusResult>,
}

impl From<proto::GraphResult> for GraphResult {
    fn from(r: proto::GraphResult) -> Self {
        match r.output {
            proto::GraphOutput::Symbols(output) => Self {
                meta: r.meta.into(),
                symbols: Some(output.into()),
                outline: None,
                search: None,
                definitions: None,
                deps: None,
                rdeps: None,
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Outline(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: Some(output.into()),
                search: None,
                definitions: None,
                deps: None,
                rdeps: None,
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Search(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: Some(output.into()),
                definitions: None,
                deps: None,
                rdeps: None,
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Definitions(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: None,
                definitions: Some(output.into()),
                deps: None,
                rdeps: None,
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Deps(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: None,
                definitions: None,
                deps: Some(output.into()),
                rdeps: None,
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Rdeps(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: None,
                definitions: None,
                deps: None,
                rdeps: Some(output.into()),
                neighborhood: None,
                status: None,
            },
            proto::GraphOutput::Neighborhood(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: None,
                definitions: None,
                deps: None,
                rdeps: None,
                neighborhood: Some(output.into()),
                status: None,
            },
            proto::GraphOutput::Status(output) => Self {
                meta: r.meta.into(),
                symbols: None,
                outline: None,
                search: None,
                definitions: None,
                deps: None,
                rdeps: None,
                neighborhood: None,
                status: Some(output.into()),
            },
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
