//! Filesystem-backed [`SourceLoader`] — the only module that touches
//! `std::fs`, so the build driver itself stays free of ambient I/O.

use std::path::PathBuf;

use crate::build::SourceLoader;

/// Filesystem-backed source loader rooted at one directory.
pub struct FsLoader {
    root: PathBuf,
}

impl FsLoader {
    /// Creates a loader that resolves every source path relative to `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SourceLoader for FsLoader {
    fn verify(&self) -> Result<(), String> {
        let metadata = std::fs::metadata(&self.root).map_err(|error| {
            format!(
                "cannot build symbol index: source root '{}' is unavailable: {error}",
                self.root.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "cannot build symbol index: source root '{}' is not a directory",
                self.root.display()
            ));
        }
        Ok(())
    }

    fn probe(&self, path: &str) -> Option<u64> {
        let metadata = std::fs::metadata(self.root.join(path)).ok()?;
        metadata.is_file().then_some(metadata.len())
    }

    fn load(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }
}
