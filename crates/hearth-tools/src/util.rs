//! Shared helpers for the tools: path resolution and the two write strategies.

use hearth_core::Engine;
use hearth_proto::{ToolError, WriteMode};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const TEMP_ATTEMPTS: usize = 128;

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
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| io_err(e, path))?;
    }
    Ok(())
}

/// An exclusively-created temporary file that removes only its own path if it
/// has not been published yet.
struct TempFile {
    path: PathBuf,
    file: Option<File>,
    dev: u64,
    ino: u64,
}

impl TempFile {
    fn create(dir: &Path, name: &str, mode: u32) -> std::io::Result<Self> {
        for _ in 0..TEMP_ATTEMPTS {
            let mut nonce = [0_u8; 16];
            getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
            let mut hex = String::with_capacity(32);
            for byte in nonce {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
            }
            let path = dir.join(format!(".{name}.hearth.{hex}.tmp"));
            let opened = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path);
            match opened {
                Ok(file) => {
                    let meta = file.metadata()?;
                    return Ok(Self {
                        path,
                        file: Some(file),
                        dev: meta.dev(),
                        ino: meta.ino(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary file",
        ))
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    fn is_same_path_identity(&self) -> bool {
        fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.dev() == self.dev && meta.ino() == self.ino)
    }

    fn publish(mut self, target: &Path) -> std::io::Result<()> {
        // Keep the descriptor open through rename. If the directory entry was
        // replaced despite the unpredictable name, fail closed rather than
        // publishing the attacker's object.
        if !self.is_same_path_identity() {
            return Err(std::io::Error::other(
                "temporary file identity changed before publication",
            ));
        }
        fs::rename(&self.path, target)?;
        self.path.clear();
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        drop(self.file.take());
        // Never unlink a replacement at the same pathname during error cleanup.
        if !self.path.as_os_str().is_empty() && self.is_same_path_identity() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Write `bytes` to `path` atomically: write an unpredictable, exclusively
/// created sibling temp file, then rename over the target so a concurrent
/// reader never sees a partial file.
///
/// The target gets a **new inode**. Permissions are copied from the previous
/// file when there was one; owner, extended attributes, and any other hardlinks
/// to the old inode are not carried over — that is inherent to `rename(2)`, and
/// the reason [`WriteMode::InPlace`] exists. A new target receives the normal
/// process-umask-derived creation mode, but the unpublished temp starts at 0600.
pub fn atomic_write(path: &Path, bytes: &[u8], create_dirs: bool) -> Result<WriteMeta, ToolError> {
    ensure_parent(path, create_dirs)?;
    let previous = fs::symlink_metadata(path).ok();
    let existed = previous.is_some();
    // A NoFollow write that replaces a symlink must not inherit permissions
    // from the link's old target. Symlink mode bits themselves are not a useful
    // regular-file creation mode.
    let previous_mode = previous
        .as_ref()
        .filter(|meta| !meta.file_type().is_symlink())
        .map(|meta| meta.permissions().mode() & 0o7777);

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut tmp = TempFile::create(dir, &name, 0o600).map_err(|e| io_err(e, path))?;
    tmp.file_mut()
        .write_all(bytes)
        .and_then(|_| tmp.file_mut().flush())
        .map_err(|e| io_err(e, path))?;

    // Carry the previous mode across so an atomic write does not silently make
    // a script non-executable or widen/narrow a private file. This is part of
    // the write contract, so failure must abort before publication.
    let final_mode = match previous_mode {
        Some(mode) => mode,
        None => creation_mode(dir, &name, 0o666).map_err(|e| io_err(e, path))?,
    };
    tmp.file_mut()
        .set_permissions(fs::Permissions::from_mode(final_mode))
        .map_err(|e| io_err(e, path))?;
    tmp.publish(path).map_err(|e| io_err(e, path))?;

    let meta = fs::metadata(path).map_err(|e| io_err(e, path))?;
    Ok(WriteMeta {
        size: meta.len(),
        mtime_ns: mtime_nanos(&meta),
        existed,
    })
}

/// Apply the process's current umask to `requested` without changing the
/// process-global umask (which would race every other file-creating thread).
/// An empty, unpredictable exclusive probe lets the kernel apply the mode; it
/// is identity-checked and removed immediately without ever holding content.
fn creation_mode(dir: &Path, name: &str, requested: u32) -> std::io::Result<u32> {
    let probe = TempFile::create(dir, &format!("{name}.mode"), requested)?;
    let mode = probe
        .file
        .as_ref()
        .expect("mode probe is open")
        .metadata()?
        .permissions()
        .mode()
        & 0o7777;
    drop(probe);
    Ok(mode)
}

/// Truncate and rewrite the existing inode, the semantics of `fs.writeFile`.
///
/// Preserves inode, mode, owner, extended attributes, and every hardlink — at
/// the cost of atomicity: a reader racing this write can observe a partial
/// file, and a crash mid-write leaves one. `O_NOFOLLOW` rejects a final symlink;
/// write-through callers resolve their intended target before this function.
pub fn inplace_write(path: &Path, bytes: &[u8], create_dirs: bool) -> Result<WriteMeta, ToolError> {
    ensure_parent(path, create_dirs)?;
    // InPlace can preserve only a regular target inode. In NoFollow mode a
    // final symlink is itself the object to replace, so publish a regular file
    // over the link atomically rather than following it or failing halfway.
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return atomic_write(path, bytes, false);
    }
    let existed = fs::metadata(path).is_ok();

    let res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o666)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = f.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "in-place write target must be a regular file",
            ));
        }
        f.set_len(0)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    })();
    if let Err(e) = res {
        return Err(io_err(e, path));
    }

    let meta = fs::metadata(path).map_err(|e| io_err(e, path))?;
    Ok(WriteMeta {
        size: meta.len(),
        mtime_ns: mtime_nanos(&meta),
        existed,
    })
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
