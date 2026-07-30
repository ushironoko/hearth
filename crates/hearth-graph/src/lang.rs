use std::borrow::Cow;
use std::path::Path;

use compact_str::CompactString;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

/// Stable identifier for one entry in a [`LanguageRegistry`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LanguageId(u16);

impl LanguageId {
    /// Returns the registry slot represented by this identifier.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Import-extraction strategy for a language. Stage B supplies the
/// extractors; the shape is fixed here so registering one later is not a
/// public-API break.
#[non_exhaustive]
pub enum ImportSpec {
    /// Extract imports with a tree-sitter query whose capture names map to
    /// import kinds.
    Query {
        /// Tree-sitter query source.
        source: Cow<'static, str>,
        /// Maps a query capture name to the import kind it represents.
        kind_map: fn(&str) -> crate::imports::ImportKind,
    },
    /// Extract imports with a language-specific syntax-tree walker, for
    /// languages a flat query cannot express (Rust use-trees).
    Custom(fn(&str, &tree_sitter::Tree) -> Vec<crate::imports::RawImport>),
}

/// Parsing and query metadata for one registered language.
///
/// Create specifications with [`LanguageSpec::new`] and its builder methods.
/// The struct is non-exhaustive so hosts can keep registering custom grammars
/// when Hearth adds optional language metadata.
#[non_exhaustive]
pub struct LanguageSpec {
    /// Wire-stable lowercase language name.
    pub name: CompactString,
    /// Tree-sitter grammar.
    pub language: tree_sitter::Language,
    /// File extensions recognized for this language, without leading dots.
    pub extensions: SmallVec<[CompactString; 4]>,
    /// Tree-sitter tags query used for symbol extraction.
    pub tags_query: Option<Cow<'static, str>>,
    /// Merge adjacent same-name definitions emitted for one logical symbol.
    ///
    /// This is intended for grammars such as Haskell, where each equation of
    /// one function is represented by a separate definition node.
    pub merge_adjacent_same_name_definitions: bool,
    /// Import extraction strategy, when available.
    pub imports: Option<ImportSpec>,
}

impl LanguageSpec {
    /// Creates a language specification with no symbol or import queries.
    ///
    /// Extensions must not include a leading dot. Optional behavior can be
    /// enabled with the builder methods without coupling hosts to every field.
    #[must_use]
    pub fn new<I, E>(
        name: impl Into<CompactString>,
        language: tree_sitter::Language,
        extensions: I,
    ) -> Self
    where
        I: IntoIterator<Item = E>,
        E: AsRef<str>,
    {
        Self {
            name: name.into(),
            language,
            extensions: extensions
                .into_iter()
                .map(|extension| CompactString::new(extension.as_ref()))
                .collect(),
            tags_query: None,
            merge_adjacent_same_name_definitions: false,
            imports: None,
        }
    }

    /// Configures the tree-sitter tags query used for symbol extraction.
    #[must_use]
    pub fn with_tags_query(mut self, tags_query: impl Into<Cow<'static, str>>) -> Self {
        self.tags_query = Some(tags_query.into());
        self
    }

    /// Configures whether adjacent same-name definitions form one symbol.
    #[must_use]
    pub const fn with_merge_adjacent_same_name_definitions(mut self, enabled: bool) -> Self {
        self.merge_adjacent_same_name_definitions = enabled;
        self
    }

    /// Configures import extraction for the language.
    #[must_use]
    pub fn with_imports(mut self, imports: ImportSpec) -> Self {
        self.imports = Some(imports);
        self
    }
}

/// Ordered language registry with last-registration-wins extension lookup.
pub struct LanguageRegistry {
    specs: Vec<LanguageSpec>,
    by_extension: FxHashMap<CompactString, LanguageId>,
    generation: u64,
}

impl LanguageRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            specs: Vec::new(),
            by_extension: FxHashMap::default(),
            generation: 0,
        }
    }

    /// Registers a language and returns its stable identifier.
    ///
    /// When an extension was already registered, this specification becomes
    /// the extension's new owner.
    pub fn register(&mut self, spec: LanguageSpec) -> LanguageId {
        let id = LanguageId(
            u16::try_from(self.specs.len()).expect("language registry exhausted its u16 id space"),
        );

        for extension in &spec.extensions {
            self.by_extension.insert(extension.clone(), id);
        }

        self.specs.push(spec);
        self.generation += 1;
        id
    }

    /// Returns the number of registry mutations observed so far.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Resolves a path by its final file extension.
    #[must_use]
    pub fn for_path(&self, path: &Path) -> Option<LanguageId> {
        let extension = path.extension()?.to_str()?;
        self.by_extension.get(extension).copied()
    }

    /// Returns the specification for an identifier.
    #[must_use]
    pub fn get(&self, id: LanguageId) -> Option<&LanguageSpec> {
        self.specs.get(id.index())
    }

    /// Returns whether the path has a registered symbol query.
    #[must_use]
    pub fn supports_symbols(&self, path: &Path) -> bool {
        self.for_path(path)
            .and_then(|id| self.get(id))
            .is_some_and(|spec| spec.tags_query.is_some())
    }

    /// Returns whether the path has a registered import extractor.
    #[must_use]
    pub fn supports_imports(&self, path: &Path) -> bool {
        self.for_path(path)
            .and_then(|id| self.get(id))
            .is_some_and(|spec| spec.imports.is_some())
    }

    /// Iterates over registered identifiers and specifications in registration order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (LanguageId, &LanguageSpec)> {
        self.specs.iter().enumerate().map(|(index, spec)| {
            let id = LanguageId(
                u16::try_from(index).expect("registered language index must fit in LanguageId"),
            );
            (id, spec)
        })
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::empty()
    }
}
