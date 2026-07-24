//! The `read` tool: windowed file reads served from the warm file cache.

use crate::util::resolve_path;
use hearth_core::{profile, Engine};
use hearth_proto::{ReadParams, ReadResult, ToolError, ToolResult};
use std::fmt::Write as _;

/// Read a file (optionally a line window), reusing the shared file cache.
pub fn read(engine: &Engine, params: &ReadParams) -> ToolResult<ReadResult> {
    profile!("tool.read", {
        let path = resolve_path(engine, &params.path);
        let trust = engine.stat_free(&path);
        let (entry, cache_hit) = engine.files().get_trusting(&path, trust)?;
        let total_lines = entry.line_index().line_count();
        let byte_len = entry.bytes().len() as u64;
        let binary = entry.is_binary();

        if binary {
            return Ok(ReadResult {
                content: String::new(),
                total_lines,
                returned_lines: 0,
                byte_len,
                truncated: false,
                binary: true,
                cache_hit,
            });
        }

        // Whole-file fast path: no window, no line numbers. `to_text` skips
        // UTF-8 re-validation on the warm path (validity is cached).
        if params.offset.is_none() && params.limit.is_none() && !params.line_numbers {
            let content = entry.to_text();
            return Ok(ReadResult {
                returned_lines: total_lines,
                content,
                total_lines,
                byte_len,
                truncated: false,
                binary: false,
                cache_hit,
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
            Some(n) if n > 0 => (start_line + n - 1).min(total_lines),
            _ => total_lines,
        };

        let idx = entry.line_index();
        let bytes = entry.bytes();
        let start_off = idx.line_range(start_line).map(|(s, _)| s).unwrap_or(0);
        // Include the trailing newline of the last line in the window (so the
        // slice matches `cat`), by ending at the *start* of the following line.
        let end_off = if end_line < total_lines {
            idx.line_range(end_line + 1).map(|(s, _)| s).unwrap_or(bytes.len())
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
            let mut out = String::with_capacity(window.len() + (returned_lines as usize) * 8);
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
        })
    })
}
