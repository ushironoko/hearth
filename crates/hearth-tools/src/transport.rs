//! Length-prefixed msgpack framing shared by the daemon (server) and CLI/napi
//! (client). Each frame is a little-endian `u32` byte length followed by the
//! msgpack encoding of a [`Request`](hearth_proto::Request) or
//! [`Response`](hearth_proto::Response).

use hearth_proto::Request;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, IoSlice, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const SOCKET_NAME: &str = "hearth.sock";
const PORTABLE_SOCKET_PATH_LIMIT: usize = 100;
const MAX_REQUEST_FRAME_DURATION: Duration = Duration::from_secs(30);
const MAX_DECODE_VALUES: usize = 1_000_000;
const MAX_DECODE_DEPTH: usize = 64;

#[derive(Clone, Debug)]
pub struct ValidatedEndpoint {
    path: PathBuf,
    parent_dev: u64,
    parent_ino: u64,
    uid: u32,
}

impl ValidatedEndpoint {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_parent_unchanged(&self) -> io::Result<()> {
        let metadata = validate_private_parent(
            self.path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no parent")
            })?,
            self.uid,
        )?;
        if metadata.dev() != self.parent_dev || metadata.ino() != self.parent_ino {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint parent changed after validation",
            ));
        }
        Ok(())
    }
}

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn default_runtime_dir() -> PathBuf {
    if cfg!(target_os = "linux")
        && let Some(dir) = validated_runtime_env("XDG_RUNTIME_DIR")
    {
        return dir.join("hearth");
    }
    // macOS exposes an euid-private, randomized temporary directory through
    // TMPDIR. Linux also commonly provides a private TMPDIR in managed
    // environments; both are safer than a predictable direct child of /tmp.
    if let Some(dir) = validated_runtime_env("TMPDIR") {
        return dir.join("hearth");
    }
    // No predictable shared-/tmp fallback: callers must supply an explicit
    // endpoint rooted in a private directory when the OS exposes no trusted
    // per-user runtime root.
    PathBuf::new()
}

fn validated_runtime_env(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return None;
    }
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return None;
    }
    fs::canonicalize(path).ok()
}

pub fn default_socket_path() -> PathBuf {
    default_runtime_dir().join(SOCKET_NAME)
}

pub fn prepare_default_endpoint() -> io::Result<ValidatedEndpoint> {
    let runtime_dir = default_runtime_dir();
    if runtime_dir.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no trusted per-user runtime directory; pass --socket under a private mode-0700 directory",
        ));
    }
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&runtime_dir) {
        Ok(()) => fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_endpoint_path(&runtime_dir.join(SOCKET_NAME))
}

pub fn validate_endpoint_path(path: &Path) -> io::Result<ValidatedEndpoint> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().len() >= PORTABLE_SOCKET_PATH_LIMIT
        || path.as_os_str().as_bytes().contains(&0)
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint path must be absolute, normalized, NUL-free, and shorter than 100 bytes",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no parent"))?;
    let uid = effective_uid();
    let metadata = validate_private_parent(parent, uid)?;
    Ok(ValidatedEndpoint {
        path: path.to_owned(),
        parent_dev: metadata.dev(),
        parent_ino: metadata.ino(),
        uid,
    })
}

fn validate_private_parent(parent: &Path, uid: u32) -> io::Result<fs::Metadata> {
    if fs::canonicalize(parent)? != parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint parent must be canonical and contain no symlink components",
        ));
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o7777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint parent must be an euid-owned mode-0700 directory",
        ));
    }
    Ok(metadata)
}

pub fn connect_verified(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    verify_peer_uid(&stream, effective_uid())?;
    Ok(stream)
}

pub fn verify_peer_uid(stream: &UnixStream, expected_uid: u32) -> io::Result<()> {
    let actual = peer_uid(stream)?;
    if actual != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Unix peer UID {actual} does not match expected UID {expected_uid}"),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credential = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credential.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result == -1 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { credential.assume_init() }.uid)
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix peer credentials are unsupported on this platform",
    ))
}

/// Maximum frame size (256 MiB) — a guard against a corrupt length prefix.
const MAX_FRAME: u32 = 256 * 1024 * 1024;
const READ_CHUNK: usize = 16 * 1024;
// XNU accepts at most 512 descriptors in one control message; Linux accepts
// fewer. Receiving the larger bound avoids losing an unnameable descriptor on
// platforms that do not close the part hidden by MSG_CTRUNC.
const RECEIVED_FD_CAPACITY: usize = 512;
const RECEIVED_FD_BYTES: usize = std::mem::size_of::<[RawFd; RECEIVED_FD_CAPACITY]>();
const CONTROL_SPACE: usize =
    unsafe { libc::CMSG_SPACE(RECEIVED_FD_BYTES as libc::c_uint) as usize };

fn frame_too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "frame exceeds maximum size")
}

fn validate_frame_len_with_limit(len: usize, limit: u32) -> io::Result<u32> {
    let len = u32::try_from(len).map_err(|_| frame_too_large())?;
    if len > limit {
        return Err(frame_too_large());
    }
    Ok(len)
}

fn validate_frame_len(len: usize) -> io::Result<u32> {
    validate_frame_len_with_limit(len, MAX_FRAME)
}

struct FrameWriter {
    frame: Vec<u8>,
    limit: u32,
}

impl FrameWriter {
    fn with_limit(limit: u32) -> Self {
        Self {
            frame: vec![0; 4],
            limit,
        }
    }

    fn finish(mut self) -> io::Result<Vec<u8>> {
        let len = validate_frame_len_with_limit(self.frame.len() - 4, self.limit)?;
        self.frame[..4].copy_from_slice(&len.to_le_bytes());
        Ok(self.frame)
    }
}

impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let payload_len = self.frame.len() - 4;
        let next_len = payload_len
            .checked_add(buf.len())
            .ok_or_else(frame_too_large)?;
        validate_frame_len_with_limit(next_len, self.limit)?;
        self.frame.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_frame<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    encode_frame_with_limit(msg, MAX_FRAME)
}

fn encode_frame_with_limit<T: Serialize>(msg: &T, limit: u32) -> io::Result<Vec<u8>> {
    let mut writer = FrameWriter::with_limit(limit);
    rmp_serde::encode::write_named(&mut writer, msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writer.finish()
}

/// Encode and write one message as a length-prefixed msgpack frame.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let frame = encode_frame(msg)?;
    w.write_all(&frame)?;
    w.flush()
}

/// Read one length-prefixed msgpack frame and decode it.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = validate_frame_len(u32::from_le_bytes(len_buf) as usize)?;
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    rmp_serde::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn validate_msgpack_structure(bytes: &[u8]) -> io::Result<()> {
    fn read_uint(bytes: &[u8], pos: &mut usize, width: usize) -> io::Result<u64> {
        let end = pos
            .checked_add(width)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated msgpack"))?;
        let mut value = 0u64;
        for &byte in &bytes[*pos..end] {
            value = (value << 8) | u64::from(byte);
        }
        *pos = end;
        Ok(value)
    }
    fn skip(bytes: &[u8], pos: &mut usize, len: u64) -> io::Result<()> {
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "msgpack length overflow"))?;
        *pos = pos
            .checked_add(len)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated msgpack"))?;
        Ok(())
    }

    let mut pos = 0usize;
    let mut values = 0usize;
    let mut stack = vec![1u64];
    while let Some(remaining) = stack.last_mut() {
        if *remaining == 0 {
            stack.pop();
            continue;
        }
        *remaining -= 1;
        values = values.saturating_add(1);
        if values > MAX_DECODE_VALUES || stack.len() > MAX_DECODE_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "msgpack structure exceeds decode budget",
            ));
        }
        let marker = *bytes
            .get(pos)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated msgpack"))?;
        pos += 1;
        match marker {
            0x00..=0x7f
            | 0xc0
            | 0xc2
            | 0xc3
            | 0xca
            | 0xcb
            | 0xcc
            | 0xcd
            | 0xce
            | 0xcf
            | 0xd0
            | 0xd1
            | 0xd2
            | 0xd3
            | 0xe0..=0xff => {
                let width = match marker {
                    0xca | 0xce | 0xd2 => 4,
                    0xcb | 0xcf | 0xd3 => 8,
                    0xcc | 0xd0 => 1,
                    0xcd | 0xd1 => 2,
                    _ => 0,
                };
                skip(bytes, &mut pos, width)?;
            }
            0xa0..=0xbf => skip(bytes, &mut pos, u64::from(marker & 0x1f))?,
            0x90..=0x9f => stack.push(u64::from(marker & 0x0f)),
            0x80..=0x8f => stack.push(u64::from(marker & 0x0f) * 2),
            0xc4 | 0xd9 => {
                let len = read_uint(bytes, &mut pos, 1)?;
                skip(bytes, &mut pos, len)?;
            }
            0xc5 | 0xda => {
                let len = read_uint(bytes, &mut pos, 2)?;
                skip(bytes, &mut pos, len)?;
            }
            0xc6 | 0xdb => {
                let len = read_uint(bytes, &mut pos, 4)?;
                skip(bytes, &mut pos, len)?;
            }
            0xdc => {
                let len = read_uint(bytes, &mut pos, 2)?;
                stack.push(len);
            }
            0xdd => {
                let len = read_uint(bytes, &mut pos, 4)?;
                stack.push(len);
            }
            0xde => {
                let len = read_uint(bytes, &mut pos, 2)?;
                stack.push(len.saturating_mul(2));
            }
            0xdf => {
                let len = read_uint(bytes, &mut pos, 4)?;
                stack.push(len.saturating_mul(2));
            }
            0xd4 => skip(bytes, &mut pos, 2)?,
            0xd5 => skip(bytes, &mut pos, 3)?,
            0xd6 => skip(bytes, &mut pos, 5)?,
            0xd7 => skip(bytes, &mut pos, 9)?,
            0xd8 => skip(bytes, &mut pos, 17)?,
            0xc7..=0xc9 => {
                let width = match marker {
                    0xc7 => 1,
                    0xc8 => 2,
                    _ => 4,
                };
                let len = read_uint(bytes, &mut pos, width)?;
                skip(bytes, &mut pos, len.saturating_add(1))?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported msgpack marker",
                ));
            }
        }
    }
    if pos != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing msgpack values",
        ));
    }
    Ok(())
}

fn errno_io(e: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

fn send_frame_with<F>(frame: &[u8], has_rights: bool, mut send: F) -> io::Result<()>
where
    F: FnMut(&[u8], bool) -> io::Result<usize>,
{
    let mut sent = 0;
    let mut rights_pending = has_rights;

    while sent < frame.len() {
        match send(&frame[sent..], rights_pending) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write the complete frame",
                ));
            }
            Ok(n) if n <= frame.len() - sent => {
                sent += n;
                rights_pending = false;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "send reported more bytes than supplied",
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

/// Client → daemon: send a request, optionally attaching a file descriptor
/// (e.g. the client's stdout) via `SCM_RIGHTS` so the daemon can write results
/// straight to it. An FD-bearing send uses the frame's first byte as an isolated
/// control slot. Rights are attached until that byte makes progress, then
/// omitted from the rest of the frame and all remaining short-send retries.
pub fn send_request_with_fd(
    stream: &UnixStream,
    req: &Request,
    fd: Option<RawFd>,
) -> io::Result<()> {
    let frame = encode_frame(req)?;

    match fd {
        Some(fd) => send_frame_with(&frame, true, |remaining, attach_rights| {
            let outgoing = if attach_rights {
                &remaining[..1]
            } else {
                remaining
            };
            let iov = [IoSlice::new(outgoing)];
            if attach_rights {
                let fds = [fd];
                let cmsgs = [ControlMessage::ScmRights(&fds)];
                sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                    .map_err(errno_io)
            } else {
                sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
                    .map_err(errno_io)
            }
        }),
        None => {
            let mut writer = stream;
            send_frame_with(&frame, false, |remaining, _| writer.write(remaining))?;
            writer.flush()
        }
    }
}

#[derive(Default)]
struct AncillaryBatch {
    present: bool,
    unexpected: bool,
    malformed: bool,
    rights: Vec<OwnedFd>,
}

struct AnchoredAncillary {
    offset: u64,
    batch: AncillaryBatch,
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn recv_cloexec_flag() -> libc::c_int {
    libc::MSG_CMSG_CLOEXEC
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn recv_cloexec_flag() -> libc::c_int {
    0
}

#[repr(C)]
union ControlBuffer {
    _alignment: std::mem::ManuallyDrop<libc::cmsghdr>,
    bytes: [u8; CONTROL_SPACE],
}

impl ControlBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; CONTROL_SPACE],
        }
    }

    fn as_mut_bytes(&mut self) -> &mut [u8; CONTROL_SPACE] {
        // SAFETY: `new` initializes the bytes field, and the kernel only writes
        // byte representations into it. The union gives those bytes the
        // alignment required by `cmsghdr` without reading a header directly.
        unsafe { &mut self.bytes }
    }
}

fn raw_recvmsg(
    fd: RawFd,
    data: &mut [u8],
    control: &mut [u8],
    flags: libc::c_int,
) -> io::Result<(usize, libc::msghdr)> {
    loop {
        control.fill(0);
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        // SAFETY: Zero is a valid initialization for msghdr; all non-null
        // pointers below remain valid for the duration of recvmsg.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control.len() as _;

        // SAFETY: header points to writable data/iovec/control buffers whose
        // lengths are recorded in the header.
        let received = unsafe { libc::recvmsg(fd, &mut header, flags) };
        if received >= 0 {
            // The parser only needs the control fields. Avoid returning a
            // dangling pointer to the stack-local iovec.
            header.msg_iov = std::ptr::null_mut();
            header.msg_iovlen = 0;
            return Ok((received as usize, header));
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn recvmsg_with_cloexec(
    fd: RawFd,
    data: &mut [u8],
    control: &mut [u8],
) -> io::Result<(usize, libc::msghdr)> {
    let flags = recv_cloexec_flag();
    match raw_recvmsg(fd, data, control, flags) {
        Err(err) if flags != 0 && err.raw_os_error() == Some(libc::EINVAL) => {
            raw_recvmsg(fd, data, control, 0)
        }
        result => result,
    }
}

fn ancillary_len_to_usize<T>(value: T) -> Option<usize>
where
    usize: TryFrom<T>,
{
    usize::try_from(value).ok()
}

fn parse_ancillary(control: &[u8], header: &libc::msghdr) -> AncillaryBatch {
    let mut batch = AncillaryBatch::default();

    let Some(reported_len) = ancillary_len_to_usize(header.msg_controllen) else {
        batch.malformed = true;
        return batch;
    };
    if reported_len == 0 {
        return batch;
    }
    batch.present = true;

    if header.msg_control.cast_const().cast::<u8>() != control.as_ptr() {
        batch.malformed = true;
        return batch;
    }
    if reported_len > control.len() {
        batch.malformed = true;
    }
    let visible_len = reported_len.min(control.len());
    let header_size = std::mem::size_of::<libc::cmsghdr>();
    let minimum_len = unsafe { libc::CMSG_LEN(0) as usize };
    if minimum_len < header_size {
        batch.malformed = true;
        return batch;
    }

    let mut offset = 0usize;
    while offset < visible_len {
        let Some(header_end) = offset.checked_add(header_size) else {
            batch.malformed = true;
            break;
        };
        if header_end > visible_len {
            batch.malformed = true;
            break;
        }

        // SAFETY: A complete header lies in `control`. `read_unaligned` keeps
        // malformed or platform-specific alignment from becoming UB.
        let cmsg = unsafe {
            std::ptr::read_unaligned(control.as_ptr().add(offset).cast::<libc::cmsghdr>())
        };
        let Some(cmsg_len) = ancillary_len_to_usize(cmsg.cmsg_len) else {
            batch.malformed = true;
            break;
        };
        if cmsg_len < minimum_len {
            batch.malformed = true;
            break;
        }
        let Some(declared_end) = offset.checked_add(cmsg_len) else {
            batch.malformed = true;
            break;
        };
        let visible_end = declared_end.min(visible_len);
        let Some(payload_start) = offset.checked_add(minimum_len) else {
            batch.malformed = true;
            break;
        };
        if payload_start > visible_end {
            batch.malformed = true;
            break;
        }

        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            // When MSG_CTRUNC is set, Darwin can retain the sender's original
            // cmsg_len even though only a prefix of its descriptors fits. Own
            // every complete RawFd visible in the buffer before rejecting it.
            let payload_len = visible_end - payload_start;
            if payload_len % std::mem::size_of::<RawFd>() != 0 {
                batch.malformed = true;
            }
            let count = payload_len / std::mem::size_of::<RawFd>();
            for index in 0..count {
                // SAFETY: Each complete RawFd lies within the payload checked
                // above. Each descriptor returned by SCM_RIGHTS is newly owned.
                let raw = unsafe {
                    std::ptr::read_unaligned(
                        control
                            .as_ptr()
                            .add(payload_start + index * std::mem::size_of::<RawFd>())
                            .cast::<RawFd>(),
                    )
                };
                if raw < 0 {
                    batch.malformed = true;
                } else {
                    // SAFETY: Ownership of every received descriptor transfers
                    // from the kernel to this process exactly once.
                    batch.rights.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        } else {
            batch.unexpected = true;
        }

        if declared_end > visible_len {
            batch.malformed = true;
            break;
        }

        let payload_len = cmsg_len - minimum_len;
        let Ok(payload_len) = libc::c_uint::try_from(payload_len) else {
            batch.malformed = true;
            break;
        };
        let occupied = unsafe { libc::CMSG_SPACE(payload_len) as usize };
        if occupied < cmsg_len {
            batch.malformed = true;
            break;
        }
        let Some(next) = offset.checked_add(occupied) else {
            batch.malformed = true;
            break;
        };
        if next <= offset {
            batch.malformed = true;
            break;
        }
        if next > visible_len {
            if declared_end != visible_len {
                batch.malformed = true;
            }
            break;
        }
        offset = next;
    }

    batch
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = loop {
        // SAFETY: F_GETFD only inspects the live descriptor.
        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if result >= 0 {
            break result;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };

    if flags & libc::FD_CLOEXEC != 0 {
        return Ok(());
    }

    loop {
        // SAFETY: F_SETFD changes only descriptor flags on the live fd.
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if result >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn validate_received_ancillary(
    batch: AncillaryBatch,
    truncated: bool,
) -> io::Result<AncillaryBatch> {
    let mut cloexec_error = None;
    for received_fd in &batch.rights {
        if let Err(err) = set_cloexec(received_fd.as_raw_fd())
            && cloexec_error.is_none()
        {
            cloexec_error = Some(err);
        }
    }

    if truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ancillary data was truncated",
        ));
    }
    if batch.malformed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed ancillary data",
        ));
    }
    if let Some(err) = cloexec_error {
        return Err(io::Error::new(
            err.kind(),
            format!("failed to set close-on-exec on received fd: {err}"),
        ));
    }

    Ok(batch)
}

fn receive_chunk(fd: RawFd, data: &mut [u8]) -> io::Result<(usize, AncillaryBatch)> {
    let mut control = ControlBuffer::new();
    let control = control.as_mut_bytes();
    let (received, header) = recvmsg_with_cloexec(fd, data, control)?;
    let truncated = header.msg_flags & libc::MSG_CTRUNC != 0;
    let batch = parse_ancillary(control, &header);
    let batch = validate_received_ancillary(batch, truncated)?;
    Ok((received, batch))
}

/// Connection-owned sequential request receiver.
///
/// Every frame's first byte is isolated in its own `recvmsg` call. This is
/// required on Linux, where control data can be returned with ordinary stream
/// bytes queued before the byte that carried it. Exact per-frame reads keep
/// that ambiguity from assigning `SCM_RIGHTS` to an earlier frame and let us
/// reject rights sent after a frame's first byte. Dropping the receiver closes
/// every descriptor that has not crossed a successful `recv_request` boundary.
pub struct RequestReceiver<'a> {
    stream: &'a UnixStream,
    max_frame: u32,
    bytes: Vec<u8>,
    start: usize,
    base_offset: u64,
    ancillary: VecDeque<AnchoredAncillary>,
    poisoned: bool,
    request_deadline: Option<Instant>,
}

impl<'a> RequestReceiver<'a> {
    pub fn new(stream: &'a UnixStream) -> Self {
        Self::with_options(stream, MAX_FRAME)
    }

    /// A receiver with a stricter per-frame limit, used by small fixed-shape
    /// control lanes that must not inherit the 256 MiB data-plane allowance.
    pub fn with_max_frame(stream: &'a UnixStream, max_frame: u32) -> Self {
        Self::with_options(stream, max_frame.min(MAX_FRAME))
    }

    fn with_options(stream: &'a UnixStream, max_frame: u32) -> Self {
        Self {
            stream,
            max_frame,
            bytes: Vec::new(),
            start: 0,
            base_offset: 0,
            ancillary: VecDeque::new(),
            poisoned: false,
            request_deadline: None,
        }
    }

    /// Receive and decode the next sequential request from this connection.
    pub fn recv_request(&mut self) -> io::Result<(Request, Option<OwnedFd>)> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request receiver is unusable after a previous error",
            ));
        }

        self.request_deadline = Instant::now().checked_add(MAX_REQUEST_FRAME_DURATION);
        let result = self.recv_request_inner();
        self.request_deadline = None;
        match result {
            Ok(request) => Ok(request),
            Err(err) => {
                self.poison();
                Err(err)
            }
        }
    }

    fn recv_request_inner(&mut self) -> io::Result<(Request, Option<OwnedFd>)> {
        // Do not combine the first byte with any later stream data. Linux may
        // report SCM_RIGHTS together with bytes queued before the sendmsg that
        // carried it, so a larger first read cannot identify its true offset.
        self.ensure_available(1, true)?;
        self.ensure_available(4, false)?;
        let prefix = &self.bytes[self.start..self.start + 4];
        let len = validate_frame_len_with_limit(
            u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize,
            self.max_frame,
        )?;
        let frame_len = 4usize
            .checked_add(len as usize)
            .ok_or_else(frame_too_large)?;
        let available = self.bytes.len() - self.start;
        if frame_len > available {
            self.bytes
                .try_reserve_exact(frame_len - available)
                .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
        }
        self.ensure_available(frame_len, false)?;

        let frame_start = self
            .base_offset
            .checked_add(self.start as u64)
            .ok_or_else(|| io::Error::other("stream offset overflow"))?;
        let frame_end = frame_start
            .checked_add(frame_len as u64)
            .ok_or_else(|| io::Error::other("stream offset overflow"))?;
        let mut frame_ancillary = AncillaryBatch::default();

        while self
            .ancillary
            .front()
            .is_some_and(|anchored| anchored.offset < frame_end)
        {
            let mut anchored = self.ancillary.pop_front().expect("front checked above");
            if anchored.offset != frame_start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ancillary data arrived after the frame's first byte",
                ));
            }
            frame_ancillary.present |= anchored.batch.present;
            frame_ancillary.unexpected |= anchored.batch.unexpected;
            frame_ancillary.malformed |= anchored.batch.malformed;
            frame_ancillary.rights.append(&mut anchored.batch.rights);
        }

        if frame_ancillary.unexpected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected ancillary control message",
            ));
        }
        if frame_ancillary.malformed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed ancillary data",
            ));
        }

        let body_start = self.start + 4;
        let body_end = self.start + frame_len;
        validate_msgpack_structure(&self.bytes[body_start..body_end])?;
        let request: Request = rmp_serde::from_slice(&self.bytes[body_start..body_end])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let received = if frame_ancillary.present {
            if frame_ancillary.rights.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS must contain exactly one file descriptor",
                ));
            }
            if !matches!(request, Request::Read(_)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS is only valid for read requests",
                ));
            }
            frame_ancillary.rights.pop()
        } else {
            None
        };

        self.consume(frame_len)?;
        Ok((request, received))
    }

    fn ensure_available(&mut self, needed: usize, ancillary_allowed: bool) -> io::Result<()> {
        while self.bytes.len() - self.start < needed {
            if self
                .request_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request frame deadline expired",
                ));
            }
            let missing = needed - (self.bytes.len() - self.start);
            // Never cross the currently known boundary. Besides avoiding
            // unbounded read-ahead, this preserves the first-byte ancillary
            // association established by `recv_request_inner`.
            let read_len = missing.min(READ_CHUNK);
            let mut chunk = [0u8; READ_CHUNK];
            let anchor = self
                .base_offset
                .checked_add(self.bytes.len() as u64)
                .ok_or_else(|| io::Error::other("stream offset overflow"))?;
            let (received, batch) = receive_chunk(self.stream.as_raw_fd(), &mut chunk[..read_len])?;
            if received == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client hung up",
                ));
            }
            let spare = self.bytes.capacity().saturating_sub(self.bytes.len());
            if spare < received {
                self.bytes
                    .try_reserve_exact(received - spare)
                    .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
            }
            self.bytes.extend_from_slice(&chunk[..received]);
            if batch.present {
                if !ancillary_allowed {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ancillary data arrived after the frame's first byte",
                    ));
                }
                if batch.unexpected || batch.malformed || batch.rights.len() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid ancillary data at the frame's first byte",
                    ));
                }
                self.ancillary.push_back(AnchoredAncillary {
                    offset: anchor,
                    batch,
                });
            }
        }
        Ok(())
    }

    fn consume(&mut self, consumed: usize) -> io::Result<()> {
        self.start += consumed;
        if self.start == self.bytes.len() {
            self.base_offset = self
                .base_offset
                .checked_add(self.start as u64)
                .ok_or_else(|| io::Error::other("stream offset overflow"))?;
            self.bytes.clear();
            if self.bytes.capacity() > READ_CHUNK * 4 {
                self.bytes.shrink_to(READ_CHUNK * 2);
            }
            self.start = 0;
        } else if self.start >= READ_CHUNK && self.start >= self.bytes.len() / 2 {
            self.base_offset = self
                .base_offset
                .checked_add(self.start as u64)
                .ok_or_else(|| io::Error::other("stream offset overflow"))?;
            self.bytes.drain(..self.start);
            self.start = 0;
        }
        Ok(())
    }

    fn poison(&mut self) {
        self.poisoned = true;
        self.bytes = Vec::new();
        self.start = 0;
        self.ancillary.clear();
    }
}

/// Compatibility adapter for callers that receive only one request.
///
/// Exact per-frame reads ensure that constructing a new receiver cannot consume
/// bytes belonging to a later frame. Multi-request connections should still
/// keep one [`RequestReceiver`] for their entire lifetime.
pub fn recv_request(stream: &UnixStream) -> io::Result<(Request, Option<OwnedFd>)> {
    RequestReceiver::with_options(stream, MAX_FRAME).recv_request()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_proto::ReadParams;
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    use std::io::Cursor;
    use std::os::fd::{AsFd, BorrowedFd};

    fn send_raw(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
        let iov = [IoSlice::new(bytes)];
        let sent = if fds.is_empty() {
            sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
        } else {
            let cmsgs = [ControlMessage::ScmRights(fds)];
            sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        }
        .unwrap();
        assert_eq!(sent, bytes.len());
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: fds points to storage for the two descriptors created by pipe.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe transferred ownership of both fresh descriptors.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn assert_pipe_eof(read: &OwnedFd) {
        // A leaked received descriptor must fail quickly instead of hanging
        // the test process in a blocking read.
        let flags = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed: {}", io::Error::last_os_error());
        assert_eq!(
            unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0,
            "F_SETFL failed: {}",
            io::Error::last_os_error()
        );
        let mut byte = [0u8; 1];
        // SAFETY: `byte` is writable and `read` owns a live descriptor.
        let received = unsafe { libc::read(read.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
        assert_eq!(
            received,
            0,
            "received write descriptor is still open: {}",
            io::Error::last_os_error()
        );
    }

    fn read_request() -> Request {
        Request::Read(ReadParams::new("test"))
    }

    #[test]
    fn sequential_receiver_preserves_consecutive_frames() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let first = encode_frame(&Request::Ping).unwrap();
        let second = encode_frame(&Request::Stats).unwrap();
        sender.write_all(&[first, second].concat()).unwrap();

        let mut requests = RequestReceiver::new(&receiver);
        assert!(matches!(requests.recv_request().unwrap().0, Request::Ping));
        assert!(matches!(requests.recv_request().unwrap().0, Request::Stats));
    }

    #[test]
    fn rights_follow_the_frame_that_starts_at_the_recvmsg_boundary() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let first = encode_frame(&Request::Ping).unwrap();
        let second = encode_frame(&read_request()).unwrap();
        let (_read, write) = pipe();

        sender.write_all(&first).unwrap();
        send_raw(&sender, &second, &[write.as_raw_fd()]);

        let mut requests = RequestReceiver::new(&receiver);
        let (first_request, first_fd) = requests.recv_request().unwrap();
        assert!(matches!(first_request, Request::Ping));
        assert!(first_fd.is_none());
        let (second_request, second_fd) = requests.recv_request().unwrap();
        assert!(matches!(second_request, Request::Read(_)));
        assert!(second_fd.is_some());
    }

    #[test]
    fn dropping_receiver_closes_rights_received_with_a_partial_frame() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&read_request()).unwrap();
        let (read, write) = pipe();
        send_raw(&sender, &frame, &[write.as_raw_fd()]);
        drop(write);

        let mut requests = RequestReceiver::new(&receiver);
        requests.ensure_available(1, true).unwrap();
        drop(requests);
        assert_pipe_eof(&read);
    }

    #[test]
    fn rights_on_a_multi_frame_send_belong_only_to_the_first_frame() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let first = encode_frame(&read_request()).unwrap();
        let second = encode_frame(&Request::Ping).unwrap();
        let (_read, write) = pipe();
        send_raw(&sender, &[first, second].concat(), &[write.as_raw_fd()]);

        let mut requests = RequestReceiver::new(&receiver);
        assert!(requests.recv_request().unwrap().1.is_some());
        let (second_request, second_fd) = requests.recv_request().unwrap();
        assert!(matches!(second_request, Request::Ping));
        assert!(second_fd.is_none());
    }

    #[test]
    fn rights_arriving_mid_frame_are_rejected_and_closed() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&read_request()).unwrap();
        let (read, write) = pipe();
        sender.write_all(&frame[..2]).unwrap();
        send_raw(&sender, &frame[2..], &[write.as_raw_fd()]);
        drop(write);

        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn mid_frame_rights_are_rejected_before_waiting_for_the_body() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let prefix = 1024u32.to_le_bytes();
        let (read, write) = pipe();
        sender.write_all(&prefix[..1]).unwrap();
        send_raw(&sender, &prefix[1..], &[write.as_raw_fd()]);
        drop(write);

        // No body is sent. A receiver that queues the illegal FD until frame
        // completion would time out instead of rejecting it immediately.
        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn rights_at_the_start_of_a_body_are_rejected_and_closed() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&read_request()).unwrap();
        let (read, write) = pipe();
        sender.write_all(&frame[..4]).unwrap();
        send_raw(&sender, &frame[4..], &[write.as_raw_fd()]);
        drop(write);

        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn unexpected_rights_are_rejected_and_closed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&Request::Ping).unwrap();
        let (read, write) = pipe();
        send_raw(&sender, &frame, &[write.as_raw_fd()]);
        drop(write);

        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn wrong_count_rights_are_rejected_and_closed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&read_request()).unwrap();
        let (read, write) = pipe();
        send_raw(&sender, &frame, &[write.as_raw_fd(), write.as_raw_fd()]);
        drop(write);

        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn truncated_rights_are_rejected_and_all_visible_fds_are_closed() {
        let (read, write) = pipe();
        // Inject the post-parse state directly: a real undersized recvmsg can
        // make descriptors outside the visible prefix impossible to identify
        // on some kernels. Production therefore sizes for the platform bound.
        // SAFETY: `write` owns a live descriptor.
        let duplicate = unsafe { libc::dup(write.as_raw_fd()) };
        assert!(duplicate >= 0);
        let batch = AncillaryBatch {
            present: true,
            // SAFETY: successful dup returns a new descriptor owned here.
            rights: vec![unsafe { OwnedFd::from_raw_fd(duplicate) }],
            ..AncillaryBatch::default()
        };
        drop(write);

        let err = match validate_received_ancillary(batch, true) {
            Ok(_) => panic!("truncated ancillary data was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("truncated"));
        assert_pipe_eof(&read);
    }

    #[test]
    fn ancillary_parser_handles_unaligned_control_and_rejects_bad_lengths() {
        let minimum_len = unsafe { libc::CMSG_LEN(0) as usize };
        let mut storage = vec![0u8; minimum_len + 1];
        let control = &mut storage[1..];
        let mut cmsg: libc::cmsghdr = unsafe { std::mem::zeroed() };
        cmsg.cmsg_len = minimum_len as _;
        cmsg.cmsg_level = libc::SOL_SOCKET;
        cmsg.cmsg_type = -1;
        // SAFETY: `control` has room for a complete, deliberately unaligned
        // cmsghdr representation.
        unsafe { std::ptr::write_unaligned(control.as_mut_ptr().cast(), cmsg) };

        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = minimum_len as _;
        let batch = parse_ancillary(control, &header);
        assert!(batch.present);
        assert!(batch.unexpected);
        assert!(!batch.malformed);

        cmsg.cmsg_len = (minimum_len - 1) as _;
        // SAFETY: same complete cmsghdr storage as above.
        unsafe { std::ptr::write_unaligned(control.as_mut_ptr().cast(), cmsg) };
        let batch = parse_ancillary(control, &header);
        assert!(batch.malformed);
    }

    #[test]
    fn received_fd_has_close_on_exec() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let frame = encode_frame(&read_request()).unwrap();
        let (_read, write) = pipe();
        send_raw(&sender, &frame, &[write.as_raw_fd()]);

        let received = RequestReceiver::new(&receiver)
            .recv_request()
            .unwrap()
            .1
            .unwrap();
        // SAFETY: F_GETFD only inspects the live descriptor.
        let flags = unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn public_fd_sender_uses_the_frame_control_slot() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (_read, write) = pipe();
        send_request_with_fd(&sender, &read_request(), Some(write.as_raw_fd())).unwrap();

        let (request, received) = RequestReceiver::new(&receiver).recv_request().unwrap();
        assert!(matches!(request, Request::Read(_)));
        assert!(received.is_some());
    }

    #[test]
    fn malformed_payload_closes_attached_fd() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let frame = [1, 0, 0, 0, 0xc1];
        let (read, write) = pipe();
        send_raw(&sender, &frame, &[write.as_raw_fd()]);
        drop(write);

        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_pipe_eof(&read);
    }

    #[test]
    fn send_retries_rights_until_first_progress_then_omits_them() {
        let frame = b"abcdef";
        let mut calls = Vec::new();
        let mut attempt = 0;
        send_frame_with(frame, true, |remaining, rights| {
            calls.push((remaining.len(), rights));
            attempt += 1;
            match attempt {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => Ok(2),
                3 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                _ => Ok(remaining.len()),
            }
        })
        .unwrap();

        assert_eq!(calls, vec![(6, true), (6, true), (4, false), (4, false)]);
    }

    #[test]
    fn send_reports_write_zero() {
        let err = send_frame_with(b"frame", true, |_, rights| {
            assert!(rights);
            Ok(0)
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    }

    #[test]
    fn cap_boundary_helpers_and_decoders_are_consistent() {
        assert_eq!(validate_frame_len(MAX_FRAME as usize).unwrap(), MAX_FRAME);
        assert_eq!(
            validate_frame_len(MAX_FRAME as usize + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut oversized = Cursor::new((MAX_FRAME + 1).to_le_bytes());
        let err = read_msg::<_, Request>(&mut oversized).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(&(MAX_FRAME + 1).to_le_bytes()).unwrap();
        let err = RequestReceiver::new(&receiver).recv_request().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let err = encode_frame_with_limit(&Request::Ping, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn receiver_releases_oversized_capacity_after_frame() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let request = Request::Stats;
        let frame = encode_frame(&request).unwrap();
        sender.write_all(&frame).unwrap();

        let mut requests = RequestReceiver::new(&receiver);
        requests.bytes.reserve(READ_CHUNK * 16);
        assert!(requests.bytes.capacity() > READ_CHUNK * 4);
        assert!(matches!(requests.recv_request().unwrap().0, Request::Stats));
        assert!(requests.bytes.capacity() <= READ_CHUNK * 2);
    }

    #[test]
    fn compatibility_receiver_does_not_consume_the_next_frame() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let first = encode_frame(&Request::Ping).unwrap();
        let second = encode_frame(&Request::Stats).unwrap();
        sender.write_all(&[first, second].concat()).unwrap();

        assert!(matches!(recv_request(&receiver).unwrap().0, Request::Ping));
        assert!(matches!(recv_request(&receiver).unwrap().0, Request::Stats));
    }

    #[test]
    fn borrowed_fd_trait_is_available_for_received_ownership() {
        let (read, _write) = pipe();
        let borrowed: BorrowedFd<'_> = read.as_fd();
        assert_eq!(borrowed.as_raw_fd(), read.as_raw_fd());
    }
}
