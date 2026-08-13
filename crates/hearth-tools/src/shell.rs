//! An opt-in pipe-based warm-shell pool for `bash` (default off).
//!
//! Keeps a small pool of long-lived shells so a command avoids the per-command
//! process spawn. Each shell owns four pipes: stdin (the script), stdout and
//! stderr (the command's output), and a private **control** pipe on fd 3 that
//! carries only Hearth's own protocol. Keeping the protocol off the output
//! streams means an exit code can never be confused with command output, and
//! the delimiter search on stdout/stderr has one job.
//!
//! Per command the shell runs:
//!
//! ```sh
//! set -m                                  # each job gets its own process group
//! ( cd CWD && { export …; eval 'CMD'; } ) </dev/null &
//! printf 'P %d\n' "$!" >&3                # the job's process group id
//! wait "$!"; printf 'X %d\n' "$?" >&3     # the exit status
//! printf '%s' NONCE ; printf '%s' NONCE >&2
//! ```
//!
//! The `( … )` subshell keeps cwd, environment and variable changes from
//! leaking between commands, and `</dev/null` keeps a command that reads stdin
//! from consuming the next script.
//!
//! **Output isolation.** A command that backgrounds a process leaves a
//! descendant holding the shell's stdout and stderr, which could otherwise
//! write into the *next* command's output. Two measures close that:
//! `set -m` puts the job in its own process group, which Hearth kills as soon
//! as the command settles; and before dispatching anything the pool drains the
//! shell's channel and retires the shell if a single stray byte shows up. The
//! second check happens *before* dispatch, so retiring there is always safe.
//!
//! **At-most-once.** A command is "dispatched" the moment any byte of its
//! script reaches the shell. If the very first write fails having written
//! nothing, the command provably never ran and the caller may fall back to a
//! fresh spawn. Any later failure returns [`Dispatch::Indeterminate`] and the
//! command is never re-run — a mutating command must not execute twice because
//! a pipe broke.
//!
//! Correctness relies on the random nonce not occurring in command output. The
//! collision probability is approximately 2^-128 per command.

use hearth_core::CancelToken;
use hearth_proto::BashChannel;
use parking_lot::Mutex;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// How long to keep reading after the command settled but the pipes are still
/// held open by a detached descendant. Re-armed on every chunk, so an actively
/// writing descendant keeps us reading while a silent one still releases us.
const IDLE_GRACE: Duration = Duration::from_millis(100);

/// How often a live cancel token is polled while waiting for output.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// The outcome of dispatching a command to a warm shell.
pub enum Dispatch {
    /// Nothing was written to the shell — the command provably did not run, so
    /// the caller may safely execute it another way.
    NotDispatched,
    /// Ran to completion.
    Done { exit_code: i32 },
    /// Exceeded the timeout; the job's process group was killed.
    TimedOut,
    /// Cancelled by the caller; the job's process group was killed.
    Aborted,
    /// Dispatched, outcome unknown. **Must not be retried.**
    Indeterminate(String),
}

/// A pool of warm shells, held as a per-engine extension.
#[derive(Default)]
pub struct WarmShellPool {
    /// Free shells, keyed by the program they run, so a caller that overrides
    /// the shell does not get handed one running a different binary.
    free: Mutex<Vec<(String, WarmShell)>>,
    seq: AtomicU64,
}

fn pool_capacity() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}

/// What a shell's reader threads forward.
enum Raw {
    Data(Src, Vec<u8>),
    /// Any pipe reaching EOF means the shell process itself is gone; which
    /// pipe saw it first carries no extra information.
    Eof,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Out,
    Err,
    Ctrl,
}

struct WarmShell {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Raw>,
    /// Process group of the shell itself. Killing it takes down the shell; a
    /// command's own job lives in a *different* group thanks to `set -m`.
    pgid: i32,
}

impl Drop for WarmShell {
    fn drop(&mut self) {
        // Kill the whole group (the shell plus anything still in it), then reap.
        // SAFETY: killing our own child's process group by negative pgid.
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

/// Spawn a shell that reads its script from stdin, with a private control pipe
/// on fd 3.
fn spawn_shell(program: &str) -> std::io::Result<WarmShell> {
    let (ctrl_read, ctrl_write) = control_pipe()?;

    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0); // own group so a shutdown can kill the whole tree

    // SAFETY: the closure runs between fork and exec, so it must be
    // async-signal-safe. `dup2` and `close` are.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(ctrl_write, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if ctrl_write != 3 {
                libc::close(ctrl_write);
            }
            Ok(())
        });
    }

    let spawned = cmd.spawn();
    // The child holds its own duplicate now (or the spawn failed); either way
    // the parent must not keep the write end open or the reader never sees EOF.
    unsafe { libc::close(ctrl_write) };
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(ctrl_read) };
            return Err(e);
        }
    };

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // SAFETY: `ctrl_read` is an owned pipe fd this function just created.
    let ctrl = unsafe { std::fs::File::from_raw_fd(ctrl_read) };
    let pgid = child.id() as i32;

    // One reader thread per pipe, alive for the shell's whole life. They send
    // raw bytes; the per-command logic does the delimiter search. Threads exit
    // on EOF or once the receiver is dropped.
    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, Src::Out, tx.clone());
    spawn_reader(stderr, Src::Err, tx.clone());
    spawn_reader(ctrl, Src::Ctrl, tx);

    Ok(WarmShell {
        child,
        stdin,
        rx,
        pgid,
    })
}

/// A `pipe(2)` whose read end is close-on-exec (so the shell never inherits it)
/// and whose write end is not (so it survives into the child for `dup2`).
fn control_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array, as pipe(2) requires.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fds[0] is the pipe read end this function owns.
    unsafe { libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok((fds[0], fds[1]))
}

fn spawn_reader<R: Read + AsRawFd + Send + 'static>(mut reader: R, src: Src, tx: Sender<Raw>) {
    let _ = std::thread::Builder::new()
        .name("hearth-shell-reader".into())
        .spawn(move || {
            let mut chunk = vec![0_u8; 65_536];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Raw::Eof);
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Raw::Data(src, chunk[..n].to_vec())).is_err() {
                            return; // the shell was retired
                        }
                    }
                }
            }
        });
}

/// Single-quote a value for safe injection into the shell script.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// A byte stream terminated by a nonce, emitting everything before it.
///
/// Holds back the last `nonce.len() - 1` bytes so a delimiter split across two
/// reads is still recognised and never leaks into the output.
struct Delimited {
    nonce: Vec<u8>,
    pending: Vec<u8>,
    done: bool,
}

impl Delimited {
    fn new(nonce: &[u8]) -> Self {
        Self {
            nonce: nonce.to_vec(),
            pending: Vec::new(),
            done: false,
        }
    }

    fn push(&mut self, bytes: &[u8], emit: &mut dyn FnMut(&[u8])) {
        if self.done {
            return;
        }
        self.pending.extend_from_slice(bytes);
        if let Some(pos) = memchr::memmem::find(&self.pending, &self.nonce) {
            emit(&self.pending[..pos]);
            self.pending.clear();
            self.done = true;
            return;
        }
        let hold = self.nonce.len().saturating_sub(1);
        if self.pending.len() > hold {
            let safe = self.pending.len() - hold;
            emit(&self.pending[..safe]);
            self.pending.drain(..safe);
        }
    }

    /// Flush whatever is left when the stream ends without a delimiter (a
    /// killed command). The held-back tail is real output in that case.
    fn flush(&mut self, emit: &mut dyn FnMut(&[u8])) {
        if !self.done && !self.pending.is_empty() {
            emit(&self.pending);
            self.pending.clear();
        }
    }
}

/// Accumulates the control channel's `P`/`X` lines.
#[derive(Default)]
struct Control {
    buf: Vec<u8>,
    job_pgid: Option<i32>,
    exit_code: Option<i32>,
}

impl Control {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = memchr::memchr(b'\n', &self.buf) {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..nl]);
            let mut parts = line.split_whitespace();
            match (
                parts.next(),
                parts.next().and_then(|v| v.parse::<i32>().ok()),
            ) {
                (Some("P"), Some(pid)) if self.job_pgid.is_none() && self.exit_code.is_none() => {
                    self.job_pgid = Some(pid);
                }
                (Some("X"), Some(code)) if self.job_pgid.is_some() && self.exit_code.is_none() => {
                    self.exit_code = Some(code);
                }
                _ => {}
            }
        }
    }
}

impl WarmShellPool {
    /// Run `command` on a warm shell running `program`.
    ///
    /// `on_bytes` receives output as it arrives, in the order the pipes
    /// produced it. The returned [`Dispatch`] tells the caller whether it may
    /// safely fall back to another execution path.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        program: &str,
        command: &str,
        cwd: &str,
        env: &[(String, String)],
        timeout: Duration,
        cancel: &CancelToken,
        on_bytes: &mut dyn FnMut(BashChannel, &[u8]),
    ) -> Dispatch {
        let mut shell = match self.take(program) {
            Some(s) => s,
            None => match spawn_shell(program) {
                Ok(s) => s,
                Err(_) => return Dispatch::NotDispatched,
            },
        };

        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let r_hi = RandomState::new().build_hasher().finish();
        let r_lo = RandomState::new().build_hasher().finish();
        let nonce = format!("__HEARTH_{pid}_{n}_{r_hi:016x}{r_lo:016x}__");

        let outcome = run_once(
            &mut shell, command, cwd, env, timeout, cancel, &nonce, on_bytes,
        );

        if matches!(outcome, Dispatch::Done { .. }) {
            self.release(program, shell);
        }
        // Every other outcome drops the shell, whose `Drop` kills its group and
        // reaps it. A shell that timed out, was cancelled, or broke its
        // protocol is never handed to another command.
        outcome
    }

    /// Take a healthy pooled shell for `program`, discarding any that have
    /// unread bytes waiting — those belong to a previous command's stragglers
    /// and must never be attributed to the next one.
    fn take(&self, program: &str) -> Option<WarmShell> {
        loop {
            let shell = {
                let mut free = self.free.lock();
                let idx = free.iter().position(|(p, _)| p == program)?;
                free.swap_remove(idx).1
            };
            match shell.rx.try_recv() {
                // Quiet: no straggler output, readers still alive.
                Err(TryRecvError::Empty) => return Some(shell),
                // Stray output, a closed pipe, or dead readers: retire it and
                // try the next one. Retiring here is free — nothing has been
                // dispatched to this shell yet.
                _ => drop(shell),
            }
        }
    }

    fn release(&self, program: &str, shell: WarmShell) {
        let mut free = self.free.lock();
        if free.len() < pool_capacity() {
            free.push((program.to_string(), shell));
        }
        // else: drop (Drop kills + reaps).
    }
}

#[allow(clippy::too_many_arguments)]
fn run_once(
    shell: &mut WarmShell,
    command: &str,
    cwd: &str,
    env: &[(String, String)],
    timeout: Duration,
    cancel: &CancelToken,
    nonce: &str,
    on_bytes: &mut dyn FnMut(BashChannel, &[u8]),
) -> Dispatch {
    let script = build_script(command, cwd, env, nonce);

    // At-most-once hinges on this write: nothing written means nothing ran.
    match write_script(&mut shell.stdin, script.as_bytes()) {
        Ok(()) => {}
        Err(0) => return Dispatch::NotDispatched,
        Err(_) => {
            return Dispatch::Indeterminate(
                "the shell closed while its command was being written; the command may have run"
                    .into(),
            );
        }
    }

    let mut out = Delimited::new(nonce.as_bytes());
    let mut err = Delimited::new(nonce.as_bytes());
    let mut ctrl = Control::default();
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return Dispatch::NotDispatched;
    };
    let mut settled_at: Option<Instant> = None;
    let mut killed: Option<Dispatch> = None;
    let mut broken = false;

    loop {
        let complete = out.done && err.done && ctrl.exit_code.is_some();
        if complete || broken {
            break;
        }
        // Once the command has settled, only wait out the idle grace for
        // stragglers rather than the full timeout.
        if let Some(at) = settled_at
            && at.elapsed() >= IDLE_GRACE
        {
            break;
        }
        if killed.is_none() {
            if cancel.is_cancelled() {
                kill_job(&ctrl, shell);
                killed = Some(Dispatch::Aborted);
                settled_at = Some(Instant::now());
            } else if Instant::now() >= deadline {
                kill_job(&ctrl, shell);
                killed = Some(Dispatch::TimedOut);
                settled_at = Some(Instant::now());
            }
        }

        let wait = next_wait(deadline, settled_at, cancel);
        match shell.rx.recv_timeout(wait) {
            Ok(Raw::Data(src, bytes)) => {
                if settled_at.is_some() {
                    settled_at = Some(Instant::now()); // a straggler: re-arm
                }
                match src {
                    Src::Out => out.push(&bytes, &mut |b| on_bytes(BashChannel::Stdout, b)),
                    Src::Err => err.push(&bytes, &mut |b| on_bytes(BashChannel::Stderr, b)),
                    Src::Ctrl => {
                        ctrl.push(&bytes);
                        if ctrl.exit_code.is_some() && killed.is_none() {
                            // The command finished on its own; anything still
                            // arriving is a detached descendant's output.
                            settled_at.get_or_insert_with(Instant::now);
                        }
                    }
                }
            }
            // A pipe hitting EOF means the shell itself died mid-command.
            Ok(Raw::Eof) => broken = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => broken = true,
        }
    }

    out.flush(&mut |b| on_bytes(BashChannel::Stdout, b));
    err.flush(&mut |b| on_bytes(BashChannel::Stderr, b));

    if let Some(outcome) = killed {
        return outcome;
    }
    match ctrl.exit_code {
        Some(code) if out.done && err.done => {
            // Destroy anything the command left running before the shell can be
            // reused, so a background writer cannot reach the next command.
            kill_job(&ctrl, shell);
            Dispatch::Done { exit_code: code }
        }
        _ => Dispatch::Indeterminate(
            "the warm shell broke before reporting its command's result; the command may have run"
                .into(),
        ),
    }
}

/// How long to block on the next message.
fn next_wait(deadline: Instant, settled_at: Option<Instant>, cancel: &CancelToken) -> Duration {
    let mut wait = match settled_at {
        Some(at) => IDLE_GRACE.saturating_sub(at.elapsed()),
        None => deadline.saturating_duration_since(Instant::now()),
    };
    if cancel.is_live() {
        wait = wait.min(CANCEL_POLL);
    }
    wait
}

/// Kill the command's own process group, leaving the warm shell alive.
///
/// Best effort: `set -m` is what gives the job its own group, and a shell that
/// ignores it leaves the job in the shell's group where this is a no-op. The
/// pool's pre-dispatch drain is the backstop for that case.
fn kill_job(ctrl: &Control, shell: &WarmShell) {
    if let Some(job) = ctrl.job_pgid
        && job > 0
        && job != shell.pgid
    {
        // SAFETY: `job` is a process group descended from our own shell.
        unsafe { libc::kill(-job, libc::SIGKILL) };
        unsafe { libc::kill(job, libc::SIGKILL) };
    }
}

/// Write the whole script, reporting how many bytes made it out on failure.
/// `Err(0)` is the only provably-not-dispatched case.
fn write_script(stdin: &mut ChildStdin, mut bytes: &[u8]) -> Result<(), usize> {
    let mut written = 0usize;
    while !bytes.is_empty() {
        match stdin.write(bytes) {
            Ok(0) => return Err(written),
            Ok(n) => {
                written += n;
                bytes = &bytes[n..];
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(written),
        }
    }
    stdin.flush().map_err(|_| written)
}

fn build_script(command: &str, cwd: &str, env: &[(String, String)], nonce: &str) -> String {
    let quoted_nonce = sh_quote(nonce);
    let mut script = String::with_capacity(command.len() + 320);
    // Job control puts the command's subshell in its own process group, which
    // is what lets Hearth kill the command's tree without killing the shell.
    // A shell without job control ignores this and still runs correctly.
    script.push_str("set -m 2>/dev/null\n");
    // Job control is switched back off *inside* the subshell so the command's
    // own background jobs stay in the subshell's process group rather than
    // splitting into groups Hearth has no handle on.
    script.push_str("( set +m 2>/dev/null; exec 3>&-; cd ");
    script.push_str(&sh_quote(cwd));
    script.push_str(" && { ");
    for (k, v) in env {
        script.push_str("export ");
        script.push_str(k);
        script.push('=');
        script.push_str(&sh_quote(v));
        script.push_str("; ");
    }
    // `eval` on a single-quoted string makes a syntactically incomplete command
    // fail immediately instead of leaving the shell waiting for more input.
    script.push_str("eval ");
    script.push_str(&sh_quote(command));
    script.push_str("; } ) </dev/null &\n");
    script.push_str("__hjob=$!\n");
    script.push_str("printf 'P %d\\n' \"$__hjob\" >&3\n");
    script.push_str("wait \"$__hjob\"\n");
    script.push_str("printf 'X %d\\n' \"$?\" >&3\n");
    // Delimiters last, so they can only appear after every byte the command
    // wrote. `printf '%s'` adds nothing of its own, so output with or without a
    // trailing newline is preserved exactly.
    script.push_str("printf '%s' ");
    script.push_str(&quoted_nonce);
    script.push('\n');
    script.push_str("printf '%s' ");
    script.push_str(&quoted_nonce);
    script.push_str(" 1>&2\n");
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimited_holds_back_a_split_nonce() {
        let mut d = Delimited::new(b"XYZ");
        let mut got = Vec::new();
        d.push(b"hello X", &mut |b| got.extend_from_slice(b));
        // The trailing " X" is held back: it could still turn into the nonce.
        assert_eq!(got, b"hello", "a possible nonce prefix must not be emitted");
        d.push(b"YZ", &mut |b| got.extend_from_slice(b));
        assert!(d.done);
        assert_eq!(
            got, b"hello ",
            "the held-back space was output, the nonce was not"
        );
    }

    #[test]
    fn delimited_flushes_partial_output_when_killed() {
        let mut d = Delimited::new(b"XYZ");
        let mut got = Vec::new();
        d.push(b"partial X", &mut |b| got.extend_from_slice(b));
        d.flush(&mut |b| got.extend_from_slice(b));
        assert_eq!(
            got, b"partial X",
            "held-back bytes are real output when no nonce arrives"
        );
    }

    #[test]
    fn control_parses_pid_and_exit_across_chunk_boundaries() {
        let mut c = Control::default();
        c.push(b"P 12");
        c.push(b"345\nX ");
        assert_eq!(c.job_pgid, Some(12345));
        assert_eq!(c.exit_code, None);
        c.push(b"7\n");
        assert_eq!(c.exit_code, Some(7));
    }

    #[test]
    fn control_rejects_forged_or_out_of_order_records() {
        let mut c = Control::default();
        c.push(b"X 0\nP 123\nP 999\nX 7\nX 0\n");
        assert_eq!(c.job_pgid, Some(123));
        assert_eq!(c.exit_code, Some(7));
    }

    #[test]
    fn command_subshell_closes_the_private_control_fd() {
        let script = build_script("printf forged >&3 2>/dev/null || :", "/", &[], "nonce");
        assert!(script.contains("exec 3>&-"));
        assert!(script.find("exec 3>&-").unwrap() < script.find("eval ").unwrap());
    }
}
