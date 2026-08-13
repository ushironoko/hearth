//! JavaScript and TypeScript resolution through `oxc_resolver`.

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU32, Ordering};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, io,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use compact_str::CompactString;
use oxc_resolver::{
    FileSystem, FileSystemOs, ResolveContext, ResolveError, ResolveOptions, ResolverGeneric,
    TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use parking_lot::Mutex;
use serde_json::Value;

use super::{
    FailedKind, ResolutionCompleteness, ResolutionOutcome, Resolve, Resolved, UnresolvedReason,
};
use crate::imports::{ImportKind, RawImport};

const MAX_TSCONFIG_BYTES: usize = 1024 * 1024;
const MAX_RESOLVER_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TSCONFIG_EXTENDS_ENTRIES: usize = 32;
const MAX_TSCONFIG_EXTENDS_VISITS: usize = 256;
const MAX_RESOLUTION_MEMO_ENTRIES: usize = 65_536;

/// Configuration for JavaScript and TypeScript module resolution.
#[derive(Debug, Clone)]
pub struct JsResolveOptions {
    /// An optional manually selected tsconfig-format file, such as
    /// `tsconfig.json` or `jsconfig.json`.
    pub tsconfig: Option<PathBuf>,
    /// Conditions accepted while resolving package `exports`.
    pub condition_names: Vec<String>,
    /// File extensions probed in priority order.
    pub extensions: Vec<String>,
}

impl Default for JsResolveOptions {
    fn default() -> Self {
        Self {
            tsconfig: None,
            // These family conditions are split between the import and require
            // resolvers by `resolver_options`.
            condition_names: vec!["import".into(), "require".into()],
            extensions: vec![
                ".ts".into(),
                ".tsx".into(),
                ".mts".into(),
                ".cts".into(),
                ".js".into(),
                ".jsx".into(),
                ".mjs".into(),
                ".cjs".into(),
                ".vue".into(),
            ],
        }
    }
}

/// Build a JavaScript resolver backed by the operating system filesystem.
pub fn js_resolver(options: JsResolveOptions) -> Box<dyn Resolve> {
    build_js_resolver(FileSystemOs::new(), options)
}

/// Build a JavaScript resolver backed by an injected filesystem.
pub fn js_resolver_with_fs<FS: FileSystem + 'static>(
    fs: FS,
    options: JsResolveOptions,
) -> Box<dyn Resolve> {
    build_js_resolver(fs, options)
}

fn build_js_resolver<FS: FileSystem + 'static>(
    fs: FS,
    options: JsResolveOptions,
) -> Box<dyn Resolve> {
    let (import_options, require_options, configured_tsconfig) = resolver_options(options);
    let file_system = SharedFileSystem::from_file_system(fs);
    let import_resolver =
        ResolverGeneric::new_with_file_system(file_system.clone(), import_options);
    let require_resolver = import_resolver.clone_with_options(require_options);
    Box::new(JsResolver {
        import_resolver,
        require_resolver,
        file_system,
        configured_tsconfig,
        dependency_memo: Mutex::new(HashMap::new()),
        #[cfg(debug_assertions)]
        in_flight: AtomicU32::new(0),
    })
}

struct JsResolver {
    import_resolver: ResolverGeneric<SharedFileSystem>,
    require_resolver: ResolverGeneric<SharedFileSystem>,
    file_system: SharedFileSystem,
    configured_tsconfig: Option<PathBuf>,
    dependency_memo: Mutex<HashMap<ResolutionMemoKey, Vec<CompactString>>>,
    #[cfg(debug_assertions)]
    in_flight: AtomicU32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolutionMemoKey {
    from_dir: PathBuf,
    specifier: CompactString,
    kind: ImportKind,
}

impl Resolve for JsResolver {
    fn resolve(&self, from_file: &str, import: &RawImport) -> ResolutionOutcome {
        #[cfg(debug_assertions)]
        let _in_flight = InFlightResolve::enter(&self.in_flight);

        if matches!(import.kind, ImportKind::RustUse | ImportKind::RustMod) {
            return unresolved(UnresolvedReason::Unsupported, Vec::new(), Vec::new());
        }

        let from_path = Path::new(from_file);
        debug_assert!(
            from_path.is_absolute(),
            "from_file must be absolute: {from_file}"
        );
        if !from_path.is_absolute() {
            return unresolved(
                failed(FailedKind::InvalidSpecifier, "from_file must be absolute"),
                Vec::new(),
                Vec::new(),
            );
        }

        let Some(parent) = from_path.parent() else {
            return unresolved(
                failed(
                    FailedKind::InvalidSpecifier,
                    "from_file must have a parent directory",
                ),
                Vec::new(),
                Vec::new(),
            );
        };
        let memo_key = ResolutionMemoKey {
            from_dir: parent.to_path_buf(),
            specifier: import.specifier.clone(),
            kind: import.kind,
        };
        let resolver = self.resolver_for(import.kind);

        let mut dependency_paths: Vec<PathBuf> = self.configured_tsconfig.iter().cloned().collect();
        let mut notes = Vec::new();
        let mut tsconfig_tracking_truncated = false;
        let discovered = match &self.configured_tsconfig {
            Some(configured) => resolver.find_tsconfig(configured),
            None => resolver.find_tsconfig(from_path),
        };
        let tsconfig = match discovered {
            Ok(tsconfig) => tsconfig,
            Err(error) => {
                if let Some(configured_tsconfig) = &self.configured_tsconfig {
                    let tracking = self.track_tsconfig_chain(configured_tsconfig);
                    tsconfig_tracking_truncated |= tracking.truncated;
                    dependency_paths.extend(tracking.dependencies);
                    notes.extend(tracking.notes);
                }
                dependency_paths.extend(error_dependency_paths(&error));
                let mut outcome = unresolved(
                    classify_error(error),
                    collect_dependencies(ResolveContext::default(), dependency_paths),
                    notes,
                );
                if tsconfig_tracking_truncated {
                    outcome.completeness = ResolutionCompleteness::Partial;
                }
                return self.replay_dependencies(memo_key, outcome);
            }
        };
        if let Some(tsconfig) = &tsconfig {
            let tracking = self.track_tsconfig_chain(tsconfig.path());
            tsconfig_tracking_truncated |= tracking.truncated;
            dependency_paths.extend(tracking.dependencies);
            notes.extend(tracking.notes);
        }

        let mut context = ResolveContext::default();
        let resolution = resolver.resolve_with_context(
            parent,
            import.specifier.as_str(),
            tsconfig.as_deref(),
            &mut context,
        );

        let mut outcome = match resolution {
            Ok(resolution) => {
                let package_json = resolution.package_json();
                dependency_paths
                    .extend(package_json.map(|package_json| package_json.path().to_path_buf()));
                ResolutionOutcome {
                    resolved: classify_resolution(
                        import.specifier.as_str(),
                        resolution.path(),
                        package_json.and_then(|package_json| package_json.name()),
                    ),
                    dependencies: collect_dependencies(context, dependency_paths),
                    notes,
                    completeness: ResolutionCompleteness::Complete,
                }
            }
            Err(error) => {
                dependency_paths.extend(error_dependency_paths(&error));
                unresolved(
                    classify_error(error),
                    collect_dependencies(context, dependency_paths),
                    notes,
                )
            }
        };
        if tsconfig_tracking_truncated {
            outcome.completeness = ResolutionCompleteness::Partial;
        }
        self.replay_dependencies(memo_key, outcome)
    }

    fn clear_cache(&self) {
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            self.in_flight.load(Ordering::Acquire),
            0,
            "clear_cache must not overlap an in-flight resolve"
        );
        self.import_resolver.clear_cache();
        self.require_resolver.clear_cache();
        self.dependency_memo.lock().clear();
    }
}

impl JsResolver {
    fn resolver_for(&self, kind: ImportKind) -> &ResolverGeneric<SharedFileSystem> {
        match kind {
            ImportKind::CommonJs | ImportKind::TsImportRequire => &self.require_resolver,
            _ => &self.import_resolver,
        }
    }

    fn replay_dependencies(
        &self,
        key: ResolutionMemoKey,
        mut outcome: ResolutionOutcome,
    ) -> ResolutionOutcome {
        let mut memo = self.dependency_memo.lock();
        if let Some(dependencies) = memo.get(&key) {
            outcome.dependencies.extend(dependencies.iter().cloned());
        }
        normalize_dependencies(&mut outcome.dependencies);
        if memo.len() >= MAX_RESOLUTION_MEMO_ENTRIES && !memo.contains_key(&key) {
            memo.clear();
        }
        memo.insert(key, outcome.dependencies.clone());
        outcome
    }

    fn track_tsconfig_chain(&self, leaf: &Path) -> TsconfigTracking {
        let extends_resolver = self
            .import_resolver
            .clone_with_options(tsconfig_extends_options());
        let mut tracking = TsconfigTracking::default();
        let mut pending = VecDeque::from([(absolute_path(leaf), Vec::new())]);
        let mut visited = HashSet::new();

        while let Some((config_path, mut ancestry)) = pending.pop_front() {
            if ancestry.contains(&config_path) {
                tracking.notes.push(
                    format!(
                        "tsconfig extends cycle while tracking dependencies: {}",
                        config_path.display()
                    )
                    .into(),
                );
                continue;
            }
            if visited.contains(&config_path) {
                continue;
            }
            if visited.len() == MAX_TSCONFIG_EXTENDS_VISITS {
                tracking.truncated = true;
                tracking.notes.push(
                    format!(
                        "tsconfig extends visit budget of {MAX_TSCONFIG_EXTENDS_VISITS} configs \
                         exhausted while tracking dependencies; {} configs remain pending",
                        pending.len() + 1
                    )
                    .into(),
                );
                break;
            }
            visited.insert(config_path.clone());
            ancestry.push(config_path.clone());
            tracking.dependencies.push(config_path.clone());

            let mut source = match self.file_system.read_to_string(&config_path) {
                Ok(source) => source,
                Err(error) => {
                    tracking.notes.push(
                        format!(
                            "could not read tsconfig extends from {}: {error}",
                            config_path.display()
                        )
                        .into(),
                    );
                    continue;
                }
            };
            if source.len() > MAX_TSCONFIG_BYTES {
                tracking.truncated = true;
                tracking.notes.push(
                    format!(
                        "tsconfig extends file {} exceeds the size limit of \
                         {MAX_TSCONFIG_BYTES} bytes ({} bytes)",
                        config_path.display(),
                        source.len()
                    )
                    .into(),
                );
                continue;
            }
            if let Err(error) = json_strip_comments::strip(&mut source) {
                tracking.notes.push(
                    format!(
                        "could not strip JSONC syntax from tsconfig extends in {}: {error}",
                        config_path.display()
                    )
                    .into(),
                );
                continue;
            }
            let value: Value = match serde_json::from_str(&source) {
                Ok(value) => value,
                Err(error) => {
                    tracking.notes.push(
                        format!(
                            "could not parse tsconfig extends from {}: {error}",
                            config_path.display()
                        )
                        .into(),
                    );
                    continue;
                }
            };
            let specifiers = match extends_specifiers(&value) {
                Ok(specifiers) => specifiers,
                Err(detail) => {
                    tracking.notes.push(
                        format!(
                            "invalid tsconfig extends in {}: {detail}",
                            config_path.display()
                        )
                        .into(),
                    );
                    continue;
                }
            };
            let mut specifiers = specifiers;
            if specifiers.len() > MAX_TSCONFIG_EXTENDS_ENTRIES {
                tracking.truncated = true;
                tracking.notes.push(
                    format!(
                        "tsconfig extends entry limit of {MAX_TSCONFIG_EXTENDS_ENTRIES} exceeded \
                         in {}; only the first {MAX_TSCONFIG_EXTENDS_ENTRIES} of {} entries were \
                         tracked",
                        config_path.display(),
                        specifiers.len()
                    )
                    .into(),
                );
                specifiers.truncate(MAX_TSCONFIG_EXTENDS_ENTRIES);
            }
            if specifiers.is_empty() {
                continue;
            }
            let Some(directory) = config_path.parent() else {
                tracking.notes.push(
                    format!(
                        "tsconfig has no parent directory while tracking extends: {}",
                        config_path.display()
                    )
                    .into(),
                );
                continue;
            };
            for specifier in specifiers {
                let package_style = is_package_style_extends(&specifier);
                let target_path =
                    (!package_style).then(|| extends_target_path(directory, &specifier));
                let absolute_specifier = target_path.as_deref().map(Path::to_string_lossy);
                let resolution_specifier =
                    absolute_specifier.as_deref().unwrap_or(specifier.as_str());
                let mut context = ResolveContext::default();
                let resolution = extends_resolver.resolve_with_context(
                    directory,
                    resolution_specifier,
                    None,
                    &mut context,
                );
                if !package_style {
                    tracking.dependencies.extend(context.file_dependencies);
                    tracking.dependencies.extend(context.missing_dependencies);
                }

                match resolution {
                    Ok(resolution) => {
                        tracking.dependencies.extend(
                            resolution
                                .package_json()
                                .map(|package_json| package_json.path().to_path_buf()),
                        );
                        pending.push_back((absolute_path(resolution.path()), ancestry.clone()));
                    }
                    Err(error) => {
                        tracking.dependencies.extend(target_path);
                        let kind = if package_style { "package-style " } else { "" };
                        tracking.notes.push(
                            format!(
                                "{kind}tsconfig extends {specifier:?} from {} could not be resolved: {error}",
                                config_path.display()
                            )
                            .into(),
                        );
                    }
                }
            }
        }

        tracking
    }
}

fn resolver_options(
    options: JsResolveOptions,
) -> (ResolveOptions, ResolveOptions, Option<PathBuf>) {
    let JsResolveOptions {
        tsconfig,
        condition_names,
        extensions,
    } = options;
    let configured_tsconfig = tsconfig.map(|path| absolute_path(&path));
    let tsconfig = configured_tsconfig.clone().map(|config_file| {
        TsconfigDiscovery::Manual(TsconfigOptions {
            config_file,
            references: TsconfigReferences::Disabled,
        })
    });
    let common_conditions: Vec<String> = condition_names
        .into_iter()
        .filter(|condition| condition != "import" && condition != "require")
        .collect();
    let import_options = ResolveOptions {
        tsconfig: tsconfig.clone(),
        condition_names: family_conditions("import", &common_conditions),
        extensions: extensions.clone(),
        ..ResolveOptions::default()
    };
    let require_options = ResolveOptions {
        tsconfig,
        condition_names: family_conditions("require", &common_conditions),
        extensions,
        ..ResolveOptions::default()
    };
    (import_options, require_options, configured_tsconfig)
}

fn family_conditions(family: &str, common: &[String]) -> Vec<String> {
    std::iter::once(family.to_owned())
        .chain(common.iter().cloned())
        .collect()
}

fn tsconfig_extends_options() -> ResolveOptions {
    ResolveOptions {
        tsconfig: None,
        condition_names: vec!["node".into(), "import".into()],
        extensions: vec![".json".into()],
        main_files: vec!["tsconfig".into()],
        ..ResolveOptions::default()
    }
}

fn collect_dependencies(context: ResolveContext, additional: Vec<PathBuf>) -> Vec<CompactString> {
    let mut dependencies: Vec<CompactString> = context
        .file_dependencies
        .into_iter()
        .chain(context.missing_dependencies)
        .chain(additional)
        .map(|path| absolute_path(&path))
        .map(|path| path_string(&path))
        .collect();
    dependencies.sort_unstable();
    dependencies.dedup();
    dependencies
}

fn extends_specifiers(value: &Value) -> Result<Vec<String>, &'static str> {
    match value.get("extends") {
        None => Ok(Vec::new()),
        Some(Value::String(specifier)) => Ok(vec![specifier.clone()]),
        Some(Value::Array(specifiers)) => specifiers
            .iter()
            .map(|specifier| {
                specifier
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("extends array entries must be strings")
            })
            .collect(),
        Some(_) => Err("extends must be a string or an array of strings"),
    }
}

fn is_package_style_extends(specifier: &str) -> bool {
    !Path::new(specifier).is_absolute() && !specifier.starts_with('.')
}

fn extends_target_path(directory: &Path, specifier: &str) -> PathBuf {
    let target = Path::new(specifier);
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        normalize_path(&absolute_path(&directory.join(target)))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn classify_error(error: ResolveError) -> UnresolvedReason {
    if is_not_found_error(&error) {
        UnresolvedReason::NotFound
    } else {
        let kind = match &error {
            ResolveError::TsconfigNotFound(_)
            | ResolveError::TsconfigSelfReference(_)
            | ResolveError::TsconfigCircularExtend(_)
            | ResolveError::TsconfigLoadFailed { .. }
            | ResolveError::Json(_)
            | ResolveError::InvalidPackageTarget(_, _, _)
            | ResolveError::InvalidPackageConfig(_)
            | ResolveError::InvalidPackageConfigDefault(_)
            | ResolveError::InvalidPackageConfigDirectory(_) => FailedKind::Config,
            ResolveError::IOError(_) => FailedKind::Io,
            ResolveError::PathNotSupported(_)
            | ResolveError::Specifier(_)
            | ResolveError::InvalidModuleSpecifier(_, _) => FailedKind::InvalidSpecifier,
            _ => FailedKind::Other,
        };
        failed(kind, error.to_string())
    }
}

fn is_not_found_error(error: &ResolveError) -> bool {
    matches!(
        error,
        ResolveError::NotFound(_)
            | ResolveError::MatchedAliasNotFound(_, _)
            | ResolveError::ExtensionAlias(_, _, _)
    )
}

fn error_dependency_paths(error: &ResolveError) -> Vec<PathBuf> {
    match error {
        ResolveError::TsconfigLoadFailed { path, source } => {
            let mut paths = vec![path.clone()];
            paths.extend(error_dependency_paths(source));
            paths
        }
        ResolveError::TsconfigCircularExtend(paths) => paths.paths().to_vec(),
        ResolveError::Json(error) => vec![error.path.clone()],
        ResolveError::InvalidModuleSpecifier(_, path)
        | ResolveError::InvalidPackageTarget(_, _, path)
        | ResolveError::InvalidPackageConfig(path)
        | ResolveError::InvalidPackageConfigDefault(path)
        | ResolveError::InvalidPackageConfigDirectory(path)
        | ResolveError::PackageImportNotDefined(_, path) => vec![path.clone()],
        ResolveError::PackagePathNotExported {
            package_json_path, ..
        } => vec![package_json_path.clone()],
        _ => Vec::new(),
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

fn path_string(path: &Path) -> CompactString {
    CompactString::from(path.to_string_lossy().as_ref())
}

fn is_node_modules_path(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(name) if name == "node_modules"))
}

fn is_path_specifier(specifier: &str) -> bool {
    specifier.starts_with("./")
        || specifier.starts_with("../")
        || Path::new(specifier).is_absolute()
}

fn classify_resolution(specifier: &str, path: &Path, manifest_name: Option<&str>) -> Resolved {
    let path = absolute_path(path);
    if is_path_specifier(specifier) || !is_node_modules_path(&path) {
        Resolved::Path(path_string(&path))
    } else {
        // Aliased specifiers (`#dep`) say nothing about the installed package,
        // so a missing manifest name falls back to the directory name under
        // the last node_modules component before the specifier text.
        let name = manifest_name
            .map(CompactString::from)
            .or_else(|| package_name_from_path(&path))
            .unwrap_or_else(|| package_name(specifier));
        Resolved::External(name)
    }
}

fn package_name_from_path(path: &Path) -> Option<CompactString> {
    let components: Vec<&str> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    let base = components
        .iter()
        .rposition(|name| *name == "node_modules")?;
    let first = components.get(base + 1)?;
    if first.starts_with('@') {
        let second = components.get(base + 2)?;
        Some(CompactString::from(format!("{first}/{second}")))
    } else {
        Some(CompactString::from(*first))
    }
}

fn package_name(specifier: &str) -> CompactString {
    let segment_count = usize::from(specifier.starts_with('@')) + 1;
    CompactString::from(
        specifier
            .split('/')
            .take(segment_count)
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn unresolved(
    reason: UnresolvedReason,
    dependencies: Vec<CompactString>,
    notes: Vec<CompactString>,
) -> ResolutionOutcome {
    let completeness = if matches!(&reason, UnresolvedReason::Failed { .. }) {
        ResolutionCompleteness::Partial
    } else {
        ResolutionCompleteness::Complete
    };
    ResolutionOutcome {
        resolved: Resolved::Unresolved(reason),
        dependencies,
        notes,
        completeness,
    }
}

fn failed(kind: FailedKind, detail: impl Into<CompactString>) -> UnresolvedReason {
    UnresolvedReason::Failed {
        kind,
        detail: detail.into(),
    }
}

fn normalize_dependencies(dependencies: &mut Vec<CompactString>) {
    dependencies.sort_unstable();
    dependencies.dedup();
}

#[derive(Default)]
struct TsconfigTracking {
    dependencies: Vec<PathBuf>,
    notes: Vec<CompactString>,
    truncated: bool,
}

#[derive(Clone)]
struct SharedFileSystem(Arc<dyn FileSystem>);

impl SharedFileSystem {
    fn from_file_system(file_system: impl FileSystem + 'static) -> Self {
        Self(Arc::new(file_system))
    }
}

impl FileSystem for SharedFileSystem {
    fn new() -> Self {
        Self::from_file_system(FileSystemOs::new())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_RESOLVER_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resolver config must be a regular file no larger than 1 MiB",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(MAX_RESOLVER_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RESOLVER_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resolver config exceeds 1 MiB",
            ));
        }
        Ok(bytes)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn metadata(&self, path: &Path) -> io::Result<oxc_resolver::FileMetadata> {
        self.0.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<oxc_resolver::FileMetadata> {
        self.0.symlink_metadata(path)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        self.0.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.0.canonicalize(path)
    }
}

#[cfg(debug_assertions)]
struct InFlightResolve<'a> {
    counter: &'a AtomicU32,
}

#[cfg(debug_assertions)]
impl<'a> InFlightResolve<'a> {
    fn enter(counter: &'a AtomicU32) -> Self {
        let previous = counter.fetch_add(1, Ordering::AcqRel);
        debug_assert_ne!(previous, u32::MAX, "in-flight resolve counter overflowed");
        Self { counter }
    }
}

#[cfg(debug_assertions)]
impl Drop for InFlightResolve<'_> {
    fn drop(&mut self) {
        let previous = self.counter.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "in-flight resolve counter underflowed");
    }
}
