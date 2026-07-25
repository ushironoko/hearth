//! Shared helpers for the tools: path resolution and the two write strategies.

use hearth_core::Engine;
use hearth_proto::{ToolError, WriteMode};
use std::fs::{self, File, OpenOptions};
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

/// The file a mutation of `path` should actually rewrite.
///
/// When `path` is a symlink and `follow` is set, that is the link's target —
/// otherwise an atomic write would `rename(2)` over the link itself and
/// silently destroy it, which is never what a caller editing "the file" means.
/// With `follow` off, the link itself is the target and gets replaced.
///
/// Returns `(target, followed)`.
pub fn resolve_write_target(path: &Path, follow: bool) -> (PathBuf, bool) {
    if !follow {
        return (path.to_path_buf(), false);
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => (target, true),
            // A dangling symlink cannot be canonicalized; read one hop by hand
            // so the write still lands on the intended (missing) target rather
            // than replacing the link.
            Err(_) => {
                let target = fs::read_link(path).ok().map(|t| {
                    if t.is_absolute() {
                        t
                    } else {
                        path.parent().unwrap_or_else(|| Path::new(".")).join(t)
                    }
                });
                (target.unwrap_or_else(|| path.to_path_buf()), true)
            }
        },
        _ => (path.to_path_buf(), false),
    }
}

/// Metadata captured right after a write, used to refresh the file cache.
pub struct WriteMeta {
    pub size: u64,
    pub mtime_ns: i128,
    /// Whether the file already existed before this write.
    pub existed: bool,
}

/// Persist `bytes` at `path` using the requested [`WriteMode`].
pub fn write_bytes(
    path: &Path,
    bytes: &[u8],
    create_dirs: bool,
    mode: WriteMode,
) -> Result<WriteMeta, ToolError> {
    match mode {
        WriteMode::Atomic => atomic_write(path, bytes, create_dirs),
        WriteMode::InPlace => inplace_write(path, bytes, create_dirs),
    }
}

fn ensure_parent(path: &Path, create_dirs: bool) -> Result<(), ToolError> {
    if !create_dirs {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).map_err(|e| io_err(e, path))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: write a sibling temp file, then rename
/// over the target so a concurrent reader never sees a partial file.
///
/// The target gets a **new inode**. Permissions are copied from the previous
/// file when there was one; owner, extended attributes, and any other hardlinks
/// to the old inode are not carried over — that is inherent to `rename(2)`, and
/// the reason [`WriteMode::InPlace`] exists.
pub fn atomic_write(path: &Path, bytes: &[u8], create_dirs: bool) -> Result<WriteMeta, ToolError> {
    ensure_parent(path, create_dirs)?;
    let previous = fs::metadata(path).ok();
    let existed = previous.is_some();

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
    // Carry the previous mode across so an atomic write does not silently make
    // a script non-executable or widen a private file.
    if let Some(meta) = &previous {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e, path));
    }

    let meta = fs::metadata(path).map_err(|e| io_err(e, path))?;
    Ok(WriteMeta { size: meta.len(), mtime_ns: mtime_nanos(&meta), existed })
}

/// Truncate and rewrite the existing inode, the semantics of `fs.writeFile`.
///
/// Preserves inode, mode, owner, extended attributes, and every hardlink — at
/// the cost of atomicity: a reader racing this write can observe a partial
/// file, and a crash mid-write leaves one.
pub fn inplace_write(path: &Path, bytes: &[u8], create_dirs: bool) -> Result<WriteMeta, ToolError> {
    ensure_parent(path, create_dirs)?;
    let existed = fs::metadata(path).is_ok();

    let res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    })();
    if let Err(e) = res {
        return Err(io_err(e, path));
    }

    let meta = fs::metadata(path).map_err(|e| io_err(e, path))?;
    Ok(WriteMeta { size: meta.len(), mtime_ns: mtime_nanos(&meta), existed })
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
