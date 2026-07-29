//! The text machinery behind the batch `edit`: BOM/line-ending handling, the
//! normalized fallback match, atomic multi-replacement, and diff hunks.
//!
//! This is a faithful port of pi 0.80.7's `edit-diff.ts`, because an override
//! that matched *slightly* differently would silently edit the wrong region.
//! The rules, in order:
//!
//! 1. A UTF-8 BOM is stripped before matching and restored on write — a model
//!    never puts an invisible BOM in its `oldText`.
//! 2. Content is normalized to LF for matching, then restored to the file's
//!    original convention (CRLF if the *first* newline in the file was part of
//!    a CRLF pair, otherwise LF).
//! 3. Every `oldText` is matched against the **same** original content, never
//!    against the result of an earlier edit in the same call.
//! 4. Exact matching first. If *any* edit needs the normalized fallback, the
//!    whole call switches to normalized space so all offsets share one
//!    coordinate system.
//! 5. Each target must be unique and must not overlap another.
//! 6. When the fallback was used, only the lines a replacement actually touches
//!    are rewritten from normalized text; every other line keeps its original
//!    bytes, so matching through normalization never rewrites the file's
//!    untouched trailing whitespace or typography.

use hearth_proto::{
    DiffHunk, DiffOp, DiffRow, EditReplacement, ErrorKind, ToolError, ToolResult,
    WhitespaceOnlyTargetPolicy,
};
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;
use unicode_normalization::UnicodeNormalization;

/// Strip a UTF-8 BOM, reporting whether one was there.
pub fn strip_bom(content: &str) -> (bool, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => (true, rest),
        None => (false, content),
    }
}

/// Whether the file's line-ending convention is CRLF.
///
/// Decided by the *first* newline only, matching pi: a file is CRLF when its
/// first `\n` is preceded by `\r`.
pub fn detect_crlf(content: &str) -> bool {
    match content.find('\n') {
        None => false,
        Some(lf) => content.find("\r\n").is_some_and(|crlf| crlf < lf),
    }
}

/// Collapse CRLF and lone CR to LF, borrowing when there is nothing to collapse.
pub fn normalize_to_lf(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Re-expand LF to the file's original convention.
pub fn restore_line_endings(text: &str, crlf: bool) -> String {
    if crlf {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// The whitespace JS's `String.prototype.trimEnd` removes.
///
/// Deliberately *not* Rust's `char::is_whitespace`: that includes U+0085 (which
/// JS does not trim) and excludes U+FEFF (which JS does trim). Matching JS here
/// is what keeps the fallback's line-trimming identical to pi's.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Whether this text can be changed by anything beyond trailing-whitespace
/// trimming.
///
/// ASCII is NFKC-stable and contains none of the folded quotes, dashes or
/// spaces, so an all-ASCII input needs only the trimming pass — which skips a
/// full Unicode normalization over the file on the overwhelmingly common path.
fn needs_unicode_pass(text: &str) -> bool {
    !text.is_ascii()
}

/// Normalize text for the fallback match: NFKC, then per-line trailing
/// whitespace removal, then typographic folding of quotes, dashes and spaces.
///
/// Idempotent — NFKC already folds every space in the replacement set, and the
/// quote/dash replacements produce ASCII — which matters because the second
/// matching pass re-normalizes already-normalized content.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    if !needs_unicode_pass(text) {
        let mut out = String::with_capacity(text.len());
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line.trim_end_matches(is_js_whitespace));
        }
        return out;
    }

    let nfkc: String = text.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for (i, line) in nfkc.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_end_matches(is_js_whitespace);
        for c in trimmed.chars() {
            out.push(match c {
                // Smart single quotes.
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                // Smart double quotes.
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                // Hyphen, non-breaking hyphen, figure dash, en/em dash,
                // horizontal bar, minus sign.
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                // Special spaces.
                '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
                other => other,
            });
        }
    }
    out
}

/// Whether any line ends in whitespace this normalization would trim.
fn has_trailing_line_whitespace(text: &str) -> bool {
    text.split('\n')
        .any(|line| line.ends_with(is_js_whitespace))
}

/// [`normalize_for_fuzzy_match`], borrowing when normalization is a no-op.
///
/// That is the common case — an ASCII file with no trailing whitespace — and it
/// is worth detecting, because the alternative is copying the whole file to
/// discover it did not change.
fn normalize_for_fuzzy_match_cow(text: &str) -> Cow<'_, str> {
    if !needs_unicode_pass(text) && !has_trailing_line_whitespace(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(normalize_for_fuzzy_match(text))
}

/// Where an `oldText` matched, and whether the normalized fallback was needed.
struct Match {
    index: usize,
    length: usize,
    used_fallback: bool,
}

/// Exact match first; on failure, match in normalized space and report offsets
/// there, so the caller can switch its whole coordinate system consistently.
///
/// Both normalized forms are passed in rather than recomputed: normalizing the
/// file is the expensive step here, and one call resolves every edit.
fn find_text(
    content: &str,
    normalized_content: &str,
    old: &str,
    normalized_old: &str,
) -> Option<Match> {
    if let Some(index) = content.find(old) {
        return Some(Match {
            index,
            length: old.len(),
            used_fallback: false,
        });
    }
    normalized_content.find(normalized_old).map(|index| Match {
        index,
        length: normalized_old.len(),
        used_fallback: true,
    })
}

/// How many times `old` occurs, always compared in normalized space so two
/// regions that differ only in trailing whitespace or typography still count as
/// ambiguous.
fn count_occurrences(normalized_content: &str, normalized_old: &str) -> usize {
    if normalized_old.is_empty() {
        return 0;
    }
    normalized_content.matches(normalized_old).count()
}

/// One resolved replacement: where it lands and what it writes.
struct Resolved {
    edit_index: usize,
    index: usize,
    length: usize,
    new_text: String,
}

/// Splice `replacements` (sorted ascending, disjoint) into `content`, whose
/// first byte sits at `offset` in the coordinate system the indices came from.
fn apply_replacements(content: &str, replacements: &[&Resolved], offset: usize) -> String {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for r in replacements {
        let start = r.index - offset;
        out.push_str(&content[cursor..start]);
        out.push_str(&r.new_text);
        cursor = start + r.length;
    }
    out.push_str(&content[cursor..]);
    out
}

/// `(start, end)` byte spans of each line, terminator included.
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    spans
}

/// The half-open line range `[start, end)` a replacement touches.
fn replacement_line_range(spans: &[(usize, usize)], r: &Resolved) -> ToolResult<(usize, usize)> {
    let end_offset = r.index + r.length;
    let start_line = spans
        .iter()
        .position(|&(s, e)| r.index >= s && r.index < e)
        .ok_or_else(|| ToolError::internal("replacement range is outside the base content"))?;
    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < end_offset {
        end_line += 1;
    }
    if end_line >= spans.len() {
        return Err(ToolError::internal(
            "replacement range is outside the base content",
        ));
    }
    Ok((start_line, end_line + 1))
}

/// Apply replacements that were matched against a *normalized view* of
/// `original`, rewriting only the lines they actually touch.
///
/// Every other line is copied back verbatim from `original`, so matching a
/// smart quote never rewrites the trailing whitespace of an unrelated line.
/// Grouping by the real replacement ranges (rather than by matching normalized
/// line text) is what stops two identical-after-normalization lines from being
/// aligned to the wrong occurrence.
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Resolved],
) -> ToolResult<String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let base_spans = line_spans(base);
    if original_lines.len() != base_spans.len() {
        return Err(ToolError::internal(
            "cannot preserve unchanged lines: normalization changed the line count",
        ));
    }

    struct Group {
        start_line: usize,
        end_line: usize,
        replacements: Vec<usize>,
    }
    let mut order: Vec<usize> = (0..replacements.len()).collect();
    order.sort_by_key(|&i| replacements[i].index);

    let mut groups: Vec<Group> = Vec::new();
    for i in order {
        let (start_line, end_line) = replacement_line_range(&base_spans, &replacements[i])?;
        match groups.last_mut() {
            Some(current) if start_line < current.end_line => {
                current.end_line = current.end_line.max(end_line);
                current.replacements.push(i);
            }
            _ => groups.push(Group {
                start_line,
                end_line,
                replacements: vec![i],
            }),
        }
    }

    let mut out = String::with_capacity(original.len());
    let mut line_cursor = 0usize;
    for group in &groups {
        for line in &original_lines[line_cursor..group.start_line] {
            out.push_str(line);
        }
        let start_offset = base_spans[group.start_line].0;
        let end_offset = base_spans[group.end_line - 1].1;
        let members: Vec<&Resolved> = group
            .replacements
            .iter()
            .map(|&i| &replacements[i])
            .collect();
        out.push_str(&apply_replacements(
            &base[start_offset..end_offset],
            &members,
            start_offset,
        ));
        line_cursor = group.end_line;
    }
    for line in &original_lines[line_cursor..] {
        out.push_str(line);
    }
    Ok(out)
}

/// The outcome of applying a batch of edits to LF-normalized content.
///
/// The pre-edit side is not carried: it is the `content` the caller passed in,
/// and duplicating it here would mean holding two copies of the file just to
/// hand one back.
#[derive(Debug)]
pub struct AppliedEdits {
    /// The post-edit content.
    pub new_content: String,
    /// Whether any edit needed the normalized fallback.
    pub used_fallback: bool,
}

/// Resolve and apply every edit against `content` (already BOM-stripped and
/// LF-normalized), atomically: any failure leaves `content` untouched because
/// nothing is written until all edits resolve.
pub fn apply_edits(content: &str, edits: &[EditReplacement]) -> ToolResult<AppliedEdits> {
    apply_edits_opts(content, edits, WhitespaceOnlyTargetPolicy::default())
}

/// As [`apply_edits`], with an explicit whitespace-only target policy.
///
/// Under [`WhitespaceOnlyTargetPolicy::ExactFile`], a target whose normalized
/// form is empty is resolved by one rule only — its LF-normalized text must
/// equal the entire `content` — because such a target has no coordinates in
/// normalized matching space: occurrence counting cannot see it, and a batch
/// that switches to the fallback would resolve it as a zero-width match at
/// offset 0. Whole-file equality is the one case with nothing left to guess.
pub fn apply_edits_opts(
    content: &str,
    edits: &[EditReplacement],
    whitespace_policy: WhitespaceOnlyTargetPolicy,
) -> ToolResult<AppliedEdits> {
    if edits.is_empty() {
        return Err(ToolError::invalid(
            "edits must contain at least one replacement",
        ));
    }

    let normalized: Vec<(Cow<'_, str>, Cow<'_, str>)> = edits
        .iter()
        .map(|e| (normalize_to_lf(&e.old_text), normalize_to_lf(&e.new_text)))
        .collect();
    // Normalize each target once, and the file exactly once. Normalization is
    // idempotent, so this single view serves both the exact and the fallback
    // coordinate system: with no fallback it is `normalize(content)`, and with
    // one it *is* the matching base, whose re-normalization is itself.
    let mut normalized_olds = Vec::with_capacity(normalized.len());
    let mut has_whitespace_only = false;
    for (i, (old, _)) in normalized.iter().enumerate() {
        if old.is_empty() {
            return Err(ToolError::invalid("oldText must not be empty").with_edit_index(i));
        }
        let normalized_old = normalize_for_fuzzy_match(old);
        // A target that normalizes away entirely (e.g. only trailing spaces)
        // has no place in normalized matching space; without the opt-in it is
        // rejected up front rather than matched at an arbitrary offset.
        if normalized_old.is_empty() {
            match whitespace_policy {
                WhitespaceOnlyTargetPolicy::Reject => {
                    return Err(ToolError::invalid(
                        "oldText contains only whitespace that normalization removes",
                    )
                    .with_edit_index(i));
                }
                WhitespaceOnlyTargetPolicy::ExactFile => {
                    if old.as_ref() != content {
                        return Err(ToolError::new(
                            ErrorKind::NoMatch,
                            "a whitespace-only oldText must match the entire file exactly",
                        )
                        .with_edit_index(i));
                    }
                    has_whitespace_only = true;
                }
            }
        }
        normalized_olds.push(normalized_old);
    }
    let normalized_content = normalize_for_fuzzy_match_cow(content);

    // If any edit needs the fallback, every edit is resolved in normalized
    // space so all offsets share one coordinate system.
    let used_fallback =
        normalized
            .iter()
            .zip(&normalized_olds)
            .any(|((old, _), normalized_old)| {
                matches!(
                    find_text(content, &normalized_content, old, normalized_old),
                    Some(m) if m.used_fallback
                )
            });

    // A whitespace-only span vanishes in normalized space, where an empty
    // pattern would "match" at offset 0 and turn the replacement into an
    // insertion. Defensive: with whole-file equality required this cannot
    // currently fire (an all-whitespace file normalizes to an empty matching
    // base, so no companion edit can resolve, let alone through the fallback),
    // but the invariant must hold even if normalization rules evolve.
    if used_fallback && has_whitespace_only {
        let index = normalized_olds
            .iter()
            .position(String::is_empty)
            .unwrap_or(0);
        return Err(ToolError::invalid(
            "a whitespace-only oldText cannot be combined with edits that need the normalized fallback",
        )
        .with_edit_index(index));
    }
    let base_for_matching: &str = if used_fallback {
        &normalized_content
    } else {
        content
    };

    let mut resolved: Vec<Resolved> = Vec::with_capacity(normalized.len());
    for (i, ((old, new), normalized_old)) in normalized.iter().zip(&normalized_olds).enumerate() {
        let found = find_text(base_for_matching, &normalized_content, old, normalized_old)
            .ok_or_else(|| {
                ToolError::new(
                    ErrorKind::NoMatch,
                    "oldText not found; it must match exactly, including all whitespace and newlines",
                )
                .with_edit_index(i)
            })?;
        let occurrences = count_occurrences(&normalized_content, normalized_old);
        if occurrences > 1 {
            return Err(ToolError::new(
                ErrorKind::MultipleMatches,
                format!("found {occurrences} occurrences of oldText; it must be unique"),
            )
            .with_edit_index(i));
        }
        resolved.push(Resolved {
            edit_index: i,
            index: found.index,
            length: found.length,
            new_text: new.to_string(),
        });
    }

    resolved.sort_by_key(|r| r.index);
    for pair in resolved.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.index + previous.length > current.index {
            return Err(ToolError::new(
                ErrorKind::Overlap,
                format!(
                    "edits[{}] and edits[{}] overlap; merge them into one edit or target disjoint regions",
                    previous.edit_index, current.edit_index
                ),
            )
            .with_edit_index(current.edit_index));
        }
    }

    let new_content = if used_fallback {
        apply_preserving_unchanged_lines(content, base_for_matching, &resolved)?
    } else {
        let members: Vec<&Resolved> = resolved.iter().collect();
        apply_replacements(base_for_matching, &members, 0)
    };

    if new_content == content {
        return Err(ToolError::new(
            ErrorKind::NoChange,
            "the replacements produced content identical to the original",
        ));
    }

    Ok(AppliedEdits {
        new_content,
        used_fallback,
    })
}

/// The changed regions of `old` → `new`, with `context` unchanged lines kept
/// around each. Also reports the 1-based line in `new` where the first change
/// appears.
pub fn diff_hunks(old: &str, new: &str, context: u32) -> (Vec<DiffHunk>, Option<u64>) {
    let diff = TextDiff::from_lines(old, new);
    let mut first_changed_line = None;
    let mut hunks = Vec::new();

    for group in diff.grouped_ops(context as usize) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;

        let mut rows = Vec::new();
        for op in &group {
            if first_changed_line.is_none() && !matches!(op.tag(), similar::DiffTag::Equal) {
                first_changed_line = Some(op.new_range().start as u64 + 1);
            }
            for change in diff.iter_changes(op) {
                let value = change.value();
                let text = value.strip_suffix('\n').unwrap_or(value).to_string();
                rows.push(DiffRow {
                    op: match change.tag() {
                        ChangeTag::Equal => DiffOp::Equal,
                        ChangeTag::Delete => DiffOp::Delete,
                        ChangeTag::Insert => DiffOp::Insert,
                    },
                    old_line: change.old_index().map(|i| i as u64 + 1),
                    new_line: change.new_index().map(|i| i as u64 + 1),
                    text,
                });
            }
        }

        hunks.push(DiffHunk {
            old_start: old_range.start as u64 + 1,
            old_lines: old_range.len() as u64,
            new_start: new_range.start as u64 + 1,
            new_lines: new_range.len() as u64,
            rows,
        });
    }

    (hunks, first_changed_line)
}

/// Elements `split('\n')` would produce — the count a JS caller sees.
pub fn split_line_count(content: &str) -> u64 {
    content.split('\n').count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> EditReplacement {
        EditReplacement {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn detects_line_ending_from_the_first_newline() {
        assert!(!detect_crlf("a\nb\r\n"));
        assert!(detect_crlf("a\r\nb\n"));
        assert!(!detect_crlf("no newline"));
        assert!(!detect_crlf(""));
    }

    #[test]
    fn normalization_folds_typography_and_trailing_space() {
        assert_eq!(normalize_for_fuzzy_match("a  \nb\t"), "a\nb");
        assert_eq!(normalize_for_fuzzy_match("\u{2018}x\u{2019}"), "'x'");
        assert_eq!(normalize_for_fuzzy_match("\u{201C}x\u{201D}"), "\"x\"");
        assert_eq!(normalize_for_fuzzy_match("a\u{2014}b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{00A0}b"), "a b");
        // Idempotent: the second matching pass re-normalizes.
        let once = normalize_for_fuzzy_match("a \u{2013} b\u{00A0}\u{3000}");
        assert_eq!(normalize_for_fuzzy_match(&once), once);
    }

    #[test]
    fn multiple_edits_match_the_original_not_each_other() {
        let content = "alpha\nbeta\ngamma\n";
        let out = apply_edits(content, &[edit("alpha", "beta"), edit("beta", "delta")]).unwrap();
        // If edits applied incrementally, the second would hit the new "beta".
        assert_eq!(out.new_content, "beta\ndelta\ngamma\n");
        assert!(!out.used_fallback);
    }

    #[test]
    fn rejects_duplicate_and_overlapping_targets() {
        let content = "x x\n";
        assert_eq!(
            apply_edits(content, &[edit("x", "y")]).unwrap_err().kind,
            ErrorKind::MultipleMatches
        );

        let content = "abcdef\n";
        let err = apply_edits(content, &[edit("abcd", "1"), edit("cdef", "2")]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overlap);

        // Nested is a special case of overlapping.
        let err = apply_edits(content, &[edit("abcdef", "1"), edit("cd", "2")]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overlap);
    }

    #[test]
    fn fallback_preserves_untouched_lines_verbatim() {
        // Line 1 has trailing whitespace the caller did not ask to change; the
        // edit targets line 2 through a smart quote. Only line 2 is rewritten.
        let content = "keep me   \nsay \u{201C}hi\u{201D}\ntail   \n";
        let out = apply_edits(content, &[edit("say \"hi\"", "say \"bye\"")]).unwrap();
        assert!(out.used_fallback);
        assert_eq!(out.new_content, "keep me   \nsay \"bye\"\ntail   \n");
    }

    #[test]
    fn identical_result_is_rejected() {
        let content = "a\n";
        // Replacing with the same text produces no change.
        let err = apply_edits(content, &[edit("a", "a")]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange);
    }

    #[test]
    fn empty_and_whitespace_only_targets_are_rejected() {
        assert_eq!(
            apply_edits("a\n", &[edit("", "x")]).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(
            apply_edits("a   \n", &[edit("   ", "x")]).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn exact_file_policy_allows_a_whole_file_whitespace_target() {
        // The issue's motivating case: a file containing exactly three spaces.
        let out = apply_edits_opts(
            "   ",
            &[edit("   ", "x")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap();
        assert_eq!(out.new_content, "x");
        assert!(!out.used_fallback);

        // Any is_js_whitespace mix works, as long as it equals the whole file.
        let out = apply_edits_opts(
            "\t \u{00A0}",
            &[edit("\t \u{00A0}", "y")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap();
        assert_eq!(out.new_content, "y");
    }

    #[test]
    fn exact_file_policy_keeps_empty_invalid_and_partial_whitespace_no_match() {
        // Empty oldText stays invalid regardless of policy.
        let err = apply_edits_opts(
            "a\n",
            &[edit("", "x")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput);

        // A whitespace target that is not the whole file is NoMatch — never a
        // positional guess inside the file.
        let err = apply_edits_opts(
            "a   b\n",
            &[edit("   ", "x")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoMatch);
        assert_eq!(err.edit_index, Some(0));

        // Identical replacement of the whole file is still NoChange.
        let err = apply_edits_opts(
            "   ",
            &[edit("   ", "   ")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NoChange);
    }

    #[test]
    fn exact_file_whitespace_edit_cannot_share_a_batch() {
        // The whole-file span overlaps everything, so the existing
        // disjointness rule keeps multi-edit batches out without a bespoke
        // check. (A fuzzy-fallback companion is impossible outright: a
        // whitespace-only file normalizes to an empty matching base, so no
        // other target can resolve at all.)
        let err = apply_edits_opts(
            "   ",
            &[edit("   ", "x"), edit("   ", "y")],
            WhitespaceOnlyTargetPolicy::ExactFile,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overlap);
    }

    #[test]
    fn hunks_carry_line_numbers_and_first_change() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
        let new = "1\n2\n3\n4\n5\n6\nSEVEN\n8\n9\n10\n11\n12\n";
        let (hunks, first) = diff_hunks(old, new, 2);
        assert_eq!(first, Some(7));
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 5);
        assert!(
            h.rows
                .iter()
                .any(|r| r.op == DiffOp::Insert && r.text == "SEVEN")
        );
        assert!(
            h.rows
                .iter()
                .any(|r| r.op == DiffOp::Delete && r.text == "7")
        );
        // Far-away context is not carried.
        assert!(!h.rows.iter().any(|r| r.text == "1"));
    }
}
