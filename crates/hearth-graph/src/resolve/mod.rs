//! Module resolution abstractions and language-specific dispatch.

use compact_str::CompactString;

use crate::imports::{ImportKind, RawImport};

/// The result of classifying an import specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// An absolute path to a workspace file.
    Path(CompactString),
    /// A dependency outside the workspace, such as an npm package or Rust crate.
    ///
    /// For JavaScript, if symlink canonicalization moves a package into a store
    /// path without a `node_modules` component, the residual classification is
    /// [`Self::Path`].
    External(CompactString),
    /// A specifier that could not be resolved.
    Unresolved(UnresolvedReason),
}

/// Why an import could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// No matching path or package was found.
    NotFound,
    /// No resolver supports this import kind.
    Unsupported,
    /// Resolution could not be completed reliably.
    Failed {
        /// Broad category suitable for programmatic handling.
        kind: FailedKind,
        /// Resolver-specific diagnostic detail.
        detail: CompactString,
    },
}

/// Broad category for a failed resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedKind {
    /// A configuration file was missing, malformed, or internally inconsistent.
    Config,
    /// Filesystem access failed.
    Io,
    /// The import or referrer specifier was invalid.
    InvalidSpecifier,
    /// A resolver failure that does not fit a more specific category.
    Other,
}

/// Whether a resolution outcome covers every relevant resolution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCompleteness {
    /// The resolver fully modeled this import.
    Complete,
    /// The resolver returned a best-effort result that may omit another target.
    Partial,
}

/// A resolution result and the filesystem paths consulted to produce it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionOutcome {
    /// The classified resolution.
    pub resolved: Resolved,
    /// Absolute paths of found and missing dependencies consulted during resolution.
    ///
    /// JavaScript outcomes retain both the configured root tsconfig and the
    /// config selected for an importing file, then follow the selected config's
    /// `extends` chain. Traversing every project reference is out of scope for v1.
    pub dependencies: Vec<CompactString>,
    /// Non-fatal observations made while collecting resolution dependencies.
    pub notes: Vec<CompactString>,
    /// Whether the resolver fully modeled every relevant resolution path.
    pub completeness: ResolutionCompleteness,
}

/// A type-erased module resolver.
pub trait Resolve: Send + Sync {
    /// Resolve an import relative to its importing file.
    ///
    /// `from_file` must be an absolute path. Relative inputs return
    /// [`UnresolvedReason::Failed`].
    fn resolve(&self, from_file: &str, import: &RawImport) -> ResolutionOutcome;

    /// Discard all cached filesystem and configuration state.
    ///
    /// This call must not overlap an in-flight [`Self::resolve`], as required by
    /// `oxc_resolver`. The Hearth adapter guarantees exclusion through
    /// single-flight sweeps.
    fn clear_cache(&self);
}

/// Resolvers available for each supported language family.
#[derive(Default)]
pub struct ResolverSet {
    /// Resolver for JavaScript and TypeScript imports.
    pub js: Option<Box<dyn Resolve>>,
    /// Resolver for Rust imports.
    pub rust: Option<Box<dyn Resolve>>,
}

impl ResolverSet {
    /// Dispatch an import to its language-specific resolver.
    pub fn resolve(&self, from_file: &str, import: &RawImport) -> ResolutionOutcome {
        let resolver = match import.kind {
            ImportKind::RustUse | ImportKind::RustMod => self.rust.as_deref(),
            _ => self.js.as_deref(),
        };

        resolver.map_or_else(unsupported, |resolver| resolver.resolve(from_file, import))
    }

    /// Clear every configured resolver cache.
    pub fn clear_cache(&self) {
        if let Some(resolver) = &self.js {
            resolver.clear_cache();
        }
        if let Some(resolver) = &self.rust {
            resolver.clear_cache();
        }
    }
}

fn unsupported() -> ResolutionOutcome {
    ResolutionOutcome {
        resolved: Resolved::Unresolved(UnresolvedReason::Unsupported),
        dependencies: Vec::new(),
        notes: Vec::new(),
        // Graph guarantees already degrade when import extraction is unsupported
        // or no matching resolver is live.
        completeness: ResolutionCompleteness::Complete,
    }
}

#[cfg(feature = "resolve-js")]
pub mod js;
#[cfg(feature = "resolve-rust")]
pub mod rust;
