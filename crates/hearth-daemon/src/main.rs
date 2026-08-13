//! `hearthd` — the resident Hearth daemon.
//!
//! Constructs one [`Engine`] (with its warm caches, profiler, and optimizer)
//! and serves length-prefixed msgpack requests over a Unix domain socket. The
//! synchronous thread-per-connection model is retained behind a hard ceiling.

mod endpoint;

use clap::Parser;
use endpoint::BoundEndpoint;
use hearth_core::{CancelToken, Engine, EngineConfig};
use hearth_proto::{ReadParams, Request, Response, StreamedResult, ToolError};
use hearth_tools::dispatch_cancellable;
use hearth_tools::transport::{
    effective_uid, prepare_default_endpoint, validate_endpoint_path, verify_peer_uid, write_msg,
};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEFAULT_MAX_CONNECTIONS: usize = 64;
const MAX_CONNECTIONS: usize = 1024;
const DEFAULT_MAX_IN_FLIGHT_FRAME_BYTES: usize = 512 * 1024 * 1024;
const MIN_FRAME_RESERVATION_BYTES: usize = 256 * 1024 * 1024;
const MAX_IN_FLIGHT_FRAME_BYTES: usize = 4 * 1024 * 1024 * 1024;
const DEFAULT_DRAIN_TIMEOUT_MS: u64 = 5_000;
const MAX_DRAIN_TIMEOUT_MS: u64 = 60_000;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const IDLE_CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    /// Unix socket path to listen on. Its existing parent must be canonical,
    /// euid-owned, non-symlinked, and mode 0700.
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
    /// Hard ceiling for simultaneously admitted client connections.
    #[arg(long, default_value_t = DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,
    /// Aggregate memory reservation for admitted request frames. Each live
    /// request receiver reserves one 256 MiB frame slot before allocating.
    #[arg(long, default_value_t = DEFAULT_MAX_IN_FLIGHT_FRAME_BYTES)]
    max_in_flight_frame_bytes: usize,
    /// Maximum time to drain admitted connections after shutdown starts.
    #[arg(long, default_value_t = DEFAULT_DRAIN_TIMEOUT_MS)]
    drain_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum LifecycleState {
    Accepting = 0,
    Draining = 1,
    Stopped = 2,
    DrainTimedOut = 3,
}

struct Lifecycle {
    state: AtomicU8,
    cancel: CancelToken,
}

impl Lifecycle {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(LifecycleState::Accepting as u8),
            cancel: CancelToken::new(),
        }
    }

    fn state(&self) -> LifecycleState {
        match self.state.load(Ordering::Acquire) {
            0 => LifecycleState::Accepting,
            1 => LifecycleState::Draining,
            2 => LifecycleState::Stopped,
            3 => LifecycleState::DrainTimedOut,
            _ => unreachable!("lifecycle state is always a valid discriminant"),
        }
    }

    fn is_accepting(&self) -> bool {
        self.state() == LifecycleState::Accepting
    }

    fn begin_draining(&self) {
        self.cancel.cancel();
        let _ = self.state.compare_exchange(
            LifecycleState::Accepting as u8,
            LifecycleState::Draining as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn finish_drain(&self, drained: bool) {
        let next = if drained {
            LifecycleState::Stopped
        } else {
            LifecycleState::DrainTimedOut
        };
        let _ = self.state.compare_exchange(
            LifecycleState::Draining as u8,
            next as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

struct ConnectionCount {
    active: usize,
}

struct ConnectionPool {
    max: usize,
    count: Mutex<ConnectionCount>,
    empty: Condvar,
}

impl ConnectionPool {
    fn new(max: usize) -> io::Result<Arc<Self>> {
        if max == 0 || max > MAX_CONNECTIONS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("max-connections must be between 1 and {MAX_CONNECTIONS}"),
            ));
        }
        Ok(Arc::new(Self {
            max,
            count: Mutex::new(ConnectionCount { active: 0 }),
            empty: Condvar::new(),
        }))
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        if count.active >= self.max {
            return None;
        }
        count.active += 1;
        Some(ConnectionPermit {
            pool: Arc::clone(self),
        })
    }

    fn wait_for_empty(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while count.active != 0 {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .empty
                .wait_timeout(count, remaining)
                .unwrap_or_else(|e| e.into_inner());
            count = next;
            if result.timed_out() && count.active != 0 {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.count.lock().unwrap_or_else(|e| e.into_inner()).active
    }
}

struct FrameBudget {
    slots: usize,
    active: Mutex<usize>,
}

impl FrameBudget {
    fn new(bytes: usize) -> io::Result<Arc<Self>> {
        if !(MIN_FRAME_RESERVATION_BYTES..=MAX_IN_FLIGHT_FRAME_BYTES).contains(&bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max-in-flight-frame-bytes must be between 256 MiB and 4 GiB",
            ));
        }
        Ok(Arc::new(Self {
            slots: bytes / MIN_FRAME_RESERVATION_BYTES,
            active: Mutex::new(0),
        }))
    }

    fn try_acquire(self: &Arc<Self>) -> Option<FramePermit> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active >= self.slots {
            return None;
        }
        *active += 1;
        Some(FramePermit {
            budget: Arc::clone(self),
        })
    }
}

struct FramePermit {
    budget: Arc<FrameBudget>,
}

impl Drop for FramePermit {
    fn drop(&mut self) {
        let mut active = self.budget.active.lock().unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
    }
}

struct ConnectionPermit {
    pool: Arc<ConnectionPool>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut count = self.pool.count.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(count.active > 0);
        count.active -= 1;
        if count.active == 0 {
            self.pool.empty.notify_all();
        }
    }
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(LifecycleState::Stopped) => {
            eprintln!("hearthd: shutdown complete");
            ExitCode::SUCCESS
        }
        Ok(LifecycleState::DrainTimedOut) => {
            eprintln!(
                "hearthd: drain timeout expired; endpoint cleaned and process returning with unfinished workers"
            );
            ExitCode::SUCCESS
        }
        Ok(state) => {
            eprintln!("hearthd: stopped in unexpected lifecycle state {state:?}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("hearthd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> io::Result<LifecycleState> {
    validate_daemon_uid(effective_uid())?;
    let connections = ConnectionPool::new(args.max_connections)?;
    let frames = FrameBudget::new(args.max_in_flight_frame_bytes)?;
    let drain_timeout = validate_drain_timeout(args.drain_timeout_ms)?;
    let endpoint = match args.socket.as_deref() {
        Some(path) => validate_endpoint_path(path)?,
        None => prepare_default_endpoint()?,
    };
    let mut bound = BoundEndpoint::bind(&endpoint)?;
    bound.listener().set_nonblocking(true)?;

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

    let lifecycle = Arc::new(Lifecycle::new());
    let mut workers = Vec::new();
    eprintln!("hearthd: listening on {}", endpoint.path().display());

    while lifecycle.is_accepting() {
        reap_finished_workers(&mut workers);
        match bound.listener().accept() {
            Ok((stream, _address)) => {
                if !lifecycle.is_accepting() {
                    drop(stream);
                    break;
                }
                if let Err(error) = verify_peer_uid(&stream, effective_uid()) {
                    eprintln!("hearthd: rejected unauthenticated Unix peer: {error}");
                    continue;
                }
                let Some(permit) = connections.try_acquire() else {
                    eprintln!("hearthd: connection ceiling reached; rejecting client");
                    continue;
                };
                if let Err(error) = stream.set_read_timeout(Some(IDLE_CONNECTION_POLL_INTERVAL)) {
                    eprintln!("hearthd: failed to configure connection timeout: {error}");
                    continue;
                }
                let worker_engine = engine.clone();
                let worker_lifecycle = Arc::clone(&lifecycle);
                let worker_frames = Arc::clone(&frames);
                match std::thread::Builder::new()
                    .name("hearthd-connection".into())
                    .spawn(move || {
                        let _permit = permit;
                        handle_conn(stream, worker_engine, worker_lifecycle, worker_frames);
                    }) {
                    Ok(worker) => workers.push(worker),
                    Err(error) => eprintln!("hearthd: failed to spawn connection thread: {error}"),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("hearthd: accept error: {error}");
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }

    // Closing the listener is the admission boundary. Existing connection
    // threads finish the request they already admitted, then observe Draining.
    bound.stop_admitting();
    let drained = connections.wait_for_empty(drain_timeout);
    lifecycle.finish_drain(drained);
    join_workers(workers, drained);

    // Endpoint cleanup runs only after bounded drain and still under the
    // per-endpoint lifetime lock. A replacement path is preserved.
    let removed = bound.cleanup()?;
    if !removed {
        eprintln!("hearthd: endpoint path was absent or replaced; cleanup preserved it");
    }
    Ok(lifecycle.state())
}

fn validate_daemon_uid(uid: u32) -> io::Result<()> {
    if uid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to run hearthd with effective UID 0",
        ));
    }
    Ok(())
}

fn validate_drain_timeout(timeout_ms: u64) -> io::Result<Duration> {
    if timeout_ms > MAX_DRAIN_TIMEOUT_MS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("drain-timeout-ms must not exceed {MAX_DRAIN_TIMEOUT_MS}"),
        ));
    }
    Ok(Duration::from_millis(timeout_ms))
}

fn join_workers(workers: Vec<JoinHandle<()>>, drained: bool) {
    for worker in workers {
        if drained || worker.is_finished() {
            let _ = worker.join();
        }
        // On timeout, dropping an unfinished handle detaches it. Returning from
        // `main` then terminates remaining process threads without an abrupt
        // `process::exit`, after the owned endpoint has been cleaned.
    }
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}

fn handle_conn(
    stream: UnixStream,
    engine: Engine,
    lifecycle: Arc<Lifecycle>,
    frames: Arc<FrameBudget>,
) {
    let mut writer = &stream;
    let mut requests = hearth_tools::transport::RequestReceiver::new(&stream);
    loop {
        if !lifecycle.is_accepting() {
            break;
        }
        let Some(_frame_permit) = frames.try_acquire() else {
            break;
        };
        let (req, fd) = match requests.recv_request() {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if !lifecycle.is_accepting() {
            break;
        }

        let is_shutdown = matches!(req, Request::Shutdown);
        if is_shutdown {
            lifecycle.begin_draining();
        }

        // Zero-copy fast path: if the client passed its stdout fd with a Read,
        // write the cached content straight to that fd and return only metadata.
        let resp = if let (Some(fd), Request::Read(params)) = (fd.as_ref(), &req) {
            stream_read(&engine, params, fd)
        } else {
            dispatch_cancellable(&engine, req, &lifecycle.cancel)
        };
        if write_msg(&mut writer, &resp).is_err() || is_shutdown {
            break;
        }
    }
}

/// Run `read`, then write its content directly to the client-supplied fd.
fn stream_read(engine: &Engine, params: &ReadParams, fd: &OwnedFd) -> Response {
    match hearth_tools::read(engine, params) {
        Ok(result) => match write_all_fd(fd.as_raw_fd(), result.content.as_bytes()) {
            Ok(()) => Response::Streamed(StreamedResult {
                bytes_written: result.content.len() as u64,
                total_lines: result.total_lines,
            }),
            Err(error) => Response::Error(ToolError::from(error)),
        },
        Err(error) => Response::Error(error),
    }
}

/// Write all bytes to a raw fd without taking ownership (so it is not closed).
fn write_all_fd(fd: i32, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is valid for its advertised length and `fd` remains
        // borrowed/owned by the caller for the duration of this call.
        let written =
            unsafe { libc::write(fd, bytes.as_ptr().cast::<std::ffi::c_void>(), bytes.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zero-length write before streamed output completed",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_effective_uid_is_rejected() {
        let error = validate_daemon_uid(0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        validate_daemon_uid(1).unwrap();
    }

    #[test]
    fn connection_ceiling_and_raii_release_are_exact() {
        let pool = ConnectionPool::new(2).unwrap();
        let first = pool.try_acquire().unwrap();
        let second = pool.try_acquire().unwrap();
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.active(), 2);

        drop(first);
        assert_eq!(pool.active(), 1);
        let replacement = pool.try_acquire().unwrap();
        assert_eq!(pool.active(), 2);
        drop((second, replacement));
        assert_eq!(pool.active(), 0);
    }

    #[test]
    fn aggregate_frame_budget_is_bounded_and_released() {
        assert!(FrameBudget::new(MIN_FRAME_RESERVATION_BYTES - 1).is_err());
        let budget = FrameBudget::new(MIN_FRAME_RESERVATION_BYTES * 2).unwrap();
        let first = budget.try_acquire().unwrap();
        let second = budget.try_acquire().unwrap();
        assert!(budget.try_acquire().is_none());
        drop(first);
        assert!(budget.try_acquire().is_some());
        drop(second);
    }

    #[test]
    fn drain_timeout_is_bounded() {
        assert_eq!(
            validate_drain_timeout(DEFAULT_DRAIN_TIMEOUT_MS).unwrap(),
            Duration::from_millis(DEFAULT_DRAIN_TIMEOUT_MS)
        );
        let error = validate_drain_timeout(MAX_DRAIN_TIMEOUT_MS + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn drain_reaches_stopped_after_last_permit_returns() {
        let lifecycle = Arc::new(Lifecycle::new());
        let pool = ConnectionPool::new(1).unwrap();
        let permit = pool.try_acquire().unwrap();
        lifecycle.begin_draining();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(permit);
        });

        let drained = pool.wait_for_empty(Duration::from_secs(1));
        lifecycle.finish_drain(drained);
        releaser.join().unwrap();
        assert!(drained);
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[test]
    fn drain_timeout_is_a_terminal_explicit_state() {
        let lifecycle = Lifecycle::new();
        let pool = ConnectionPool::new(1).unwrap();
        let _permit = pool.try_acquire().unwrap();
        lifecycle.begin_draining();

        let drained = pool.wait_for_empty(Duration::from_millis(1));
        lifecycle.finish_drain(drained);
        assert!(!drained);
        assert_eq!(lifecycle.state(), LifecycleState::DrainTimedOut);
    }

    #[test]
    fn shutdown_request_stops_admission_and_returns_ack_without_process_exit() {
        let engine = Engine::new(EngineConfig {
            enable_optimizer: false,
            enable_watch: false,
            ..EngineConfig::default()
        });
        let lifecycle = Arc::new(Lifecycle::new());
        let worker_lifecycle = Arc::clone(&lifecycle);
        let (server, client) = UnixStream::pair().unwrap();
        let frames = FrameBudget::new(DEFAULT_MAX_IN_FLIGHT_FRAME_BYTES).unwrap();
        let worker =
            std::thread::spawn(move || handle_conn(server, engine, worker_lifecycle, frames));

        hearth_tools::transport::send_request_with_fd(&client, &Request::Shutdown, None).unwrap();
        let mut reader = &client;
        let response: Response = hearth_tools::transport::read_msg(&mut reader).unwrap();
        worker.join().unwrap();

        assert!(matches!(response, Response::ShuttingDown));
        assert_eq!(lifecycle.state(), LifecycleState::Draining);
    }
}
