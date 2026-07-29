use compact_str::CompactString;
use tree_sitter::Node;

use super::{ImportKind, RawImport};

pub(crate) fn extract(source: &str, tree: &tree_sitter::Tree) -> Vec<RawImport> {
    let bytes = source.as_bytes();
    let mut imports = Vec::new();
    // Keeping inline-module specifiers as written loses some resolution
    // precision, which is acceptable while Rust outcomes are constitutively
    // Partial and their graph edges are Approximate.
    visit(tree.root_node(), bytes, &mut imports);
    imports.sort_by_key(|import| import.span.0);
    imports
}

fn visit(node: Node<'_>, source: &[u8], imports: &mut Vec<RawImport>) {
    match node.kind() {
        "use_declaration" => {
            if let Some(argument) = node.child_by_field_name("argument") {
                expand_use_tree(argument, "", source, imports);
            }
        }
        "mod_item" => {
            if node.child_by_field_name("body").is_none()
                && let Some(name) = node.child_by_field_name("name")
            {
                let specifier = node_text(name, source);
                push_import(name, specifier, ImportKind::RustMod, imports);
            }
        }
        _ => {}
    }

    visit_children(node, source, imports);
}

fn visit_children(node: Node<'_>, source: &[u8], imports: &mut Vec<RawImport>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, source, imports);
    }
}

fn expand_use_tree(node: Node<'_>, prefix: &str, source: &[u8], imports: &mut Vec<RawImport>) {
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|path| normalized_path(path, source));
            let next_prefix = path
                .as_deref()
                .map_or_else(|| prefix.to_owned(), |path| join_path(prefix, path));
            if let Some(list) = node.child_by_field_name("list") {
                expand_use_tree(list, &next_prefix, source, imports);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                expand_use_tree(child, prefix, source, imports);
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path")
                && let Some(segment) = normalized_path(path, source)
            {
                push_use_leaf(path, prefix, &segment, imports);
            }
        }
        "use_wildcard" => {
            let segment = node
                .named_child(0)
                .and_then(|path| normalized_path(path, source))
                .map_or_else(|| "*".to_owned(), |path| format!("{path}::*"));
            push_use_leaf(node, prefix, &segment, imports);
        }
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" | "metavariable" => {
            if let Some(segment) = normalized_path(node, source) {
                push_use_leaf(node, prefix, &segment, imports);
            }
        }
        _ => {}
    }
}

fn push_use_leaf(node: Node<'_>, prefix: &str, segment: &str, imports: &mut Vec<RawImport>) {
    let specifier = if segment == "self" && !prefix.is_empty() {
        prefix.to_owned()
    } else {
        join_path(prefix, segment)
    };
    push_import(node, Some(specifier), ImportKind::RustUse, imports);
}

fn push_import(
    node: Node<'_>,
    specifier: Option<String>,
    kind: ImportKind,
    imports: &mut Vec<RawImport>,
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
    imports.push(RawImport {
        specifier: CompactString::from(specifier),
        kind,
        line,
        span: (start, end),
    });
}

fn normalized_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "scoped_identifier" {
        let name = node
            .child_by_field_name("name")
            .and_then(|name| normalized_path(name, source))?;
        return node
            .child_by_field_name("path")
            .and_then(|path| normalized_path(path, source))
            .map_or(Some(name.clone()), |path| Some(join_path(&path, &name)));
    }
    node_text(node, source)
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.utf8_text(source)
        .ok()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else if segment.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}::{segment}")
    }
}
