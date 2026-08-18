//! Shared caches drawn from by every tool.

mod file;
mod walk;

pub use file::{FileCache, FileEntry};
pub use walk::{WalkCache, WalkEntry, WalkFailure, WalkKey};
