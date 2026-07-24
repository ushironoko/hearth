//! An opt-in pipe-based warm-shell pool for `bash` (default off).
//!
//! Keeps a small pool of long-lived `/bin/sh` processes so a command avoids the
//! per-command process spawn. Each shell keeps persistent stdout and stderr
//! pipes. Two reader threads drain those pipes concurrently for every command,
//! preventing either stream from filling up and deadlocking the other.
//!
//! A fresh, random 128-bit nonce delimits each command on both streams. Output
//! is everything before the nonce, so output with or without a trailing newline
//! is preserved exactly. Commands run through `eval` on a single-quoted string,
//! which makes syntactically incomplete commands fail immediately instead of
//! leaving the warm shell waiting for more input. Every command runs in an
//! isolated `( … )` subshell with stdin from `/dev/null`, so cwd, environment,
//! and variable changes never leak between commands.
//!
//! On any protocol anomaly the caller falls back to a fresh spawn, so a broken
//! warm shell never returns incorrect output. A timeout kills the shell's whole
//! process group and returns the partial output collected from both pipes.
//!
//! Unlike the temp-file protocol, correctness relies on the random nonce not
//! occurring naturally in command output. The collision probability is
//! approximately 2^-128 per command.

use parking_lot::Mutex;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The outcome of a warm-shell run.
pub enum Outcome {
    /// Completed: (stdout, stderr, exit_code).
    Done(String, String, i32),
    /// Exceeded the timeout; the shell was killed. Carries partial output.
    TimedOut(String, String),
    /// Protocol anomaly — the caller must fall back to a fresh spawn.
    Retry,
}

/// A pool of warm shells, held as a per-engine extension.
pub struct WarmShellPool {
    free: Mutex<Vec<WarmShell>>,
    max: usize,
    seq: AtomicU64,
}

impl Default for WarmShellPool {
    fn default() -> Self {
        let max = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8);
        Self {
            free: Mutex::new(Vec::new()),
            max,
            seq: AtomicU64::new(0),
        }
    }
}

struct WarmShell {
    child: Child,
    stdin: ChildStdin,
    out: Option<BufReader<ChildStdout>>,
    err: Option<BufReader<ChildStderr>>,
    pgid: i32,
}

impl Drop for WarmShell {
    fn drop(&mut self) {
        // Kill the whole group (the shell + any lingering child), then reap.
        // SAFETY: killing our own child's process group by negative pgid.
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn spawn_shell() -> std::io::Result<WarmShell> {
    let mut child = Command::new("/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0) // own group so a timeout can kill the whole tree
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let out = BufReader::new(child.stdout.take().expect("piped stdout"));
    let err = BufReader::new(child.stderr.take().expect("piped stderr"));
    let pgid = child.id() as i32;
    Ok(WarmShell {
        child,
        stdin,
        out: Some(out),
        err: Some(err),
        pgid,
    })
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

enum Msg {
    Out {
        data: Vec<u8>,
        exit: Option<i32>,
        reader: BufReader<ChildStdout>,
    },
    OutEof {
        data: Vec<u8>,
    },
    Err {
        data: Vec<u8>,
        reader: BufReader<ChildStderr>,
    },
    ErrEof {
        data: Vec<u8>,
    },
}

#[derive(Default)]
struct Collected {
    out: Option<Vec<u8>>,
    err: Option<Vec<u8>>,
    exit: Option<i32>,
    out_reader: Option<BufReader<ChildStdout>>,
    err_reader: Option<BufReader<ChildStderr>>,
    broken: bool,
}

impl Collected {
    fn receive(&mut self, msg: Msg) {
        match msg {
            Msg::Out { data, exit, reader } => {
                self.out = Some(data);
                self.exit = exit;
                self.out_reader = Some(reader);
            }
            Msg::OutEof { data } => {
                self.out = Some(data);
                self.broken = true;
            }
            Msg::Err { data, reader } => {
                self.err = Some(data);
                self.err_reader = Some(reader);
            }
            Msg::ErrEof { data } => {
                self.err = Some(data);
                self.broken = true;
            }
        }
    }

    fn has_both(&self) -> bool {
        self.out.is_some() && self.err.is_some()
    }

    fn drain(&mut self, rx: &Receiver<Msg>) {
        while !self.has_both() {
            match rx.recv() {
                Ok(msg) => self.receive(msg),
                Err(_) => break,
            }
        }
    }

    fn strings(&mut self) -> (String, String) {
        let out = String::from_utf8_lossy(&self.out.take().unwrap_or_default()).into_owned();
        let err = String::from_utf8_lossy(&self.err.take().unwrap_or_default()).into_owned();
        (out, err)
    }
}

fn read_stdout(mut reader: BufReader<ChildStdout>, nonce: Vec<u8>, tx: mpsc::Sender<Msg>) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 65_536];
    let mut marker = None;

    loop {
        if let Some(pos) = marker {
            let suffix = &buf[pos + nonce.len()..];
            if let Some(newline) = memchr::memchr(b'\n', suffix) {
                let code = std::str::from_utf8(&suffix[..newline])
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(-1);
                let _ = tx.send(Msg::Out {
                    data: buf[..pos].to_vec(),
                    exit: Some(code),
                    reader,
                });
                return;
            }
        }

        let prev_len = buf.len();
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Msg::OutEof { data: buf });
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if marker.is_none() {
                    let overlap = nonce.len().saturating_sub(1);
                    let search_from = prev_len.saturating_sub(overlap);
                    marker =
                        memchr::memmem::find(&buf[search_from..], &nonce).map(|p| search_from + p);
                }
            }
        }
    }
}

fn read_stderr(mut reader: BufReader<ChildStderr>, nonce: Vec<u8>, tx: mpsc::Sender<Msg>) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 65_536];

    loop {
        let prev_len = buf.len();
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Msg::ErrEof { data: buf });
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let overlap = nonce.len().saturating_sub(1);
                let search_from = prev_len.saturating_sub(overlap);
                if let Some(pos) =
                    memchr::memmem::find(&buf[search_from..], &nonce).map(|p| search_from + p)
                {
                    let _ = tx.send(Msg::Err {
                        data: buf[..pos].to_vec(),
                        reader,
                    });
                    return;
                }
            }
        }
    }
}

impl WarmShellPool {
    /// Run a command on a warm shell. Returns [`Outcome::Retry`] if the caller
    /// should fall back to spawning a fresh shell.
    pub fn run(
        &self,
        command: &str,
        cwd: &str,
        env: &[(String, String)],
        timeout: Duration,
    ) -> Outcome {
        let mut shell = match self.free.lock().pop() {
            Some(s) => s,
            None => match spawn_shell() {
                Ok(s) => s,
                Err(_) => return Outcome::Retry,
            },
        };

        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let r_hi = RandomState::new().build_hasher().finish();
        let r_lo = RandomState::new().build_hasher().finish();
        let nonce = format!("__HEARTH_{pid}_{n}_{r_hi:016x}{r_lo:016x}__");

        let outcome = self.run_once(&mut shell, command, cwd, env, timeout, &nonce);

        match &outcome {
            Outcome::Done(..) => {
                // Healthy: return it to the pool (bounded).
                let mut free = self.free.lock();
                if free.len() < self.max {
                    free.push(shell);
                }
                // else: drop (Drop kills + reaps).
            }
            // Killed / broken: drop the shell (its Drop kills the group + reaps).
            Outcome::TimedOut(..) | Outcome::Retry => {}
        }
        outcome
    }

    fn run_once(
        &self,
        shell: &mut WarmShell,
        command: &str,
        cwd: &str,
        env: &[(String, String)],
        timeout: Duration,
        nonce: &str,
    ) -> Outcome {
        let mut script = String::with_capacity(command.len() + 192);
        script.push_str("( cd ");
        script.push_str(&sh_quote(cwd));
        script.push_str(" && { ");
        for (k, v) in env {
            script.push_str("export ");
            script.push_str(k);
            script.push('=');
            script.push_str(&sh_quote(v));
            script.push_str("; ");
        }
        script.push_str("eval ");
        script.push_str(&sh_quote(command));
        script.push_str("; } ) </dev/null\n");
        script.push_str("__hec=$?\n");
        script.push_str("printf '%s %d\\n' ");
        script.push_str(&sh_quote(nonce));
        script.push_str(" \"$__hec\"\n");
        script.push_str("printf '%s\\n' ");
        script.push_str(&sh_quote(nonce));
        script.push_str(" 1>&2\n");

        if shell.stdin.write_all(script.as_bytes()).is_err() || shell.stdin.flush().is_err() {
            return Outcome::Retry;
        }

        let out = shell.out.take().expect("stdout reader present");
        let err = shell.err.take().expect("stderr reader present");
        let (tx, rx) = mpsc::channel();
        let out_tx = tx.clone();
        let out_nonce = nonce.as_bytes().to_vec();
        let err_nonce = out_nonce.clone();
        let out_handle = std::thread::spawn(move || read_stdout(out, out_nonce, out_tx));
        let err_handle = std::thread::spawn(move || read_stderr(err, err_nonce, tx));

        let deadline = Instant::now() + timeout;
        let mut collected = Collected::default();
        let mut timed_out = false;

        while !collected.has_both() && !collected.broken {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(msg) => collected.receive(msg),
                Err(RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    collected.broken = true;
                    break;
                }
            }
        }

        if timed_out || collected.broken {
            // SAFETY: kill our own child's process group so both pipe readers
            // hit EOF and can always be drained and joined.
            unsafe { libc::kill(-shell.pgid, libc::SIGKILL) };
            collected.drain(&rx);
        }

        let _ = out_handle.join();
        let _ = err_handle.join();

        if timed_out {
            let (out, err) = collected.strings();
            return Outcome::TimedOut(out, err);
        }
        if collected.broken || !collected.has_both() {
            return Outcome::Retry;
        }

        shell.out = collected.out_reader.take();
        shell.err = collected.err_reader.take();
        let exit = collected.exit.unwrap_or(-1);
        let (out, err) = collected.strings();
        Outcome::Done(out, err, exit)
    }
}

