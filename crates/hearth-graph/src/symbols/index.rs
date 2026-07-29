use compact_str::CompactString;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use super::score::{
    CachedSearch, LoweredNeedle, SearchCandidate, compare_search_candidates, fuzzy_score_lowered,
};
use super::{FileSymbols, Symbol, SymbolRef, jump_priority};

/// The result of inserting extracted symbols for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The path was not previously present.
    Inserted,
    /// The path was present but its content or registry generation changed.
    Updated,
    /// The indexed content and registry generation were already current.
    Unchanged,
}

/// An incremental repository-wide symbol index.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    files: Vec<Option<FileSymbols>>,
    /// Lower-cased names parallel to `files`, with `None` when the original is
    /// already lower-case.
    lowered_names: Vec<Vec<Option<Box<str>>>>,
    registry_generations: Vec<u64>,
    by_path: FxHashMap<CompactString, u32>,
    /// Lower-cased name -> (slot, symbol index) pairs.
    by_name: FxHashMap<CompactString, SmallVec<[(u32, u32); 2]>>,
    free_slots: Vec<u32>,
    symbol_count: usize,
    /// Number of inputs walked by [`Self::from_files`], including duplicates.
    scanned_files: usize,
    generation: u64,
    /// Test-only cost probe counting comparator invocations for one search.
    ///
    /// Counts the ordering *work*, not a number recorded next to it: a probe
    /// that reports the post-truncate candidate count reads the same value
    /// whether the top-N partition or an ordinary full sort produced it, so it
    /// cannot tell the two apart. Comparison counts can.
    #[cfg(test)]
    search_comparisons: AtomicUsize,
    /// Last search, cached so callers can redraw without rescoring.
    ///
    /// `parking_lot` mutexes cannot poison, so octorus's poisoning-recovery
    /// test has no analogue here.
    memo: Mutex<Option<CachedSearch>>,
}

impl SymbolIndex {
    /// Creates an empty symbol index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an index from already-extracted per-file symbols.
    ///
    /// Duplicate paths are processed in order, so the last effective upsert
    /// wins. `scanned_file_count` retains the input count rather than the
    /// number of occupied slots.
    #[must_use]
    pub fn from_files(files: Vec<FileSymbols>, registry_generation: u64) -> Self {
        let scanned_files = files.len();
        let mut index = files.into_iter().fold(Self::new(), |mut index, file| {
            index.upsert(file, registry_generation);
            index
        });
        index.scanned_files = scanned_files;
        index
    }

    /// Inserts or replaces the symbols for one path.
    ///
    /// A matching content hash is unchanged only when the language registry
    /// generation also matches, because host-injected grammars may change the
    /// extraction result without changing source bytes.
    pub fn upsert(&mut self, file: FileSymbols, registry_generation: u64) -> UpsertOutcome {
        if let Some(&slot) = self.by_path.get(file.path.as_str()) {
            let slot_index = slot as usize;
            let current = self.files[slot_index]
                .as_ref()
                .expect("by_path must reference an occupied slot");
            if current.content_hash == file.content_hash
                && self.registry_generations[slot_index] == registry_generation
            {
                return UpsertOutcome::Unchanged;
            }

            let old_file = self.files[slot_index]
                .take()
                .expect("by_path must reference an occupied slot");
            let old_lowered = std::mem::take(&mut self.lowered_names[slot_index]);
            self.remove_name_entries(slot, &old_file, &old_lowered);
            self.symbol_count -= old_file.symbols.len();

            let lowered = self.add_name_entries(slot, &file.symbols);
            self.symbol_count += file.symbols.len();
            self.files[slot_index] = Some(file);
            self.lowered_names[slot_index] = lowered;
            self.registry_generations[slot_index] = registry_generation;
            self.record_mutation();
            return UpsertOutcome::Updated;
        }

        let path = file.path.clone();
        let slot = if let Some(slot) = self.free_slots.pop() {
            let slot_index = slot as usize;
            debug_assert!(self.files[slot_index].is_none());
            let lowered = self.add_name_entries(slot, &file.symbols);
            self.symbol_count += file.symbols.len();
            self.files[slot_index] = Some(file);
            self.lowered_names[slot_index] = lowered;
            self.registry_generations[slot_index] = registry_generation;
            slot
        } else {
            let slot = self.files.len() as u32;
            let lowered = self.add_name_entries(slot, &file.symbols);
            self.symbol_count += file.symbols.len();
            self.files.push(Some(file));
            self.lowered_names.push(lowered);
            self.registry_generations.push(registry_generation);
            slot
        };
        self.by_path.insert(path, slot);
        self.record_mutation();
        UpsertOutcome::Inserted
    }

    /// Removes a path and its symbols from the index.
    ///
    /// Returns `false` when the path was not indexed.
    pub fn remove(&mut self, path: &str) -> bool {
        let Some(slot) = self.by_path.remove(path) else {
            return false;
        };
        let slot_index = slot as usize;
        let file = self.files[slot_index]
            .take()
            .expect("by_path must reference an occupied slot");
        let lowered = std::mem::take(&mut self.lowered_names[slot_index]);
        self.remove_name_entries(slot, &file, &lowered);
        self.registry_generations[slot_index] = 0;
        self.symbol_count -= file.symbols.len();
        self.free_slots.push(slot);
        self.record_mutation();
        true
    }

    /// Returns whether the path is indexed for the given source and registry
    /// generations.
    #[must_use]
    pub fn contains(&self, path: &str, content_hash: u64, registry_generation: u64) -> bool {
        let Some(&slot) = self.by_path.get(path) else {
            return false;
        };
        self.files[slot as usize]
            .as_ref()
            .is_some_and(|file| file.content_hash == content_hash)
            && self.registry_generations[slot as usize] == registry_generation
    }

    /// Returns the indexed content hash for a path.
    #[must_use]
    pub fn file_hash(&self, path: &str) -> Option<u64> {
        let slot = *self.by_path.get(path)?;
        self.files[slot as usize]
            .as_ref()
            .map(|file| file.content_hash)
    }

    /// Returns the mutation generation of the index.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the symbols for one indexed file.
    ///
    /// Files with no symbols remain indexed and return `Some(&[])`.
    #[must_use]
    pub fn file_symbols(&self, path: &str) -> Option<&[Symbol]> {
        let slot = *self.by_path.get(path)?;
        self.files[slot as usize]
            .as_ref()
            .map(|file| file.symbols.as_slice())
    }

    /// Returns all definitions with exactly this name, case-insensitively.
    ///
    /// Results prefer likely jump destinations, then shallower definitions,
    /// path order, and source order.
    #[must_use]
    pub fn definitions(&self, name: &str) -> Vec<SymbolRef<'_>> {
        let lowered = name.to_lowercase();
        let Some(hits) = self.by_name.get(lowered.as_str()) else {
            return Vec::new();
        };

        let mut refs: Vec<SymbolRef<'_>> = hits
            .iter()
            .filter_map(|(slot, symbol_index)| self.symbol_ref(*slot, *symbol_index))
            .collect();
        refs.sort_by(|a, b| {
            jump_priority(a.symbol.kind)
                .cmp(&jump_priority(b.symbol.kind))
                .then(a.symbol.depth.cmp(&b.symbol.depth))
                .then(a.path.cmp(b.path))
                .then(a.symbol.line.cmp(&b.symbol.line))
        });
        refs
    }

    /// Fuzzy-searches symbol names, best match first, capped at `limit`.
    ///
    /// An empty query returns nothing rather than the entire repository.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolRef<'_>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = LoweredNeedle::new(query);
        #[cfg(test)]
        self.search_comparisons.store(0, Ordering::Relaxed);

        {
            let memo = self.memo.lock();
            if let Some(cached) = memo
                .as_ref()
                .filter(|cached| cached.needle == needle && cached.limit == limit)
            {
                return self.symbol_refs(&cached.hits);
            }
        }

        // Do not hold the memo lock while scoring. Concurrent readers should
        // not form a convoy that delays an external index write lock.
        let hits = self.search_hits(&needle, limit, |_| true);
        *self.memo.lock() = Some(CachedSearch {
            needle,
            limit,
            hits: hits.clone(),
        });
        self.symbol_refs(&hits)
    }

    /// Fuzzy-searches symbols in allowed files without populating the global
    /// search memo.
    ///
    /// The predicate is evaluated once per occupied file before any of its
    /// symbols are scored.
    #[must_use]
    pub fn search_filtered(
        &self,
        query: &str,
        limit: usize,
        allowed: impl Fn(&str) -> bool,
    ) -> Vec<SymbolRef<'_>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = LoweredNeedle::new(query);
        #[cfg(test)]
        self.search_comparisons.store(0, Ordering::Relaxed);

        let hits = self.search_hits(&needle, limit, allowed);
        self.symbol_refs(&hits)
    }

    /// Returns the number of indexed symbols.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.symbol_count
    }

    /// Returns the number of occupied file slots.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.by_path.len()
    }

    /// Returns the number of inputs walked by [`Self::from_files`].
    #[must_use]
    pub fn scanned_file_count(&self) -> usize {
        self.scanned_files
    }

    pub(crate) fn set_scanned_files(&mut self, scanned: usize) {
        self.scanned_files = scanned;
    }

    /// Iterates over all indexed paths in slot order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter_map(|file| file.as_ref().map(|file| file.path.as_str()))
    }

    /// Returns comparator invocations made by the last search.
    #[cfg(test)]
    pub(crate) fn search_comparisons(&self) -> usize {
        self.search_comparisons.load(Ordering::Relaxed)
    }

    fn symbol_ref(&self, slot: u32, symbol_index: u32) -> Option<SymbolRef<'_>> {
        let file = self.files.get(slot as usize)?.as_ref()?;
        let symbol = file.symbols.get(symbol_index as usize)?;
        Some(SymbolRef {
            path: &file.path,
            symbol,
        })
    }

    fn symbol_refs(&self, hits: &[(u32, u32)]) -> Vec<SymbolRef<'_>> {
        hits.iter()
            .filter_map(|(slot, symbol_index)| self.symbol_ref(*slot, *symbol_index))
            .collect()
    }

    fn search_hits(
        &self,
        needle: &LoweredNeedle,
        limit: usize,
        allowed: impl Fn(&str) -> bool,
    ) -> Vec<(u32, u32)> {
        // Do not reserve from the caller-controlled `limit`: the vector grows
        // only for actual matches, keeping `usize::MAX` safe.
        let mut candidates = Vec::new();
        for (slot, (file, lowered_names)) in self.files.iter().zip(&self.lowered_names).enumerate()
        {
            let Some(file) = file else {
                continue;
            };
            if !allowed(file.path.as_str()) {
                continue;
            }
            for (symbol_index, (symbol, lowered_name)) in
                file.symbols.iter().zip(lowered_names).enumerate()
            {
                let lowered_name = lowered_name.as_deref().unwrap_or(&symbol.name);
                if let Some(score) = fuzzy_score_lowered(lowered_name, needle) {
                    candidates.push(SearchCandidate {
                        score,
                        name_len: symbol.name.len(),
                        path: &file.path,
                        line: symbol.line,
                        symbol_index: symbol_index as u32,
                        slot: slot as u32,
                    });
                }
            }
        }

        let compare = |a: &SearchCandidate<'_>, b: &SearchCandidate<'_>| {
            #[cfg(test)]
            self.search_comparisons.fetch_add(1, Ordering::Relaxed);
            compare_search_candidates(a, b)
        };
        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, &compare);
            candidates.truncate(limit);
        }
        // The partition discarded every candidate outside the top N, so this
        // full ordering never handles more than `limit` entries.
        candidates.sort_by(&compare);

        candidates
            .into_iter()
            .map(|candidate| (candidate.slot, candidate.symbol_index))
            .collect()
    }

    fn add_name_entries(&mut self, slot: u32, symbols: &[Symbol]) -> Vec<Option<Box<str>>> {
        let mut lowered_names = Vec::with_capacity(symbols.len());
        for (symbol_index, symbol) in symbols.iter().enumerate() {
            let lowered = symbol.name.to_lowercase();
            let cached_lowered =
                (lowered != symbol.name).then(|| Box::<str>::from(lowered.as_str()));
            self.by_name
                .entry(lowered)
                .or_default()
                .push((slot, symbol_index as u32));
            lowered_names.push(cached_lowered);
        }
        lowered_names
    }

    fn remove_name_entries(
        &mut self,
        slot: u32,
        file: &FileSymbols,
        lowered_names: &[Option<Box<str>>],
    ) {
        let mut keys: Vec<&str> = file
            .symbols
            .iter()
            .zip(lowered_names)
            .map(|(symbol, lowered)| lowered.as_deref().unwrap_or(&symbol.name))
            .collect();
        keys.sort_unstable();
        keys.dedup();

        for key in keys {
            let remove_key = self.by_name.get_mut(key).is_some_and(|hits| {
                hits.retain(|(hit_slot, _)| *hit_slot != slot);
                hits.is_empty()
            });
            if remove_key {
                self.by_name.remove(key);
            }
        }
    }

    fn record_mutation(&mut self) {
        self.memo.get_mut().take();
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use compact_str::CompactString;

    use super::*;
    use crate::symbols::SymbolKind;

    fn test_symbol(name: &str, kind: SymbolKind, line: u32, column: u32) -> Symbol {
        Symbol {
            name: CompactString::from(name),
            kind,
            line,
            column,
            depth: 0,
            name_start: 0,
            def_start: 0,
            def_end: 0,
        }
    }

    fn file_fixture(path: &str, content_hash: u64, symbols: Vec<Symbol>) -> FileSymbols {
        FileSymbols {
            path: CompactString::from(path),
            content_hash,
            symbols,
        }
    }

    fn sample_index() -> SymbolIndex {
        SymbolIndex::from_files(
            vec![
                file_fixture(
                    "src/app.rs",
                    1,
                    vec![
                        test_symbol("App", SymbolKind::Class, 10, 0),
                        test_symbol("render_app", SymbolKind::Function, 20, 0),
                    ],
                ),
                file_fixture(
                    "src/ui.rs",
                    2,
                    vec![test_symbol("app", SymbolKind::Constant, 5, 0)],
                ),
            ],
            0,
        )
    }

    fn full_sort_search_reference<'a>(
        index: &'a SymbolIndex,
        query: &str,
        limit: usize,
    ) -> Vec<SymbolRef<'a>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = LoweredNeedle::new(query);
        let mut scored = Vec::new();

        for file in index.files.iter().flatten() {
            for symbol in &file.symbols {
                let lowered_name = symbol.name.to_lowercase();
                if let Some(score) = fuzzy_score_lowered(&lowered_name, &needle) {
                    scored.push((
                        score,
                        SymbolRef {
                            path: &file.path,
                            symbol,
                        },
                    ));
                }
            }
        }

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.symbol.name.len().cmp(&b.1.symbol.name.len()))
                .then(a.1.path.cmp(b.1.path))
                .then(a.1.symbol.line.cmp(&b.1.symbol.line))
        });
        scored.truncate(limit);
        scored.into_iter().map(|(_, symbol)| symbol).collect()
    }

    fn rendered(hits: Vec<SymbolRef<'_>>) -> Vec<(String, String, u32)> {
        hits.into_iter()
            .map(|hit| {
                (
                    hit.path.to_owned(),
                    hit.symbol.name.to_string(),
                    hit.symbol.line,
                )
            })
            .collect()
    }

    #[test]
    fn test_index_counts() {
        let index = sample_index();
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 3);
        assert_ne!(index.symbol_count(), 0);
    }

    #[test]
    fn test_empty_index_is_empty() {
        let index = SymbolIndex::default();
        assert_eq!(index.symbol_count(), 0);
        assert!(index.definitions("anything").is_empty());
        assert!(index.search("anything", 10).is_empty());
    }

    #[test]
    fn test_definitions_are_case_insensitive_and_ranked() {
        let index = sample_index();
        let hits = index.definitions("APP");
        let rendered: Vec<_> = hits
            .iter()
            .map(|hit| (hit.path, hit.symbol.name.as_str(), hit.symbol.kind))
            .collect();
        // The class outranks the constant regardless of insertion order.
        assert_eq!(
            rendered,
            vec![
                ("src/app.rs", "App", SymbolKind::Class),
                ("src/ui.rs", "app", SymbolKind::Constant),
            ]
        );
    }

    #[test]
    fn test_definitions_unknown_name() {
        assert!(sample_index().definitions("nope").is_empty());
    }

    #[test]
    fn test_file_symbols_lookup() {
        let index = sample_index();
        assert_eq!(
            index.file_symbols("src/ui.rs").map(<[Symbol]>::len),
            Some(1)
        );
        assert!(index.file_symbols("src/missing.rs").is_none());
    }

    #[test]
    fn test_search_prefers_exact_over_boundary_match() {
        let index = sample_index();
        let hits = index.search("app", 10);
        let names: Vec<_> = hits.iter().map(|hit| hit.symbol.name.as_str()).collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);
    }

    #[test]
    fn test_search_respects_limit() {
        assert_eq!(sample_index().search("app", 1).len(), 1);
        assert!(sample_index().search("app", 0).is_empty());
    }

    #[test]
    fn test_search_with_an_unbounded_limit_returns_every_match() {
        let index = sample_index();
        let expected = index.search("app", index.symbol_count());

        assert_eq!(index.search("app", usize::MAX), expected);
        assert_eq!(index.search("app", index.symbol_count() * 1_000), expected);
    }

    #[test]
    fn test_search_empty_query_returns_nothing() {
        assert!(sample_index().search("", 10).is_empty());
        assert!(sample_index().search("   ", 10).is_empty());
    }

    #[test]
    fn test_search_subsequence() {
        let index = sample_index();
        let hits = index.search("rndap", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "render_app");
    }

    #[test]
    fn test_search_reads_a_matching_cached_result() {
        let index = sample_index();
        *index.memo.lock() = Some(CachedSearch {
            needle: LoweredNeedle::new("app"),
            limit: 10,
            hits: vec![(0, 1)],
        });

        let hits = index.search("app", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "render_app");
    }

    /// Ties are broken all the way down to insertion order.
    ///
    /// The top-N partition is an *unstable* selection, so candidates the
    /// comparator calls equal may be kept, dropped and ordered differently
    /// between runs. Every user-visible key can genuinely tie — same score,
    /// same name length, same file, same line describes overloads on one line
    /// and macro-generated pairs — and only the symbol index makes the result
    /// reproducible.
    #[test]
    fn test_candidates_equal_on_every_visible_key_still_have_one_order() {
        const TOTAL: usize = 300;
        const LIMIT: usize = 200;
        // Equal-length names on one line in one file: score, name length, path
        // and line are all identical across the set.
        let symbols = (0..TOTAL)
            .map(|index| test_symbol(&format!("tie_{index:04}"), SymbolKind::Function, 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![file_fixture("src/generated.rs", 1, symbols)], 0);

        let hits = index.search("tie", LIMIT);

        assert_eq!(hits.len(), LIMIT);
        let names: Vec<&str> = hits.iter().map(|hit| hit.symbol.name.as_str()).collect();
        let expected: Vec<String> = (0..LIMIT).map(|index| format!("tie_{index:04}")).collect();
        assert_eq!(
            names, expected,
            "candidates that tie on every visible key must still come back in \
             index order; without it the unstable partition keeps a different \
             subset each run"
        );
    }

    #[test]
    fn test_search_never_orders_more_than_the_limit() {
        // Scrambled, not ascending. Candidates are collected in index order, so
        // ascending lines would arrive already in final order and Rust's sort
        // would finish in a single linear run — making a full sort *cheaper*
        // than the partition and hiding the very substitution this test exists
        // to catch. 7,919 is coprime with 5,000, so this is a permutation.
        let symbols = (0..5_000)
            .map(|index| {
                test_symbol(
                    &format!("matching_symbol_{index:04}"),
                    SymbolKind::Function,
                    ((index * 7_919) % 5_000 + 1) as u32,
                    0,
                )
            })
            .collect();
        let index = SymbolIndex::from_files(vec![file_fixture("src/generated.rs", 1, symbols)], 0);

        assert_eq!(index.symbol_count(), 5_000);
        assert_eq!(index.search("matching", usize::MAX).len(), 5_000);

        let sorted = index.search("matching", 10);
        assert_eq!(sorted.len(), 10);

        // Ordering *work*, not a number recorded beside it: a probe reporting
        // the post-truncate candidate count reads `limit` either way, so moving
        // the truncate above it hides a full sort. Comparisons cannot be
        // reordered around — top-N partitioning is linear in the match count
        // while a full sort of 5,000 unordered candidates costs n log n.
        let comparisons = index.search_comparisons();
        assert!(
            comparisons < 25_000,
            "ordering 10 of 5,000 matches took {comparisons} comparisons; \
             the top-N partition looks to have been replaced by a full sort"
        );
    }

    #[test]
    fn test_search_matches_a_full_sort_reference() {
        let index = SymbolIndex::from_files(
            vec![
                file_fixture(
                    "src/z.rs",
                    1,
                    vec![
                        test_symbol("sym", SymbolKind::Function, 50, 0),
                        test_symbol("symé", SymbolKind::Function, 40, 0),
                        test_symbol("syma", SymbolKind::Function, 40, 0),
                        test_symbol("sym_a", SymbolKind::Function, 12, 0),
                        test_symbol("sym_b", SymbolKind::Method, 12, 0),
                        test_symbol("sym_a", SymbolKind::Constant, 12, 4),
                        test_symbol("sym_long", SymbolKind::Function, 2, 0),
                    ],
                ),
                file_fixture(
                    "src/a.rs",
                    2,
                    vec![
                        test_symbol("sym_c", SymbolKind::Function, 30, 0),
                        test_symbol("sym_d", SymbolKind::Function, 10, 0),
                        test_symbol("do_sym", SymbolKind::Function, 1, 0),
                    ],
                ),
                // D1: keep the oracle fixture distinct because duplicate paths
                // are intentionally last-wins in the incremental index.
                file_fixture(
                    "src/b.rs",
                    3,
                    vec![
                        test_symbol("sym_e", SymbolKind::Method, 10, 2),
                        test_symbol("asym", SymbolKind::Class, 5, 0),
                    ],
                ),
            ],
            0,
        );

        for limit in [1, 2, 5, 9, 20] {
            let expected = full_sort_search_reference(&index, "sym", limit);
            assert_eq!(index.search("sym", limit), expected, "limit {limit}");
        }
    }

    #[test]
    fn test_filtered_search_matches_materialize_then_filter_and_preserves_memo() {
        let index = SymbolIndex::from_files(
            vec![
                file_fixture(
                    "src/allowed-a.rs",
                    1,
                    vec![
                        test_symbol("sym", SymbolKind::Function, 50, 0),
                        test_symbol("sym_long", SymbolKind::Function, 2, 0),
                        test_symbol("do_sym", SymbolKind::Method, 1, 0),
                    ],
                ),
                file_fixture(
                    "src/disallowed.rs",
                    2,
                    vec![
                        test_symbol("sym_a", SymbolKind::Function, 12, 0),
                        test_symbol("symbol", SymbolKind::Function, 8, 0),
                        test_symbol("asym", SymbolKind::Class, 5, 0),
                    ],
                ),
                file_fixture(
                    "src/allowed-b.rs",
                    3,
                    vec![
                        test_symbol("sym_b", SymbolKind::Method, 12, 0),
                        test_symbol("my_sym", SymbolKind::Constant, 4, 0),
                    ],
                ),
            ],
            0,
        );
        let allowed = |path: &str| path != "src/disallowed.rs";
        let expected_all: Vec<_> = index
            .search("sym", usize::MAX)
            .into_iter()
            .filter(|hit| allowed(hit.path))
            .collect();
        let expected = expected_all[..4].to_vec();
        let memo_before = {
            let memo = index.memo.lock();
            let memo = memo.as_ref().unwrap();
            (memo.needle.clone(), memo.limit, memo.hits.clone())
        };

        let actual = index.search_filtered("sym", 4, allowed);

        assert_eq!(actual, expected);
        let memo = index.memo.lock();
        let memo = memo.as_ref().unwrap();
        assert_eq!(
            (&memo.needle, memo.limit, &memo.hits),
            (&memo_before.0, memo_before.1, &memo_before.2)
        );
        assert!(index.search_filtered("sym", 0, |_| true).is_empty());
        assert_eq!(
            index.search_filtered("sym", usize::MAX, allowed),
            expected_all
        );
        assert!(index.search_filtered("sym", 10, |_| false).is_empty());
    }

    #[test]
    fn test_filtered_search_top_n_does_not_sort_disallowed_matches() {
        let disallowed = (0..4_900)
            .map(|index| {
                test_symbol(
                    &format!("matching_symbol_{index:04}"),
                    SymbolKind::Function,
                    ((index * 7_919) % 5_000 + 1) as u32,
                    0,
                )
            })
            .collect();
        let allowed = (4_900..5_000)
            .map(|index| {
                test_symbol(
                    &format!("matching_symbol_{index:04}"),
                    SymbolKind::Function,
                    ((index * 7_919) % 5_000 + 1) as u32,
                    0,
                )
            })
            .collect();
        let index = SymbolIndex::from_files(
            vec![
                file_fixture("src/disallowed.rs", 1, disallowed),
                file_fixture("src/allowed.rs", 2, allowed),
            ],
            0,
        );

        let hits = index.search_filtered("matching", 10, |path| path == "src/allowed.rs");

        assert_eq!(hits.len(), 10);
        let comparisons = index.search_comparisons();
        assert!(
            comparisons < 1_000,
            "ordering 10 allowed matches took {comparisons} comparisons; \
             disallowed files appear to have reached the global sort"
        );
    }

    #[test]
    fn test_search_cache_is_keyed_by_needle_and_limit() {
        let index = sample_index();
        *index.memo.lock() = Some(CachedSearch {
            needle: LoweredNeedle::new("app"),
            limit: 1,
            hits: vec![(0, 1)],
        });
        let names: Vec<_> = index
            .search("app", 10)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);

        *index.memo.lock() = Some(CachedSearch {
            needle: LoweredNeedle::new("render"),
            limit: 10,
            hits: vec![(0, 1)],
        });
        let names: Vec<_> = index
            .search("app", 10)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);
    }

    #[test]
    fn test_search_writes_computed_results_to_cache() {
        let index = sample_index();
        let hits = index.search("app", 10);
        let memo = index.memo.lock();
        let cached = memo.as_ref().expect("computed search was not cached");

        assert_eq!(cached.needle.as_str(), "app");
        assert_eq!(cached.limit, 10);
        assert_eq!(cached.hits.len(), hits.len());
    }

    #[test]
    fn test_search_scores_against_the_precomputed_lowered_names() {
        let mut index = SymbolIndex::from_files(
            vec![file_fixture(
                "src/zebra.rs",
                1,
                vec![test_symbol("Zebra", SymbolKind::Function, 1, 0)],
            )],
            0,
        );
        index.lowered_names[0][0] = Some(Box::<str>::from("apple"));

        let hits = index.search("apple", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "Zebra");
        assert!(index.search("zebra", 10).is_empty());
    }

    #[test]
    fn test_search_results_are_unchanged_by_precomputed_lowercasing() {
        let names = ["MixedCase", "UPPERCASE", "名前", "İ", "ß", "Éclair"];
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| test_symbol(name, SymbolKind::Function, index as u32 + 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![file_fixture("src/unicode.rs", 1, symbols)], 0);

        for query in ["case", "名前", "i", "ß", "é", "air", "xyz"] {
            let expected = full_sort_search_reference(&index, query, names.len());
            assert_eq!(
                index.search(query, names.len()),
                expected,
                "query {query:?}"
            );
        }
    }

    #[test]
    fn test_search_is_case_insensitive_for_queries() {
        let names = ["MixedCase", "UPPERCASE", "名前", "İ", "ß", "Éclair"];
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| test_symbol(name, SymbolKind::Function, index as u32 + 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![file_fixture("src/unicode.rs", 1, symbols)], 0);

        for query in ["MiXeDcAsE", "uPpErCaSe", "名前", "İ", "ß", "ÉcLaIr"] {
            let result = index.search(query, names.len());
            let expected = full_sort_search_reference(&index, query, names.len());
            assert_eq!(
                result, expected,
                "query {query:?} differs from the independent reference"
            );
            assert_eq!(
                result,
                index.search(&query.to_lowercase(), names.len()),
                "query {query:?} differs from its lowercase form"
            );
            assert!(!result.is_empty(), "query {query:?} must match");
        }
    }

    #[test]
    fn test_symbol_index_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}

        assert_send_and_sync::<SymbolIndex>();
    }

    #[test]
    fn test_symbol_ref_search_label() {
        let cases = [
            (
                test_symbol("alpha", SymbolKind::Function, 3, 0),
                "src/a.rs",
                "ƒ alpha  src/a.rs:3",
            ),
            (
                test_symbol("名前", SymbolKind::Method, 27, 4),
                "src/parser/names.rs",
                "m 名前  src/parser/names.rs:27",
            ),
            (
                test_symbol("Configuration", SymbolKind::Class, 91, 0),
                "crates/core/src/config.rs",
                "C Configuration  crates/core/src/config.rs:91",
            ),
        ];

        for (symbol, path, expected) in &cases {
            assert_eq!(SymbolRef { path, symbol }.search_label(), *expected);
        }
    }

    #[test]
    fn upsert_remove_keep_by_name_and_by_path_consistent() {
        let mut index = SymbolIndex::from_files(
            vec![
                file_fixture(
                    "src/a.rs",
                    1,
                    vec![test_symbol("alpha_symbol", SymbolKind::Function, 1, 0)],
                ),
                file_fixture(
                    "src/b.rs",
                    2,
                    vec![test_symbol("beta_symbol", SymbolKind::Function, 2, 0)],
                ),
            ],
            0,
        );
        let freed_slot = index.by_path["src/a.rs"];

        assert!(index.remove("src/a.rs"));
        assert!(index.definitions("alpha_symbol").is_empty());
        assert!(index.search("alpha", 10).is_empty());
        assert!(index.file_symbols("src/a.rs").is_none());

        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/c.rs",
                    3,
                    vec![test_symbol("gamma_symbol", SymbolKind::Function, 3, 0,)],
                ),
                0,
            ),
            UpsertOutcome::Inserted
        );
        assert_eq!(index.by_path["src/c.rs"], freed_slot);
        assert_eq!(index.file_count(), 2);
        let mut paths: Vec<_> = index.paths().collect();
        paths.sort_unstable();
        assert_eq!(paths, ["src/b.rs", "src/c.rs"]);
        assert_eq!(
            rendered(index.search("symbol", usize::MAX)),
            vec![
                ("src/b.rs".to_owned(), "beta_symbol".to_owned(), 2,),
                ("src/c.rs".to_owned(), "gamma_symbol".to_owned(), 3,),
            ]
        );
    }

    #[test]
    fn identical_upsert_is_unchanged_without_generation_or_memo_changes() {
        let mut index = SymbolIndex::from_files(
            vec![file_fixture(
                "src/cache.rs",
                11,
                vec![
                    test_symbol("cache_a", SymbolKind::Function, 1, 0),
                    test_symbol("cache_b", SymbolKind::Function, 2, 0),
                ],
            )],
            7,
        );
        assert_eq!(index.search("cache", 10).len(), 2);
        assert!(index.search_comparisons() > 0);
        let generation = index.generation();

        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/cache.rs",
                    11,
                    vec![test_symbol("ignored", SymbolKind::Class, 99, 0)],
                ),
                7,
            ),
            UpsertOutcome::Unchanged
        );
        assert_eq!(index.generation(), generation);
        assert!(index.memo.lock().is_some());
        assert_eq!(index.search("cache", 10).len(), 2);
        assert_eq!(index.search_comparisons(), 0);
        assert_eq!(index.file_symbols("src/cache.rs").unwrap().len(), 2);
    }

    #[test]
    fn effective_mutations_invalidate_memo_and_bump_generation() {
        let mut index = sample_index();
        assert!(!index.search("app", 10).is_empty());
        assert!(index.memo.lock().is_some());
        let generation = index.generation();

        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/app.rs",
                    99,
                    vec![test_symbol("replacement", SymbolKind::Function, 1, 0)],
                ),
                0,
            ),
            UpsertOutcome::Updated
        );
        assert_eq!(index.generation(), generation + 1);
        assert!(index.memo.lock().is_none());

        let generation = index.generation();
        assert!(index.remove("src/app.rs"));
        assert_eq!(index.generation(), generation + 1);
        assert!(index.memo.lock().is_none());

        let generation = index.generation();
        assert!(!index.remove("src/missing.rs"));
        assert_eq!(index.generation(), generation);
    }

    #[test]
    fn from_files_equals_shuffled_upsert_sequences() {
        let files: Vec<_> = (0..8)
            .map(|index| {
                file_fixture(
                    &format!("src/file_{index}.rs"),
                    index as u64 + 10,
                    vec![
                        test_symbol(
                            if index % 2 == 0 { "Shared" } else { "shared" },
                            if index % 3 == 0 {
                                SymbolKind::Class
                            } else {
                                SymbolKind::Function
                            },
                            index as u32 + 1,
                            0,
                        ),
                        test_symbol(
                            &format!("worker_{index}"),
                            SymbolKind::Method,
                            index as u32 + 20,
                            2,
                        ),
                    ],
                )
            })
            .collect();
        let baseline = SymbolIndex::from_files(files.clone(), 42);

        for seed in [1_u64, 0x9e37_79b9, 0xd1b5_4a32_d192_ed03] {
            let mut order: Vec<usize> = (0..files.len()).collect();
            let mut state = seed;
            for index in (1..order.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                order.swap(index, state as usize % (index + 1));
            }

            let mut shuffled = SymbolIndex::new();
            for index in order {
                assert_eq!(
                    shuffled.upsert(files[index].clone(), 42),
                    UpsertOutcome::Inserted
                );
            }

            let mut expected_paths: Vec<_> = baseline.paths().collect();
            let mut actual_paths: Vec<_> = shuffled.paths().collect();
            expected_paths.sort_unstable();
            actual_paths.sort_unstable();
            assert_eq!(actual_paths, expected_paths, "seed {seed}");
            assert_eq!(
                shuffled.symbol_count(),
                baseline.symbol_count(),
                "seed {seed}"
            );
            for path in expected_paths {
                assert_eq!(
                    shuffled.file_symbols(path),
                    baseline.file_symbols(path),
                    "seed {seed}, path {path}"
                );
            }
            for name in ["shared", "worker_0", "worker_3", "missing"] {
                assert_eq!(
                    rendered(shuffled.definitions(name)),
                    rendered(baseline.definitions(name)),
                    "seed {seed}, definition {name}"
                );
            }
            for query in ["sha", "worker", "wrkr7", "missing"] {
                assert_eq!(
                    rendered(shuffled.search(query, usize::MAX)),
                    rendered(baseline.search(query, usize::MAX)),
                    "seed {seed}, query {query}"
                );
            }
        }
    }

    #[test]
    fn by_path_lookup_stays_correct_across_slot_churn() {
        let mut index = SymbolIndex::new();
        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/a.rs",
                    1,
                    vec![test_symbol("from_a", SymbolKind::Function, 1, 0)],
                ),
                0,
            ),
            UpsertOutcome::Inserted
        );
        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/b.rs",
                    2,
                    vec![test_symbol("from_b", SymbolKind::Function, 2, 0)],
                ),
                0,
            ),
            UpsertOutcome::Inserted
        );
        let a_slot = index.by_path["src/a.rs"];
        assert!(index.remove("src/a.rs"));
        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/c.rs",
                    3,
                    vec![test_symbol("from_c", SymbolKind::Class, 3, 0)],
                ),
                0,
            ),
            UpsertOutcome::Inserted
        );

        assert_eq!(index.by_path["src/c.rs"], a_slot);
        assert_eq!(index.file_symbols("src/c.rs").unwrap()[0].name, "from_c");
        assert!(index.file_symbols("src/a.rs").is_none());
        assert_eq!(index.definitions("from_b")[0].path, "src/b.rs");
        assert_eq!(index.definitions("from_c")[0].path, "src/c.rs");
    }

    #[test]
    fn registry_generation_change_updates_even_with_the_same_hash() {
        let mut index = SymbolIndex::from_files(
            vec![file_fixture(
                "src/registry.rs",
                5,
                vec![test_symbol("old_name", SymbolKind::Function, 1, 0)],
            )],
            1,
        );
        assert!(!index.search("old", 10).is_empty());
        let generation = index.generation();

        assert_eq!(
            index.upsert(
                file_fixture(
                    "src/registry.rs",
                    5,
                    vec![test_symbol("new_name", SymbolKind::Class, 2, 0)],
                ),
                2,
            ),
            UpsertOutcome::Updated
        );
        assert_eq!(index.generation(), generation + 1);
        assert!(index.memo.lock().is_none());
        assert!(index.definitions("old_name").is_empty());
        assert_eq!(index.definitions("new_name")[0].path, "src/registry.rs");
        assert!(index.contains("src/registry.rs", 5, 2));
        assert!(!index.contains("src/registry.rs", 5, 1));
    }

    #[test]
    fn from_files_uses_last_path_and_keeps_empty_files() {
        let index = SymbolIndex::from_files(
            vec![
                file_fixture(
                    "src/duplicate.rs",
                    1,
                    vec![test_symbol("first", SymbolKind::Function, 1, 0)],
                ),
                file_fixture(
                    "src/duplicate.rs",
                    2,
                    vec![test_symbol("last", SymbolKind::Function, 2, 0)],
                ),
                file_fixture("src/empty.rs", 3, Vec::new()),
            ],
            4,
        );

        assert_eq!(index.scanned_file_count(), 3);
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.file_hash("src/duplicate.rs"), Some(2));
        assert_eq!(
            index.file_symbols("src/duplicate.rs").unwrap()[0].name,
            "last"
        );
        assert_eq!(index.file_symbols("src/empty.rs"), Some([].as_slice()));
    }
}
