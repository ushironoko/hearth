//! Bash contract: ordered streaming, cancellation, process-tree cleanup,
//! configurable shells, and the warm pool's at-most-once guarantee.

mod common;

use common::{engine, trusting_engine, warm_engine};
use hearth_core::CancelToken;
use hearth_proto::*;
use hearth_tools::{bash, bash_cancellable, bash_stream};
use std::time::{Duration, Instant};

fn collect(
    eng: &hearth_core::Engine,
    params: &BashParams,
    cancel: &CancelToken,
) -> (BashResult, Vec<BashChunk>) {
    let mut chunks = Vec::new();
    // The closure is a named local so its borrow of `chunks` ends with this
    // block rather than at the end of the function.
    let result = {
        let mut push = |c| chunks.push(c);
        bash_stream(eng, params, cancel, &mut push).unwrap()
    };
    (result, chunks)
}

fn channel_text(chunks: &[BashChunk], channel: BashChannel) -> String {
    chunks
        .iter()
        .filter(|c| c.channel == channel)
        .map(|c| c.text.as_str())
        .collect()
}

/// Poll until `pid` is gone, or give up. A killed process that is not our child
/// is reaped by init, so the check has to tolerate a short delay.
fn wait_for_exit(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        // SAFETY: signal 0 only probes for the process's existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn dispatched_bash_globally_invalidates_trusted_file_cache() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let inside = dir.path().join("inside.txt");
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("outside.txt");
    std::fs::write(&inside, "old-inside").unwrap();
    std::fs::write(&outside, "old-outside").unwrap();

    for path in [&inside, &outside] {
        hearth_tools::read(&eng, &ReadParams::new(path.display().to_string())).unwrap();
    }
    assert_eq!(eng.files().len(), 2);

    let command = format!(
        "printf new-inside > '{}'; printf new-outside > '{}'",
        inside.display(),
        outside.display()
    );
    bash(&eng, &BashParams::new(command)).unwrap();

    assert_eq!(
        eng.files().len(),
        0,
        "Bash must clear cwd-external state too"
    );
    let inside_read =
        hearth_tools::read(&eng, &ReadParams::new(inside.display().to_string())).unwrap();
    let outside_read =
        hearth_tools::read(&eng, &ReadParams::new(outside.display().to_string())).unwrap();
    assert_eq!(inside_read.content, "new-inside");
    assert_eq!(outside_read.content, "new-outside");
}

#[test]
fn a_pre_cancelled_bash_preserves_caches_because_nothing_was_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let path = dir.path().join("cached.txt");
    std::fs::write(&path, "cached").unwrap();
    hearth_tools::read(&eng, &ReadParams::new(path.display().to_string())).unwrap();
    assert_eq!(eng.files().len(), 1);

    let cancel = CancelToken::new();
    cancel.cancel();
    let err = bash_cancellable(&eng, &BashParams::new("exit 0"), &cancel).unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(eng.files().len(), 1);
}

#[test]
fn chunks_reconstruct_the_final_output_with_one_monotonic_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let params =
        BashParams::new("for i in 1 2 3; do printf 'out%s\\n' $i; printf 'err%s\\n' $i 1>&2; done");
    let (r, chunks) = collect(&eng, &params, &CancelToken::none());

    assert_eq!(r.exit_code, 0);
    assert_eq!(channel_text(&chunks, BashChannel::Stdout), r.stdout);
    assert_eq!(channel_text(&chunks, BashChannel::Stderr), r.stderr);
    assert_eq!(
        r.chunks,
        chunks.len() as u64,
        "the result must agree on how many chunks it sent"
    );

    let seqs: Vec<u64> = chunks.iter().map(|c| c.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "sequence must be strictly increasing"
    );
    assert_eq!(seqs.first().copied(), Some(1));
}

#[test]
fn output_without_a_trailing_newline_is_preserved_exactly() {
    let dir = tempfile::tempdir().unwrap();
    for eng in [engine(dir.path()), warm_engine(dir.path())] {
        let r = bash(&eng, &BashParams::new("printf 'no newline'")).unwrap();
        assert_eq!(r.stdout, "no newline");
        assert!(r.stderr.is_empty());
    }
}

#[test]
fn large_output_on_both_pipes_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    for eng in [engine(dir.path()), warm_engine(dir.path())] {
        let r = bash(
            &eng,
            &BashParams {
                timeout_ms: Some(60_000),
                ..BashParams::new("seq 50000; seq 50000 1>&2")
            },
        )
        .unwrap();
        assert_eq!(r.stdout.lines().count(), 50_000);
        assert_eq!(r.stderr.lines().count(), 50_000);
        assert_eq!(r.exit_code, 0);
    }
}

#[test]
fn non_zero_exit_is_reported_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    for eng in [engine(dir.path()), warm_engine(dir.path())] {
        let r = bash(
            &eng,
            &BashParams::new("printf out; printf err 1>&2; exit 42"),
        )
        .unwrap();
        assert_eq!(r.exit_code, 42);
        assert_eq!(r.stdout, "out");
        assert_eq!(r.stderr, "err");
        assert!(!r.timed_out && !r.aborted);
    }
}

#[test]
fn timeout_keeps_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    for eng in [engine(dir.path()), warm_engine(dir.path())] {
        let r = bash(
            &eng,
            &BashParams {
                timeout_ms: Some(300),
                ..BashParams::new("printf partial; sleep 30")
            },
        )
        .unwrap();
        assert!(r.timed_out);
        assert!(!r.aborted);
        assert_eq!(
            r.stdout, "partial",
            "output produced before the timeout must survive"
        );
    }
}

#[test]
fn a_pre_aborted_call_never_starts_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let marker = dir.path().join("ran");
    let cancel = CancelToken::new();
    cancel.cancel();

    let err = bash_cancellable(
        &eng,
        &BashParams::new(format!("touch {}", marker.display())),
        &cancel,
    )
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!marker.exists(), "a pre-aborted call must not run anything");
}

#[test]
fn abort_keeps_partial_output_and_kills_the_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let pidfile = dir.path().join("pid");

    let cancel = CancelToken::new();
    let ticker = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        ticker.cancel();
    });

    let command = format!(
        "sleep 30 & echo $! > {}; printf started; wait",
        pidfile.display()
    );
    let (r, chunks) = collect(
        &eng,
        &BashParams {
            timeout_ms: Some(30_000),
            ..BashParams::new(command)
        },
        &cancel,
    );

    assert!(r.aborted, "the result must report the abort");
    assert!(!r.timed_out);
    assert_eq!(
        r.stdout, "started",
        "output produced before the abort must survive"
    );
    assert_eq!(channel_text(&chunks, BashChannel::Stdout), "started");

    let grandchild: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .expect("child pid");
    assert!(
        wait_for_exit(grandchild, Duration::from_secs(5)),
        "no process may outlive the settled abort"
    );
}

#[test]
fn a_detached_descendant_holding_the_pipes_does_not_hang_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let start = Instant::now();

    // The shell exits immediately, but `sleep` inherits its stdout and keeps
    // the pipe open far longer than the call should wait.
    let r = bash(
        &eng,
        &BashParams {
            timeout_ms: Some(30_000),
            ..BashParams::new("sleep 20 & printf done")
        },
    )
    .unwrap();

    assert_eq!(r.stdout, "done");
    assert!(!r.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the call must not wait for a detached descendant (took {:?})",
        start.elapsed()
    );
}

#[test]
fn marker_like_output_is_not_mistaken_for_the_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());
    // Output that mimics the delimiter's shape, plus a plausible control line.
    let r = bash(
        &eng,
        &BashParams::new(
            "printf '__HEARTH_1_2_abc__\\nP 999\\nX 7\\n'; printf '__HEARTH_x__' 1>&2",
        ),
    )
    .unwrap();
    assert_eq!(r.stdout, "__HEARTH_1_2_abc__\nP 999\nX 7\n");
    assert_eq!(r.stderr, "__HEARTH_x__");
    assert_eq!(
        r.exit_code, 0,
        "the real exit code must come from the control channel"
    );
}

#[test]
fn warm_shell_reuse_isolates_commands_from_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());

    // A background writer must not reach the next command's output.
    let first = bash(
        &eng,
        &BashParams::new("printf first; (sleep 0.3; printf LEAK) &"),
    )
    .unwrap();
    assert_eq!(first.exit_code, 0);
    assert!(!first.stdout.contains("LEAK"));

    // Environment and variables must not survive either.
    bash(&eng, &BashParams::new("export CARRIED=yes; MYVAR=1")).unwrap();
    let second = bash(
        &eng,
        &BashParams::new("printf 'second:%s:%s' \"$CARRIED\" \"$MYVAR\""),
    )
    .unwrap();
    assert_eq!(second.stdout, "second::");

    std::thread::sleep(Duration::from_millis(500));
    let third = bash(&eng, &BashParams::new("printf third")).unwrap();
    assert_eq!(
        third.stdout, "third",
        "a retired background writer must not leak in later"
    );
}

#[test]
fn an_ambiguous_warm_shell_failure_is_reported_and_never_retried() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());
    let counter = dir.path().join("runs");

    // Killing the pooled shell mid-command destroys the protocol *after* the
    // command was dispatched: Hearth cannot know whether it completed.
    let command = format!("echo x >> {}; kill -9 $$", counter.display());
    let err = bash(
        &eng,
        &BashParams {
            timeout_ms: Some(5000),
            ..BashParams::new(command)
        },
    )
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::Indeterminate);
    let runs = std::fs::read_to_string(&counter)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(runs, 1, "an indeterminate command must not be re-executed");

    // The pool recovers for the next command.
    let ok = bash(&eng, &BashParams::new("printf recovered")).unwrap();
    assert_eq!(ok.stdout, "recovered");
}

#[test]
fn the_shell_and_its_transport_are_configurable() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());

    let via_arg = bash(
        &eng,
        &BashParams {
            shell: Some(ShellSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into()],
                transport: ShellTransport::Arg,
            }),
            ..BashParams::new("printf arg-transport")
        },
    )
    .unwrap();
    assert_eq!(via_arg.stdout, "arg-transport");

    let via_stdin = bash(
        &eng,
        &BashParams {
            shell: Some(ShellSpec {
                program: "/bin/sh".into(),
                args: vec!["-s".into()],
                transport: ShellTransport::Stdin,
            }),
            ..BashParams::new("printf stdin-transport")
        },
    )
    .unwrap();
    assert_eq!(via_stdin.stdout, "stdin-transport");
}

#[test]
fn cwd_and_environment_are_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let eng = engine(dir.path());

    let r = bash(
        &eng,
        &BashParams {
            cwd: Some(sub.display().to_string()),
            env: vec![("HEARTH_TEST".into(), "on".into())],
            ..BashParams::new("printf '%s' \"$HEARTH_TEST\"; pwd")
        },
    )
    .unwrap();
    assert!(r.stdout.starts_with("on"));
    assert!(
        r.stdout
            .contains(sub.file_name().unwrap().to_str().unwrap())
    );

    // With the environment cleared, only what the call passes is visible.
    let cleared = bash(
        &eng,
        &BashParams {
            env: vec![("ONLY".into(), "this".into())],
            env_clear: true,
            ..BashParams::new("printf '%s|%s' \"$ONLY\" \"$HOME\"")
        },
    )
    .unwrap();
    assert_eq!(cleared.stdout, "this|");
}

#[test]
fn warm_shell_mode_honours_env_clear_via_fresh_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());

    let cleared = bash(
        &eng,
        &BashParams {
            env: vec![("ONLY".into(), "this".into())],
            env_clear: true,
            ..BashParams::new("printf '%s|%s' \"$ONLY\" \"$HOME\"")
        },
    )
    .unwrap();

    assert_eq!(cleared.stdout, "this|");
}

#[test]
fn extreme_timeout_is_clamped_before_deadline_arithmetic() {
    let dir = tempfile::tempdir().unwrap();
    for eng in [engine(dir.path()), warm_engine(dir.path())] {
        let result = bash(
            &eng,
            &BashParams {
                timeout_ms: Some(u64::MAX),
                ..BashParams::new("printf bounded")
            },
        )
        .unwrap();
        assert_eq!(result.stdout, "bounded");
        assert_eq!(result.exit_code, 0);
    }
}

#[test]
fn collect_output_can_be_turned_off_for_a_streaming_caller() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let params = BashParams {
        collect_output: false,
        ..BashParams::new("printf streamed")
    };
    let (r, chunks) = collect(&eng, &params, &CancelToken::none());

    assert!(
        r.stdout.is_empty(),
        "the result must not hold a second copy"
    );
    assert_eq!(channel_text(&chunks, BashChannel::Stdout), "streamed");
}
