//! Byte-offset ↔ line/column mapping built once and reused for many lookups.
//!
//! Adapted from `vize_carton`'s `LineIndex`, but tuned for CLI tools: it works
//! on raw bytes (so it is `\n`-scan-friendly via SIMD `memchr`), reports
//! **1-based** line/column with **byte** columns, and can slice a line's text
//! back out. Building is O(n); a line lookup is O(log lines).

/// Precomputed byte offsets of the start of each line.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// `line_starts[i]` is the byte offset where line `i` (0-based) begins.
    /// `line_starts[0]` is always 0. Length == number of lines.
    line_starts: Box<[u32]>,
    len: u32,
}

impl LineIndex {
    /// Hard upper bound for the heap allocation made by [`LineIndex::new`].
    ///
    /// There can be at most one line start per source byte plus the initial
    /// zero. The file cache reserves this full amount before retaining an
    /// entry, so building the lazy index never grows accounted cache memory.
    pub(crate) fn max_heap_bytes(source_len: usize) -> Option<u64> {
        u64::try_from(source_len)
            .ok()?
            .checked_add(1)?
            .checked_mul(size_of::<u32>() as u64)
    }

    /// Exact logical heap bytes held by this index.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> u64 {
        self.line_starts.len() as u64 * size_of::<u32>() as u64
    }

    /// Build the index for `source` in two SIMD-accelerated passes.
    pub fn new(source: &[u8]) -> Self {
        // Count first so the boxed table has no spare Vec capacity. This makes
        // its retained allocation exact and bounded by `max_heap_bytes`.
        let newline_count = memchr::memchr_iter(b'\n', source).count();
        let mut line_starts = Vec::with_capacity(newline_count + 1);
        line_starts.push(0u32);
        for pos in memchr::memchr_iter(b'\n', source) {
            // Guard against absurdly large files (u32 offsets keep the table small).
            let next = (pos + 1).min(u32::MAX as usize) as u32;
            line_starts.push(next);
        }
        // A trailing '\n' produced a final "line start" at EOF; that is a real
        // empty last line only if the file does not end in '\n'. We keep it and
        // let `line_count` account for the trailing newline.
        Self {
            line_starts: line_starts.into_boxed_slice(),
            len: source.len().min(u32::MAX as usize) as u32,
        }
    }

    /// Number of lines. A file ending in `\n` does not count a phantom line.
    pub fn line_count(&self) -> u64 {
        let n = self.line_starts.len() as u64;
        // If the last recorded start is exactly EOF, it was produced by a
        // trailing newline and is not a content line.
        if n > 1 && *self.line_starts.last().unwrap() == self.len {
            n - 1
        } else {
            n
        }
    }

    /// Number of elements `split('\n')` would produce: one more than the number
    /// of newlines, so a trailing newline *does* contribute a final empty
    /// element. This is the count JS callers see, and it differs from
    /// [`line_count`](Self::line_count) by exactly that phantom line.
    pub fn split_count(&self) -> u64 {
        self.line_starts.len() as u64
    }

    /// Map a byte offset to a **1-based** `(line, column)`, column in bytes.
    pub fn line_col(&self, offset: usize) -> (u64, u64) {
        let offset = (offset.min(self.len as usize)) as u32;
        let line = match self.line_starts.binary_search(&offset) {
            Ok(l) => l,
            Err(next) => next - 1,
        };
        let col = offset - self.line_starts[line];
        (line as u64 + 1, col as u64 + 1)
    }

    /// The 1-based line number containing `offset`.
    #[inline]
    pub fn line_of(&self, offset: usize) -> u64 {
        self.line_col(offset).0
    }

    /// Byte range `[start, end)` of the given **1-based** line, excluding the
    /// trailing newline. Returns `None` if the line does not exist.
    pub fn line_range(&self, line: u64) -> Option<(usize, usize)> {
        if line == 0 || line > self.line_starts.len() as u64 {
            return None;
        }
        let idx = (line - 1) as usize;
        let start = self.line_starts[idx] as usize;
        let end = self
            .line_starts
            .get(idx + 1)
            .map(|&s| (s as usize).saturating_sub(1)) // drop the '\n'
            .unwrap_or(self.len as usize);
        Some((start, end.max(start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let s = b"abc\ndef\nghi";
        let idx = LineIndex::new(s);
        assert_eq!(idx.line_count(), 3);
        assert_eq!(idx.line_col(0), (1, 1));
        assert_eq!(idx.line_col(4), (2, 1));
        assert_eq!(idx.line_col(5), (2, 2));
        assert_eq!(idx.line_range(2), Some((4, 7)));
    }

    #[test]
    fn trailing_newline() {
        let s = b"a\nb\n";
        let idx = LineIndex::new(s);
        assert_eq!(idx.line_count(), 2);
        assert_eq!(idx.line_range(1), Some((0, 1)));
        assert_eq!(idx.line_range(2), Some((2, 3)));
    }

    #[test]
    fn heap_allocation_is_exact_and_bounded_by_source_length() {
        let source = b"\n\nnot-a-newline\n";
        let idx = LineIndex::new(source);

        assert_eq!(idx.heap_bytes(), 4 * 4);
        assert!(idx.heap_bytes() <= LineIndex::max_heap_bytes(source.len()).unwrap());
    }
}
