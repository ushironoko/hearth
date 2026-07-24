//! The `bash` tool.
//!
//! Default path: spawn a fresh `/bin/sh -c` per command in its own process
//! group, drain both pipes on reader threads (no output-size deadlock), and
//! enforce a hard timeout by killing the whole process group. Always correct.
//!
//! Opt-in path (`EngineConfig::warm_shell`): run on a pooled warm shell
//! (`crate::shell`) to skip the per-command spawn, falling back to a fresh spawn
//! on any protocol anomaly.

use crate::shell::{Outcome, WarmShellPool};
use hearth_core::{profile, Engine};
use hearth_proto::{BashParams, BashResult, ErrorKind, ToolError, ToolResult};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

pub fn bash(engine: &Engine, params: &BashParams) -> ToolResult<BashResult> {
    profile!("tool.bash", {
        let cwd = params
            .cwd
            .clone()
            .unwrap_or_else(|| engine.config().default_cwd.display().to_string());
        let timeout = Duration::from_millis(
            params.timeout_ms.unwrap_or(engine.config().bash_timeout_ms).max(1),
        );
        let start = Instant::now();

        // Opt-in warm-shell fast path (falls back to spawn on any anomaly).
        if engine.config().warm_shell {
            let pool = engine.extension::<WarmShellPool>();
            match pool.run(&params.command, &cwd, &params.env, timeout) {
                Outcome::Done(stdout, stderr, exit_code) => {
                    return Ok(BashResult {
                        stdout,
                        stderr,
                        exit_code,
                        timed_out: false,
                        duration_us: start.elapsed().as_micros() as u64,
                    });
                }
                Outcome::TimedOut(stdout, stderr) => {
                    return Ok(BashResult {
                        stdout,
                        stderr,
                        exit_code: -1,
                        timed_out: true,
                        duration_us: start.elapsed().as_micros() as u64,
                    });
                }
                Outcome::Retry => {} // fall through to a fresh spawn
            }
        }

        spawn_bash(&cwd, &params.command, &params.env, timeout, start)
    })
}

/// Always-correct spawn-per-command path (also the warm-path fallback).
fn spawn_bash(
    cwd: &str,
    command: &str,
    env: &[(String, String)],
    timeout: Duration,
    start: Instant,
) -> ToolResult<BashResult> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::new(ErrorKind::Io, format!("failed to spawn shell: {e}")))?;
    let pid = child.id() as i32;

    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = tx.send(());
        status
    });

    let timed_out = match rx.recv_timeout(timeout) {
        Ok(()) => false,
        Err(RecvTimeoutError::Timeout) => {
            // SAFETY: kill(2) on our own child's process group; -pid targets the group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            true
        }
        Err(RecvTimeoutError::Disconnected) => false,
    };

    let status = waiter.join().ok().flatten();
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();

    let exit_code = match status {
        Some(s) => s.code().unwrap_or(-1),
        None => -1,
    };

    Ok(BashResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        timed_out,
        duration_us: start.elapsed().as_micros() as u64,
    })
}
