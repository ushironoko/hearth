//! End-to-end coverage of the five tools against one engine.

mod common;

use common::{abs, engine, trusting_engine, warm_engine};
use hearth_proto::*;
use hearth_tools::{bash, edit, grep, read, write};

fn run(eng: &hearth_core::Engine, command: &str, timeout_ms: Option<u64>) -> BashResult {
    bash(eng, &BashParams { timeout_ms, ..BashParams::new(command) }).unwrap()
}

#[test]
fn write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = abs(dir.path(), "a.txt");

    let w = write(&eng, &WriteParams::new(path.clone(), "hello\nworld\n")).unwrap();
    assert_eq!(w.bytes_written, 12);
    assert!(!w.existed);

    let r = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert_eq!(r.content, "hello\nworld\n");
    assert_eq!(r.total_lines, 2);
    // Second read is a warm cache hit.
    let r2 = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert!(r2.cache_hit, "second read should hit the warm cache");
}

#[test]
fn read_window_and_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = abs(dir.path(), "n.txt");
    write(&eng, &WriteParams::new(path.clone(), "l1\nl2\nl3\nl4\nl5\n")).unwrap();

    let r = read(
        &eng,
        &ReadParams { offset: Some(2), limit: Some(2), line_numbers: true, ..ReadParams::new(&path) },
    )
    .unwrap();
    assert!(r.truncated);
    assert_eq!(r.returned_lines, 2);
    assert!(r.content.contains("     2\tl2"));
    assert!(r.content.contains("     3\tl3"));
    assert!(!r.content.contains("l1"));
}

#[test]
fn edit_unique_and_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = abs(dir.path(), "e.txt");
    write(&eng, &WriteParams::new(path.clone(), "foo bar foo\n")).unwrap();

    // Non-unique without replace_all → error.
    let err = edit(
        &eng,
        &EditParams {
            path: path.clone(),
            old_string: "foo".into(),
            new_string: "baz".into(),
            replace_all: false,
        },
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::MultipleMatches);

    let ok = edit(
        &eng,
        &EditParams {
            path: path.clone(),
            old_string: "foo".into(),
            new_string: "baz".into(),
            replace_all: true,
        },
    )
    .unwrap();
    assert_eq!(ok.replacements, 2);

    let r = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert_eq!(r.content, "baz bar baz\n");
}

#[test]
fn grep_content_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    write(&eng, &WriteParams::new(abs(dir.path(), "x.rs"), "fn main() {}\nlet x = 1;\n")).unwrap();
    write(&eng, &WriteParams::new(abs(dir.path(), "y.rs"), "fn helper() {}\n")).unwrap();
    write(&eng, &WriteParams::new(abs(dir.path(), "z.txt"), "no functions here\n")).unwrap();

    let base = GrepParams {
        globs: vec!["*.rs".into()],
        ..GrepParams::new("fn ", dir.path().display().to_string())
    };

    let g = grep(&eng, &GrepParams { mode: GrepMode::Content, ..base.clone() }).unwrap();
    assert_eq!(g.total_matches, 2);
    assert_eq!(g.files.len(), 2);
    assert!(g.root_is_dir);

    // Second grep over the same tree → warm walk cache hit.
    let g2 = grep(&eng, &GrepParams { mode: GrepMode::FilesWithMatches, ..base }).unwrap();
    assert!(g2.walk_cache_hit, "second grep should reuse the walk cache");
    assert_eq!(g2.files.len(), 2);
}

#[test]
fn trust_cache_stays_coherent_for_self_writes() {
    // In trust_cache mode warm hits skip the freshness stat; writes/edits through
    // Hearth must still be observed because they refresh the cache in place.
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let path = abs(dir.path(), "t.txt");

    write(&eng, &WriteParams::new(path.clone(), "one\n")).unwrap();
    let r1 = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert_eq!(r1.content, "one\n");

    // Edit through Hearth → cache refreshed → trust read sees the new content.
    edit(
        &eng,
        &EditParams {
            path: path.clone(),
            old_string: "one".into(),
            new_string: "two".into(),
            replace_all: false,
        },
    )
    .unwrap();
    let r2 = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert_eq!(r2.content, "two\n");
    assert!(r2.cache_hit);

    // Overwrite through Hearth → still coherent.
    write(
        &eng,
        &WriteParams { create_dirs: false, ..WriteParams::new(path.clone(), "three\n") },
    )
    .unwrap();
    let r3 = read(&eng, &ReadParams::new(path.clone())).unwrap();
    assert_eq!(r3.content, "three\n");
}

#[test]
fn warm_shell_correctness() {
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());

    // 1. stdout + exit 0
    let r = run(&eng, "echo hi", None);
    assert_eq!(r.stdout, "hi\n");
    assert_eq!(r.exit_code, 0);
    assert!(!r.timed_out);

    // 2. stderr + non-zero exit
    let r = run(&eng, "printf err 1>&2; exit 3", None);
    assert_eq!(r.stderr, "err");
    assert_eq!(r.exit_code, 3);

    // 3. multiline command
    let r = run(&eng, "echo a\necho b", None);
    assert_eq!(r.stdout, "a\nb\n");

    // 4. special characters (balanced quotes)
    let r = run(&eng, "printf '%s' '}{;#`'", None);
    assert_eq!(r.stdout, "}{;#`");

    // 5. cwd isolation across calls (subshell must not leak cwd)
    run(&eng, "cd /tmp", None);
    let r = run(&eng, "pwd", None);
    assert_ne!(r.stdout.trim(), "/tmp", "cwd must not leak between warm-shell commands");

    // 6. large output
    let r = run(&eng, "seq 200000", None);
    assert_eq!(r.stdout.lines().count(), 200000);
    assert_eq!(r.exit_code, 0);

    // 7. a command that reads stdin must not hang (stdin is /dev/null)
    let r = run(&eng, "cat", Some(2000));
    assert_eq!(r.stdout, "");
    assert!(!r.timed_out);

    // 8. timeout kills the shell, and the pool recovers for the next command
    let r = run(&eng, "sleep 5", Some(200));
    assert!(r.timed_out);
    let r = run(&eng, "echo recovered", None);
    assert_eq!(r.stdout, "recovered\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn warm_shell_incomplete_command_fast_fail() {
    // A syntactically incomplete command (unbalanced quote) must fail fast via
    // `eval`, not block until the timeout, and the pool must stay healthy.
    let dir = tempfile::tempdir().unwrap();
    let eng = warm_engine(dir.path());
    let start = std::time::Instant::now();
    let r = run(&eng, "echo \"foo", Some(5000));
    assert!(!r.timed_out, "incomplete command must NOT hit the timeout");
    assert_ne!(r.exit_code, 0, "incomplete command must fail with a non-zero exit");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(2000),
        "incomplete command must fail fast, well under the 5s timeout"
    );
    let r2 = run(&eng, "echo ok", None);
    assert_eq!(r2.stdout, "ok\n");
    assert_eq!(r2.exit_code, 0);
}

#[test]
fn bash_runs_and_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());

    let ok = run(&eng, "printf 'hi'; printf 'err' 1>&2; exit 3", None);
    assert_eq!(ok.stdout, "hi");
    assert_eq!(ok.stderr, "err");
    assert_eq!(ok.exit_code, 3);
    assert!(!ok.timed_out);

    let to = run(&eng, "sleep 5", Some(150));
    assert!(to.timed_out);
}
