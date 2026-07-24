//! Shared helpers for the tools: path resolution and atomic writes.

use hearth_core::Engine;
use hearth_proto::ToolError;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolve a caller-supplied path to an absolute path, joining the engine's
/// default cwd when it is relative.
pub fn resolve_path(engine: &Engine, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        engine.config().default_cwd.join(p)
    }
}

/// Metadata captured right after a write, used to refresh the file cache.
pub struct WriteMeta {
    pub size: u64,
    pub mtime_ns: i128,
}

/// Write `bytes` to `path` atomically: write a sibling temp file, then rename
/// over the target so a concurrent reader never sees a partial file.
pub fn atomic_write(path: &Path, bytes: &[u8], create_dirs: bool) -> Result<WriteMeta, ToolError> {
    if create_dirs {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| io_err(e, path))?;
            }
        }
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp = dir.join(format!(".{name}.hearth.{pid}.{seq}.tmp"));

    // Write + flush the temp file, then atomically rename into place.
    let write_res = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e, path));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e, path));
    }

    let meta = fs::metadata(path).map_err(|e| io_err(e, path))?;
    Ok(WriteMeta { size: meta.len(), mtime_ns: mtime_nanos(&meta) })
}

/// Nanoseconds since the Unix epoch for the file's mtime (signed pre-epoch).
pub fn mtime_nanos(meta: &fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

pub fn io_err(e: std::io::Error, path: &Path) -> ToolError {
    let mut err = ToolError::from(e);
    err.path = Some(path.display().to_string());
    err
}
