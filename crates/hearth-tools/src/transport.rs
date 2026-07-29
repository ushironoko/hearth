//! Length-prefixed msgpack framing shared by the daemon (server) and CLI/napi
//! (client). Each frame is a little-endian `u32` byte length followed by the
//! msgpack encoding of a [`Request`](hearth_proto::Request) or
//! [`Response`](hearth_proto::Response).

use hearth_proto::Request;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// The default daemon socket path, guarded against the platform's `sun_path`
/// length limit (~104 bytes on macOS). A long `$TMPDIR` (common on macOS, where
/// it lives under `/var/folders/…`) would otherwise fail to `bind`, so we fall
/// back to `/tmp`.
pub fn default_socket_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let name = format!("hearth-{uid}.sock");
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let candidate = base.join(&name);
    if candidate.as_os_str().len() < 100 {
        candidate
    } else {
        PathBuf::from("/tmp").join(&name)
    }
}

/// Maximum frame size (256 MiB) — a guard against a corrupt length prefix.
const MAX_FRAME: u32 = 256 * 1024 * 1024;

/// Encode and write one message as a length-prefixed msgpack frame.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let buf =
        rmp_serde::to_vec_named(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(buf.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&buf)?;
    w.flush()
}

/// Read one length-prefixed msgpack frame and decode it.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds maximum size",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    rmp_serde::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn errno_io(e: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Client → daemon: send a request, optionally attaching a file descriptor
/// (e.g. the client's stdout) via `SCM_RIGHTS` so the daemon can write results
/// straight to it. Without an fd this is just a plain framed write.
pub fn send_request_with_fd(
    stream: &UnixStream,
    req: &Request,
    fd: Option<RawFd>,
) -> io::Result<()> {
    let body =
        rmp_serde::to_vec_named(req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);

    match fd {
        Some(fd) => {
            let iov = [IoSlice::new(&frame)];
            let fds = [fd];
            let cmsgs = [ControlMessage::ScmRights(&fds)];
            // Requests are small (<16 KiB); one sendmsg delivers the whole frame.
            sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .map_err(errno_io)?;
            Ok(())
        }
        None => {
            let mut s = stream;
            s.write_all(&frame)?;
            s.flush()
        }
    }
}

/// Daemon side: receive one request frame plus any `SCM_RIGHTS` fd the client
/// attached. Works for both fd-carrying (`sendmsg`) and plain (`write`) clients.
pub fn recv_request(stream: &UnixStream) -> io::Result<(Request, Option<OwnedFd>)> {
    let raw = stream.as_raw_fd();
    let mut buf = vec![0u8; 16 * 1024];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
    let msg =
        recvmsg::<()>(raw, &mut iov, Some(&mut cmsg_space), MsgFlags::empty()).map_err(errno_io)?;
    let n = msg.bytes;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "client hung up",
        ));
    }

    // Take at most one received fd; close any extras defensively.
    let mut received: Option<OwnedFd> = None;
    for cmsg in msg.cmsgs().map_err(errno_io)? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            for fd in fds {
                if received.is_none() {
                    // SAFETY: fd is freshly received via SCM_RIGHTS; we own it.
                    received = Some(unsafe { OwnedFd::from_raw_fd(fd) });
                } else {
                    // SAFETY: dropping ownership of an extra received fd.
                    drop(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
    }
    // End `iov`'s mutable borrow of `buf` before the bytes are read back.
    let _ = iov;

    if n < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short request frame",
        ));
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len as u32 > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds maximum size",
        ));
    }
    let mut body = Vec::with_capacity(len);
    let have = (n - 4).min(len);
    body.extend_from_slice(&buf[4..4 + have]);
    // Rare: the frame spanned more than one recv — pull the remainder.
    if body.len() < len {
        let mut rest = vec![0u8; len - body.len()];
        let mut s = stream;
        s.read_exact(&mut rest)?;
        body.extend_from_slice(&rest);
    }
    let req =
        rmp_serde::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((req, received))
}
