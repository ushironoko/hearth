//! `hearthd` — the resident Hearth daemon.
//!
//! Constructs one [`Engine`] (with its warm caches, profiler, and optimizer)
//! and serves length-prefixed msgpack requests over a Unix domain socket. One
//! thread per connection; the engine is a cheap `Arc` clone shared across them.

use clap::Parser;
use hearth_core::{Engine, EngineConfig};
use hearth_proto::{ReadParams, Request, Response, StreamedResult, ToolError};
use hearth_tools::dispatch;
use hearth_tools::transport::{recv_request, write_msg};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL: hearth_core::profiler::ProfilingAllocator<mimalloc::MiMalloc> =
    hearth_core::profiler::ProfilingAllocator::from_allocator(mimalloc::MiMalloc);

#[derive(Parser, Debug)]
#[command(name = "hearthd", about = "Resident Hearth tool orchestrator daemon")]
struct Args {
    /// Unix socket path to listen on.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Default working directory for tools.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Enable the fs-watcher for proactive cache invalidation.
    #[arg(long)]
    watch: bool,
    /// Skip the freshness stat on warm hits (single-writer / bounded-staleness
    /// fast path; no watcher). Use only when Hearth owns the workspace.
    #[arg(long)]
    trust_cache: bool,
    /// Use the pooled warm-shell fast path for `bash` (opt-in; falls back to a
    /// fresh spawn on any anomaly).
    #[arg(long)]
    warm_shell: bool,
    /// Disable the background self-optimization loop.
    #[arg(long)]
    no_optimizer: bool,
    /// Enable the profiler at startup.
    #[arg(long)]
    profile: bool,
}

fn main() {
    let args = Args::parse();
    let socket = args
        .socket
        .unwrap_or_else(hearth_tools::transport::default_socket_path);

    // Clear any stale socket from a previous (crashed) run.
    let _ = std::fs::remove_file(&socket);

    let mut cfg = EngineConfig::default();
    if let Some(cwd) = args.cwd {
        cfg.default_cwd = cwd;
    }
    cfg.enable_watch = args.watch;
    cfg.trust_cache = args.trust_cache;
    cfg.warm_shell = args.warm_shell;
    cfg.enable_optimizer = !args.no_optimizer;
    let engine = Engine::new(cfg);

    if args.profile {
        hearth_core::profiler::global_profiler().enable();
    }

    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hearthd: failed to bind {}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    eprintln!("hearthd: listening on {}", socket.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = engine.clone();
                let socket = socket.clone();
                std::thread::spawn(move || handle_conn(stream, engine, socket));
            }
            Err(e) => eprintln!("hearthd: accept error: {e}"),
        }
    }
}

fn handle_conn(mut stream: UnixStream, engine: Engine, socket: PathBuf) {
    loop {
        let (req, fd) = match recv_request(&stream) {
            Ok(v) => v,
            Err(_) => break, // client hung up
        };

        // Zero-copy fast path: if the client passed its stdout fd with a Read,
        // write the cached content straight to that fd and return only metadata.
        if let (Some(fd), Request::Read(p)) = (fd.as_ref(), &req) {
            let resp = stream_read(&engine, p, fd);
            if write_msg(&mut stream, &resp).is_err() {
                break;
            }
            continue;
        }

        let is_shutdown = matches!(req, Request::Shutdown);
        let resp = dispatch(&engine, req);
        if write_msg(&mut stream, &resp).is_err() {
            break;
        }
        if is_shutdown {
            let _ = std::fs::remove_file(&socket);
            std::process::exit(0);
        }
    }
}

/// Run `read`, then write its content directly to the client-supplied fd.
fn stream_read(engine: &Engine, params: &ReadParams, fd: &OwnedFd) -> Response {
    match hearth_tools::read(engine, params) {
        Ok(r) => match write_all_fd(fd.as_raw_fd(), r.content.as_bytes()) {
            Ok(()) => Response::Streamed(StreamedResult {
                bytes_written: r.content.len() as u64,
                total_lines: r.total_lines,
            }),
            Err(e) => Response::Error(ToolError::from(e)),
        },
        Err(e) => Response::Error(e),
    }
}

/// Write all bytes to a raw fd without taking ownership (so it is not closed).
fn write_all_fd(fd: i32, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const std::ffi::c_void, bytes.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}
