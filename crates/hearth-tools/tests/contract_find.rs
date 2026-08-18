//! Find contract: pi-compatible glob semantics over the resident walk cache.

mod common;

use common::{engine, seed};
use hearth_core::{CancelToken, Engine, EngineConfig};
use hearth_proto::{ErrorKind, FindParams};
use hearth_tools::{find, find_cancellable};
use std::path::{Path, PathBuf};

fn params(root: &Path, pattern: &str) -> FindParams {
    FindParams {
        path: root.display().to_string(),
        respect_gitignore: false,
        ..FindParams::new(pattern)
    }
}

#[test]
fn basename_globs_return_files_directories_and_hidden_entries_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a.txt", "a");
    seed(dir.path(), "src/b.txt", "b");
    seed(dir.path(), ".hidden", "h");
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let eng = engine(dir.path());

    let result = find(&eng, &params(dir.path(), "*")).unwrap();

    assert_eq!(
        result.paths,
        vec![".hidden", "a.txt", "empty/", "src/", "src/b.txt"]
    );
    assert_eq!(result.total_matches, 5);
    assert!(!result.limit_reached);
    assert!(!result.output_limit_reached);
    assert!(!result.walk_cache_hit);
}

#[test]
fn slash_globs_use_pi_full_path_semantics_and_literal_separators() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "root.rs", "");
    seed(dir.path(), "src/a.rs", "");
    seed(dir.path(), "src/nested/b.rs", "");
    let eng = engine(dir.path());

    let direct = find(&eng, &params(dir.path(), "src/*.rs")).unwrap();
    assert_eq!(direct.paths, vec!["src/a.rs"]);

    let recursive = find(&eng, &params(dir.path(), "src/**/*.rs")).unwrap();
    assert_eq!(recursive.paths, vec!["src/a.rs", "src/nested/b.rs"]);

    let all = find(&eng, &params(dir.path(), "**/*.rs")).unwrap();
    assert_eq!(all.paths, vec!["root.rs", "src/a.rs", "src/nested/b.rs"]);

    // fd's glob mode is smart-case and treats an empty pattern as match-all.
    seed(dir.path(), "README.md", "");
    eng.invalidate_root(dir.path());
    assert_eq!(
        find(&eng, &params(dir.path(), "readme.md")).unwrap().paths,
        vec!["README.md"]
    );
    assert!(
        find(&eng, &params(dir.path(), "READme.md"))
            .unwrap()
            .paths
            .is_empty()
    );
    assert_eq!(
        find(&eng, &params(dir.path(), "")).unwrap().total_matches,
        6
    );
    assert!(
        find(&eng, &params(dir.path(), "src/"))
            .unwrap()
            .paths
            .is_empty(),
        "fd does not decorate directories before full-path matching"
    );

    // pi prefixes a slash-containing relative pattern with **/ and matches the
    // absolute candidate. The root component itself can therefore satisfy it.
    let src_root = find(&eng, &params(&dir.path().join("src"), "src/**/*.rs")).unwrap();
    assert_eq!(src_root.paths, vec!["a.rs", "nested/b.rs"]);
}

#[test]
fn exclusions_are_applied_before_count_and_result_limits() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), ".git/config", "x");
    seed(dir.path(), "node_modules/pkg/index.js", "x");
    seed(dir.path(), "src/a.js", "x");
    seed(dir.path(), "src/b.js", "x");
    let eng = engine(dir.path());
    let mut p = params(dir.path(), "*.js");
    p.limit = Some(2);
    p.exclude_globs = vec!["**/.git/**".into(), "**/node_modules/**".into()];

    let result = find(&eng, &p).unwrap();

    assert_eq!(result.paths, vec!["src/a.js", "src/b.js"]);
    assert_eq!(result.total_matches, 2);
    assert!(!result.limit_reached);

    let all = find(
        &eng,
        &FindParams {
            exclude_globs: p.exclude_globs,
            ..params(dir.path(), "*")
        },
    )
    .unwrap();
    assert!(!all.paths.iter().any(|path| path.starts_with(".git")));
    assert!(
        !all.paths
            .iter()
            .any(|path| path.starts_with("node_modules"))
    );
}

#[test]
fn zero_count_limit_still_reports_the_exact_total() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a", "");
    seed(dir.path(), "b", "");
    let eng = engine(dir.path());
    let mut p = params(dir.path(), "*");
    p.limit = Some(0);

    let result = find(&eng, &p).unwrap();

    assert!(result.paths.is_empty());
    assert_eq!(result.total_matches, 2);
    assert!(result.limit_reached);
    assert!(!result.output_limit_reached);
}

#[test]
fn path_text_is_bounded_to_pis_fifty_kibibyte_budget() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..240 {
        seed(dir.path(), &format!("{i:03}-{}.txt", "x".repeat(230)), "");
    }
    let eng = engine(dir.path());
    let mut p = params(dir.path(), "*.txt");
    p.limit = Some(1000);

    let result = find(&eng, &p).unwrap();
    let output_bytes =
        result.paths.iter().map(String::len).sum::<usize>() + result.paths.len().saturating_sub(1);

    assert_eq!(result.total_matches, 240);
    assert!(result.output_limit_reached);
    assert!(!result.limit_reached);
    assert!(
        output_bytes > 50 * 1024,
        "one complete crossing path lets Pi detect and report truncation"
    );
    let prefix_bytes = result.paths[..result.paths.len() - 1]
        .iter()
        .map(String::len)
        .sum::<usize>()
        + result.paths.len().saturating_sub(2);
    assert!(prefix_bytes <= 50 * 1024);
}

#[test]
fn positive_and_default_count_limits_keep_the_sorted_prefix() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..1001 {
        seed(dir.path(), &format!("f{i:04}.txt"), "");
    }
    let eng = engine(dir.path());

    let defaulted = find(&eng, &params(dir.path(), "*.txt")).unwrap();
    assert_eq!(defaulted.paths.len(), 1000);
    assert_eq!(defaulted.total_matches, 1001);
    assert!(defaulted.limit_reached);
    assert_eq!(defaulted.paths.first().unwrap(), "f0000.txt");
    assert_eq!(defaulted.paths.last().unwrap(), "f0999.txt");

    let limited = find(
        &eng,
        &FindParams {
            limit: Some(2),
            ..params(dir.path(), "*.txt")
        },
    )
    .unwrap();
    assert_eq!(limited.paths, vec!["f0000.txt", "f0001.txt"]);
    assert_eq!(limited.total_matches, 1001);
    assert!(limited.limit_reached);
}

#[test]
fn hidden_ignore_and_warm_cache_options_share_walk_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "visible.txt", "");
    seed(dir.path(), ".hidden.txt", "");
    seed(dir.path(), "ignored.txt", "");
    seed(dir.path(), ".ignore", "ignored.txt\n");
    let eng = engine(dir.path());
    let mut p = FindParams::new("*.txt");
    p.path = dir.path().display().to_string();
    p.hidden = false;

    let cold = find(&eng, &p).unwrap();
    let warm = find(&eng, &p).unwrap();
    assert_eq!(cold.paths, vec!["visible.txt"]);
    assert!(!cold.walk_cache_hit);
    assert!(warm.walk_cache_hit);

    p.hidden = true;
    let hidden = find(&eng, &p).unwrap();
    assert_eq!(hidden.paths, vec![".hidden.txt", "visible.txt"]);
    assert!(!hidden.walk_cache_hit, "different WalkKey must not alias");

    seed(dir.path(), "rgignored.txt", "");
    seed(dir.path(), ".rgignore", "rgignored.txt\n");
    eng.invalidate_root(dir.path());
    let rgignored = find(&eng, &p).unwrap();
    assert_eq!(rgignored.paths, vec![".hidden.txt", "visible.txt"]);
}

#[test]
fn explicit_invalidation_refreshes_a_warm_find() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "before.txt", "");
    let eng = engine(dir.path());
    let p = params(dir.path(), "*.txt");
    assert_eq!(find(&eng, &p).unwrap().paths, vec!["before.txt"]);
    seed(dir.path(), "after.txt", "");
    assert_eq!(find(&eng, &p).unwrap().paths, vec!["before.txt"]);

    eng.invalidate(dir.path(), true, hearth_proto::CacheScope::Walks);
    assert_eq!(
        find(&eng, &p).unwrap().paths,
        vec!["after.txt", "before.txt"]
    );
}

#[test]
fn invalid_patterns_roots_and_limits_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());

    let missing = params(&dir.path().join("missing"), "*");
    assert_eq!(find(&eng, &missing).unwrap_err().kind, ErrorKind::NotFound);

    let file = seed(dir.path(), "file", "");
    let not_dir = params(Path::new(&file), "*");
    assert_eq!(
        find(&eng, &not_dir).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    let too_long = params(dir.path(), &"x".repeat(4097));
    assert_eq!(
        find(&eng, &too_long).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    let invalid_glob = params(dir.path(), "[");
    assert_eq!(
        find(&eng, &invalid_glob).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    let mut too_many = params(dir.path(), "*");
    too_many.limit = Some(1_000_001);
    assert_eq!(
        find(&eng, &too_many).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    let mut excludes = params(dir.path(), "*");
    excludes.exclude_globs = vec!["x".into(); 129];
    assert_eq!(
        find(&eng, &excludes).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
}

#[test]
fn a_relative_cwd_is_normalized_to_an_absolute_lexical_root() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a.txt", "");
    let cwd = dir.path().join("child/..");
    let eng = Engine::new(EngineConfig {
        default_cwd: cwd,
        enable_optimizer: false,
        enable_watch: false,
        ..EngineConfig::default()
    });

    let result = find(&eng, &FindParams::new("*.txt")).unwrap();

    assert_eq!(PathBuf::from(&result.root), dir.path());
    assert!(Path::new(&result.root).is_absolute());
}

#[cfg(unix)]
#[test]
fn symlink_results_follow_pi_entry_and_traversal_rules() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "target/a.txt", "");
    symlink("target/a.txt", dir.path().join("link-file")).unwrap();
    symlink("target", dir.path().join("link-dir")).unwrap();
    symlink("missing", dir.path().join("dangling")).unwrap();
    let eng = engine(dir.path());

    let plain = find(&eng, &params(dir.path(), "link-*")).unwrap();
    assert_eq!(plain.paths, vec!["link-dir", "link-file"]);
    let dangling = find(&eng, &params(dir.path(), "dangling")).unwrap();
    assert_eq!(dangling.paths, vec!["dangling"]);

    let followed_dangling = find(
        &eng,
        &FindParams {
            follow_symlinks: true,
            ..params(dir.path(), "dangling")
        },
    )
    .unwrap();
    assert_eq!(followed_dangling.paths, vec!["dangling"]);

    let followed = find(
        &eng,
        &FindParams {
            follow_symlinks: true,
            ..params(dir.path(), "link-*")
        },
    )
    .unwrap();
    assert_eq!(followed.paths, vec!["link-dir/", "link-file"]);
    let descendant = find(
        &eng,
        &FindParams {
            follow_symlinks: true,
            ..params(dir.path(), "link-dir/**/*.txt")
        },
    )
    .unwrap();
    assert_eq!(descendant.paths, vec!["link-dir/a.txt"]);

    symlink("missing-hidden", dir.path().join(".hidden-dangling")).unwrap();
    symlink("missing-ignored", dir.path().join("ignored-link")).unwrap();
    seed(dir.path(), ".ignore", "ignored-link\n");
    eng.invalidate_root(dir.path());
    let filtered = find(
        &eng,
        &FindParams {
            path: dir.path().display().to_string(),
            hidden: false,
            respect_gitignore: true,
            follow_symlinks: true,
            ..FindParams::new("*")
        },
    )
    .unwrap();
    assert!(!filtered.paths.iter().any(|path| path == ".hidden-dangling"));
    assert!(!filtered.paths.iter().any(|path| path == "ignored-link"));
}

// APFS rejects arbitrary non-UTF-8 byte sequences; Linux filesystems permit
// them and exercise the lossy stdout-compatible rendering contract.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_use_lossy_stdout_compatible_rendering() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(OsString::from_vec(b"bad-\xff".to_vec())),
        b"",
    )
    .unwrap();
    let eng = engine(dir.path());

    let result = find(&eng, &params(dir.path(), "*")).unwrap();
    assert_eq!(result.paths, vec!["bad-�"]);
}

#[test]
fn pre_cancelled_find_returns_the_structured_cancelled_error() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "a", "");
    let eng = engine(dir.path());
    let cancel = CancelToken::new();
    cancel.cancel();

    let error = find_cancellable(&eng, &params(dir.path(), "*"), &cancel).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
}
