//! The `write` tool: atomic full-file writes that refresh the warm cache.

use crate::util::{atomic_write, resolve_path};
use hearth_core::{profile, Engine};
use hearth_proto::{ToolResult, WriteParams, WriteResult};
use std::sync::Arc;

/// Write `content` to `path` atomically and update the file cache in place so a
/// following `read`/`edit` is warm without touching the disk again.
pub fn write(engine: &Engine, params: &WriteParams) -> ToolResult<WriteResult> {
    profile!("tool.write", {
        let path = resolve_path(engine, &params.path);
        let existed = path.exists();
        let bytes = params.content.as_bytes();

        let meta = atomic_write(&path, bytes, params.create_dirs)?;

        // Refresh the cache with the exact bytes we just wrote.
        let arc: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        engine.files().put_written(&path, arc, meta.size, meta.mtime_ns);

        hearth_core::profiler::count("tool.write.bytes", bytes.len() as u64);
        Ok(WriteResult { bytes_written: bytes.len() as u64, existed })
    })
}
