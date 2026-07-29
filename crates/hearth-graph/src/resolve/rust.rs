//! Rust module-tree resolution.
//!
//! v1 does not reproduce the module declaration tree, including multi-level
//! inline modules, `#[path]`, `cfg`, macros, and `mod` ambiguity. To keep the
//! label constitutively sound, every outcome is `Partial`. Making Rust
//! resolution exact requires Cargo target metadata plus a declaration-tree
//! model, which is future work.

use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
};

use compact_str::CompactString;

use super::{
    FailedKind, ResolutionCompleteness, ResolutionOutcome, Resolve, Resolved, UnresolvedReason,
};
use crate::imports::{ImportKind, RawImport};

const COMPLETENESS: ResolutionCompleteness = ResolutionCompleteness::Partial;

/// Configuration for Rust module-tree resolution.
#[derive(Debug, Clone)]
pub struct RustResolveOptions {
    /// Absolute paths to crate-root files such as `src/lib.rs` and `src/main.rs`.
    pub crate_roots: Vec<CompactString>,
}

/// Build a Rust resolver backed by the operating system filesystem.
pub fn rust_resolver(options: RustResolveOptions) -> Box<dyn Resolve> {
    Box::new(RustResolver { options })
}

struct RustResolver {
    options: RustResolveOptions,
}

impl Resolve for RustResolver {
    fn baseline_completeness(&self) -> ResolutionCompleteness {
        COMPLETENESS
    }

    fn resolve(&self, from_file: &str, import: &RawImport) -> ResolutionOutcome {
        let from_path = Path::new(from_file);
        debug_assert!(
            from_path.is_absolute(),
            "from_file must be absolute: {from_file}"
        );
        if !from_path.is_absolute() {
            return unresolved(
                failed(FailedKind::InvalidSpecifier, "from_file must be absolute"),
                Vec::new(),
            );
        }
        let Some(_) = from_path.parent() else {
            return unresolved(
                failed(
                    FailedKind::InvalidSpecifier,
                    "from_file must have a parent directory",
                ),
                Vec::new(),
            );
        };

        match import.kind {
            ImportKind::RustMod => self.resolve_mod(from_path, import.specifier.as_str()),
            ImportKind::RustUse => self.resolve_use(from_path, import.specifier.as_str()),
            _ => unresolved(UnresolvedReason::Unsupported, Vec::new()),
        }
    }

    fn clear_cache(&self) {}
}

impl RustResolver {
    fn resolve_use(&self, from_path: &Path, specifier: &str) -> ResolutionOutcome {
        let segments: Vec<&str> = specifier.split("::").collect();
        let Some(first) = segments.first().copied() else {
            return unresolved(UnresolvedReason::NotFound, Vec::new());
        };

        match first {
            "crate" => self.resolve_from_crate_root(from_path, &segments[1..]),
            "self" => {
                let Some(base) = children_directory(from_path, self.is_crate_root_file(from_path))
                else {
                    return invalid_from_file();
                };
                resolve_segments(base, &segments[1..], Some(from_path.to_path_buf()))
            }
            "super" => self.resolve_super(from_path, &segments),
            external => self.resolve_bare(from_path, external, &segments[1..]),
        }
    }

    fn resolve_mod(&self, from_file: &Path, specifier: &str) -> ResolutionOutcome {
        let Some(base) = children_directory(from_file, self.is_crate_root_file(from_file)) else {
            return invalid_from_file();
        };
        let mut dependencies = Vec::new();
        match probe_module(&base, specifier, &mut dependencies, false) {
            Ok(Some(found)) => resolved_path(found, dependencies),
            Ok(None) => unresolved(UnresolvedReason::NotFound, dependencies),
            Err(error) => io_failure(error, dependencies),
        }
    }

    fn resolve_from_crate_root(&self, from_file: &Path, segments: &[&str]) -> ResolutionOutcome {
        let Some(root) = self.select_crate_root(from_file) else {
            return unresolved(UnresolvedReason::NotFound, Vec::new());
        };
        let Some(root_dir) = root.parent() else {
            return invalid_from_file();
        };
        resolve_segments(root_dir.to_path_buf(), segments, Some(root))
    }

    fn resolve_bare(&self, from_file: &Path, first: &str, remaining: &[&str]) -> ResolutionOutcome {
        let Some(base) = children_directory(from_file, self.is_crate_root_file(from_file)) else {
            return invalid_from_file();
        };
        let mut dependencies = Vec::new();
        match probe_module(&base, first, &mut dependencies, true) {
            Ok(Some(found)) => resolve_segments_with_dependencies(
                base.join(first),
                remaining,
                Some(found),
                dependencies,
            ),
            Ok(None) => ResolutionOutcome {
                resolved: Resolved::External(first.into()),
                dependencies,
                notes: Vec::new(),
                completeness: COMPLETENESS,
            },
            Err(error) => io_failure(error, dependencies),
        }
    }

    fn resolve_super(&self, from_file: &Path, segments: &[&str]) -> ResolutionOutcome {
        if self.is_crate_root_file(from_file) {
            return unresolved(UnresolvedReason::NotFound, Vec::new());
        }

        let crate_root = self.select_crate_root(from_file);
        let root_directory = crate_root
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let Some(mut logical_directory) = children_directory(from_file, false) else {
            return invalid_from_file();
        };
        let super_count = segments
            .iter()
            .take_while(|segment| **segment == "super")
            .count();
        let mut dependencies = Vec::new();
        let mut seed = None;

        for _ in 0..super_count {
            if root_directory
                .as_deref()
                .is_some_and(|root| logical_directory == root)
                || is_standard_source_root(&logical_directory)
            {
                return unresolved(UnresolvedReason::NotFound, dependencies);
            }
            let Some(ancestor_directory) = logical_directory.parent().map(Path::to_path_buf) else {
                return unresolved(UnresolvedReason::NotFound, dependencies);
            };
            if root_directory
                .as_deref()
                .is_some_and(|root| !ancestor_directory.starts_with(root))
            {
                return unresolved(UnresolvedReason::NotFound, dependencies);
            }

            seed = match ancestor_module_file(
                &ancestor_directory,
                crate_root.as_deref(),
                &mut dependencies,
            ) {
                Ok(seed) => seed,
                Err(error) => {
                    return io_failure(error, dependencies);
                }
            };
            logical_directory = ancestor_directory;
        }

        resolve_segments_with_dependencies(
            logical_directory,
            &segments[super_count..],
            seed,
            dependencies,
        )
    }

    fn select_crate_root(&self, from_file: &Path) -> Option<PathBuf> {
        if is_implicit_crate_root(from_file) {
            return Some(from_file.to_path_buf());
        }
        let from_dir = from_file.parent()?;
        let mut roots: Vec<PathBuf> = self
            .options
            .crate_roots
            .iter()
            .map(|root| PathBuf::from(root.as_str()))
            .filter(|root| {
                root.parent()
                    .is_some_and(|root_dir| from_dir.starts_with(root_dir))
            })
            .collect();
        roots.sort_unstable_by(|left, right| compare_crate_roots(left, right));
        roots.dedup();
        roots.into_iter().next()
    }

    fn is_crate_root_file(&self, path: &Path) -> bool {
        is_implicit_crate_root(path)
            || matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs")
            )
            || self
                .options
                .crate_roots
                .iter()
                .any(|root| Path::new(root.as_str()) == path)
    }
}

fn resolve_segments(base: PathBuf, segments: &[&str], seed: Option<PathBuf>) -> ResolutionOutcome {
    resolve_segments_with_dependencies(base, segments, seed, Vec::new())
}

fn resolve_segments_with_dependencies(
    base: PathBuf,
    segments: &[&str],
    seed: Option<PathBuf>,
    mut dependencies: Vec<CompactString>,
) -> ResolutionOutcome {
    match walk_segments(base, segments, seed, &mut dependencies) {
        Ok(Some(found)) => resolved_path(found, dependencies),
        Ok(None) => unresolved(UnresolvedReason::NotFound, dependencies),
        Err(error) => io_failure(error, dependencies),
    }
}

fn walk_segments(
    mut directory: PathBuf,
    segments: &[&str],
    seed: Option<PathBuf>,
    dependencies: &mut Vec<CompactString>,
) -> Result<Option<PathBuf>, CandidateIoError> {
    let mut last_found = seed;
    for segment in segments {
        let file_candidate = directory.join(format!("{segment}.rs"));
        dependencies.push(compact_path(&file_candidate));
        if candidate_is_file(&file_candidate)? {
            last_found = Some(file_candidate);
            directory.push(segment);
            continue;
        }

        let module_candidate = directory.join(segment).join("mod.rs");
        dependencies.push(compact_path(&module_candidate));
        if candidate_is_file(&module_candidate)? {
            last_found = Some(module_candidate);
            directory.push(segment);
            continue;
        }
        break;
    }
    Ok(last_found)
}

fn probe_module(
    directory: &Path,
    segment: &str,
    dependencies: &mut Vec<CompactString>,
    track_both_candidates: bool,
) -> Result<Option<PathBuf>, CandidateIoError> {
    let file_candidate = directory.join(format!("{segment}.rs"));
    let module_candidate = directory.join(segment).join("mod.rs");
    dependencies.push(compact_path(&file_candidate));
    if track_both_candidates {
        dependencies.push(compact_path(&module_candidate));
    }
    if candidate_is_file(&file_candidate)? {
        return Ok(Some(file_candidate));
    }
    if !track_both_candidates {
        dependencies.push(compact_path(&module_candidate));
    }
    candidate_is_file(&module_candidate).map(|found| found.then_some(module_candidate))
}

fn ancestor_module_file(
    directory: &Path,
    crate_root: Option<&Path>,
    dependencies: &mut Vec<CompactString>,
) -> Result<Option<PathBuf>, CandidateIoError> {
    if let Some(root) = crate_root
        && root.parent() == Some(directory)
    {
        return Ok(Some(root.to_path_buf()));
    }

    if is_standard_source_root(directory) {
        for name in ["lib.rs", "main.rs"] {
            let candidate = directory.join(name);
            dependencies.push(compact_path(&candidate));
            if candidate_is_file(&candidate)? {
                return Ok(Some(candidate));
            }
        }
        return Ok(None);
    }

    let file_candidate = directory.with_extension("rs");
    dependencies.push(compact_path(&file_candidate));
    if candidate_is_file(&file_candidate)? {
        return Ok(Some(file_candidate));
    }
    let module_candidate = directory.join("mod.rs");
    dependencies.push(compact_path(&module_candidate));
    candidate_is_file(&module_candidate).map(|found| found.then_some(module_candidate))
}

fn children_directory(from_file: &Path, crate_root: bool) -> Option<PathBuf> {
    let parent = from_file.parent()?;
    if crate_root
        || matches!(
            from_file.file_name().and_then(|name| name.to_str()),
            Some("mod.rs" | "lib.rs" | "main.rs")
        )
    {
        Some(parent.to_path_buf())
    } else {
        Some(parent.join(from_file.file_stem()?))
    }
}

fn is_implicit_crate_root(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    match parent.file_name().and_then(|name| name.to_str()) {
        Some("examples" | "tests") => true,
        Some("bin") => parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "src"),
        _ => false,
    }
}

fn is_standard_source_root(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "src")
}

fn compare_crate_roots(left: &Path, right: &Path) -> Ordering {
    let left_dir = left.parent().expect("filtered crate root has a parent");
    let right_dir = right.parent().expect("filtered crate root has a parent");
    right_dir
        .components()
        .count()
        .cmp(&left_dir.components().count())
        .then_with(|| crate_root_priority(left).cmp(&crate_root_priority(right)))
        .then_with(|| left.cmp(right))
}

fn crate_root_priority(path: &Path) -> u8 {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs") => 0,
        Some("main.rs") => 1,
        _ => 2,
    }
}

fn is_file(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn candidate_is_file(path: &Path) -> Result<bool, CandidateIoError> {
    is_file(path).map_err(|source| CandidateIoError {
        path: path.to_path_buf(),
        source,
    })
}

fn resolved_path(path: PathBuf, dependencies: Vec<CompactString>) -> ResolutionOutcome {
    ResolutionOutcome {
        resolved: Resolved::Path(compact_path(&path)),
        dependencies,
        notes: Vec::new(),
        completeness: COMPLETENESS,
    }
}

fn unresolved(reason: UnresolvedReason, dependencies: Vec<CompactString>) -> ResolutionOutcome {
    ResolutionOutcome {
        resolved: Resolved::Unresolved(reason),
        dependencies,
        notes: Vec::new(),
        completeness: COMPLETENESS,
    }
}

fn invalid_from_file() -> ResolutionOutcome {
    unresolved(
        failed(
            FailedKind::InvalidSpecifier,
            "from_file must have a file name and parent directory",
        ),
        Vec::new(),
    )
}

fn io_failure(error: CandidateIoError, dependencies: Vec<CompactString>) -> ResolutionOutcome {
    unresolved(
        failed(
            FailedKind::Io,
            format!(
                "could not inspect {}: {}",
                error.path.display(),
                error.source
            ),
        ),
        dependencies,
    )
}

fn failed(kind: FailedKind, detail: impl Into<CompactString>) -> UnresolvedReason {
    UnresolvedReason::Failed {
        kind,
        detail: detail.into(),
    }
}

fn compact_path(path: &Path) -> CompactString {
    CompactString::from(path.to_string_lossy().as_ref())
}

struct CandidateIoError {
    path: PathBuf,
    source: io::Error,
}
