use std::cmp::Ordering;

/// A search needle, lower-cased by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LoweredNeedle(String);

impl LoweredNeedle {
    pub(super) fn new(query: &str) -> Self {
        Self(query.to_lowercase())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(super) struct CachedSearch {
    pub(super) needle: LoweredNeedle,
    pub(super) limit: usize,
    pub(super) hits: Vec<(u32, u32)>,
}

/// A scored search hit ordered by [`compare_search_candidates`].
#[derive(Debug)]
pub(super) struct SearchCandidate<'a> {
    pub(super) score: i64,
    pub(super) name_len: usize,
    pub(super) path: &'a str,
    pub(super) line: u32,
    pub(super) symbol_index: u32,
    pub(super) slot: u32,
}

pub(super) fn compare_search_candidates(
    a: &SearchCandidate<'_>,
    b: &SearchCandidate<'_>,
) -> Ordering {
    // `by_path` is injective, so path identifies the file. Unlike octorus's
    // immutable index, no slot tie-breaker is needed: equal path and symbol
    // index already identify the same candidate.
    b.score
        .cmp(&a.score)
        .then(a.name_len.cmp(&b.name_len))
        .then(a.path.cmp(b.path))
        .then(a.line.cmp(&b.line))
        .then(a.symbol_index.cmp(&b.symbol_index))
}

/// Score a lower-cased name against a lower-cased search needle.
pub(super) fn fuzzy_score_lowered(lowered_name: &str, needle: &LoweredNeedle) -> Option<i64> {
    let needle = needle.as_str();
    if needle.is_empty() {
        return None;
    }
    let length_penalty = lowered_name.chars().count() as i64;

    if lowered_name == needle {
        return Some(10_000 - length_penalty);
    }
    if lowered_name.starts_with(needle) {
        return Some(8_000 - length_penalty);
    }
    if let Some(position) = lowered_name.find(needle) {
        let boundary = lowered_name[..position]
            .chars()
            .next_back()
            .is_some_and(|c| c == '_' || c == '-' || c == '.' || c == ':');
        let base = if boundary { 6_000 } else { 4_000 };
        return Some(base - position as i64 - length_penalty);
    }

    subsequence_score(lowered_name, needle).map(|score| score - length_penalty)
}

/// Score a scattered-subsequence match (`fdc` matching `find_diff_cache`).
///
/// Returns `None` unless every needle character appears in order.
pub(super) fn subsequence_score(haystack: &str, needle: &str) -> Option<i64> {
    let mut haystack_chars = haystack.chars().enumerate().peekable();
    let mut gaps: i64 = 0;
    let mut previous: Option<usize> = None;

    for wanted in needle.chars() {
        loop {
            let (position, actual) = haystack_chars.next()?;
            if actual == wanted {
                if let Some(previous) = previous {
                    gaps += (position - previous - 1) as i64;
                }
                previous = Some(position);
                break;
            }
        }
    }

    Some(2_000 - gaps.min(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_score_tiers() {
        let needle = LoweredNeedle::new("parse");
        let exact = fuzzy_score_lowered("parse", &needle).unwrap();
        let prefix = fuzzy_score_lowered("parse_line", &needle).unwrap();
        let boundary = fuzzy_score_lowered("do_parse", &needle).unwrap();
        let middle = fuzzy_score_lowered("reparsed", &needle).unwrap();
        let scattered = fuzzy_score_lowered("please_advance_rest_of_set", &needle).unwrap();

        assert!(exact > prefix, "{exact} > {prefix}");
        assert!(prefix > boundary, "{prefix} > {boundary}");
        assert!(boundary > middle, "{boundary} > {middle}");
        assert!(middle > scattered, "{middle} > {scattered}");
    }

    #[test]
    fn test_fuzzy_score_no_match() {
        assert!(fuzzy_score_lowered("parse", &LoweredNeedle::new("xyz")).is_none());
        assert!(fuzzy_score_lowered("parse", &LoweredNeedle::new("")).is_none());
    }

    #[test]
    fn test_fuzzy_score_shorter_name_wins_within_tier() {
        let needle = LoweredNeedle::new("parse");
        let short = fuzzy_score_lowered("parse_a", &needle).unwrap();
        let long = fuzzy_score_lowered("parse_a_very_long_name", &needle).unwrap();
        assert!(short > long);
    }

    #[test]
    fn test_fuzzy_score_is_case_insensitive() {
        let mixed_case = fuzzy_score_lowered("parse", &LoweredNeedle::new("PaRsE"));
        let lower_case = fuzzy_score_lowered("parse", &LoweredNeedle::new("parse"));

        assert_eq!(mixed_case, lower_case);
        assert!(mixed_case.is_some());
    }

    #[test]
    fn test_fuzzy_score_is_case_insensitive_for_tricky_names() {
        let names = [
            "lowercase",
            "UPPERCASE",
            "MixedCase",
            "identifier_123_name",
            "名前",
            "ǅ",
            "İ",
            "ß",
            "Éclair",
        ];
        let needle_pairs = [
            ("LoWeR", "lower"),
            ("UpPeR", "upper"),
            ("MiXeD", "mixed"),
            ("Identifier_123", "identifier_123"),
            ("ǅ", "ǆ"),
            ("İ", "i\u{307}"),
            ("ẞ", "ß"),
            ("ÉcLaIr", "éclair"),
            ("XyZ", "xyz"),
        ];

        for (mixed, lower) in needle_pairs {
            assert_eq!(
                mixed.to_lowercase(),
                lower,
                "invalid mixed/lower pair: {mixed:?}, {lower:?}"
            );
            let mixed_needle = LoweredNeedle::new(mixed);
            let lower_needle = LoweredNeedle::new(lower);
            for name in names {
                let lowered_name = name.to_lowercase();
                assert_eq!(
                    fuzzy_score_lowered(&lowered_name, &mixed_needle),
                    fuzzy_score_lowered(&lowered_name, &lower_needle),
                    "name={name:?}, mixed={mixed:?}, lower={lower:?}"
                );
            }
        }
    }
}
