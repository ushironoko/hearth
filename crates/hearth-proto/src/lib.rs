//! Shared protocol types for the Hearth tool orchestrator.
//!
//! These param/result types are the single contract shared by every surface:
//! the native Rust API (`hearth-tools`), the resident daemon's msgpack
//! transport, and the napi-rs Node binding. They are plain, `serde`-friendly
//! data with `camelCase` field names so a JS caller sees idiomatic objects.
//!
//! Keeping the contract in one dependency-light crate means the daemon, the
//! CLI, the benchmarks, and the Node addon never drift out of sync.

use serde::{Deserialize, Serialize};

pub mod error;
pub use error::{ErrorKind, ToolError};

/// Result alias used across tool implementations.
pub type ToolResult<T> = Result<T, ToolError>;

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

/// How a line window is cut out of a file.
///
/// The two modes differ only around newlines, but that difference is visible in
/// every returned byte, so it is part of the contract rather than a detail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LineWindowMode {
    /// `cat`-style slice: the window keeps the newline that terminates its last
    /// line, and a file's trailing newline does not add a phantom empty line to
    /// `totalLines`.
    #[default]
    Slice,
    /// `split('\n')` semantics: the file is split on `\n` and the selected
    /// elements are re-joined with `\n`, so the window never ends with a
    /// newline and a trailing newline yields a final empty element.
    SplitLines,
}

/// Parameters for the `read` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadParams {
    /// Absolute path of the file to read.
    pub path: String,
    /// 1-based line to start from. `None` starts at the first line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Maximum number of lines to return. `None` reads to EOF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// When true, prefix each returned line with its 1-based number (cat -n).
    #[serde(default)]
    pub line_numbers: bool,
    /// How the line window is cut. See [`LineWindowMode`].
    #[serde(default)]
    pub line_mode: LineWindowMode,
}

impl ReadParams {
    /// A whole-file read of `path` with default windowing.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: None,
            limit: None,
            line_numbers: false,
            line_mode: LineWindowMode::default(),
        }
    }
}

/// Result of the `read` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResult {
    /// The (possibly windowed) file content as UTF-8 text.
    pub content: String,
    /// Total number of lines in the underlying file, counted per `lineMode`.
    pub total_lines: u64,
    /// Number of lines actually returned.
    pub returned_lines: u64,
    /// Byte length of the underlying file.
    pub byte_len: u64,
    /// True when `limit`/`offset` clipped the returned content.
    pub truncated: bool,
    /// True when the file appears to be binary (content is best-effort).
    pub binary: bool,
    /// True when this response was served from the warm file cache.
    pub cache_hit: bool,
    /// Whether the underlying file ends with a newline. Lets a caller
    /// reconstruct either line convention from one read.
    pub ends_with_newline: bool,
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

/// How `write` puts bytes on disk. The two modes have genuinely different
/// filesystem semantics; see `docs/ARCHITECTURE.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriteMode {
    /// Crash-safe: write a sibling temp file, then `rename(2)` over the target.
    /// A reader never sees a partial file, but the target gets a **new inode**,
    /// so mode/owner/xattrs are those of the temp file and other hardlinks to
    /// the old inode keep the old content.
    #[default]
    Atomic,
    /// `open(O_TRUNC) + write` on the existing inode — the semantics of
    /// `fs.writeFile`. Preserves inode, mode, owner, xattrs and hardlinks, and
    /// a concurrent reader can observe a partially written file.
    InPlace,
}

/// Parameters for the `write` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteParams {
    /// Absolute path of the file to write.
    pub path: String,
    /// Full UTF-8 content to write.
    pub content: String,
    /// Create parent directories when they do not exist.
    #[serde(default = "default_true")]
    pub create_dirs: bool,
    /// How the bytes reach the disk. See [`WriteMode`].
    #[serde(default)]
    pub mode: WriteMode,
    /// When `path` is a symlink, write through to its target instead of
    /// replacing the link. Default true, matching `fs.writeFile`.
    #[serde(default = "default_true")]
    pub follow_symlinks: bool,
}

impl WriteParams {
    /// A default-mode write of `content` to `path`, creating parents.
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
            create_dirs: true,
            mode: WriteMode::default(),
            follow_symlinks: true,
        }
    }
}

/// Result of the `write` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResult {
    /// Number of bytes written.
    pub bytes_written: u64,
    /// True when the file already existed and was overwritten.
    pub existed: bool,
    /// The path actually written, after symlink resolution. Equals the request
    /// path unless a symlink was followed.
    pub path: String,
    /// True when `path` was a symlink and the write went through to its target.
    pub followed_symlink: bool,
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// Parameters for the single-replacement `edit` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditParams {
    /// Absolute path of the file to edit.
    pub path: String,
    /// Exact string to search for.
    pub old_string: String,
    /// Replacement string.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Result of the single-replacement `edit` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResult {
    /// Number of occurrences replaced.
    pub replacements: u64,
    /// Byte length of the file after editing.
    pub byte_len: u64,
}

/// One targeted replacement inside an [`EditBatchParams`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditReplacement {
    /// Exact text to replace. Must be unique in the original file and must not
    /// overlap another edit's target.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

/// How the batch `edit` tool treats a whitespace-only `oldText` — one whose
/// fuzzy-normalized form is empty while the text itself is not.
///
/// Such a target has no coordinates in normalized matching space, so anything
/// broader than exact whole-file replacement would silently weaken the
/// unique-target contract (occurrence counting cannot see it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhitespaceOnlyTargetPolicy {
    /// Reject the edit as invalid input — Hearth 0.1.0 behavior.
    #[default]
    Reject,
    /// Allow it only when the LF-normalized `oldText` equals the entire
    /// BOM-stripped, LF-normalized file content. Matching is exact, never
    /// normalized; a batch that also needs the normalized fallback is
    /// rejected; empty `oldText` stays invalid regardless.
    ExactFile,
}

/// Parameters for the batch `edit` tool: several disjoint replacements applied
/// atomically, each matched against the *original* file rather than
/// incrementally.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBatchParams {
    /// Absolute path of the file to edit.
    pub path: String,
    /// At least one replacement.
    pub edits: Vec<EditReplacement>,
    /// Lines of context to keep around each change in the returned hunks.
    #[serde(default = "default_diff_context")]
    pub diff_context: u32,
    /// Skip diff computation entirely (hunks come back empty). Useful for a
    /// caller that only needs the file changed.
    #[serde(default)]
    pub skip_diff: bool,
    /// Also return the full post-edit content. Off by default so the common
    /// case does not ship a second copy of the file across the boundary.
    #[serde(default)]
    pub return_content: bool,
    /// Also return the exact pre-edit text (BOM and line endings intact),
    /// captured while the mutation lock is held. Off by default for the same
    /// reason as `return_content`.
    #[serde(default)]
    pub return_original_content: bool,
    /// How a whitespace-only `oldText` is treated. Defaults to rejection.
    #[serde(default)]
    pub whitespace_only_target_policy: WhitespaceOnlyTargetPolicy,
    /// How the bytes reach the disk. See [`WriteMode`].
    #[serde(default)]
    pub mode: WriteMode,
    /// When `path` is a symlink, edit its target instead of replacing the link.
    #[serde(default = "default_true")]
    pub follow_symlinks: bool,
}

impl EditBatchParams {
    /// A default-configuration batch edit.
    pub fn new(path: impl Into<String>, edits: Vec<EditReplacement>) -> Self {
        Self {
            path: path.into(),
            edits,
            diff_context: default_diff_context(),
            skip_diff: false,
            return_content: false,
            return_original_content: false,
            whitespace_only_target_policy: WhitespaceOnlyTargetPolicy::default(),
            mode: WriteMode::default(),
            follow_symlinks: true,
        }
    }
}

/// What one row of a [`DiffHunk`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffOp {
    /// Context: present in both the old and the new file.
    Equal,
    /// Present only in the new file.
    Insert,
    /// Present only in the old file.
    Delete,
}

/// One line of a [`DiffHunk`], without its line terminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    pub op: DiffOp,
    /// 1-based line number in the pre-edit content (`None` for inserts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u64>,
    /// 1-based line number in the post-edit content (`None` for deletes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u64>,
    pub text: String,
}

/// A contiguous changed region plus its surrounding context, in the shape a
/// unified patch or a line-numbered display diff is rendered from. Runs of
/// unchanged lines longer than twice the context split into separate hunks, so
/// the gap between `old_start + old_lines` and the next hunk's `old_start` is
/// exactly the number of unchanged lines a renderer elides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    /// 1-based first line of this hunk in the pre-edit content.
    pub old_start: u64,
    /// Number of pre-edit lines covered.
    pub old_lines: u64,
    /// 1-based first line of this hunk in the post-edit content.
    pub new_start: u64,
    /// Number of post-edit lines covered.
    pub new_lines: u64,
    pub rows: Vec<DiffRow>,
}

/// Result of the batch `edit` tool.
///
/// Carries everything an adapter needs to render a display diff, a unified
/// patch, and a "first changed line" jump target — without re-reading the file
/// (which could observe a *different* state) and without reimplementing the
/// matching rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditBatchResult {
    /// The path actually written, after symlink resolution.
    pub path: String,
    /// Number of replacements applied (always `edits.len()`).
    pub replacements: u64,
    /// Byte length of the file after editing, including any BOM.
    pub byte_len: u64,
    /// True when at least one edit only matched after normalization.
    pub used_normalized_fallback: bool,
    /// True when the original file started with a UTF-8 BOM (preserved).
    pub had_bom: bool,
    /// True when the original file's line-ending convention was CRLF
    /// (preserved).
    pub crlf: bool,
    /// Line count of the pre-edit content (LF-normalized, BOM-stripped).
    pub old_line_count: u64,
    /// Line count of the post-edit content (LF-normalized, BOM-stripped).
    pub new_line_count: u64,
    /// 1-based line in the post-edit content where the first change appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<u64>,
    /// The changed regions with context. Empty when `skipDiff` was set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<DiffHunk>,
    /// Full post-edit content, only when `returnContent` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Exact pre-edit text (BOM and line endings intact), only when
    /// `returnOriginalContent` was set. Captured under the same mutation lock
    /// as the write, so no writer going through this engine can have touched
    /// the file between this snapshot and the commit — writers outside the
    /// engine are not serialized by that lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_content: Option<String>,
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

/// How a command reaches the shell process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellTransport {
    /// Appended as a final argv entry, e.g. `bash -c '<command>'`.
    #[default]
    Arg,
    /// Written to the shell's stdin, e.g. `bash -s`.
    Stdin,
}

/// The shell a `bash` call runs under. Configurable so an adapter can preserve
/// its own shell semantics instead of inheriting Hearth's default `/bin/sh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSpec {
    /// Executable to spawn (e.g. `/bin/bash`).
    pub program: String,
    /// Arguments before the command, e.g. `["-c"]` or `["-s"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// How `command` is handed over. See [`ShellTransport`].
    #[serde(default)]
    pub transport: ShellTransport,
}

impl Default for ShellSpec {
    fn default() -> Self {
        Self {
            program: "/bin/sh".into(),
            args: vec!["-c".into()],
            transport: ShellTransport::Arg,
        }
    }
}

/// Which pipe a [`BashChunk`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BashChannel {
    Stdout,
    Stderr,
}

/// One streamed slice of command output.
///
/// `seq` is a single monotonic counter shared by both channels, so replaying
/// chunks in `seq` order reproduces the order Hearth observed the bytes in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashChunk {
    pub seq: u64,
    pub channel: BashChannel,
    /// UTF-8 text. Multi-byte sequences are never split across chunks: an
    /// incomplete trailing sequence is held back until its continuation
    /// arrives, and invalid bytes are replaced.
    pub text: String,
}

/// Parameters for the `bash` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashParams {
    /// The command line to execute.
    pub command: String,
    /// Working directory. `None` uses the engine's default cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Hard timeout in milliseconds. `None` uses the engine default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Extra environment variables to set for this command only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Start from an empty environment instead of inheriting the engine's.
    /// Ignored in warm-shell mode, which always extends the pooled shell's
    /// environment.
    #[serde(default)]
    pub env_clear: bool,
    /// Shell to run under. `None` uses `/bin/sh -c`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellSpec>,
    /// Accumulate the full stdout/stderr into the result. A caller that only
    /// consumes the stream can turn this off to avoid holding the output twice.
    #[serde(default = "default_true")]
    pub collect_output: bool,
}

impl BashParams {
    /// A default-configuration command in the engine's cwd.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            timeout_ms: None,
            env: Vec::new(),
            env_clear: false,
            shell: None,
            collect_output: true,
        }
    }
}

/// Result of the `bash` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    /// Everything written to stdout. Empty when `collectOutput` was off.
    pub stdout: String,
    /// Everything written to stderr. Empty when `collectOutput` was off.
    pub stderr: String,
    /// Process exit code, or -1 when terminated by signal/timeout.
    pub exit_code: i32,
    /// The signal number that killed the process, when it was signalled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// True when the command was killed for exceeding `timeoutMs`.
    pub timed_out: bool,
    /// True when the command was killed because the caller cancelled. The call
    /// still returns the partial output rather than erroring, so a streaming
    /// caller keeps what it already rendered.
    pub aborted: bool,
    /// Wall-clock duration in microseconds.
    pub duration_us: u64,
    /// Number of chunks emitted to the stream callback. A streaming caller can
    /// assert it observed exactly this many.
    pub chunks: u64,
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

/// Output shape for the `grep` tool, mirroring ripgrep's common modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GrepMode {
    /// Only the paths of files that contain at least one match (`rg -l`).
    #[default]
    FilesWithMatches,
    /// Every matching line with metadata (`rg` default).
    Content,
    /// Per-file match counts (`rg -c`).
    Count,
}

/// Parameters for the `grep` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepParams {
    /// The regular expression (or literal, when `fixedStrings`).
    pub pattern: String,
    /// Root path to search. A file searches just that file.
    pub path: String,
    /// Output shape.
    #[serde(default)]
    pub mode: GrepMode,
    /// Restrict to paths matching these globs (e.g. `*.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// Case-insensitive matching.
    #[serde(default)]
    pub case_insensitive: bool,
    /// Smart case: case-insensitive unless the pattern has an uppercase letter.
    #[serde(default)]
    pub smart_case: bool,
    /// Treat `pattern` as a literal string, not a regex.
    #[serde(default)]
    pub fixed_strings: bool,
    /// Allow `.` to match newlines and the pattern to span lines.
    #[serde(default)]
    pub multiline: bool,
    /// Lines of context to include before each match (Content mode only).
    #[serde(default)]
    pub before_context: u32,
    /// Lines of context to include after each match (Content mode only).
    #[serde(default)]
    pub after_context: u32,
    /// Stop after this many matches **per file**. `None` is unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_count: Option<u64>,
    /// Stop after this many matches **across all files**. `None` is unlimited.
    ///
    /// The kept matches are the first `maxTotalCount` in path order, which does
    /// not depend on how the parallel search happened to interleave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_count: Option<u64>,
    /// Search hidden files/directories.
    #[serde(default)]
    pub hidden: bool,
    /// Honor `.gitignore`/`.ignore` rules (default true).
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    /// Follow symbolic links while walking.
    #[serde(default)]
    pub follow_symlinks: bool,
}

impl Default for GrepParams {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            path: ".".into(),
            mode: GrepMode::default(),
            globs: Vec::new(),
            case_insensitive: false,
            smart_case: false,
            fixed_strings: false,
            multiline: false,
            before_context: 0,
            after_context: 0,
            max_count: None,
            max_total_count: None,
            hidden: false,
            respect_gitignore: true,
            follow_symlinks: false,
        }
    }
}

impl GrepParams {
    /// A default-configuration search for `pattern` under `path`.
    pub fn new(pattern: impl Into<String>, path: impl Into<String>) -> Self {
        Self { pattern: pattern.into(), path: path.into(), ..Default::default() }
    }
}

/// A single matching line (Content mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepLine {
    /// 1-based line number.
    pub line_number: u64,
    /// The line text (without trailing newline).
    pub text: String,
    /// True when this line is a match, false when it is context.
    pub is_match: bool,
}

/// Matches for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMatches {
    pub path: String,
    /// Number of matching lines in this file.
    pub match_count: u64,
    /// Matching (and context) lines — empty in FilesWithMatches/Count modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<GrepLine>,
}

/// Result of the `grep` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepResult {
    /// Per-file results (order is deterministic: sorted by path).
    pub files: Vec<FileMatches>,
    /// Total matching lines across all returned files.
    pub total_matches: u64,
    /// Number of files searched (after ignore/glob filtering).
    pub files_searched: u64,
    /// True when the directory walk was served from the warm walk cache.
    pub walk_cache_hit: bool,
    /// True when `maxTotalCount` capped the result. More matches exist.
    pub limit_reached: bool,
    /// The resolved search root, so a caller can relativize `files[].path`
    /// exactly the way it intends without re-resolving the request path.
    pub root: String,
    /// Whether `root` is a directory (a file root searches only that file).
    pub root_is_dir: bool,
}

// ---------------------------------------------------------------------------
// cache invalidation
// ---------------------------------------------------------------------------

/// Which caches an invalidation request should drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheScope {
    /// The cached file contents only.
    Files,
    /// The cached directory walks only.
    Walks,
    /// Both.
    #[default]
    All,
}

/// Parameters for an explicit cache invalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidateParams {
    /// Path to invalidate. A file drops just that entry; a directory drops
    /// every cached entry under it.
    pub path: String,
    /// Invalidate everything under `path` rather than just `path` itself.
    #[serde(default)]
    pub recursive: bool,
    /// Which caches to touch.
    #[serde(default)]
    pub scope: CacheScope,
}

/// How much an invalidation actually dropped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidateResult {
    /// Number of file-cache entries removed.
    pub files_invalidated: u64,
    /// Number of walk-cache entries removed.
    pub walks_invalidated: u64,
}

// ---------------------------------------------------------------------------
// Envelope for the daemon transport
// ---------------------------------------------------------------------------

/// A request sent to the resident daemon.
///
/// Externally tagged (the default) so it round-trips cleanly through msgpack —
/// `rmp-serde` does not handle internally-tagged enums reliably.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Request {
    Read(ReadParams),
    Write(WriteParams),
    Edit(EditParams),
    EditBatch(EditBatchParams),
    Bash(BashParams),
    Grep(GrepParams),
    Invalidate(InvalidateParams),
    /// Drop every cached file and walk.
    ClearCaches,
    /// Health check.
    Ping,
    /// Return the profiler report as text.
    Stats,
    /// Ask the daemon to shut down gracefully.
    Shutdown,
}

/// A response returned by the resident daemon. Externally tagged (see [`Request`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Response {
    Read(ReadResult),
    Write(WriteResult),
    Edit(EditResult),
    EditBatch(EditBatchResult),
    Bash(BashResult),
    Grep(GrepResult),
    Invalidate(InvalidateResult),
    Pong,
    Stats(String),
    ShuttingDown,
    /// The `read` content was streamed straight to a client-passed stdout fd
    /// (via SCM_RIGHTS), bypassing payload serialization. Carries only metadata.
    Streamed(StreamedResult),
    Error(ToolError),
}

/// Metadata for a zero-copy streamed response (content already written to the
/// client's fd).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamedResult {
    pub bytes_written: u64,
    pub total_lines: u64,
}

fn default_true() -> bool {
    true
}

/// pi 0.80.7 renders both its display diff and its unified patch with four
/// lines of context; matching that default means an adapter never has to ask
/// for a second, differently-shaped diff.
fn default_diff_context() -> u32 {
    4
}
