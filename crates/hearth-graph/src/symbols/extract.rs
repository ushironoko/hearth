use std::collections::hash_map::Entry;

use compact_str::CompactString;
use rustc_hash::FxHashMap;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use super::{MAX_SYMBOLS_PER_FILE, Symbol, SymbolKind, kind_specificity};
use crate::{LanguageId, ParserPool};

/// Intermediate match before nesting depth and public integer widths are known.
struct RawTag {
    name: String,
    kind: SymbolKind,
    line: usize,
    column: usize,
    /// Byte offset of the name node, which identifies the tagged entity.
    name_byte: usize,
    /// Byte range of the whole definition node, used to derive nesting.
    start_byte: usize,
    end_byte: usize,
}

/// Extracts the symbols of one file in source order.
///
/// Unsupported languages and parse or query failures produce an empty
/// outline, because an unavailable outline is not a source-processing error.
pub fn extract_symbols(source: &str, path: &str, pool: &mut ParserPool<'_>) -> Vec<Symbol> {
    crate::analyze::extract_symbols_only(source, path, pool)
}

pub(crate) fn extract_symbols_from_tree(
    source: &str,
    tree: &Tree,
    id: LanguageId,
    pool: &mut ParserPool<'_>,
) -> Vec<Symbol> {
    const _: () = assert!(MAX_SYMBOLS_PER_FILE <= u16::MAX as usize);
    let merge_adjacent_same_name_definitions = pool
        .registry()
        .get(id)
        .is_some_and(|spec| spec.merge_adjacent_same_name_definitions);
    let Some(query) = pool.tags_query(id) else {
        return Vec::new();
    };

    let capture_names = query.capture_names();
    let bytes = source.as_bytes();
    let mut raw: Vec<RawTag> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut definition = None;

        for capture in m.captures {
            let capture_name = capture_names[capture.index as usize];
            if capture_name == "name" {
                name_node.get_or_insert(capture.node);
            } else if let Some(kind) = SymbolKind::from_capture(capture_name) {
                definition.get_or_insert((kind, capture.node));
            }
        }

        let (Some(name_node), Some((kind, def_node))) = (name_node, definition) else {
            continue;
        };

        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        let position = name_node.start_position();
        raw.push(RawTag {
            name: name.to_string(),
            kind,
            line: position.row + 1,
            column: char_column(source, name_node.start_byte(), position.column),
            name_byte: name_node.start_byte(),
            start_byte: def_node.start_byte(),
            end_byte: def_node.end_byte(),
        });

        if raw.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }

    let mut raw = collapse_duplicate_tags(raw);

    // Source order, with outer definitions before the ones they contain.
    // The trailing name_byte key makes this a total order: without it, tags
    // tied on all four keys would surface map iteration order (octorus's
    // std-HashMap version is nondeterministic in that corner; divergence D5).
    raw.sort_by(compare_raw_tags);

    let mut symbols: Vec<Symbol> = Vec::with_capacity(raw.len());
    // Stack of enclosing definition end offsets; its height is the depth.
    let mut enclosing: Vec<usize> = Vec::new();

    for tag in raw {
        while enclosing.last().is_some_and(|end| *end <= tag.start_byte) {
            enclosing.pop();
        }
        let depth = enclosing.len();
        enclosing.push(tag.end_byte);

        // Some grammars emit one definition per equation of the same logical
        // symbol. Only languages that explicitly opt in merge those adjacent
        // definitions; ordinary sibling definitions may be overloads.
        if merge_adjacent_same_name_definitions
            && let Some(previous) = symbols.last()
            && previous.name.as_str() == tag.name
            && previous.kind == tag.kind
            && usize::from(previous.depth) == depth
        {
            continue;
        }

        // Build inputs cap sources at 2 MiB and each outline at 10k symbols,
        // so these public compact widths cannot truncate extracted values.
        debug_assert!(tag.line <= u32::MAX as usize);
        debug_assert!(tag.column <= u32::MAX as usize);
        debug_assert!(depth <= u16::MAX as usize);
        debug_assert!(tag.name_byte <= u32::MAX as usize);
        debug_assert!(tag.start_byte <= u32::MAX as usize);
        debug_assert!(tag.end_byte <= u32::MAX as usize);

        symbols.push(Symbol {
            name: CompactString::from(tag.name),
            kind: tag.kind,
            line: tag.line as u32,
            column: tag.column as u32,
            depth: depth as u16,
            name_start: tag.name_byte as u32,
            def_start: tag.start_byte as u32,
            def_end: tag.end_byte as u32,
        });
    }

    symbols
}

fn compare_raw_tags(a: &RawTag, b: &RawTag) -> std::cmp::Ordering {
    a.start_byte
        .cmp(&b.start_byte)
        .then(b.end_byte.cmp(&a.end_byte))
        .then(a.line.cmp(&b.line))
        .then(a.name.cmp(&b.name))
        .then(a.name_byte.cmp(&b.name_byte))
}

/// Keeps one tag per tagged name node.
fn collapse_duplicate_tags(raw: Vec<RawTag>) -> Vec<RawTag> {
    // FxHashMap only changes iteration order. Per-key selection is
    // order-independent, and the following total sort removes that difference.
    let mut best: FxHashMap<usize, RawTag> = FxHashMap::default();
    best.reserve(raw.len());

    for tag in raw {
        match best.entry(tag.name_byte) {
            Entry::Vacant(slot) => {
                slot.insert(tag);
            }
            Entry::Occupied(mut slot) => {
                let current = slot.get();
                let span = tag.end_byte.saturating_sub(tag.start_byte);
                let current_span = current.end_byte.saturating_sub(current.start_byte);
                let better = (kind_specificity(tag.kind), span)
                    < (kind_specificity(current.kind), current_span);
                if better {
                    slot.insert(tag);
                }
            }
        }
    }

    best.into_values().collect()
}

/// Converts a tree-sitter byte column to a character column.
fn char_column(source: &str, start_byte: usize, byte_column: usize) -> usize {
    let line_start = start_byte.saturating_sub(byte_column);
    if line_start >= source.len() || !source.is_char_boundary(line_start) {
        return byte_column;
    }
    let end = start_byte.min(source.len());
    if !source.is_char_boundary(end) {
        return byte_column;
    }
    source[line_start..end].chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_tag_sort_uses_name_byte_as_the_final_tie_breaker() {
        let raw_tag = |name_byte| RawTag {
            name: "same".to_owned(),
            kind: SymbolKind::Function,
            line: 7,
            column: 0,
            name_byte,
            start_byte: 10,
            end_byte: 20,
        };
        let mut tags = vec![raw_tag(19), raw_tag(11), raw_tag(15)];

        tags.sort_by(compare_raw_tags);

        assert_eq!(
            tags.into_iter()
                .map(|tag| tag.name_byte)
                .collect::<Vec<_>>(),
            [11, 15, 19]
        );
    }
}
