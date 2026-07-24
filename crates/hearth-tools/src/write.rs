//! The `write` tool: atomic full-file writes that refresh the warm cache.

use crate::util::{atomic_write, resolve_path};
use hearth_core::{profile, Engine};
use hearth_proto::{ToolResult, WriteParams, WriteResult};
use std::sync::Arc;

/// Write `content` to `path` atomically and update the file cache in place so a
/// following `read`/`edit` is warm without touching the disk again.
pub fn write(engine: &Engine, params: &WriteParams) -> ToolResult<WriteResult> {
    // The dispatch/CLI path only has a borrowed `&params`, so it clones the
    // content once (unavoidable there). The napi fast path calls `write_owned`
    // directly and avoids that clone.
    write_owned(engine, &params.path, params.content.clone(), params.create_dirs)
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
    profile!("tool.write", {
        let path = resolve_path(engine, path);
        let existed = path.exists();
        let bytes_written = content.len() as u64;

        let meta = atomic_write(&path, content.as_bytes(), create_dirs)?;

        // Move the content's own allocation into the cache Arc (no extra copy).
        let arc: Arc<[u8]> = Arc::from(content.into_bytes().into_boxed_slice());
        engine.files().put_written(&path, arc, meta.size, meta.mtime_ns);

        hearth_core::profiler::count("tool.write.bytes", bytes_written);
        Ok(WriteResult { bytes_written, existed })
    })
}
