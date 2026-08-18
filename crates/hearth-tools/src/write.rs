//! The `write` tool: full-file writes that refresh the warm cache.

use crate::util::{resolve_path, resolve_write_target, write_bytes};
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{ToolResult, WriteMode, WriteParams, WriteResult};
use std::sync::Arc;

/// Write `content` to `path` and update the file cache in place so a following
/// `read`/`edit` is warm without touching the disk again.
pub fn write(engine: &Engine, params: &WriteParams) -> ToolResult<WriteResult> {
    write_cancellable(engine, params, &CancelToken::none())
}

/// As [`write`], but polls `cancel` around the blocking steps.
///
/// Cancellation is observed *before* the write is issued and again after it
/// commits; it is never observed in between, because releasing the path's
/// mutation lock while the bytes were still landing would let a queued mutation
/// of the same file interleave with this one.
pub fn write_cancellable(
    engine: &Engine,
    params: &WriteParams,
    cancel: &CancelToken,
) -> ToolResult<WriteResult> {
    // The dispatch/CLI path only has a borrowed `&params`, so it clones the
    // content once (unavoidable there). The napi fast path calls `write_owned`
    // directly and avoids that clone.
    write_owned_cancellable(engine, params, params.content.clone(), cancel)
}

/// As [`write`], but takes ownership of `content` and **moves** it into the
/// cache instead of copying it — used by the napi fast path, which already owns
/// the string, to remove one full content copy from the write.
pub fn write_owned(
    engine: &Engine,
    path: &str,
    content: String,
    create_dirs: bool,
) -> ToolResult<WriteResult> {
    let params = WriteParams {
        path: path.to_string(),
        content: String::new(),
        create_dirs,
        mode: WriteMode::default(),
        follow_symlinks: true,
    };
    write_owned_cancellable(engine, &params, content, &CancelToken::none())
}

/// The single implementation every `write` entry point funnels into.
///
/// `params.content` is ignored; `content` is the payload, so a caller that
/// already owns the string hands it over without a copy.
pub fn write_owned_cancellable(
    engine: &Engine,
    params: &WriteParams,
    content: String,
    cancel: &CancelToken,
) -> ToolResult<WriteResult> {
    profile!("tool.write", {
        cancel.check()?;
        let requested = resolve_path(engine, &params.path);
        let (target, followed_symlink) = resolve_write_target(&requested, params.follow_symlinks);

        // Hold the path's mutation lock across the write *and* the cache
        // refresh: a concurrent writer must not observe the file changed while
        // the cache still describes the old bytes.
        let _guard = engine.lock_path(&target);
        cancel.check()?;

        let bytes_written = content.len() as u64;
        let meta = write_bytes(&target, content.as_bytes(), params.create_dirs, params.mode)?;

        // Invalidate every cached spelling of the target before republishing
        // the freshly-written bytes. Doing this after `put_written` would let
        // alias invalidation remove the replacement too.
        engine.note_mutation(&target, !meta.existed);
        if followed_symlink {
            // Preserve the requested spelling in the journal as well. This is
            // required both for creation (walk membership) and overwrite
            // (derived state rooted through the alias).
            engine.note_mutation(&requested, !meta.existed);
        }
        // Move the content's own allocation into the cache Arc (no extra copy).
        let arc: Arc<[u8]> = Arc::from(content.into_bytes().into_boxed_slice());
        engine
            .files()
            .put_written(&target, arc, meta.size, meta.mtime_ns);

        hearth_core::profiler::count("tool.write.bytes", bytes_written);
        Ok(WriteResult {
            bytes_written,
            existed: meta.existed,
            path: target.display().to_string(),
            followed_symlink,
        })
    })
}
