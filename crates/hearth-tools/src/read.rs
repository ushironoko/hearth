//! The `read` tool: windowed file reads served from the warm file cache.

use crate::util::resolve_path;
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{LineWindowMode, ReadParams, ReadResult, ToolError, ToolResult};
use std::fmt::Write as _;

/// Read the whole file's raw bytes from the warm cache (a single copy out of the
/// shared buffer). Used by the napi `readBytes` binary-safe API. Returning the
/// cached buffer *without* copying is unsafe here — a JS `Buffer` is mutable and
/// would corrupt the shared cache entry — so this copies, exactly like the
/// `read` String path.
pub fn read_bytes(engine: &Engine, params: &ReadParams) -> ToolResult<Vec<u8>> {
    read_bytes_cancellable(engine, params, &CancelToken::none())
}

/// As [`read_bytes`], but rejects a pre-aborted or mid-flight cancellation.
pub fn read_bytes_cancellable(
    engine: &Engine,
    params: &ReadParams,
    cancel: &CancelToken,
) -> ToolResult<Vec<u8>> {
    profile!("tool.read_bytes", {
        cancel.check()?;
        let path = resolve_path(engine, &params.path);
        let trust = engine.stat_free(&path);
        let (entry, _hit) = engine
            .files()
            .get_bounded_trusting(&path, engine.config().max_tool_file_bytes, trust)?
            .ok_or_else(|| ToolError::invalid("file exceeds read byte limit"))?;
        cancel.check()?;
        Ok(entry.bytes().to_vec())
    })
}

/// Read a file (optionally a line window), reusing the shared file cache.
pub fn read(engine: &Engine, params: &ReadParams) -> ToolResult<ReadResult> {
    read_cancellable(engine, params, &CancelToken::none())
}

/// As [`read`], but rejects a pre-aborted or mid-flight cancellation.
pub fn read_cancellable(
    engine: &Engine,
    params: &ReadParams,
    cancel: &CancelToken,
) -> ToolResult<ReadResult> {
    profile!("tool.read", {
        cancel.check()?;
        let path = resolve_path(engine, &params.path);
        let trust = engine.stat_free(&path);
        let (entry, cache_hit) = engine
            .files()
            .get_bounded_trusting(&path, engine.config().max_tool_file_bytes, trust)?
            .ok_or_else(|| ToolError::invalid("file exceeds read byte limit"))?;
        cancel.check()?;

        let idx = entry.line_index();
        let bytes = entry.bytes();
        let split_mode = params.line_mode == LineWindowMode::SplitLines;
        // `split('\n')` counts the empty element after a trailing newline; the
        // `cat`-style slice does not.
        let total_lines = if split_mode {
            idx.split_count()
        } else {
            idx.line_count()
        };
        let byte_len = bytes.len() as u64;
        let binary = entry.is_binary();
        let ends_with_newline = bytes.last() == Some(&b'\n');

        if binary {
            return Ok(ReadResult {
                content: String::new(),
                total_lines,
                returned_lines: 0,
                byte_len,
                truncated: false,
                binary: true,
                cache_hit,
                ends_with_newline,
            });
        }

        // Whole-file fast path: no window, no line numbers, and — in split mode
        // — no trailing newline to drop. `to_text` skips UTF-8 re-validation on
        // the warm path (validity is cached).
        let whole_file = params.offset.is_none()
            && params.limit.is_none()
            && !params.line_numbers
            && !(split_mode && ends_with_newline);
        if whole_file {
            let content = entry.to_text();
            return Ok(ReadResult {
                returned_lines: total_lines,
                content,
                total_lines,
                byte_len,
                truncated: false,
                binary: false,
                cache_hit,
                ends_with_newline,
            });
        }

        // Windowed read via the line index.
        let start_line = params.offset.unwrap_or(1).max(1);
        if start_line > total_lines {
            return Err(ToolError::invalid(format!(
                "offset {start_line} is past end of file ({total_lines} lines)"
            )));
        }
        let end_line = match params.limit {
            Some(n) if n > 0 => start_line.saturating_add(n - 1).min(total_lines),
            _ => total_lines,
        };

        let start_off = idx.line_range(start_line).map(|(s, _)| s).unwrap_or(0);
        let end_off = if split_mode {
            // Join semantics: stop at the end of the last element's text, so the
            // window never carries a trailing newline.
            idx.line_range(end_line)
                .map(|(_, e)| e)
                .unwrap_or(bytes.len())
        } else if end_line < total_lines {
            // Slice semantics: include the trailing newline of the last line in
            // the window (so it matches `cat`) by ending at the start of the
            // following line.
            idx.line_range(end_line + 1)
                .map(|(s, _)| s)
                .unwrap_or(bytes.len())
        } else {
            bytes.len()
        };
        let slice = &bytes[start_off.min(bytes.len())..end_off.min(bytes.len())];
        // Slices at line boundaries of valid UTF-8 are themselves valid, so skip
        // re-validation on the warm path.
        let window: std::borrow::Cow<'_, str> = if entry.is_valid_utf8() {
            // SAFETY: whole file is valid UTF-8 and the slice starts/ends on line
            // boundaries (ASCII '\n'), so it is valid UTF-8.
            std::borrow::Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(slice) })
        } else {
            String::from_utf8_lossy(slice)
        };

        let returned_lines = end_line - start_line + 1;
        let content = if params.line_numbers {
            let numbered_capacity = usize::try_from(returned_lines)
                .ok()
                .and_then(|lines| lines.checked_mul(8))
                .and_then(|prefixes| window.len().checked_add(prefixes))
                .ok_or_else(|| ToolError::invalid("numbered read output size overflow"))?;
            if numbered_capacity > engine.config().max_tool_file_bytes as usize {
                return Err(ToolError::invalid(
                    "numbered read output exceeds byte limit",
                ));
            }
            let mut out = String::with_capacity(numbered_capacity);
            for (i, line) in window.split_inclusive('\n').enumerate() {
                let n = start_line + i as u64;
                let has_nl = line.ends_with('\n');
                let text = line.strip_suffix('\n').unwrap_or(line);
                let _ = write!(out, "{n:>6}\t{text}");
                if has_nl {
                    out.push('\n');
                }
            }
            out
        } else {
            window.into_owned()
        };

        let truncated = start_line > 1 || end_line < total_lines;
        Ok(ReadResult {
            content,
            total_lines,
            returned_lines,
            byte_len,
            truncated,
            binary: false,
            cache_hit,
            ends_with_newline,
        })
    })
}
