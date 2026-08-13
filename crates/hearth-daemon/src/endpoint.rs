use hearth_tools::transport::{ValidatedEndpoint, effective_uid};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static UMASK_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathIdentity {
    dev: u64,
    ino: u64,
}

impl PathIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        self == Self::from_metadata(metadata)
    }
}

/// Keeps the adjacent lock file open and exclusively locked for the complete
/// endpoint lifetime. The lock file is intentionally retained after shutdown:
/// unlinking a locked file would let another process lock a new inode.
struct LifetimeLock {
    _file: File,
}

impl LifetimeLock {
    fn acquire(endpoint: &Path) -> io::Result<Self> {
        let lock_path = lock_path(endpoint)?;
        let bytes = lock_path.as_os_str().as_bytes();
        let c_path = CString::new(bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "lock path contains a NUL byte")
        })?;
        // Create atomically so a restrictive process umask can be normalized
        // only for an inode we created. Existing lock files are opened without
        // O_CREAT and must already satisfy the strict metadata policy below.
        let (raw_fd, created) = unsafe {
            let fd = libc::open(
                c_path.as_ptr(),
                libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_RDWR,
                0o600,
            );
            if fd >= 0 {
                (fd, true)
            } else if io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
                (
                    libc::open(
                        c_path.as_ptr(),
                        libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_RDWR,
                    ),
                    false,
                )
            } else {
                (fd, false)
            }
        };
        if raw_fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `open` returned a fresh descriptor now owned by this scope.
        let file = unsafe { File::from_raw_fd(raw_fd) };
        if created && unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint lock must be an euid-owned mode-0600 regular file with one link",
            ));
        }

        // SAFETY: `file` owns a valid descriptor and `flock` only changes the
        // advisory lock associated with that open file description.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "endpoint lifetime lock is already held",
                ));
            }
            return Err(error);
        }
        let path_metadata = fs::symlink_metadata(&lock_path)?;
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint lock path changed while acquiring the lifetime lock",
            ));
        }
        Ok(Self { _file: file })
    }
}

fn lock_path(endpoint: &Path) -> io::Result<PathBuf> {
    let mut name = endpoint
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no basename"))?
        .to_os_string();
    name.push(".lock");
    Ok(endpoint.with_file_name(name))
}

struct EndpointCleanup {
    path: PathBuf,
    identity: PathIdentity,
    uid: u32,
    armed: bool,
    _lock: LifetimeLock,
}

impl EndpointCleanup {
    fn remove_if_same(&mut self) -> io::Result<bool> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.armed = false;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_socket()
            || metadata.uid() != self.uid
            || !self.identity.matches(&metadata)
        {
            // A replacement or ambiguous object is never removed.
            self.armed = false;
            return Ok(false);
        }
        fs::remove_file(&self.path)?;
        self.armed = false;
        Ok(true)
    }
}

impl Drop for EndpointCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.remove_if_same();
        }
    }
}

/// A listener paired with its lifetime lock and identity-conditioned cleanup.
pub(crate) struct BoundEndpoint {
    listener: Option<UnixListener>,
    cleanup: EndpointCleanup,
}

impl BoundEndpoint {
    pub(crate) fn bind(endpoint: &ValidatedEndpoint) -> io::Result<Self> {
        endpoint.ensure_parent_unchanged()?;
        // This must precede every inspection or removal of an existing path.
        let lock = LifetimeLock::acquire(endpoint.path())?;
        endpoint.ensure_parent_unchanged()?;
        inspect_and_remove_stale(endpoint, effective_uid())?;
        endpoint.ensure_parent_unchanged()?;

        let listener = bind_owner_only(endpoint.path())?;
        let metadata = fs::symlink_metadata(endpoint.path())?;
        if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bound endpoint path is not an euid-owned socket",
            ));
        }
        let cleanup = EndpointCleanup {
            path: endpoint.path().to_owned(),
            identity: PathIdentity::from_metadata(&metadata),
            uid: effective_uid(),
            armed: true,
            _lock: lock,
        };

        endpoint.ensure_parent_unchanged()?;
        let final_metadata = fs::symlink_metadata(endpoint.path())?;
        if !cleanup.identity.matches(&final_metadata)
            || !final_metadata.file_type().is_socket()
            || final_metadata.uid() != cleanup.uid
            || final_metadata.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint changed or has unsafe permissions after bind",
            ));
        }

        Ok(Self {
            listener: Some(listener),
            cleanup,
        })
    }

    pub(crate) fn listener(&self) -> &UnixListener {
        self.listener.as_ref().expect("listener is still admitting")
    }

    pub(crate) fn stop_admitting(&mut self) {
        self.listener.take();
    }

    pub(crate) fn cleanup(&mut self) -> io::Result<bool> {
        self.stop_admitting();
        self.cleanup.remove_if_same()
    }
}

impl Drop for BoundEndpoint {
    fn drop(&mut self) {
        // Close the listener before identity-conditioned path cleanup while the
        // adjacent lifetime lock is still held by `cleanup`.
        self.listener.take();
    }
}

fn bind_owner_only(path: &Path) -> io::Result<UnixListener> {
    // Unix bind normally creates a 0777 socket filtered by umask. Temporarily
    // applying 0177 before any daemon threads exist makes the initial inode
    // mode 0600, eliminating a permissive pre-chmod exposure window.
    let _process_guard = UMASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    struct UmaskGuard(libc::mode_t);
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            // SAFETY: restoring a value previously returned by `umask` is valid.
            unsafe { libc::umask(self.0) };
        }
    }
    // SAFETY: `umask` accepts every mode value and returns the prior mask.
    let _umask = UmaskGuard(unsafe { libc::umask(0o177) });
    UnixListener::bind(path)
}

fn inspect_and_remove_stale(endpoint: &ValidatedEndpoint, uid: u32) -> io::Result<()> {
    endpoint.ensure_parent_unchanged()?;
    let path = endpoint.path();
    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if initial.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "existing endpoint is owned by another UID",
        ));
    }
    if !initial.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "existing endpoint path is not a socket",
        ));
    }
    let identity = PathIdentity::from_metadata(&initial);

    match UnixStream::connect(path) {
        Ok(stream) => {
            drop(stream);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "existing endpoint socket is live",
            ));
        }
        Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("existing endpoint socket liveness is ambiguous: {error}"),
            ));
        }
    }

    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !current.file_type().is_socket() || current.uid() != uid || !identity.matches(&current) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint changed during stale-socket inspection",
        ));
    }
    endpoint.ensure_parent_unchanged()?;
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_tools::transport::validate_endpoint_path;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn private_endpoint() -> (TestDir, ValidatedEndpoint) {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let path = workspace.join(format!(".h{}-{sequence}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let root = TestDir(path);
        let canonical = root.0.canonicalize().unwrap();
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = validate_endpoint_path(&canonical.join("s")).unwrap();
        (root, endpoint)
    }

    fn filesystem_socket_bind_supported(endpoint: &ValidatedEndpoint) -> bool {
        let probe = endpoint.path().with_file_name("p");
        match UnixListener::bind(&probe) {
            Ok(listener) => {
                drop(listener);
                fs::remove_file(probe).unwrap();
                true
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("filesystem Unix socket bind denied by test sandbox; skipping");
                false
            }
            Err(error) => panic!("filesystem Unix socket bind preflight failed: {error}"),
        }
    }

    #[test]
    fn live_socket_is_never_removed() {
        let (_root, endpoint) = private_endpoint();
        if !filesystem_socket_bind_supported(&endpoint) {
            return;
        }
        let listener = UnixListener::bind(endpoint.path()).unwrap();
        let before = fs::symlink_metadata(endpoint.path()).unwrap();

        let error = BoundEndpoint::bind(&endpoint).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        let after = fs::symlink_metadata(endpoint.path()).unwrap();
        assert_eq!(
            PathIdentity::from_metadata(&before),
            PathIdentity::from_metadata(&after)
        );
        drop(listener);
    }

    #[test]
    fn non_socket_path_is_never_removed() {
        let (_root, endpoint) = private_endpoint();
        let mut file = File::create(endpoint.path()).unwrap();
        file.write_all(b"keep").unwrap();

        let error = BoundEndpoint::bind(&endpoint).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(endpoint.path()).unwrap(), b"keep");
    }

    #[test]
    fn lifetime_lock_is_exclusive_without_socket_binding() {
        let (_root, endpoint) = private_endpoint();
        let first = LifetimeLock::acquire(endpoint.path()).unwrap();
        let error = LifetimeLock::acquire(endpoint.path()).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);

        drop(first);
        LifetimeLock::acquire(endpoint.path()).unwrap();
    }

    #[test]
    fn stale_owned_socket_is_replaced_under_lock() {
        let (_root, endpoint) = private_endpoint();
        if !filesystem_socket_bind_supported(&endpoint) {
            return;
        }
        let stale = UnixListener::bind(endpoint.path()).unwrap();
        drop(stale);
        let stale_identity =
            PathIdentity::from_metadata(&fs::symlink_metadata(endpoint.path()).unwrap());

        let mut bound = BoundEndpoint::bind(&endpoint).unwrap();
        let current_identity =
            PathIdentity::from_metadata(&fs::symlink_metadata(endpoint.path()).unwrap());
        assert_ne!(stale_identity, current_identity);
        assert!(bound.cleanup().unwrap());
    }

    #[test]
    fn lifetime_lock_rejects_a_second_binder() {
        let (_root, endpoint) = private_endpoint();
        if !filesystem_socket_bind_supported(&endpoint) {
            return;
        }
        let mut first = BoundEndpoint::bind(&endpoint).unwrap();
        let error = BoundEndpoint::bind(&endpoint).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(first.cleanup().unwrap());
    }

    #[test]
    fn cleanup_preserves_a_replacement_path() {
        let (_root, endpoint) = private_endpoint();
        if !filesystem_socket_bind_supported(&endpoint) {
            return;
        }
        let mut bound = BoundEndpoint::bind(&endpoint).unwrap();
        let displaced = endpoint.path().with_extension("displaced");
        fs::rename(endpoint.path(), &displaced).unwrap();
        fs::write(endpoint.path(), b"replacement").unwrap();

        assert!(!bound.cleanup().unwrap());
        assert_eq!(fs::read(endpoint.path()).unwrap(), b"replacement");
    }

    #[test]
    fn cleanup_guard_preserves_changed_path_without_socket_binding() {
        let (_root, endpoint) = private_endpoint();
        fs::write(endpoint.path(), b"original").unwrap();
        let original = fs::symlink_metadata(endpoint.path()).unwrap();
        let lock = LifetimeLock::acquire(endpoint.path()).unwrap();
        let mut cleanup = EndpointCleanup {
            path: endpoint.path().to_owned(),
            identity: PathIdentity::from_metadata(&original),
            uid: effective_uid(),
            armed: true,
            _lock: lock,
        };
        let displaced = endpoint.path().with_extension("old");
        fs::rename(endpoint.path(), displaced).unwrap();
        fs::write(endpoint.path(), b"replacement").unwrap();

        assert!(!cleanup.remove_if_same().unwrap());
        assert_eq!(fs::read(endpoint.path()).unwrap(), b"replacement");
    }
}
