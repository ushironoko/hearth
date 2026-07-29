use compact_str::CompactString;
use rustc_hash::FxHashSet;
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use super::{ImportKind, RawImport};

const COMMONJS_CALLEE_CAPTURE: &str = "import.callee.commonjs";

pub(crate) struct ExtractedImports {
    pub(crate) imports: Vec<RawImport>,
    pub(crate) opaque_count: usize,
}

pub(crate) fn extract(
    source: &str,
    tree: &Tree,
    query: &Query,
    kind_map: fn(&str) -> ImportKind,
) -> ExtractedImports {
    let bytes = source.as_bytes();
    let capture_names = query.capture_names();
    let mut seen_nodes = FxHashSet::default();
    let mut imports = Vec::new();
    let mut opaque_count = 0;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    while let Some(query_match) = matches.next() {
        let Some(source_capture) = query_match
            .captures
            .iter()
            .find(|capture| capture_names[capture.index as usize].starts_with("import.source."))
        else {
            continue;
        };
        let capture_name = capture_names[source_capture.index as usize];

        if capture_name == "import.source.commonjs"
            && !query_match.captures.iter().any(|capture| {
                capture_names[capture.index as usize] == COMMONJS_CALLEE_CAPTURE
                    && capture.node.utf8_text(bytes) == Ok("require")
            })
        {
            continue;
        }

        // Comments are named extras in the JS grammar, so `arguments . (_)`
        // can capture a leading comment (webpack magic comments). The real
        // argument is the first non-comment named sibling that follows.
        let mut node = source_capture.node;
        while node.kind() == "comment" {
            match node.next_named_sibling() {
                Some(next) => node = next,
                None => break,
            }
        }

        if !seen_nodes.insert(node.id()) {
            continue;
        }

        let kind = kind_map(capture_name);
        if node.kind() != "string" {
            if matches!(kind, ImportKind::EsDynamic | ImportKind::CommonJs) {
                opaque_count += 1;
            }
            continue;
        }

        let Ok(literal) = node.utf8_text(bytes) else {
            continue;
        };
        let Some(specifier) = strip_plain_string_quotes(literal) else {
            continue;
        };
        let Ok(start) = u32::try_from(node.start_byte()) else {
            continue;
        };
        let Ok(end) = u32::try_from(node.end_byte()) else {
            continue;
        };
        let Ok(line) = u32::try_from(node.start_position().row + 1) else {
            continue;
        };

        imports.push(RawImport {
            specifier: CompactString::from(specifier),
            kind,
            line,
            span: (start, end),
        });
    }

    imports.sort_by_key(|import| import.span.0);
    ExtractedImports {
        imports,
        opaque_count,
    }
}

fn strip_plain_string_quotes(literal: &str) -> Option<&str> {
    let bytes = literal.as_bytes();
    let (&first, middle) = bytes.split_first()?;
    let (&last, _) = middle.split_last()?;
    if first != last || !matches!(first, b'\'' | b'"') {
        return None;
    }
    literal.get(1..literal.len() - 1)
}
