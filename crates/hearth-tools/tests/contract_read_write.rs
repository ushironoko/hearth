//! Read windowing and write semantics: the two places where Hearth's behaviour
//! deliberately differs from `fs.readFile`/`fs.writeFile`, plus cancellation.

mod common;

use common::{abs, engine, seed};
use hearth_core::CancelToken;
use hearth_proto::*;
use hearth_tools::{read, read_bytes, read_bytes_cancellable, read_cancellable, write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn windowed(path: &str, offset: u64, limit: u64, mode: LineWindowMode) -> ReadParams {
    ReadParams {
        offset: Some(offset),
        limit: Some(limit),
        line_mode: mode,
        ..ReadParams::new(path)
    }
}

// -- read ------------------------------------------------------------------

#[test]
fn the_two_line_modes_differ_only_around_newlines() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "t.txt", "a\nb\nc\n");

    // `cat`-style: a trailing newline adds no phantom line, and a window keeps
    // the newline that ends it.
    let slice = read(&eng, &ReadParams::new(&path)).unwrap();
    assert_eq!(slice.total_lines, 3);
    assert!(slice.ends_with_newline);
    let window = read(&eng, &windowed(&path, 1, 2, LineWindowMode::Slice)).unwrap();
    assert_eq!(window.content, "a\nb\n");

    // `split('\n')`-style: the empty element after the last newline counts as a
    // line, and a window is those elements re-joined, so it never ends with a
    // newline. Reading the *whole* file still reproduces it byte for byte,
    // because joining the trailing empty element puts the newline back.
    let split = read(
        &eng,
        &ReadParams { line_mode: LineWindowMode::SplitLines, ..ReadParams::new(&path) },
    )
    .unwrap();
    assert_eq!(split.total_lines, 4);
    assert_eq!(split.content, "a\nb\nc\n");
    let window = read(&eng, &windowed(&path, 1, 2, LineWindowMode::SplitLines)).unwrap();
    assert_eq!(window.content, "a\nb");
    // The last element is the empty string after the final newline.
    let tail = read(&eng, &windowed(&path, 4, 1, LineWindowMode::SplitLines)).unwrap();
    assert_eq!(tail.content, "");
}

#[test]
fn a_file_without_a_trailing_newline_counts_the_same_in_both_modes() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "bare.txt", "a\nb");

    let slice = read(&eng, &ReadParams::new(&path)).unwrap();
    let split =
        read(&eng, &ReadParams { line_mode: LineWindowMode::SplitLines, ..ReadParams::new(&path) })
            .unwrap();
    assert_eq!(slice.total_lines, 2);
    assert_eq!(split.total_lines, 2);
    assert!(!slice.ends_with_newline);
    assert_eq!(slice.content, "a\nb");
    assert_eq!(split.content, "a\nb");
}

#[test]
fn an_offset_past_the_end_is_an_error_and_a_limit_past_it_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "short.txt", "a\nb\n");

    let err = read(&eng, &windowed(&path, 99, 1, LineWindowMode::Slice)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);

    let clipped = read(&eng, &windowed(&path, 2, 99, LineWindowMode::Slice)).unwrap();
    assert_eq!(clipped.content, "b\n");
    assert_eq!(clipped.returned_lines, 1);
    assert!(clipped.truncated, "starting past line 1 counts as truncated");
}

#[test]
fn an_empty_file_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "empty.txt", "");

    let r = read(&eng, &ReadParams::new(&path)).unwrap();
    assert_eq!(r.content, "");
    assert_eq!(r.byte_len, 0);
    assert!(!r.ends_with_newline);
}

#[test]
fn binary_content_is_flagged_and_readable_as_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = abs(dir.path(), "blob.bin");
    let bytes: Vec<u8> = vec![0x00, 0xff, 0x10, b'a', 0x00];
    std::fs::write(&path, &bytes).unwrap();

    let r = read(&eng, &ReadParams::new(&path)).unwrap();
    assert!(r.binary);
    assert!(r.content.is_empty(), "binary content is not guessed at as text");
    assert_eq!(r.byte_len, 5);

    assert_eq!(read_bytes(&eng, &ReadParams::new(&path)).unwrap(), bytes);
}

#[test]
fn a_pre_aborted_read_rejects_without_touching_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "c.txt", "data\n");
    let cancel = CancelToken::new();
    cancel.cancel();

    assert_eq!(
        read_cancellable(&eng, &ReadParams::new(&path), &cancel).unwrap_err().kind,
        ErrorKind::Cancelled
    );
    assert_eq!(
        read_bytes_cancellable(&eng, &ReadParams::new(&path), &cancel).unwrap_err().kind,
        ErrorKind::Cancelled
    );
    assert!(eng.files().is_empty(), "an aborted read must not warm the cache");
}

// -- write -----------------------------------------------------------------

#[test]
fn write_creates_parents_overwrites_and_handles_empty_content() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let nested = abs(dir.path(), "a/b/c.txt");

    let created = write(&eng, &WriteParams::new(&nested, "first")).unwrap();
    assert!(!created.existed);
    assert_eq!(created.bytes_written, 5);

    let overwritten = write(&eng, &WriteParams::new(&nested, "")).unwrap();
    assert!(overwritten.existed);
    assert_eq!(overwritten.bytes_written, 0);
    assert_eq!(std::fs::read(&nested).unwrap(), b"");

    // Without create_dirs, a missing parent is an error rather than a surprise.
    let deeper = abs(dir.path(), "x/y/z.txt");
    let err = write(
        &eng,
        &WriteParams { create_dirs: false, ..WriteParams::new(&deeper, "nope") },
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
}

#[test]
fn the_atomic_mode_replaces_the_inode_but_carries_the_mode_across() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "script.sh", "#!/bin/sh\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = std::fs::metadata(&path).unwrap();

    write(&eng, &WriteParams::new(&path, "#!/bin/sh\necho hi\n")).unwrap();

    let after = std::fs::metadata(&path).unwrap();
    assert_ne!(before.ino(), after.ino(), "an atomic write is a rename over the target");
    assert_eq!(
        after.permissions().mode() & 0o777,
        0o755,
        "the executable bit must not be silently dropped"
    );
}

#[test]
fn the_in_place_mode_keeps_the_inode_and_every_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "linked.txt", "before\n");
    let hardlink = dir.path().join("hardlink.txt");
    std::fs::hard_link(&path, &hardlink).unwrap();
    let before = std::fs::metadata(&path).unwrap();

    write(
        &eng,
        &WriteParams { mode: WriteMode::InPlace, ..WriteParams::new(&path, "after\n") },
    )
    .unwrap();

    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.ino(), after.ino(), "an in-place write rewrites the same inode");
    assert_eq!(
        std::fs::read_to_string(&hardlink).unwrap(),
        "after\n",
        "the other hardlink must see the new content"
    );

    // The atomic mode is the documented opposite: the hardlink keeps the old
    // content because the target became a different inode.
    write(&eng, &WriteParams::new(&path, "atomic\n")).unwrap();
    assert_eq!(std::fs::read_to_string(&hardlink).unwrap(), "after\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "atomic\n");
}

#[test]
fn writing_through_a_symlink_does_not_replace_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let target = seed(dir.path(), "target.txt", "old\n");
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let r = write(&eng, &WriteParams::new(link.display().to_string(), "new\n")).unwrap();
    assert!(r.followed_symlink);
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");

    // Opting out replaces the link with a regular file, as asked.
    let r = write(
        &eng,
        &WriteParams {
            follow_symlinks: false,
            ..WriteParams::new(link.display().to_string(), "replaced\n")
        },
    )
    .unwrap();
    assert!(!r.followed_symlink);
    assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
}

#[test]
fn concurrent_writes_to_one_path_are_serialized_and_never_interleave() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = abs(dir.path(), "hot.txt");
    write(&eng, &WriteParams::new(&path, "seed")).unwrap();

    // Every writer writes a distinct, uniform body. Serialization means a reader
    // can only ever observe one writer's body, never a mix of two.
    let bodies: Vec<String> =
        ('a'..='h').map(|c| std::iter::repeat_n(c, 200_000).collect()).collect();
    std::thread::scope(|scope| {
        for body in &bodies {
            let (eng, path) = (&eng, &path);
            scope.spawn(move || {
                write(eng, &WriteParams::new(path.as_str(), body.as_str())).unwrap();
            });
        }
    });

    let final_content = std::fs::read_to_string(&path).unwrap();
    assert!(
        bodies.contains(&final_content),
        "the file must hold exactly one writer's content, not a blend"
    );
}
