//! Grep contract: deterministic global limiting, cancellation, and the
//! regex/case/glob/ignore/context behaviour an adapter relies on.

mod common;

use common::{engine, seed};
use hearth_core::CancelToken;
use hearth_proto::*;
use hearth_tools::{grep, grep_cancellable};
use std::time::{Duration, Instant};

/// A tree with a predictable number of matches per file, in path order.
fn tree(dir: &std::path::Path) {
    for file in 0..10 {
        let body: String =
            (0..5).map(|line| format!("needle {file}-{line}\nfiller\n")).collect();
        seed(dir, &format!("f{file:02}.txt"), &body);
    }
}

fn base(dir: &std::path::Path) -> GrepParams {
    GrepParams { mode: GrepMode::Content, ..GrepParams::new("needle", dir.display().to_string()) }
}

#[test]
fn global_limit_is_deterministic_and_takes_the_first_matches_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());

    let mut runs = Vec::new();
    for _ in 0..8 {
        // A fresh engine each time so no warm cache smooths over a scheduling
        // difference between runs.
        let eng = engine(dir.path());
        let r = grep(
            &eng,
            &GrepParams { max_total_count: Some(12), ..base(dir.path()) },
        )
        .unwrap();
        assert_eq!(r.total_matches, 12);
        assert!(r.limit_reached);
        runs.push(
            r.files
                .iter()
                .flat_map(|f| f.lines.iter().filter(|l| l.is_match).map(|l| l.text.clone()))
                .collect::<Vec<_>>(),
        );
    }

    // The first 12 matches in path order: f00 and f01 contribute 5 each, f02 two.
    let expected: Vec<String> = (0..12)
        .map(|i| format!("needle {}-{}", i / 5, i % 5))
        .collect();
    for run in &runs {
        assert_eq!(run, &expected, "the kept matches must not depend on worker interleaving");
    }
}

#[test]
fn the_global_limit_is_independent_of_the_per_file_limit() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let eng = engine(dir.path());

    // Two matches per file, six overall: three files contribute.
    let r = grep(
        &eng,
        &GrepParams { max_count: Some(2), max_total_count: Some(6), ..base(dir.path()) },
    )
    .unwrap();
    assert_eq!(r.total_matches, 6);
    assert_eq!(r.files.len(), 3);
    assert!(r.files.iter().all(|f| f.match_count == 2));
}

#[test]
fn a_limit_that_is_never_reached_is_reported_as_such() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let eng = engine(dir.path());

    let r = grep(&eng, &GrepParams { max_total_count: Some(1000), ..base(dir.path()) }).unwrap();
    assert_eq!(r.total_matches, 50);
    assert!(!r.limit_reached);
    assert_eq!(r.files_searched, 10);
}

#[test]
fn context_lines_survive_truncation_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "ctx.txt", "before\nneedle one\nafter\nbefore\nneedle two\nafter\n");
    let eng = engine(dir.path());

    let r = grep(
        &eng,
        &GrepParams {
            before_context: 1,
            after_context: 1,
            max_total_count: Some(1),
            ..base(dir.path())
        },
    )
    .unwrap();

    assert_eq!(r.total_matches, 1);
    let lines: Vec<&str> = r.files[0].lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(lines, vec!["before", "needle one", "after"]);
}

#[test]
fn cancellation_stops_the_search_and_joins_every_worker() {
    let dir = tempfile::tempdir().unwrap();
    // Enough files that a cancellation lands mid-search.
    for file in 0..400 {
        let body: String = (0..200).map(|l| format!("needle {file}-{l}\n")).collect();
        seed(dir.path(), &format!("f{file:04}.txt"), &body);
    }
    let eng = engine(dir.path());

    let cancel = CancelToken::new();
    let ticker = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        ticker.cancel();
    });

    let start = Instant::now();
    let err = grep_cancellable(&eng, &base(dir.path()), &cancel).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "cancellation must be prompt (took {:?})",
        start.elapsed()
    );
}

#[test]
fn a_pre_aborted_search_does_no_work() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let eng = engine(dir.path());
    let cancel = CancelToken::new();
    cancel.cancel();

    let err = grep_cancellable(&eng, &base(dir.path()), &cancel).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
}

#[test]
fn regex_literal_case_and_glob_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a.rs", "let Value = 1;\nlet other = 2;\n");
    seed(dir.path(), "b.txt", "let Value = 3;\n");
    let eng = engine(dir.path());
    let root = dir.path().display().to_string();

    // Regex.
    let r = grep(&eng, &GrepParams { mode: GrepMode::Content, ..GrepParams::new(r"let \w+ =", &root) })
        .unwrap();
    assert_eq!(r.total_matches, 3);

    // The same text as a literal matches nothing.
    let r = grep(
        &eng,
        &GrepParams {
            fixed_strings: true,
            mode: GrepMode::Content,
            ..GrepParams::new(r"let \w+ =", &root)
        },
    )
    .unwrap();
    assert_eq!(r.total_matches, 0);

    // Case sensitivity.
    let sensitive =
        grep(&eng, &GrepParams { mode: GrepMode::Content, ..GrepParams::new("value", &root) })
            .unwrap();
    assert_eq!(sensitive.total_matches, 0);
    let insensitive = grep(
        &eng,
        &GrepParams {
            case_insensitive: true,
            mode: GrepMode::Content,
            ..GrepParams::new("value", &root)
        },
    )
    .unwrap();
    assert_eq!(insensitive.total_matches, 2);

    // Globs.
    let globbed = grep(
        &eng,
        &GrepParams {
            globs: vec!["*.rs".into()],
            mode: GrepMode::Content,
            ..GrepParams::new("Value", &root)
        },
    )
    .unwrap();
    assert_eq!(globbed.files.len(), 1);
    assert!(globbed.files[0].path.ends_with("a.rs"));
}

#[test]
fn hidden_and_ignored_files_are_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "visible.txt", "target\n");
    seed(dir.path(), ".hidden.txt", "target\n");
    seed(dir.path(), "skipped.txt", "target\n");
    // `.ignore` rather than `.gitignore`: like ripgrep, git's ignore files only
    // apply inside a git repository, and this fixture is a bare temp directory.
    seed(dir.path(), ".ignore", "skipped.txt\n");
    let eng = engine(dir.path());
    let root = dir.path().display().to_string();

    let default = grep(&eng, &GrepParams::new("target", &root)).unwrap();
    assert_eq!(default.files.len(), 1, "hidden and ignored files are skipped by default");
    assert!(default.files[0].path.ends_with("visible.txt"));

    let with_hidden =
        grep(&eng, &GrepParams { hidden: true, ..GrepParams::new("target", &root) }).unwrap();
    assert_eq!(with_hidden.files.len(), 2);

    let everything = grep(
        &eng,
        &GrepParams { hidden: true, respect_gitignore: false, ..GrepParams::new("target", &root) },
    )
    .unwrap();
    assert_eq!(everything.files.len(), 3);
}

#[test]
fn a_file_root_searches_only_that_file_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "only.txt", "match\n");
    seed(dir.path(), "other.txt", "match\n");
    let eng = engine(dir.path());

    let r = grep(&eng, &GrepParams::new("match", &file)).unwrap();
    assert_eq!(r.files.len(), 1);
    assert!(!r.root_is_dir);
    assert_eq!(r.root, file);
}

#[test]
fn a_missing_root_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let err = grep(&eng, &GrepParams::new("x", dir.path().join("nope").display().to_string()))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
}
