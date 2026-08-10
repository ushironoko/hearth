use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use tree_sitter::{Parser, Query};

use crate::{ImportSpec, LanguageId, LanguageRegistry};

/// Lazily initialized parser and query cache for a language registry.
pub struct ParserPool<'r> {
    registry: &'r LanguageRegistry,
    parsers: FxHashMap<LanguageId, Option<Parser>>,
    tags_queries: FxHashMap<LanguageId, Option<Query>>,
    imports_queries: FxHashMap<LanguageId, Option<Query>>,
    injections_queries: FxHashMap<LanguageId, Option<Query>>,
}

impl<'r> ParserPool<'r> {
    /// Creates an empty cache backed by `registry`.
    #[must_use]
    pub fn new(registry: &'r LanguageRegistry) -> Self {
        Self {
            registry,
            parsers: FxHashMap::default(),
            tags_queries: FxHashMap::default(),
            imports_queries: FxHashMap::default(),
            injections_queries: FxHashMap::default(),
        }
    }

    /// Returns the registry whose identifiers this pool accepts.
    pub(crate) fn registry(&self) -> &'r LanguageRegistry {
        self.registry
    }

    /// Returns a parser for `id`, creating and configuring it on first use.
    ///
    /// A grammar rejected by tree-sitter is cached as a permanent miss.
    pub fn parser(&mut self, id: LanguageId) -> Option<&mut Parser> {
        if let Entry::Vacant(entry) = self.parsers.entry(id) {
            let parser = self.registry.get(id).and_then(|spec| {
                let mut parser = Parser::new();
                parser.set_language(&spec.language).ok()?;
                Some(parser)
            });
            entry.insert(parser);
        }

        self.parsers.get_mut(&id).and_then(Option::as_mut)
    }

    /// Returns the compiled tags query for `id`.
    ///
    /// Missing query sources and compilation failures are cached permanently.
    pub fn tags_query(&mut self, id: LanguageId) -> Option<&Query> {
        if let Entry::Vacant(entry) = self.tags_queries.entry(id) {
            let query = self.registry.get(id).and_then(|spec| {
                let source = spec.tags_query.as_deref()?;
                Query::new(&spec.language, source).ok()
            });
            entry.insert(query);
        }

        self.tags_queries.get(&id).and_then(Option::as_ref)
    }

    /// Returns the compiled query-based import extractor for `id`.
    ///
    /// Custom extractors, missing queries, and compilation failures are cached
    /// as permanent misses.
    pub fn imports_query(&mut self, id: LanguageId) -> Option<&Query> {
        if let Entry::Vacant(entry) = self.imports_queries.entry(id) {
            let query = self
                .registry
                .get(id)
                .and_then(|spec| match spec.imports.as_ref()? {
                    ImportSpec::Query { source, .. } => {
                        Query::new(&spec.language, source.as_ref()).ok()
                    }
                    ImportSpec::Custom(_) => None,
                });
            entry.insert(query);
        }

        self.imports_queries.get(&id).and_then(Option::as_ref)
    }

    /// Returns the compiled injection query for `id`.
    ///
    /// Missing query sources and compilation failures are cached permanently.
    pub fn injections_query(&mut self, id: LanguageId) -> Option<&Query> {
        if let Entry::Vacant(entry) = self.injections_queries.entry(id) {
            let query = self.registry.get(id).and_then(|spec| {
                let source = spec.injections_query.as_deref()?;
                Query::new(&spec.language, source).ok()
            });
            entry.insert(query);
        }

        self.injections_queries.get(&id).and_then(Option::as_ref)
    }
}
