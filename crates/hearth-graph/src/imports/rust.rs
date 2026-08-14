use compact_str::CompactString;
use tree_sitter::Node;

use super::{ImportKind, RawImport};

const MAX_RUST_IMPORTS: usize = 100_000;
const MAX_RUST_IMPORT_SPECIFIER_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn extract(source: &str, tree: &tree_sitter::Tree) -> Vec<RawImport> {
    let bytes = source.as_bytes();
    let mut imports = Vec::new();
    // Keeping inline-module specifiers as written loses some resolution
    // precision, which is acceptable while Rust outcomes are constitutively
    // Partial and their graph edges are Approximate.
    let mut specifier_bytes = 0usize;
    visit(tree.root_node(), bytes, &mut imports, &mut specifier_bytes);
    imports.sort_by_key(|import| import.span.0);
    imports
}

fn visit(node: Node<'_>, source: &[u8], imports: &mut Vec<RawImport>, specifier_bytes: &mut usize) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    expand_use_tree(argument, source, imports, specifier_bytes);
                }
            }
            "mod_item" => {
                if node.child_by_field_name("body").is_none()
                    && let Some(name) = node.child_by_field_name("name")
                {
                    let specifier = node_text(name, source);
                    push_import(
                        name,
                        specifier,
                        ImportKind::RustMod,
                        imports,
                        specifier_bytes,
                    );
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
}

#[derive(Clone, Copy, Default)]
struct UsePrefix(Option<usize>);

struct UsePrefixPart {
    parent: UsePrefix,
    segment: String,
    rendered_len: usize,
}

fn expand_use_tree(
    node: Node<'_>,
    source: &[u8],
    imports: &mut Vec<RawImport>,
    specifier_bytes: &mut usize,
) {
    let mut prefixes = Vec::new();
    let mut pending = vec![(node, UsePrefix::default())];
    while let Some((node, prefix)) = pending.pop() {
        match node.kind() {
            "scoped_use_list" => {
                let next_prefix = node
                    .child_by_field_name("path")
                    .and_then(|path| normalized_path(path, source))
                    .map_or(prefix, |path| extend_prefix(prefix, path, &mut prefixes));
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, next_prefix));
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                pending.extend(children.into_iter().rev().map(|child| (child, prefix)));
            }
            "use_as_clause" => {
                if let Some(path) = node.child_by_field_name("path")
                    && let Some(segment) = normalized_path(path, source)
                {
                    push_use_leaf(path, prefix, &prefixes, &segment, imports, specifier_bytes);
                }
            }
            "use_wildcard" => {
                let segment = node
                    .named_child(0)
                    .and_then(|path| normalized_path(path, source))
                    .map_or_else(|| "*".to_owned(), |path| format!("{path}::*"));
                push_use_leaf(node, prefix, &prefixes, &segment, imports, specifier_bytes);
            }
            "identifier" | "scoped_identifier" | "crate" | "self" | "super" | "metavariable" => {
                if let Some(segment) = normalized_path(node, source) {
                    push_use_leaf(node, prefix, &prefixes, &segment, imports, specifier_bytes);
                }
            }
            _ => {}
        }
    }
}

fn extend_prefix(
    prefix: UsePrefix,
    segment: String,
    prefixes: &mut Vec<UsePrefixPart>,
) -> UsePrefix {
    if segment.is_empty() {
        return prefix;
    }
    let rendered_len = prefix.0.map_or(segment.len(), |index| {
        prefixes[index].rendered_len + 2 + segment.len()
    });
    prefixes.push(UsePrefixPart {
        parent: prefix,
        segment,
        rendered_len,
    });
    UsePrefix(Some(prefixes.len() - 1))
}

fn push_use_leaf(
    node: Node<'_>,
    prefix: UsePrefix,
    prefixes: &[UsePrefixPart],
    segment: &str,
    imports: &mut Vec<RawImport>,
    specifier_bytes: &mut usize,
) {
    if imports.len() >= MAX_RUST_IMPORTS {
        return;
    }
    let prefix_len = prefix.0.map_or(0, |index| prefixes[index].rendered_len);
    let append_segment = segment != "self" || prefix.0.is_none();
    let projected_len = prefix_len
        .saturating_add(usize::from(prefix.0.is_some() && append_segment) * 2)
        .saturating_add(append_segment as usize * segment.len());
    if projected_len > MAX_RUST_IMPORT_SPECIFIER_BYTES.saturating_sub(*specifier_bytes) {
        return;
    }
    let specifier = render_use_path(prefix, prefixes, segment);
    push_import(
        node,
        Some(specifier),
        ImportKind::RustUse,
        imports,
        specifier_bytes,
    );
}

fn render_use_path(prefix: UsePrefix, prefixes: &[UsePrefixPart], segment: &str) -> String {
    let prefix_len = prefix.0.map_or(0, |index| prefixes[index].rendered_len);
    let append_segment = segment != "self" || prefix.0.is_none();
    let mut specifier = String::with_capacity(
        prefix_len
            + usize::from(prefix.0.is_some() && append_segment) * 2
            + append_segment as usize * segment.len(),
    );
    let mut parts = Vec::new();
    let mut current = prefix;
    while let Some(index) = current.0 {
        let part = &prefixes[index];
        parts.push(part.segment.as_str());
        current = part.parent;
    }
    for part in parts.into_iter().rev() {
        if !specifier.is_empty() {
            specifier.push_str("::");
        }
        specifier.push_str(part);
    }
    if append_segment {
        if !specifier.is_empty() {
            specifier.push_str("::");
        }
        specifier.push_str(segment);
    }
    specifier
}

fn push_import(
    node: Node<'_>,
    specifier: Option<String>,
    kind: ImportKind,
    imports: &mut Vec<RawImport>,
    specifier_bytes: &mut usize,
) {
    let Some(specifier) = specifier else {
        return;
    };
    let Ok(start) = u32::try_from(node.start_byte()) else {
        return;
    };
    let Ok(end) = u32::try_from(node.end_byte()) else {
        return;
    };
    let Ok(line) = u32::try_from(node.start_position().row + 1) else {
        return;
    };
    if imports.len() >= MAX_RUST_IMPORTS
        || specifier.len() > MAX_RUST_IMPORT_SPECIFIER_BYTES.saturating_sub(*specifier_bytes)
    {
        return;
    }
    *specifier_bytes += specifier.len();
    imports.push(RawImport {
        specifier: CompactString::from(specifier),
        kind,
        line,
        span: (start, end),
    });
}

fn normalized_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    const MAX_NORMALIZED_PATH_BYTES: usize = 64 * 1024;

    // Collect source-ordered leaf segments and join once. Rebuilding a growing
    // prefix at every nested scoped identifier is quadratic.
    let mut pending = vec![node];
    let mut segments = Vec::new();
    let mut rendered_len = 0usize;
    while let Some(node) = pending.pop() {
        if node.kind() == "scoped_identifier" {
            let name = node.child_by_field_name("name")?;
            if let Some(path) = node.child_by_field_name("path") {
                pending.push(name);
                pending.push(path);
            } else {
                pending.push(name);
            }
            continue;
        }
        let segment = node_text(node, source)?;
        rendered_len = rendered_len
            .checked_add(usize::from(!segments.is_empty()) * 2)?
            .checked_add(segment.len())?;
        if rendered_len > MAX_NORMALIZED_PATH_BYTES {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("::"))
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.utf8_text(source)
        .ok()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}
