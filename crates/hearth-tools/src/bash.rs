//! The `bash` tool.
//!
//! Default path: spawn a fresh shell per command in its own process group,
//! drain both pipes on reader threads (no output-size deadlock), and enforce a
//! hard timeout by killing the whole process group. Always correct.
//!
//! Opt-in path (`EngineConfig::warm_shell`): run on a pooled warm shell
//! (`crate::shell`) to skip the per-command spawn. The pool falls back to a
//! fresh spawn **only** when it can prove the command was never dispatched;
//! any ambiguity surfaces as [`ErrorKind::Indeterminate`] rather than a silent
//! second execution.
//!
//! Both paths stream output through the caller's callback as it arrives, with
//! one monotonic sequence across stdout and stderr so the observed interleaving
//! can be replayed exactly. The same bytes are also accumulated into the result
//! unless `collectOutput` is off, so a caller never has to choose between
//! streaming and having the final output.

use crate::shell::{Dispatch, WarmShellPool};
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{
    BashChannel, BashChunk, BashParams, BashResult, ErrorKind, ShellSpec, ShellTransport,
    ToolError, ToolResult,
};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long to keep reading after the child exits but its pipes are still held
/// open by a detached descendant. Re-armed on every chunk: an actively writing
/// descendant keeps us reading, a silent one releases us.
const IDLE_GRACE: Duration = Duration::from_millis(100);

/// How often a live cancel token is polled while waiting for output.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// How long to wait for a killed process group to be reaped before giving up.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Maximum caller-selectable command timeout. Long builds remain supported,
/// while hostile `u64::MAX` input can never overflow `Instant` arithmetic or
/// pin a daemon operation indefinitely.
pub const MAX_BASH_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Run a command, discarding the stream.
pub fn bash(engine: &Engine, params: &BashParams) -> ToolResult<BashResult> {
    bash_stream(engine, params, &CancelToken::none(), &mut |_| {})
}

/// Run a command with cancellation but no streaming.
pub fn bash_cancellable(
    engine: &Engine,
    params: &BashParams,
    cancel: &CancelToken,
) -> ToolResult<BashResult> {
    bash_stream(engine, params, cancel, &mut |_| {})
}

/// Run a command, delivering ordered output chunks to `on_chunk` as they
/// arrive.
///
/// Cancellation kills the command's whole process tree and returns the partial
/// output rather than erroring, so a caller keeps whatever it already rendered.
/// A pre-cancelled token rejects before anything is spawned.
pub fn bash_stream(
    engine: &Engine,
    params: &BashParams,
    cancel: &CancelToken,
    on_chunk: &mut dyn FnMut(BashChunk),
) -> ToolResult<BashResult> {
    profile!("tool.bash", {
        cancel.check()?;
        // An unrestricted command can mutate any path reachable by this UID,
        // not merely cwd. Once this call is admitted, conservatively discard
        // every filesystem-derived cache even on timeout, cancellation,
        // indeterminate completion, spawn failure, or unwind.
        let _invalidate = BashInvalidation(engine);
        let cwd = params
            .cwd
            .clone()
            .unwrap_or_else(|| engine.config().default_cwd.display().to_string());
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(engine.config().bash_timeout_ms)
                .max(1),
        )
        .min(MAX_BASH_TIMEOUT);
        let spec = params
            .shell
            .clone()
            .or_else(|| engine.config().shell.clone())
            .unwrap_or_default();
        let start = Instant::now();
        let mut emitter = Emitter::new(
            params.collect_output,
            engine.config().max_bash_output_bytes,
            on_chunk,
        );

        // Opt-in warm-shell fast path.
        // A pooled shell inherited the daemon environment before this call, so
        // it cannot truthfully implement env_clear. Route such calls through
        // the fresh-spawn path, where Command::env_clear is authoritative.
        let warm_compatible = spec.program == "/bin/sh"
            && spec.transport == ShellTransport::Arg
            && spec.args == ["-c"]
            && params.command.len() <= 4096;
        if engine.config().warm_shell && !params.env_clear && warm_compatible {
            let pool = engine.extension::<WarmShellPool>();
            let dispatch = pool.run(
                &spec.program,
                &params.command,
                &cwd,
                &params.env,
                timeout,
                cancel,
                &mut |channel, bytes| emitter.push(channel, bytes),
            );
            match dispatch {
                Dispatch::Done { exit_code } => {
                    return Ok(emitter.finish(exit_code, None, false, false, start));
                }
                Dispatch::TimedOut => return Ok(emitter.finish(-1, None, true, false, start)),
                Dispatch::Aborted => return Ok(emitter.finish(-1, None, false, true, start)),
                Dispatch::Indeterminate(message) => return Err(ToolError::indeterminate(message)),
                // Provably never reached the shell — running it now is the
                // first and only execution.
                Dispatch::NotDispatched => {}
            }
        }

        let exit = spawn_bash(&spec, &cwd, params, timeout, cancel, &mut emitter)?;
        Ok(emitter.finish(exit.code, exit.signal, exit.timed_out, exit.aborted, start))
    })
}

/// Clears filesystem-derived resident state when an admitted Bash call exits.
struct BashInvalidation<'a>(&'a Engine);

impl Drop for BashInvalidation<'_> {
    fn drop(&mut self) {
        self.0.clear_filesystem_caches();
        crate::graph::graph_clear(self.0);
    }
}

/// Accumulates output and hands it to the caller as ordered chunks.
struct Emitter<'a> {
    on_chunk: &'a mut dyn FnMut(BashChunk),
    seq: u64,
    collect: bool,
    max_bytes: usize,
    emitted_bytes: usize,
    output_truncated: bool,
    stdout: String,
    stderr: String,
    out_decoder: Utf8Stream,
    err_decoder: Utf8Stream,
}

impl<'a> Emitter<'a> {
    fn new(collect: bool, max_bytes: usize, on_chunk: &'a mut dyn FnMut(BashChunk)) -> Self {
        Self {
            on_chunk,
            seq: 0,
            collect,
            max_bytes,
            emitted_bytes: 0,
            output_truncated: false,
            stdout: String::new(),
            stderr: String::new(),
            out_decoder: Utf8Stream::default(),
            err_decoder: Utf8Stream::default(),
        }
    }

    fn push(&mut self, channel: BashChannel, bytes: &[u8]) {
        let text = match channel {
            BashChannel::Stdout => self.out_decoder.push(bytes),
            BashChannel::Stderr => self.err_decoder.push(bytes),
        };
        self.emit(channel, text);
    }

    fn emit(&mut self, channel: BashChannel, mut text: String) {
        if text.is_empty() {
            return;
        }
        let remaining = self.max_bytes.saturating_sub(self.emitted_bytes);
        if text.len() > remaining {
            let mut end = remaining.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            self.output_truncated = true;
        }
        if remaining == 0 {
            self.output_truncated = true;
        }
        if text.is_empty() {
            return;
        }
        self.emitted_bytes += text.len();
        if self.collect {
            match channel {
                BashChannel::Stdout => self.stdout.push_str(&text),
                BashChannel::Stderr => self.stderr.push_str(&text),
            }
        }
        self.seq += 1;
        (self.on_chunk)(BashChunk {
            seq: self.seq,
            channel,
            text,
        });
    }

    fn finish(
        mut self,
        exit_code: i32,
        signal: Option<i32>,
        timed_out: bool,
        aborted: bool,
        start: Instant,
    ) -> BashResult {
        // A truncated multi-byte sequence at end of stream is real output; emit
        // it lossily rather than dropping bytes on the floor.
        let tail = self.out_decoder.finish();
        self.emit(BashChannel::Stdout, tail);
        let tail = self.err_decoder.finish();
        self.emit(BashChannel::Stderr, tail);
        BashResult {
            stdout: std::mem::take(&mut self.stdout),
            stderr: std::mem::take(&mut self.stderr),
            exit_code,
            signal,
            timed_out,
            aborted,
            duration_us: start.elapsed().as_micros() as u64,
            chunks: self.seq,
            output_truncated: self.output_truncated,
        }
    }
}

/// Incremental UTF-8 decoding that never splits a multi-byte sequence across
/// two chunks — a 64 KiB pipe read lands wherever it lands, and a caller
/// concatenating chunk texts must get the same string as the final output.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // SAFETY: `valid_up_to` is by definition a valid boundary.
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&self.pending[..valid]) });
                    match e.error_len() {
                        // Truncated tail: hold it back for the next chunk.
                        None => {
                            self.pending.drain(..valid);
                            return out;
                        }
                        // Genuinely invalid bytes: replace and keep going.
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

struct Exit {
    code: i32,
    signal: Option<i32>,
    timed_out: bool,
    aborted: bool,
}

enum Ev {
    Data(BashChannel, Vec<u8>),
    Eof(BashChannel),
    Exited(Option<ExitStatus>),
}

/// Always-correct spawn-per-command path (also the warm path's fallback).
fn spawn_bash(
    spec: &ShellSpec,
    cwd: &str,
    params: &BashParams,
    timeout: Duration,
    cancel: &CancelToken,
    emitter: &mut Emitter<'_>,
) -> ToolResult<Exit> {
    let command_on_stdin = spec.transport == ShellTransport::Stdin;

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    if !command_on_stdin {
        cmd.arg(&params.command);
    }
    cmd.current_dir(cwd)
        .stdin(if command_on_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if params.env_clear {
        cmd.env_clear();
    }
    for (k, v) in &params.env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| {
        ToolError::new(
            ErrorKind::Io,
            format!("failed to spawn {}: {e}", spec.program),
        )
    })?;
    let pid = child.id() as i32;

    if let Some(mut stdin) = child.stdin.take() {
        // From a thread: a large script can outgrow the pipe buffer, and the
        // shell will not drain it while we are not yet draining its output.
        let command = params.command.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(command.as_bytes());
            let _ = stdin.flush();
            // Dropping `stdin` closes the pipe, which is what tells `sh -s`
            // that the script is complete.
        });
    }

    let (tx, rx) = mpsc::sync_channel(8);
    if let Some(mut pipe) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(&mut pipe, BashChannel::Stdout, tx));
    }
    if let Some(mut pipe) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(&mut pipe, BashChannel::Stderr, tx));
    }
    std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = tx.send(Ev::Exited(status));
    });

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| ToolError::invalid("bash timeout exceeds platform range"))?;
    let mut out_eof = false;
    let mut err_eof = false;
    let mut exited: Option<Option<ExitStatus>> = None;
    let mut settled_at: Option<Instant> = None;
    let mut killed_at: Option<Instant> = None;
    let mut timed_out = false;
    let mut aborted = false;

    loop {
        if exited.is_some() && out_eof && err_eof {
            break;
        }
        // The child is gone but a detached descendant still holds the pipes.
        // Wait out the idle grace rather than the full timeout, and never
        // destroy the streams mid-write.
        if exited.is_some() && settled_at.is_some_and(|at| at.elapsed() >= IDLE_GRACE) {
            break;
        }
        // A killed process that somehow never reports must not wedge the call.
        if exited.is_none() && killed_at.is_some_and(|at| at.elapsed() >= REAP_GRACE) {
            break;
        }

        if killed_at.is_none() {
            if cancel.is_cancelled() {
                kill_group(pid);
                aborted = true;
                killed_at = Some(Instant::now());
            } else if Instant::now() >= deadline {
                kill_group(pid);
                timed_out = true;
                killed_at = Some(Instant::now());
            }
        }

        let wait = next_wait(deadline, settled_at, killed_at, cancel);
        match rx.recv_timeout(wait) {
            Ok(Ev::Data(channel, bytes)) => {
                if settled_at.is_some() {
                    settled_at = Some(Instant::now()); // a straggler: re-arm
                }
                emitter.push(channel, &bytes);
            }
            Ok(Ev::Eof(BashChannel::Stdout)) => out_eof = true,
            Ok(Ev::Eof(BashChannel::Stderr)) => err_eof = true,
            Ok(Ev::Exited(status)) => {
                exited = Some(status);
                settled_at.get_or_insert_with(Instant::now);
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is gone: nothing more can arrive.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = exited.flatten();
    Ok(Exit {
        code: status.and_then(|s| s.code()).unwrap_or(-1),
        signal: status.and_then(|s| s.signal()),
        timed_out,
        aborted,
    })
}

fn pump<R: Read>(pipe: &mut R, channel: BashChannel, tx: mpsc::SyncSender<Ev>) {
    let mut chunk = vec![0_u8; 65_536];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Ev::Eof(channel));
                return;
            }
            Ok(n) => {
                if tx.send(Ev::Data(channel, chunk[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
}

fn next_wait(
    deadline: Instant,
    settled_at: Option<Instant>,
    killed_at: Option<Instant>,
    cancel: &CancelToken,
) -> Duration {
    let mut wait = match (settled_at, killed_at) {
        (Some(at), _) => IDLE_GRACE.saturating_sub(at.elapsed()),
        (None, Some(at)) => REAP_GRACE.saturating_sub(at.elapsed()),
        (None, None) => deadline.saturating_duration_since(Instant::now()),
    };
    if cancel.is_live() {
        wait = wait.min(CANCEL_POLL);
    }
    wait
}

/// SIGKILL the command's whole process group, so a shell's children die with
/// it rather than being orphaned and left running.
fn kill_group(pid: i32) {
    // SAFETY: kill(2) on our own child's process group; -pid targets the group.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_stream_never_splits_a_multibyte_sequence() {
        let mut s = Utf8Stream::default();
        let text = "日本語";
        let bytes = text.as_bytes();
        // Split in the middle of the first character.
        assert_eq!(s.push(&bytes[..2]), "");
        let rest = s.push(&bytes[2..]);
        assert_eq!(rest, text);
        assert_eq!(s.finish(), "");
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes_but_keeps_the_rest() {
        let mut s = Utf8Stream::default();
        assert_eq!(s.push(b"a\xffb"), "a\u{FFFD}b");
    }

    #[test]
    fn utf8_stream_flushes_a_truncated_tail() {
        let mut s = Utf8Stream::default();
        assert_eq!(s.push(&"あ".as_bytes()[..1]), "");
        assert_eq!(s.finish(), "\u{FFFD}");
    }
}
