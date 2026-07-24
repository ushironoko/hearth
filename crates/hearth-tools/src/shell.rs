//! An opt-in warm-shell pool for `bash` (default off).
//!
//! Keeps a small pool of long-lived `/bin/sh` processes so a command avoids the
//! ~1–2 ms per-command process spawn. Robustness comes from a deliberately
//! simple protocol: the command's stdout **and** stderr are redirected to temp
//! files, so the shell's control pipe carries only a single unambiguous marker
//! line (`<nonce> <exit_code>`). That removes every classic warm-shell hazard —
//! stdout/stderr marker collision, the trailing-newline problem, and the
//! read-both-pipes deadlock. Each command runs in a `( … )` subshell with stdin
//! from `/dev/null`, so cwd/env/variable changes never leak between commands
//! (parity with spawn-per-command).
//!
//! On any anomaly (shell died, write failed, we can't spawn) the caller falls
//! back to a fresh spawn, so a broken warm shell never returns wrong output.
//! A timeout kills the shell's whole process group and discards it.
//!
//! Known limitation: a syntactically incomplete command (e.g. an unbalanced
//! quote) leaves the warm shell waiting for input and will hit the timeout; the
//! always-correct spawn path (the default) does not have this property.

use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

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
        let max = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        Self { free: Mutex::new(Vec::new()), max, seq: AtomicU64::new(0) }
    }
}

struct WarmShell {
    child: Child,
    stdin: ChildStdin,
    out: Option<BufReader<ChildStdout>>,
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
        .stderr(Stdio::null())
        .process_group(0) // own group so a timeout can kill the whole tree
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let out = BufReader::new(child.stdout.take().expect("piped stdout"));
    let pgid = child.id() as i32;
    Ok(WarmShell { child, stdin, out: Some(out), pgid })
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
        let nonce = format!("__HEARTH_{pid}_{n}__");
        let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
        let out_file = tmp.join(format!(".hearth-sh-{pid}-{n}.out"));
        let err_file = tmp.join(format!(".hearth-sh-{pid}-{n}.err"));

        let outcome = self.run_once(&mut shell, command, cwd, env, timeout, &nonce, &out_file, &err_file);

        let _ = std::fs::remove_file(&out_file);
        let _ = std::fs::remove_file(&err_file);

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

    #[allow(clippy::too_many_arguments)]
    fn run_once(
        &self,
        shell: &mut WarmShell,
        command: &str,
        cwd: &str,
        env: &[(String, String)],
        timeout: Duration,
        nonce: &str,
        out_file: &std::path::Path,
        err_file: &std::path::Path,
    ) -> Outcome {
        let mut script = String::with_capacity(command.len() + 128);
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
        script.push_str(command);
        script.push_str("; } ) </dev/null >");
        script.push_str(&sh_quote(&out_file.to_string_lossy()));
        script.push_str(" 2>");
        script.push_str(&sh_quote(&err_file.to_string_lossy()));
        script.push('\n');
        script.push_str("printf '%s %d\\n' ");
        script.push_str(&sh_quote(nonce));
        script.push_str(" \"$?\"\n");

        if shell.stdin.write_all(script.as_bytes()).is_err() || shell.stdin.flush().is_err() {
            return Outcome::Retry;
        }

        // The control pipe carries only the marker line. Read it on a thread so
        // we can time out; hand the reader back on success.
        let mut out = shell.out.take().expect("reader present");
        let (tx, rx) = mpsc::channel();
        let nonce_owned = nonce.to_string();
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match out.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send((None, out));
                        return;
                    }
                    Ok(_) => {
                        if let Some(rest) = line.strip_prefix(&nonce_owned) {
                            let code = rest.trim().parse::<i32>().unwrap_or(-1);
                            let _ = tx.send((Some(code), out));
                            return;
                        }
                        // ignore anything that isn't the marker (shouldn't occur)
                    }
                    Err(_) => {
                        let _ = tx.send((None, out));
                        return;
                    }
                }
            }
        });

        let read_file = |p: &std::path::Path| -> String {
            std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
        };

        match rx.recv_timeout(timeout) {
            Ok((Some(code), out)) => {
                shell.out = Some(out);
                Outcome::Done(read_file(out_file), read_file(err_file), code)
            }
            // Shell hit EOF before the marker → it died; fall back to spawn.
            Ok((None, _)) => Outcome::Retry,
            Err(RecvTimeoutError::Timeout) => {
                // SAFETY: kill our own child's process group; the reader thread
                // then sees EOF and exits, dropping the moved-out reader.
                unsafe { libc::kill(-shell.pgid, libc::SIGKILL) };
                Outcome::TimedOut(read_file(out_file), read_file(err_file))
            }
            Err(_) => Outcome::Retry,
        }
    }
}
