//! The single error type shared across the protocol boundary.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A coarse classification so callers (and JS) can branch on failure kind
/// without string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// A path did not exist.
    NotFound,
    /// Permission denied by the OS.
    Permission,
    /// An `edit` target was not found.
    NoMatch,
    /// An `edit` target matched more than once.
    MultipleMatches,
    /// Two `edit` targets covered overlapping (or nested) ranges.
    Overlap,
    /// The edits applied cleanly but produced content identical to the original.
    NoChange,
    /// Invalid parameters (bad regex, bad glob, non-absolute path, …).
    InvalidInput,
    /// A bash command exceeded its timeout.
    Timeout,
    /// The operation was cancelled through its `AbortSignal` / cancel token.
    /// Distinct from [`Timeout`](Self::Timeout) and from ordinary I/O failure.
    Cancelled,
    /// A mutation was dispatched but its outcome could not be determined — for
    /// example a warm shell that died mid-protocol after the command had
    /// already been written to it.
    ///
    /// **At-most-once contract**: the caller must not retry an operation that
    /// failed this way. Hearth never re-runs it internally either.
    Indeterminate,
    /// Underlying I/O error.
    Io,
    /// Anything else.
    Internal,
}

impl ErrorKind {
    /// The stable string tag JS callers branch on (matches the serde name).
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "notFound",
            ErrorKind::Permission => "permission",
            ErrorKind::NoMatch => "noMatch",
            ErrorKind::MultipleMatches => "multipleMatches",
            ErrorKind::Overlap => "overlap",
            ErrorKind::NoChange => "noChange",
            ErrorKind::InvalidInput => "invalidInput",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Indeterminate => "indeterminate",
            ErrorKind::Io => "io",
            ErrorKind::Internal => "internal",
        }
    }
}

/// A serializable, transport-safe tool error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolError {
    pub kind: ErrorKind,
    pub message: String,
    /// The path involved, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The 0-based index into `edits[]` this error is about, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_index: Option<u32>,
}

impl ToolError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), path: None, edit_index: None }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_edit_index(mut self, index: usize) -> Self {
        self.edit_index = Some(index as u32);
        self
    }

    pub fn not_found(path: impl Into<String>) -> Self {
        let path = path.into();
        Self::new(ErrorKind::NotFound, format!("no such file or directory: {path}")).with_path(path)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// The canonical cancellation error. Callers branch on
    /// [`ErrorKind::Cancelled`], never on this message.
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "operation aborted")
    }

    /// A dispatched-but-unknown-outcome failure. See [`ErrorKind::Indeterminate`].
    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Indeterminate, message)
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{:?}: {} ({})", self.kind, self.message, p),
            None => write!(f, "{:?}: {}", self.kind, self.message),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as IoKind;
        let kind = match e.kind() {
            IoKind::NotFound => ErrorKind::NotFound,
            IoKind::PermissionDenied => ErrorKind::Permission,
            IoKind::TimedOut => ErrorKind::Timeout,
            IoKind::InvalidInput => ErrorKind::InvalidInput,
            _ => ErrorKind::Io,
        };
        ToolError::new(kind, e.to_string())
    }
}
