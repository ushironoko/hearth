//! Pi-compatible glob discovery over the resident directory-walk snapshot.
//!
//! Inclusion and exclusion globs are deliberately post-filters: one bounded,
//! path-sorted walk serves every pattern without fragmenting the cache key.

use crate::util::resolve_path;
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use hearth_core::cache::{WalkEntry, WalkKey};
use hearth_core::{CancelToken, Engine, profile};
use hearth_proto::{FindParams, FindResult, ToolError, ToolResult};
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

const MAX_PATTERN_BYTES: usize = 4 * 1024;
const MAX_EXCLUDE_GLOBS: usize = 128;
const MAX_EXCLUDE_GLOB_BYTES: usize = 16 * 1024;
const DEFAULT_FIND_LIMIT: u64 = 1_000;
const MAX_FIND_LIMIT: u64 = 1_000_000;
/// Pi's shared `truncateHead` presentation budget.
const MAX_FIND_OUTPUT_BYTES: usize = 50 * 1024;

/// Search `params.path` for entries matching `params.pattern`.
pub fn find(engine: &Engine, params: &FindParams) -> ToolResult<FindResult> {
    find_cancellable(engine, params, &CancelToken::none())
}

/// As [`find`], with cooperative cancellation before/after the cold walk and
/// while filtering a warm snapshot. A cold walk is one non-preemptive safe
/// step, matching the other walk-backed tools.
pub fn find_cancellable(
    engine: &Engine,
    params: &FindParams,
    cancel: &CancelToken,
) -> ToolResult<FindResult> {
    profile!("tool.find", {
        cancel.check()?;
        validate_params(params)?;
        let include = IncludeMatcher::new(&params.pattern)?;
        let excludes = ExcludeMatchers::new(&params.exclude_globs)?;
        let limit = params.limit.unwrap_or(DEFAULT_FIND_LIMIT);

        let root = lexical_absolute(engine, &params.path)?;
        let metadata = std::fs::metadata(&root).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ToolError::not_found(root.display().to_string()),
            _ => ToolError::from(error).with_path(root.display().to_string()),
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::invalid("find target must be a directory"));
        }

        let key = WalkKey {
            respect_gitignore: params.respect_gitignore,
            hidden: params.hidden,
            follow_symlinks: params.follow_symlinks,
        };
        engine.watch_root(&root);
        let (entry, walk_cache_hit) = engine.walks().get(&root, key);
        cancel.check()?;
        if !entry.complete {
            return Err(ToolError::invalid("find walk exceeded its work budget"));
        }

        let mut paths = Vec::with_capacity((limit as usize).min(1_000));
        let mut retained_bytes = 0usize;
        let mut output_exhausted = false;
        let mut output_limit_reached = false;
        let mut total_matches = 0u64;

        for candidate in MergedEntries::new(&entry) {
            cancel.check()?;
            let relative = candidate.path.strip_prefix(&root).map_err(|_| {
                ToolError::internal("walk snapshot contains a path outside its root")
            })?;
            if !include.is_match(candidate.path) || excludes.is_match(relative, candidate.is_dir) {
                continue;
            }
            let relative_text = relative.as_os_str().to_string_lossy();
            let rendered = render_relative(&relative_text, candidate.is_dir);

            total_matches = total_matches.saturating_add(1);
            if total_matches > limit {
                continue;
            }
            if output_exhausted {
                output_limit_reached = true;
                continue;
            }

            let separator_bytes = usize::from(!paths.is_empty());
            let added_bytes = separator_bytes.saturating_add(rendered.len());
            if added_bytes > MAX_FIND_OUTPUT_BYTES.saturating_sub(retained_bytes) {
                // Keep the first complete line that crosses Pi's presentation
                // budget. The custom-operation wrapper can then observe a
                // >50 KiB string and emit Pi's standard truncation warning,
                // while native retention stays bounded to one path of headroom.
                output_exhausted = true;
                output_limit_reached = true;
                retained_bytes = retained_bytes.saturating_add(added_bytes);
                paths.push(rendered.into_owned());
                continue;
            }
            retained_bytes += added_bytes;
            paths.push(rendered.into_owned());
        }
        cancel.check()?;

        let limit_reached = total_matches > limit;
        hearth_core::profiler::count("tool.find.matches", total_matches);
        hearth_core::profiler::count("tool.find.paths_returned", paths.len() as u64);

        Ok(FindResult {
            paths,
            total_matches,
            walk_cache_hit,
            limit_reached,
            output_limit_reached,
            root: root.as_os_str().to_string_lossy().into_owned(),
        })
    })
}

fn validate_params(params: &FindParams) -> ToolResult<()> {
    if params.pattern.len() > MAX_PATTERN_BYTES {
        return Err(ToolError::invalid("find pattern exceeds 4 KiB"));
    }
    if params.limit.is_some_and(|limit| limit > MAX_FIND_LIMIT) {
        return Err(ToolError::invalid("find result limit exceeds 1000000"));
    }
    if params.exclude_globs.len() > MAX_EXCLUDE_GLOBS {
        return Err(ToolError::invalid(
            "find accepts at most 128 exclusion globs",
        ));
    }
    let exclude_bytes = params.exclude_globs.iter().try_fold(0usize, |sum, glob| {
        sum.checked_add(glob.len())
            .ok_or_else(|| ToolError::invalid("find exclusion globs exceed 16 KiB"))
    })?;
    if exclude_bytes > MAX_EXCLUDE_GLOB_BYTES {
        return Err(ToolError::invalid("find exclusion globs exceed 16 KiB"));
    }
    Ok(())
}

/// Make the cache key and reported root absolute without resolving symlinks.
fn lexical_absolute(engine: &Engine, path: &str) -> ToolResult<PathBuf> {
    let resolved = resolve_path(engine, path);
    let absolute = if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()
            .map_err(ToolError::from)?
            .join(resolved)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // An absolute path cannot escape its root; `pop` on the root is
                // a no-op. This is lexical normalization, not canonicalization.
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn build_glob(pattern: &str, label: &str, case_insensitive: bool) -> ToolResult<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|error| ToolError::invalid(format!("invalid {label} glob {pattern:?}: {error}")))
}

fn compile_glob(pattern: &str, label: &str, case_insensitive: bool) -> ToolResult<GlobMatcher> {
    build_glob(pattern, label, case_insensitive).map(|glob| glob.compile_matcher())
}

enum IncludeMatcher {
    Everything,
    Basename(GlobMatcher),
    FullPath(GlobMatcher),
}

impl IncludeMatcher {
    fn new(pattern: &str) -> ToolResult<Self> {
        if pattern.is_empty() {
            return Ok(Self::Everything);
        }
        // fd's default is smart-case: an all-lowercase pattern ignores case,
        // while any uppercase character makes the match case-sensitive.
        let case_insensitive = !pattern.chars().any(char::is_uppercase);
        if !pattern.contains('/') {
            return compile_glob(pattern, "find", case_insensitive).map(Self::Basename);
        }
        let effective = if pattern.starts_with('/') || pattern.starts_with("**/") {
            Cow::Borrowed(pattern)
        } else {
            Cow::Owned(format!("**/{pattern}"))
        };
        compile_glob(&effective, "find", case_insensitive).map(Self::FullPath)
    }

    fn is_match(&self, path: &Path) -> bool {
        match self {
            Self::Everything => true,
            Self::Basename(matcher) => path
                .file_name()
                .is_some_and(|name| matcher.is_match(Path::new(name))),
            Self::FullPath(matcher) => matcher.is_match(path),
        }
    }
}

struct ExcludeMatchers {
    basenames: GlobSet,
    paths: GlobSet,
}

impl ExcludeMatchers {
    fn new(patterns: &[String]) -> ToolResult<Self> {
        let mut basenames = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = build_glob(pattern, "exclusion", false)?;
            if pattern.contains('/') {
                paths.add(glob);
            } else {
                basenames.add(glob);
            }
        }
        let build = |builder: GlobSetBuilder| {
            builder
                .build()
                .map_err(|error| ToolError::invalid(format!("invalid exclusion globs: {error}")))
        };
        Ok(Self {
            basenames: build(basenames)?,
            paths: build(paths)?,
        })
    }

    fn is_match(&self, relative: &Path, is_dir: bool) -> bool {
        if relative
            .file_name()
            .is_some_and(|name| self.basenames.is_match(Path::new(name)))
        {
            return true;
        }
        if !is_dir {
            return self.paths.is_match(relative);
        }
        let relative_text = relative.as_os_str().to_string_lossy();
        self.paths
            .is_match(render_relative(&relative_text, true).as_ref())
    }
}

fn render_relative<'a>(relative: &'a Cow<'a, str>, is_dir: bool) -> Cow<'a, str> {
    if is_dir {
        let mut rendered = String::with_capacity(relative.len() + 1);
        rendered.push_str(relative);
        rendered.push('/');
        Cow::Owned(rendered)
    } else {
        Cow::Borrowed(relative)
    }
}

#[derive(Clone, Copy)]
struct Candidate<'a> {
    path: &'a Path,
    is_dir: bool,
}

/// Allocation-free three-way merge over the path-sorted walk slices.
struct MergedEntries<'a> {
    entry: &'a WalkEntry,
    file: usize,
    directory: usize,
    symlink: usize,
}

impl<'a> MergedEntries<'a> {
    fn new(entry: &'a WalkEntry) -> Self {
        Self {
            entry,
            file: 0,
            directory: 0,
            symlink: 0,
        }
    }
}

impl<'a> Iterator for MergedEntries<'a> {
    type Item = Candidate<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let candidates = [
            self.entry
                .files
                .get(self.file)
                .map(|path| (path.as_path(), false, 0u8)),
            self.entry
                .directories
                .get(self.directory)
                .map(|path| (path.as_path(), true, 1u8)),
            self.entry
                .symlinks
                .get(self.symlink)
                .map(|path| (path.as_path(), false, 2u8)),
        ];
        let (path, is_dir, source) = candidates.into_iter().flatten().min_by(
            |(left_path, _, left_source), (right_path, _, right_source)| {
                left_path
                    .cmp(right_path)
                    .then_with(|| left_source.cmp(right_source))
            },
        )?;
        match source {
            0 => self.file += 1,
            1 => self.directory += 1,
            _ => self.symlink += 1,
        }
        Some(Candidate { path, is_dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_full_path_transform_keeps_literal_separator() {
        let matcher = IncludeMatcher::new("src/*.rs").unwrap();
        assert!(matcher.is_match(Path::new("/tmp/work/src/a.rs")));
        assert!(!matcher.is_match(Path::new("/tmp/work/src/nested/a.rs")));
    }

    #[test]
    fn lexical_normalization_does_not_touch_normal_components() {
        let engine = Engine::new(hearth_core::EngineConfig {
            default_cwd: PathBuf::from("relative/base"),
            enable_optimizer: false,
            ..hearth_core::EngineConfig::default()
        });
        let normalized = lexical_absolute(&engine, "a/../b").unwrap();
        assert!(normalized.is_absolute());
        assert!(normalized.ends_with("relative/base/b"));
    }
}
