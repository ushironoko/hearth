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
    /// The `edit` old_string was not found, or not unique without replace_all.
    NoMatch,
    /// The `edit` old_string matched more than once without replace_all.
    MultipleMatches,
    /// Invalid parameters (bad regex, bad glob, non-absolute path, …).
    InvalidInput,
    /// A bash command exceeded its timeout.
    Timeout,
    /// Underlying I/O error.
    Io,
    /// Anything else.
    Internal,
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
}

impl ToolError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), path: None }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
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
