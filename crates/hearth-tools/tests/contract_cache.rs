//! Cache-coherence contract: what Hearth invalidates on its own, and what an
//! adapter has to invalidate explicitly once `trustCache` is on.

mod common;

use common::{abs, seed, trusting_engine};
use hearth_proto::*;
use hearth_tools::{bash, edit_batch, grep, read, write};
use std::path::Path;

fn files_found(eng: &hearth_core::Engine, root: &Path) -> Vec<String> {
    let r = grep(eng, &GrepParams::new("marker", root.display().to_string())).unwrap();
    let mut names: Vec<String> = r
        .files
        .iter()
        .map(|f| Path::new(&f.path).file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn creating_a_file_invalidates_a_cached_walk() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    seed(dir.path(), "first.txt", "marker\n");

    assert_eq!(files_found(&eng, dir.path()), vec!["first.txt"]);

    // A create through Hearth must be visible to the very next search.
    write(&eng, &WriteParams::new(abs(dir.path(), "second.txt"), "marker\n")).unwrap();
    assert_eq!(files_found(&eng, dir.path()), vec!["first.txt", "second.txt"]);
}

#[test]
fn overwriting_an_existing_file_does_not_needlessly_drop_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    seed(dir.path(), "a.txt", "marker\n");

    grep(&eng, &GrepParams::new("marker", dir.path().display().to_string())).unwrap();
    write(&eng, &WriteParams::new(abs(dir.path(), "a.txt"), "marker again\n")).unwrap();

    let r = grep(&eng, &GrepParams::new("marker", dir.path().display().to_string())).unwrap();
    assert!(r.walk_cache_hit, "rewriting an existing file cannot change what a walk enumerates");
    assert_eq!(r.total_matches, 1);
}

#[test]
fn changing_an_ignore_file_invalidates_the_walk_it_governs() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    seed(dir.path(), "kept.txt", "marker\n");
    seed(dir.path(), "hidden.txt", "marker\n");
    let ignore = seed(dir.path(), ".ignore", "hidden.txt\n");

    assert_eq!(files_found(&eng, dir.path()), vec!["kept.txt"]);

    // Editing the ignore file changes which files exist as far as a walk is
    // concerned, even though no file was created or removed.
    edit_batch(
        &eng,
        &EditBatchParams::new(
            ignore.clone(),
            vec![EditReplacement { old_text: "hidden.txt".into(), new_text: "nothing.txt".into() }],
        ),
    )
    .unwrap();
    assert_eq!(files_found(&eng, dir.path()), vec!["hidden.txt", "kept.txt"]);

    // The same via `write`.
    write(&eng, &WriteParams::new(&ignore, "hidden.txt\n")).unwrap();
    assert_eq!(files_found(&eng, dir.path()), vec!["kept.txt"]);
}

#[test]
fn explicit_invalidation_covers_a_change_hearth_did_not_make() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let path = seed(dir.path(), "outside.txt", "before\n");

    assert_eq!(read(&eng, &ReadParams::new(&path)).unwrap().content, "before\n");

    // An out-of-band write is exactly what `trustCache` promises *not* to see.
    std::fs::write(&path, "after\n").unwrap();
    assert_eq!(
        read(&eng, &ReadParams::new(&path)).unwrap().content,
        "before\n",
        "trustCache serves the cached bytes until told otherwise"
    );

    let dropped = eng.invalidate_path(Path::new(&path));
    assert_eq!(dropped.files_invalidated, 1);
    assert_eq!(read(&eng, &ReadParams::new(&path)).unwrap().content, "after\n");
}

#[test]
fn invalidating_a_root_covers_a_shell_command_that_rewrote_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let kept = seed(dir.path(), "kept.txt", "marker\n");
    seed(dir.path(), "doomed.txt", "marker\n");

    assert_eq!(files_found(&eng, dir.path()), vec!["doomed.txt", "kept.txt"]);
    assert_eq!(read(&eng, &ReadParams::new(&kept)).unwrap().content, "marker\n");

    // An arbitrary command can create, delete and rewrite anything.
    let r = bash(
        &eng,
        &BashParams::new(
            "rm doomed.txt && printf 'marker rewritten\\n' > kept.txt && printf 'marker\\n' > fresh.txt",
        ),
    )
    .unwrap();
    assert_eq!(r.exit_code, 0);

    // Which is why the adapter invalidates the command's cwd conservatively.
    let dropped = eng.invalidate_root(dir.path());
    assert!(dropped.files_invalidated >= 1);
    assert!(dropped.walks_invalidated >= 1);

    assert_eq!(files_found(&eng, dir.path()), vec!["fresh.txt", "kept.txt"]);
    assert_eq!(read(&eng, &ReadParams::new(&kept)).unwrap().content, "marker rewritten\n");
}

#[test]
fn scoped_and_recursive_invalidation_do_only_what_they_say() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let inner = dir.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let a = seed(&inner, "a.txt", "marker\n");
    let b = seed(&inner, "b.txt", "marker\n");

    read(&eng, &ReadParams::new(&a)).unwrap();
    read(&eng, &ReadParams::new(&b)).unwrap();
    grep(&eng, &GrepParams::new("marker", dir.path().display().to_string())).unwrap();

    // Files only, non-recursive: one entry, no walks.
    let dropped = eng.invalidate(Path::new(&a), false, CacheScope::Files);
    assert_eq!(dropped.files_invalidated, 1);
    assert_eq!(dropped.walks_invalidated, 0);

    // Recursive over the directory catches the sibling that is still cached.
    let dropped = eng.invalidate(&inner, true, CacheScope::All);
    assert_eq!(dropped.files_invalidated, 1);
    assert_eq!(dropped.walks_invalidated, 1);
}

#[test]
fn clearing_the_caches_empties_both() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    seed(dir.path(), "a.txt", "marker\n");
    grep(&eng, &GrepParams::new("marker", dir.path().display().to_string())).unwrap();
    assert!(!eng.files().is_empty());

    let dropped = eng.clear_caches();
    assert!(dropped.files_invalidated >= 1);
    assert_eq!(dropped.walks_invalidated, 1);
    assert!(eng.files().is_empty());

    let r = grep(&eng, &GrepParams::new("marker", dir.path().display().to_string())).unwrap();
    assert!(!r.walk_cache_hit, "the walk must be rebuilt after a clear");
}
