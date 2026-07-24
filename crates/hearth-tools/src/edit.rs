//! The `edit` tool: exact string replacement on the cached buffer, persisted
//! atomically.

use crate::util::{atomic_write, resolve_path};
use hearth_core::{profile, Engine};
use hearth_proto::{EditParams, EditResult, ErrorKind, ToolError, ToolResult};
use std::sync::Arc;

/// Replace `old_string` with `new_string` in a file. Without `replace_all`, the
/// match must be unique. Operates on the cached buffer, then writes atomically.
pub fn edit(engine: &Engine, params: &EditParams) -> ToolResult<EditResult> {
    profile!("tool.edit", {
        if params.old_string == params.new_string {
            return Err(ToolError::invalid("old_string and new_string are identical"));
        }
        if params.old_string.is_empty() {
            return Err(ToolError::invalid("old_string must not be empty"));
        }

        let path = resolve_path(engine, &params.path);
        let (entry, _hit) = engine.files().get(&path)?;

        // Editing requires valid UTF-8; refuse binary/invalid content.
        let text = std::str::from_utf8(entry.bytes()).map_err(|_| {
            ToolError::new(ErrorKind::InvalidInput, "file is not valid UTF-8").with_path(params.path.clone())
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

        let (new_text, replacements) = if params.replace_all {
            (text.replace(&params.old_string, &params.new_string), count as u64)
        } else {
            (text.replacen(&params.old_string, &params.new_string, 1), 1)
        };

        let bytes = new_text.as_bytes();
        let meta = atomic_write(&path, bytes, false)?;
        let arc: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        engine.files().put_written(&path, arc, meta.size, meta.mtime_ns);

        hearth_core::profiler::count("tool.edit.replacements", replacements);
        Ok(EditResult { replacements, byte_len: bytes.len() as u64 })
    })
}
