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
}

/// Result of the `read` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResult {
    /// The (possibly windowed) file content as UTF-8 text.
    pub content: String,
    /// Total number of lines in the underlying file.
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
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

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
}

/// Result of the `write` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResult {
    /// Number of bytes written.
    pub bytes_written: u64,
    /// True when the file already existed and was overwritten.
    pub existed: bool,
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// Parameters for the `edit` tool.
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

/// Result of the `edit` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResult {
    /// Number of occurrences replaced.
    pub replacements: u64,
    /// Byte length of the file after editing.
    pub byte_len: u64,
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

/// Parameters for the `bash` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashParams {
    /// The command line to execute (run through the pooled warm shell).
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
}

/// Result of the `bash` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub stdout: String,
    pub stderr: String,
    /// Process exit code, or -1 when terminated by signal/timeout.
    pub exit_code: i32,
    /// True when the command was killed for exceeding `timeout_ms`.
    pub timed_out: bool,
    /// Wall-clock duration in microseconds.
    pub duration_us: u64,
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

/// Output shape for the `grep` tool, mirroring ripgrep's common modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GrepMode {
    /// Only the paths of files that contain at least one match (`rg -l`).
    FilesWithMatches,
    /// Every matching line with metadata (`rg` default).
    Content,
    /// Per-file match counts (`rg -c`).
    Count,
}

impl Default for GrepMode {
    fn default() -> Self {
        GrepMode::FilesWithMatches
    }
}

/// Parameters for the `grep` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepParams {
    /// The regular expression (or literal, when `fixed_strings`).
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
    /// Stop after this many matches per file. `None` is unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_count: Option<u64>,
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
            hidden: false,
            respect_gitignore: true,
            follow_symlinks: false,
        }
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
    /// Total matching lines across all files.
    pub total_matches: u64,
    /// Number of files searched (after ignore/glob filtering).
    pub files_searched: u64,
    /// True when the directory walk was served from the warm walk cache.
    pub walk_cache_hit: bool,
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
    Bash(BashParams),
    Grep(GrepParams),
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
    Bash(BashResult),
    Grep(GrepResult),
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
