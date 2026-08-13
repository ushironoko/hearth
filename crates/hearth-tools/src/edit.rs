//! The `edit` tools: a single exact replacement, and an atomic batch of
//! disjoint replacements matched against the original file.
//!
//! Both operate on the cached buffer and persist through the shared write path,
//! holding the target's mutation lock for the whole read-modify-write so two
//! concurrent edits of the same file cannot lose an update.

use crate::edit_text;
use crate::util::{resolve_path, resolve_write_target, write_bytes};
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{
    EditBatchParams, EditBatchResult, EditParams, EditResult, ErrorKind, ToolError, ToolResult,
};
use std::sync::Arc;

const MAX_BATCH_EDITS: usize = 10_000;

/// Replace `old_string` with `new_string` in a file. Without `replace_all`, the
/// match must be unique. Operates on the cached buffer, then writes atomically.
pub fn edit(engine: &Engine, params: &EditParams) -> ToolResult<EditResult> {
    edit_cancellable(engine, params, &CancelToken::none())
}

/// As [`edit`], but polls `cancel` at its safe points.
pub fn edit_cancellable(
    engine: &Engine,
    params: &EditParams,
    cancel: &CancelToken,
) -> ToolResult<EditResult> {
    profile!("tool.edit", {
        cancel.check()?;
        if params.old_string == params.new_string {
            return Err(ToolError::invalid(
                "old_string and new_string are identical",
            ));
        }
        if params.old_string.is_empty() {
            return Err(ToolError::invalid("old_string must not be empty"));
        }

        let requested = resolve_path(engine, &params.path);
        let (path, _followed) = resolve_write_target(&requested, true);
        let _guard = engine.lock_path(&path);
        cancel.check()?;

        let (entry, _hit) = engine
            .files()
            .get_bounded(&path, engine.config().max_tool_file_bytes)?
            .ok_or_else(|| ToolError::invalid("file exceeds edit byte limit"))?;

        // Editing requires valid UTF-8; refuse binary/invalid content.
        let text = std::str::from_utf8(entry.bytes()).map_err(|_| {
            ToolError::new(ErrorKind::InvalidInput, "file is not valid UTF-8")
                .with_path(params.path.clone())
        })?;

        let count = text.matches(&params.old_string).count();
        if count == 0 {
            return Err(ToolError::new(ErrorKind::NoMatch, "old_string not found")
                .with_path(params.path.clone()));
        }
        if count > 1 && !params.replace_all {
            return Err(ToolError::new(
                ErrorKind::MultipleMatches,
                format!("old_string is not unique ({count} matches); pass replace_all"),
            )
            .with_path(params.path.clone()));
        }

        let replacements = if params.replace_all { count } else { 1 };
        validate_result_size(
            text.len(),
            params.old_string.len(),
            params.new_string.len(),
            replacements,
            engine.config().max_edit_result_bytes,
        )?;
        let (new_text, replacements) = if params.replace_all {
            (
                text.replace(&params.old_string, &params.new_string),
                count as u64,
            )
        } else {
            (text.replacen(&params.old_string, &params.new_string, 1), 1)
        };

        cancel.check()?;
        let byte_len = commit(engine, &requested, &path, new_text)?;

        hearth_core::profiler::count("tool.edit.replacements", replacements);
        Ok(EditResult {
            replacements,
            byte_len,
        })
    })
}

/// Apply several disjoint replacements to one file in a single atomic commit.
///
/// Every `oldText` is matched against the original content, so the edits do not
/// see each other. Either all of them apply or the file is untouched.
pub fn edit_batch(engine: &Engine, params: &EditBatchParams) -> ToolResult<EditBatchResult> {
    edit_batch_cancellable(engine, params, &CancelToken::none())
}

/// As [`edit_batch`], but polls `cancel` at its safe points.
///
/// The mutation lock is held across matching, writing and the cache refresh —
/// cancellation is never observed while native work could still commit.
pub fn edit_batch_cancellable(
    engine: &Engine,
    params: &EditBatchParams,
    cancel: &CancelToken,
) -> ToolResult<EditBatchResult> {
    profile!("tool.edit_batch", {
        cancel.check()?;
        if params.edits.is_empty() {
            return Err(ToolError::invalid(
                "edits must contain at least one replacement",
            ));
        }
        if params.edits.len() > MAX_BATCH_EDITS {
            return Err(ToolError::invalid("edit batch exceeds 10000 replacements"));
        }

        let requested = resolve_path(engine, &params.path);
        let (path, followed_symlink) = resolve_write_target(&requested, params.follow_symlinks);
        let _guard = engine.lock_path(&path);
        cancel.check()?;

        let (entry, _hit) = engine
            .files()
            .get_bounded(&path, engine.config().max_tool_file_bytes)?
            .ok_or_else(|| ToolError::invalid("file exceeds edit byte limit"))?;
        let raw = std::str::from_utf8(entry.bytes()).map_err(|_| {
            ToolError::new(ErrorKind::InvalidInput, "file is not valid UTF-8")
                .with_path(params.path.clone())
        })?;

        // The raw pre-edit text, BOM and line endings intact, snapshotted here
        // — while the mutation lock is held — so it and the commit below are
        // one transaction. Normalized `content` must not stand in for it: the
        // deletion and trailing-newline edge cases exist only in the raw form.
        let original_content = params.return_original_content.then(|| raw.to_string());

        let (had_bom, without_bom) = edit_text::strip_bom(raw);
        let crlf = edit_text::detect_crlf(without_bom);
        // Borrows the cached bytes when there is no CRLF to collapse, so the
        // common case matches and diffs without copying the file at all. `entry`
        // is held until the commit for exactly that reason.
        let content = edit_text::normalize_to_lf(without_bom);

        let added = params
            .edits
            .iter()
            .try_fold(0usize, |total, edit| total.checked_add(edit.new_text.len()))
            .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
        if content.len().saturating_add(added) > engine.config().max_edit_result_bytes {
            return Err(ToolError::invalid("edit result exceeds byte limit"));
        }

        cancel.check()?;
        let applied = edit_text::apply_edits_opts(
            &content,
            &params.edits,
            params.whitespace_only_target_policy,
        )
        .map_err(|e| annotate(e, &params.path))?;

        // Diff rows and optional content/original copies are response memory on
        // top of the rewritten file. Reserve their coarse worst case before
        // materializing a diff.
        let optional_bytes = usize::from(params.return_original_content)
            .saturating_mul(raw.len())
            .saturating_add(
                usize::from(params.return_content).saturating_mul(applied.new_content.len()),
            );
        let diff_budget = content
            .len()
            .saturating_add(applied.new_content.len())
            .saturating_add(optional_bytes);
        if diff_budget > engine.config().max_edit_result_bytes {
            return Err(ToolError::invalid("edit response exceeds byte limit"));
        }
        let (hunks, first_changed_line) = if params.skip_diff {
            (Vec::new(), None)
        } else {
            edit_text::diff_hunks(&content, &applied.new_content, params.diff_context)
        };
        let old_line_count = edit_text::split_line_count(&content);
        let new_line_count = edit_text::split_line_count(&applied.new_content);

        // Restore the file's own byte conventions only at the very end, so
        // matching and diffing both ran over one canonical representation.
        let final_capacity = applied
            .new_content
            .len()
            .checked_add(3)
            .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
        let newline_expansion = if crlf {
            applied
                .new_content
                .bytes()
                .filter(|&byte| byte == b'\n')
                .count()
        } else {
            0
        };
        let restored_capacity = final_capacity
            .checked_add(newline_expansion)
            .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
        if restored_capacity > engine.config().max_edit_result_bytes {
            return Err(ToolError::invalid("edit result exceeds byte limit"));
        }
        let mut final_text = String::with_capacity(restored_capacity);
        if had_bom {
            final_text.push('\u{FEFF}');
        }
        final_text.push_str(&edit_text::restore_line_endings(&applied.new_content, crlf));

        cancel.check()?;
        let byte_len = commit_with(engine, &requested, &path, final_text, params.mode)?;
        if followed_symlink {
            engine.files().invalidate(&requested);
        }

        hearth_core::profiler::count("tool.edit_batch.replacements", params.edits.len() as u64);
        Ok(EditBatchResult {
            path: path.display().to_string(),
            replacements: params.edits.len() as u64,
            byte_len,
            used_normalized_fallback: applied.used_fallback,
            had_bom,
            crlf,
            old_line_count,
            new_line_count,
            first_changed_line,
            hunks,
            content: params.return_content.then_some(applied.new_content),
            original_content,
        })
    })
}

fn validate_result_size(
    original: usize,
    old_len: usize,
    new_len: usize,
    replacements: usize,
    limit: usize,
) -> ToolResult<()> {
    let removed = old_len
        .checked_mul(replacements)
        .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
    let added = new_len
        .checked_mul(replacements)
        .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
    let result = original
        .checked_sub(removed)
        .and_then(|size| size.checked_add(added))
        .ok_or_else(|| ToolError::invalid("edit result size overflow"))?;
    if result > limit {
        return Err(ToolError::invalid("edit result exceeds byte limit"));
    }
    Ok(())
}

/// Attach the caller's path to an error raised by the text layer, which only
/// knows about strings.
fn annotate(mut err: ToolError, path: &str) -> ToolError {
    if err.path.is_none() {
        err.path = Some(path.to_string());
    }
    err
}

fn commit(
    engine: &Engine,
    requested: &std::path::Path,
    path: &std::path::Path,
    new_text: String,
) -> ToolResult<u64> {
    commit_with(
        engine,
        requested,
        path,
        new_text,
        hearth_proto::WriteMode::default(),
    )
}

/// Persist the edited text and refresh the caches. The caller must already hold
/// `path`'s mutation lock.
fn commit_with(
    engine: &Engine,
    _requested: &std::path::Path,
    path: &std::path::Path,
    new_text: String,
    mode: hearth_proto::WriteMode,
) -> ToolResult<u64> {
    let byte_len = new_text.len() as u64;
    let meta = write_bytes(path, new_text.as_bytes(), false, mode)?;
    let arc: Arc<[u8]> = Arc::from(new_text.into_bytes().into_boxed_slice());
    engine
        .files()
        .put_written(path, arc, meta.size, meta.mtime_ns);
    // An edit never creates the file, but it can rewrite one that steers
    // directory traversal — `.gitignore` and friends — which changes what a
    // cached walk would enumerate.
    engine.note_mutation(path, false);
    Ok(byte_len)
}
