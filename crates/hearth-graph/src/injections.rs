use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

/// One embedded source range and the registered language that parses it.
pub(crate) struct Injection {
    pub(crate) range: tree_sitter::Range,
    pub(crate) language: String,
}

/// Extracts, normalizes, and deduplicates language injections from one tree.
pub(crate) fn extract(tree: &Tree, source: &str, query: &Query) -> Vec<Injection> {
    let bytes = source.as_bytes();
    let capture_names = query.capture_names();
    let mut by_range: FxHashMap<(usize, usize), Injection> = FxHashMap::default();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    while let Some(query_match) = matches.next() {
        let mut content = None;
        let mut language = None;

        for capture in query_match.captures {
            match capture_names[capture.index as usize] {
                "injection.content" => content.get_or_insert(capture.node),
                "injection.language" => {
                    if let Ok(name) = capture.node.utf8_text(bytes) {
                        language = Some(name);
                    }
                    continue;
                }
                _ => continue,
            };
        }

        let Some(content) = content else {
            continue;
        };
        let language = language.or_else(|| {
            query
                .property_settings(query_match.pattern_index)
                .iter()
                .find(|property| property.key.as_ref() == "injection.language")
                .and_then(|property| property.value.as_deref())
        });
        let Some(language) = language else {
            continue;
        };
        let language = normalize_language_name(language).to_owned();
        let range = tree_sitter::Range {
            start_byte: content.start_byte(),
            end_byte: content.end_byte(),
            start_point: content.start_position(),
            end_point: content.end_position(),
        };
        if range.start_byte == range.end_byte {
            continue;
        }

        let key = (range.start_byte, range.end_byte);
        let injection = Injection { range, language };
        match by_range.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(injection);
            }
            Entry::Occupied(mut slot) => {
                let current = slot.get();
                if language_preference(&injection.language) < language_preference(&current.language)
                {
                    slot.insert(injection);
                }
            }
        }
    }

    let mut injections: Vec<_> = by_range.into_values().collect();
    injections.sort_unstable_by(|a, b| {
        a.range
            .start_byte
            .cmp(&b.range.start_byte)
            .then(a.range.end_byte.cmp(&b.range.end_byte))
            .then(a.language.cmp(&b.language))
    });
    injections
}

fn normalize_language_name(name: &str) -> &str {
    if name.eq_ignore_ascii_case("ts") || name.eq_ignore_ascii_case("typescript") {
        "typescript"
    } else if name.eq_ignore_ascii_case("js") || name.eq_ignore_ascii_case("javascript") {
        "javascript"
    } else if name.eq_ignore_ascii_case("tsx") {
        "tsx"
    } else if name.eq_ignore_ascii_case("jsx") {
        "jsx"
    } else {
        name
    }
}

fn language_preference(language: &str) -> (u8, &str) {
    let specificity = match language {
        "tsx" | "jsx" => 0,
        "typescript" => 1,
        "javascript" => 100,
        _ => 50,
    };
    (specificity, language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_aliases_are_normalized() {
        assert_eq!(normalize_language_name("TS"), "typescript");
        assert_eq!(normalize_language_name("js"), "javascript");
        assert_eq!(normalize_language_name("tsx"), "tsx");
        assert_eq!(normalize_language_name("css"), "css");
    }

    #[test]
    fn explicit_script_languages_beat_the_javascript_fallback() {
        assert!(language_preference("tsx") < language_preference("typescript"));
        assert!(language_preference("typescript") < language_preference("javascript"));
    }
}
