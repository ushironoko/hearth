//! Batch-edit contract: the pi 0.80.7 matching rules, atomicity, byte-level
//! preservation, and the metadata an adapter renders a diff from.

mod common;

use common::{engine, seed, trusting_engine};
use hearth_proto::*;
use hearth_tools::{edit_batch, read};

fn one(old: &str, new: &str) -> Vec<EditReplacement> {
    vec![EditReplacement {
        old_text: old.into(),
        new_text: new.into(),
    }]
}

fn on_disk(path: &str) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn single_and_multiple_disjoint_edits() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(
        dir.path(),
        "a.rs",
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    );

    let r = edit_batch(
        &eng,
        &EditBatchParams::new(
            path.clone(),
            vec![
                EditReplacement {
                    old_text: "fn one".into(),
                    new_text: "fn ONE".into(),
                },
                EditReplacement {
                    old_text: "fn three".into(),
                    new_text: "fn THREE".into(),
                },
            ],
        ),
    )
    .unwrap();

    assert_eq!(r.replacements, 2);
    assert!(!r.used_normalized_fallback);
    assert_eq!(on_disk(&path), "fn ONE() {}\nfn two() {}\nfn THREE() {}\n");
    assert_eq!(r.first_changed_line, Some(1));
}

#[test]
fn each_edit_matches_the_original_file_not_the_running_result() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "swap.txt", "alpha\nbeta\n");

    edit_batch(
        &eng,
        &EditBatchParams::new(
            path.clone(),
            vec![
                EditReplacement {
                    old_text: "alpha".into(),
                    new_text: "beta".into(),
                },
                EditReplacement {
                    old_text: "beta".into(),
                    new_text: "gamma".into(),
                },
            ],
        ),
    )
    .unwrap();

    // Applied incrementally, the second edit would have hit the freshly written
    // "beta" and produced "gamma\nbeta".
    assert_eq!(on_disk(&path), "beta\ngamma\n");
}

#[test]
fn duplicate_target_is_rejected_with_its_edit_index() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "dup.txt", "x\nx\n");

    let err = edit_batch(&eng, &EditBatchParams::new(path.clone(), one("x", "y"))).unwrap_err();
    assert_eq!(err.kind, ErrorKind::MultipleMatches);
    assert_eq!(err.edit_index, Some(0));
    assert_eq!(
        on_disk(&path),
        "x\nx\n",
        "a rejected batch must not touch the file"
    );
}

#[test]
fn overlapping_and_nested_targets_are_rejected_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "ov.txt", "abcdef\n");

    for edits in [
        vec![
            EditReplacement {
                old_text: "abcd".into(),
                new_text: "1".into(),
            },
            EditReplacement {
                old_text: "cdef".into(),
                new_text: "2".into(),
            },
        ],
        vec![
            EditReplacement {
                old_text: "abcdef".into(),
                new_text: "1".into(),
            },
            EditReplacement {
                old_text: "cd".into(),
                new_text: "2".into(),
            },
        ],
    ] {
        let err = edit_batch(&eng, &EditBatchParams::new(path.clone(), edits)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overlap);
        assert_eq!(on_disk(&path), "abcdef\n");
    }
}

#[test]
fn one_failing_edit_leaves_the_whole_batch_unapplied() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let original = "keep\nchange me\n";
    let path = seed(dir.path(), "atomic.txt", original);

    let err = edit_batch(
        &eng,
        &EditBatchParams::new(
            path.clone(),
            vec![
                EditReplacement {
                    old_text: "change me".into(),
                    new_text: "changed".into(),
                },
                EditReplacement {
                    old_text: "absent".into(),
                    new_text: "x".into(),
                },
            ],
        ),
    )
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::NoMatch);
    assert_eq!(err.edit_index, Some(1));
    assert_eq!(
        on_disk(&path),
        original,
        "the first edit must not have been applied either"
    );
}

#[test]
fn bom_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "bom.txt", "\u{FEFF}hello\nworld\n");

    // The caller does not include the invisible BOM in oldText.
    let r = edit_batch(
        &eng,
        &EditBatchParams::new(path.clone(), one("hello", "HELLO")),
    )
    .unwrap();
    assert!(r.had_bom);
    assert_eq!(on_disk(&path), "\u{FEFF}HELLO\nworld\n");
}

#[test]
fn crlf_convention_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "crlf.txt", "one\r\ntwo\r\nthree\r\n");

    // The caller writes LF; the file's CRLF convention survives.
    let r = edit_batch(&eng, &EditBatchParams::new(path.clone(), one("two", "TWO"))).unwrap();
    assert!(r.crlf);
    assert_eq!(on_disk(&path), "one\r\nTWO\r\nthree\r\n");
}

#[test]
fn lf_file_stays_lf() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "lf.txt", "one\ntwo\n");

    let r = edit_batch(&eng, &EditBatchParams::new(path.clone(), one("two", "TWO"))).unwrap();
    assert!(!r.crlf);
    assert_eq!(on_disk(&path), "one\nTWO\n");
}

#[test]
fn normalized_fallback_matches_typography_and_keeps_other_lines_byte_exact() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    // Line 1 carries trailing whitespace the caller never mentions; line 2 uses
    // smart quotes and an em dash the caller typed as ASCII.
    let path = seed(
        dir.path(),
        "typo.txt",
        "untouched   \nsay \u{201C}hi\u{201D} \u{2014} now\ntail\t\n",
    );

    let r = edit_batch(
        &eng,
        &EditBatchParams::new(path.clone(), one("say \"hi\" - now", "say \"bye\" - now")),
    )
    .unwrap();

    assert!(r.used_normalized_fallback);
    assert_eq!(
        on_disk(&path),
        "untouched   \nsay \"bye\" - now\ntail\t\n",
        "only the matched line may be rewritten from normalized text"
    );
}

#[test]
fn edits_through_a_symlink_rewrite_the_target_and_keep_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let target = seed(dir.path(), "real.txt", "value = 1\n");
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let r = edit_batch(
        &eng,
        &EditBatchParams::new(link.display().to_string(), one("value = 1", "value = 2")),
    )
    .unwrap();

    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink itself must survive"
    );
    assert_eq!(on_disk(&target), "value = 2\n");
    assert_eq!(
        r.path,
        std::fs::canonicalize(&target)
            .unwrap()
            .display()
            .to_string()
    );
}

#[test]
fn concurrent_edits_of_the_same_file_do_not_lose_updates() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "race.txt", "a=0\nb=0\nc=0\nd=0\n");
    let alias_dir = dir.path().join("alias");
    std::os::unix::fs::symlink(dir.path(), &alias_dir).unwrap();
    let alias = alias_dir.join("race.txt").display().to_string();

    // Half the writers go through a symlinked directory, so serialization has to
    // key on the canonical path rather than the string the caller passed.
    let targets = [path.clone(), alias.clone(), path.clone(), alias];
    let keys = ["a", "b", "c", "d"];
    std::thread::scope(|scope| {
        for (target, key) in targets.iter().zip(keys) {
            let eng = &eng;
            scope.spawn(move || {
                edit_batch(
                    eng,
                    &EditBatchParams::new(
                        target.clone(),
                        one(&format!("{key}=0"), &format!("{key}=1")),
                    ),
                )
                .unwrap();
            });
        }
    });

    assert_eq!(
        on_disk(&path),
        "a=1\nb=1\nc=1\nd=1\n",
        "every concurrent edit must survive"
    );
}

#[test]
fn diff_metadata_locates_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    let path = seed(dir.path(), "big.txt", &body);

    let r = edit_batch(
        &eng,
        &EditBatchParams {
            return_content: true,
            ..EditBatchParams::new(path.clone(), one("line 20", "LINE TWENTY"))
        },
    )
    .unwrap();

    assert_eq!(r.first_changed_line, Some(20));
    assert_eq!(
        r.old_line_count, 31,
        "split('\\n') counts the empty element after the last newline"
    );
    assert_eq!(r.new_line_count, 31);
    assert_eq!(r.hunks.len(), 1);

    let hunk = &r.hunks[0];
    // Four lines of context on each side, by default.
    assert_eq!(hunk.old_start, 16);
    assert!(
        hunk.rows
            .iter()
            .any(|row| row.op == DiffOp::Delete && row.text == "line 20")
    );
    assert!(
        hunk.rows
            .iter()
            .any(|row| row.op == DiffOp::Insert && row.text == "LINE TWENTY")
    );
    assert!(
        !hunk.rows.iter().any(|row| row.text == "line 1"),
        "distant context must be elided, not shipped"
    );
    assert_eq!(
        r.content.as_deref().unwrap().lines().nth(19),
        Some("LINE TWENTY")
    );
}

#[test]
fn skip_diff_and_no_content_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "quiet.txt", "a\nb\n");

    let r = edit_batch(
        &eng,
        &EditBatchParams {
            skip_diff: true,
            ..EditBatchParams::new(path.clone(), one("a", "A"))
        },
    )
    .unwrap();
    assert!(r.hunks.is_empty());
    assert!(
        r.content.is_none(),
        "the full file must not ship unless asked for"
    );
}

#[test]
fn empty_edit_list_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "e.txt", "a\n");
    let err = edit_batch(&eng, &EditBatchParams::new(path, vec![])).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
}

#[test]
fn original_content_is_the_raw_pre_edit_text() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    // BOM + CRLF: the two representations `content` deliberately normalizes
    // away, and exactly what `originalContent` must preserve.
    let path = seed(dir.path(), "raw.txt", "\u{FEFF}one\r\ntwo\r\n");

    let r = edit_batch(
        &eng,
        &EditBatchParams {
            return_content: true,
            return_original_content: true,
            ..EditBatchParams::new(path.clone(), one("two", "TWO"))
        },
    )
    .unwrap();

    assert_eq!(
        r.original_content.as_deref(),
        Some("\u{FEFF}one\r\ntwo\r\n")
    );
    assert_eq!(
        r.content.as_deref(),
        Some("one\nTWO\n"),
        "content stays normalized"
    );
    assert_eq!(on_disk(&path), "\u{FEFF}one\r\nTWO\r\n");

    // Lone CR is collapsed on persistence, but the snapshot keeps it.
    let cr = seed(dir.path(), "cr.txt", "a\rb\r");
    let r = edit_batch(
        &eng,
        &EditBatchParams {
            return_original_content: true,
            ..EditBatchParams::new(cr.clone(), one("b", "B"))
        },
    )
    .unwrap();
    assert_eq!(r.original_content.as_deref(), Some("a\rb\r"));
    assert_eq!(on_disk(&cr), "a\nB\n");
}

#[test]
fn original_content_is_absent_unless_requested() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());
    let path = seed(dir.path(), "quiet-raw.txt", "a\n");
    let r = edit_batch(&eng, &EditBatchParams::new(path, one("a", "A"))).unwrap();
    assert!(
        r.original_content.is_none(),
        "the pre-edit file must not ship unless asked for"
    );
}

#[test]
fn whitespace_only_policy_flows_through_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());

    // Default keeps the Hearth 0.1.0 rejection.
    let path = seed(dir.path(), "ws.txt", "   ");
    let err = edit_batch(&eng, &EditBatchParams::new(path.clone(), one("   ", "x"))).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
    assert_eq!(
        on_disk(&path),
        "   ",
        "a rejected edit must leave the file untouched"
    );

    // Opting in permits exactly the whole-file case, atomically with the
    // original snapshot.
    let r = edit_batch(
        &eng,
        &EditBatchParams {
            whitespace_only_target_policy: WhitespaceOnlyTargetPolicy::ExactFile,
            return_original_content: true,
            ..EditBatchParams::new(path.clone(), one("   ", "x"))
        },
    )
    .unwrap();
    assert_eq!(r.original_content.as_deref(), Some("   "));
    assert_eq!(on_disk(&path), "x");
}

#[test]
fn cache_is_coherent_after_a_batch_edit_under_trust_cache() {
    let dir = tempfile::tempdir().unwrap();
    let eng = trusting_engine(dir.path());
    let path = seed(dir.path(), "warm.txt", "old\n");

    // Warm the cache first, so a stale entry would be observable.
    assert_eq!(
        read(&eng, &ReadParams::new(&path)).unwrap().content,
        "old\n"
    );
    edit_batch(&eng, &EditBatchParams::new(path.clone(), one("old", "new"))).unwrap();

    let r = read(&eng, &ReadParams::new(&path)).unwrap();
    assert_eq!(r.content, "new\n");
    assert!(
        r.cache_hit,
        "the refreshed entry should still be a warm hit"
    );
}
