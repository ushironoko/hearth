//! Best-effort filesystem watching that proactively invalidates the caches.
//!
//! Correctness never depends on this in the default configuration: [`FileCache`]
//! still stat-validates every hit. But when the engine is put in `trust_watch`
//! mode, warm reads/greps skip even that stat and rely on the watcher to have
//! invalidated stale entries — so the watcher tracks a `healthy` flag and any
//! backend error flips the engine back to strict stat validation.

use crate::cache::{FileCache, WalkCache};
use crate::invalidation::InvalidationLog;
#[cfg(not(any(test, feature = "test-poll-watcher")))]
use notify::RecommendedWatcher;
use notify::event::ModifyKind;
#[cfg(any(test, feature = "test-poll-watcher"))]
use notify::{Config, PollWatcher};
use notify::{Event, EventHandler, EventKind, RecursiveMode, Watcher};
use parking_lot::RwLock;
use std::collections::{HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(test, feature = "test-poll-watcher"))]
use std::time::Duration;

const MAX_EXPANDED_PATHS: usize = 64;

#[cfg(not(any(test, feature = "test-poll-watcher")))]
type WatcherBackend = RecommendedWatcher;
#[cfg(any(test, feature = "test-poll-watcher"))]
type WatcherBackend = PollWatcher;

#[derive(Clone, Debug, Eq, PartialEq)]
struct WatchedTree {
    canonical_root: PathBuf,
    spellings: Vec<PathBuf>,
    conservative: bool,
}

/// Owns the OS watcher; dropping it stops the background thread.
pub struct WatchHandle {
    watcher: WatcherBackend,
    healthy: Arc<AtomicBool>,
    watched: HashSet<PathBuf>,
    watched_trees: Arc<RwLock<Vec<WatchedTree>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventEffect {
    invalidate_content: bool,
    invalidate_walk: bool,
    record: bool,
}

#[derive(Debug, Eq, PartialEq)]
enum EventPathExpansion {
    Resolved(HashSet<PathBuf>),
    Conservative(Vec<PathBuf>),
}

fn event_effect(kind: &EventKind) -> EventEffect {
    // Metadata-only events (for example atime bumps caused by our own reads)
    // must not trigger a read -> atime -> invalidate feedback loop.
    let invalidate_content = matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Name(_))
    );
    let invalidate_walk = matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    EventEffect {
        invalidate_content,
        invalidate_walk,
        record: invalidate_content || invalidate_walk,
    }
}

#[cfg(test)]
fn normalize_root(root: &Path) -> PathBuf {
    resolve_root(root).0
}

#[cfg(not(any(test, feature = "test-poll-watcher")))]
fn new_watcher(event_handler: impl EventHandler) -> Result<WatcherBackend, notify::Error> {
    notify::recommended_watcher(event_handler)
}

#[cfg(any(test, feature = "test-poll-watcher"))]
fn new_watcher(event_handler: impl EventHandler) -> Result<WatcherBackend, notify::Error> {
    // Polling is a real watcher backend and keeps callback integration tests
    // deterministic on temporary filesystems where native event delivery can
    // be unavailable or indefinitely delayed.
    PollWatcher::new(
        event_handler,
        Config::default()
            .with_poll_interval(Duration::from_millis(25))
            .with_compare_contents(true),
    )
}

fn resolve_root(root: &Path) -> (PathBuf, bool) {
    match std::fs::canonicalize(root) {
        Ok(canonical) => (canonical, false),
        Err(_) => (lexical_normalize(root), true),
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir) => {}
                Some(Component::ParentDir) | Some(Component::Prefix(_)) | None => {
                    normalized.push(component.as_os_str());
                }
                Some(Component::CurDir) => unreachable!("current-directory components are dropped"),
            },
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn register_watched_tree(
    watched_trees: &RwLock<Vec<WatchedTree>>,
    original: PathBuf,
    canonical: PathBuf,
    conservative: bool,
) {
    let mut watched_trees = watched_trees.write();
    if let Some(tree) = watched_trees.iter_mut().find(|tree| {
        tree.canonical_root == canonical
            && tree.conservative == conservative
            && (!conservative || tree.spellings.contains(&original))
    }) {
        push_unique_path(&mut tree.spellings, original);
        return;
    }

    watched_trees.push(WatchedTree {
        canonical_root: canonical,
        spellings: vec![original],
        conservative,
    });
}

fn canonicalize_event_path(path: &Path) -> Result<PathBuf, PathBuf> {
    let mut ancestor = path;

    loop {
        if ancestor.as_os_str().is_empty() {
            return Err(lexical_normalize(path));
        }
        if let Ok(canonical_ancestor) = std::fs::canonicalize(ancestor) {
            let suffix = path
                .strip_prefix(ancestor)
                .expect("ancestor is derived from the raw event path");
            if suffix
                .components()
                .any(|component| component == Component::ParentDir)
            {
                return Err(canonical_ancestor);
            }
            return Ok(if suffix.as_os_str().is_empty() {
                canonical_ancestor
            } else {
                canonical_ancestor.join(suffix)
            });
        }

        let Some(parent) = ancestor.parent() else {
            return Err(lexical_normalize(path));
        };
        if parent == ancestor {
            return Err(lexical_normalize(path));
        }
        ancestor = parent;
    }
}

fn tree_matches_path(tree: &WatchedTree, path: &Path) -> bool {
    path.starts_with(&tree.canonical_root)
        || tree
            .spellings
            .iter()
            .any(|spelling| path.starts_with(spelling))
}

fn mark_matching_trees(
    path: &Path,
    watched_trees: &[WatchedTree],
    matched_trees: &mut HashSet<usize>,
) -> bool {
    let mut matched_conservative = false;
    for (index, tree) in watched_trees.iter().enumerate() {
        if tree_matches_path(tree, path) {
            matched_trees.insert(index);
            matched_conservative |= tree.conservative;
        }
    }
    matched_conservative
}

fn rewrite_once(
    path: &Path,
    watched_trees: &[WatchedTree],
    matched_trees: &mut HashSet<usize>,
) -> (Vec<PathBuf>, bool) {
    let mut rewritten = Vec::new();
    let mut matched_conservative = false;

    for (index, tree) in watched_trees.iter().enumerate() {
        if !tree_matches_path(tree, path) {
            continue;
        }
        matched_trees.insert(index);
        if tree.conservative {
            matched_conservative = true;
            continue;
        }
        if let Ok(suffix) = path.strip_prefix(&tree.canonical_root) {
            for spelling in &tree.spellings {
                push_unique_path(&mut rewritten, spelling.join(suffix));
            }
        }
        for spelling in &tree.spellings {
            let Ok(suffix) = path.strip_prefix(spelling) else {
                continue;
            };
            push_unique_path(&mut rewritten, tree.canonical_root.join(suffix));
        }
    }

    (rewritten, matched_conservative)
}

fn enqueue_expanded_path(
    expanded: &mut HashSet<PathBuf>,
    queue: &mut VecDeque<PathBuf>,
    path: PathBuf,
    cap: usize,
) -> Result<(), ()> {
    if expanded.contains(&path) {
        return Ok(());
    }
    if expanded.len() >= cap {
        return Err(());
    }
    expanded.insert(path.clone());
    queue.push_back(path);
    Ok(())
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn conservative_roots(
    watched_trees: &[WatchedTree],
    mut matched_trees: HashSet<usize>,
    fallback_roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in fallback_roots {
        push_unique_path(&mut roots, root);
    }
    for index in matched_trees.iter().copied() {
        let tree = &watched_trees[index];
        push_unique_path(&mut roots, tree.canonical_root.clone());
        for spelling in &tree.spellings {
            push_unique_path(&mut roots, spelling.clone());
        }
    }

    // Expansion can hit its path cap before a rewritten spelling is dequeued.
    // Close over overlapping registered roots so every tree reachable from an
    // already-matched equivalence is invalidated as a whole.
    loop {
        let newly_matched: Vec<usize> = watched_trees
            .iter()
            .enumerate()
            .filter(|(index, tree)| {
                !matched_trees.contains(index)
                    && std::iter::once(&tree.canonical_root)
                        .chain(tree.spellings.iter())
                        .any(|candidate| {
                            roots
                                .iter()
                                .any(|root| roots_overlap(candidate, root.as_path()))
                        })
            })
            .map(|(index, _)| index)
            .collect();
        if newly_matched.is_empty() {
            return roots;
        }
        for index in newly_matched {
            matched_trees.insert(index);
            let tree = &watched_trees[index];
            push_unique_path(&mut roots, tree.canonical_root.clone());
            for spelling in &tree.spellings {
                push_unique_path(&mut roots, spelling.clone());
            }
        }
    }
}

fn expand_event_paths_with_cap(
    path: &Path,
    watched_trees: &[WatchedTree],
    cap: usize,
) -> EventPathExpansion {
    let mut matched_trees = HashSet::new();
    let canonical = match canonicalize_event_path(path) {
        Ok(canonical) => canonical,
        Err(ancestor) => {
            mark_matching_trees(path, watched_trees, &mut matched_trees);
            mark_matching_trees(&ancestor, watched_trees, &mut matched_trees);
            return EventPathExpansion::Conservative(conservative_roots(
                watched_trees,
                matched_trees,
                [ancestor],
            ));
        }
    };

    let seeds = if canonical == path {
        vec![canonical]
    } else {
        vec![canonical, path.to_path_buf()]
    };
    let mut expanded = HashSet::new();
    let mut queue = VecDeque::new();
    for seed in &seeds {
        if enqueue_expanded_path(&mut expanded, &mut queue, seed.clone(), cap).is_err() {
            for seed in &seeds {
                mark_matching_trees(seed, watched_trees, &mut matched_trees);
            }
            return EventPathExpansion::Conservative(conservative_roots(
                watched_trees,
                matched_trees,
                seeds,
            ));
        }
    }

    while let Some(path) = queue.pop_front() {
        let (rewritten_paths, matched_conservative) =
            rewrite_once(&path, watched_trees, &mut matched_trees);
        if matched_conservative {
            return EventPathExpansion::Conservative(conservative_roots(
                watched_trees,
                matched_trees,
                [],
            ));
        }
        for rewritten in rewritten_paths {
            if enqueue_expanded_path(&mut expanded, &mut queue, rewritten, cap).is_err() {
                return EventPathExpansion::Conservative(conservative_roots(
                    watched_trees,
                    matched_trees,
                    [],
                ));
            }
        }
    }

    EventPathExpansion::Resolved(expanded)
}

#[cfg(test)]
fn expand_event_paths(path: &Path, watched_trees: &[WatchedTree]) -> EventPathExpansion {
    expand_event_paths_with_cap(path, watched_trees, MAX_EXPANDED_PATHS)
}

fn process_event_with_cap(
    effect: EventEffect,
    paths: &[PathBuf],
    watched_trees: &[WatchedTree],
    files: &FileCache,
    walks: &WalkCache,
    log: &InvalidationLog,
    cap: usize,
) {
    if !effect.invalidate_content && !effect.invalidate_walk && !effect.record {
        return;
    }

    let mut resolved_paths = HashSet::new();
    let mut conservative_roots = HashSet::new();
    for path in paths {
        match expand_event_paths_with_cap(path, watched_trees, cap) {
            EventPathExpansion::Resolved(spellings) => resolved_paths.extend(spellings),
            EventPathExpansion::Conservative(roots) => conservative_roots.extend(roots),
        }
    }

    if !conservative_roots.is_empty() {
        // A prefix invalidation cannot be expressed as exact-path log entries:
        // consumers would keep derived state for unlisted descendants. A wipe
        // makes every consumer discard derived state, matching the caches.
        log.record_wipe();
    }
    for root in &conservative_roots {
        files.invalidate_prefix(root);
        walks.invalidate_under(root);
    }
    for path in resolved_paths {
        if conservative_roots.iter().any(|root| path.starts_with(root)) {
            continue;
        }
        if effect.invalidate_content {
            files.invalidate(&path);
        }
        if effect.invalidate_walk {
            walks.invalidate_under(&path);
        }
        if effect.record {
            log.record(&path);
        }
    }
}

fn process_event(
    effect: EventEffect,
    paths: &[PathBuf],
    watched_trees: &[WatchedTree],
    files: &FileCache,
    walks: &WalkCache,
    log: &InvalidationLog,
) {
    process_event_with_cap(
        effect,
        paths,
        watched_trees,
        files,
        walks,
        log,
        MAX_EXPANDED_PATHS,
    );
}

impl WatchHandle {
    /// Start watching `root` recursively, routing events to the caches.
    pub fn start(
        root: &Path,
        files: Arc<FileCache>,
        walks: Arc<WalkCache>,
        log: Arc<InvalidationLog>,
    ) -> Result<Self, notify::Error> {
        let healthy = Arc::new(AtomicBool::new(true));
        let cb_healthy = Arc::clone(&healthy);
        let original = root.to_path_buf();
        let (canonical, conservative) = resolve_root(root);
        let watched_trees = Arc::new(RwLock::new(Vec::new()));
        register_watched_tree(
            watched_trees.as_ref(),
            original,
            canonical.clone(),
            conservative,
        );
        let cb_watched_trees = Arc::clone(&watched_trees);
        let mut watcher = new_watcher(move |res: Result<Event, notify::Error>| {
            let event = match res {
                Ok(e) => e,
                Err(_) => {
                    // A backend error means we may miss invalidations — no longer
                    // safe to skip stats. Fall back to strict mode permanently.
                    cb_healthy.store(false, Ordering::Relaxed);
                    return;
                }
            };
            let effect = event_effect(&event.kind);
            let watched_trees = cb_watched_trees.read();
            process_event(
                effect,
                &event.paths,
                watched_trees.as_slice(),
                files.as_ref(),
                walks.as_ref(),
                log.as_ref(),
            );
        })?;
        watcher.watch(&canonical, RecursiveMode::Recursive)?;
        let watched = HashSet::from([canonical]);
        Ok(Self {
            watcher,
            healthy,
            watched,
            watched_trees,
        })
    }

    /// Add another root to the same watcher.
    pub fn add_root(&mut self, root: &Path) -> Result<(), notify::Error> {
        // FsEventWatcher restarts its FSEvents stream "since now" on every watch()
        // call, so re-watching a root can drop events racing the restart. A genuinely
        // new root still requires a restart and can drop in-flight events from roots
        // already being watched.
        let original = root.to_path_buf();
        let (key, conservative) = resolve_root(root);
        // Even a covered subtree needs its alias recorded: callers may cache that
        // spelling although no additional backend watch is necessary.
        register_watched_tree(
            self.watched_trees.as_ref(),
            original,
            key.clone(),
            conservative,
        );
        if self.watched.iter().any(|watched| key.starts_with(watched)) {
            return Ok(());
        }
        self.watcher.watch(&key, RecursiveMode::Recursive)?;
        self.watched.insert(key);
        Ok(())
    }

    /// Whether the watcher has seen no backend errors (safe to skip stats).
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventEffect, EventPathExpansion, WatchHandle, WatchedTree, event_effect,
        expand_event_paths, expand_event_paths_with_cap, process_event, process_event_with_cap,
    };
    use crate::cache::{FileCache, WalkCache, WalkKey};
    use crate::invalidation::InvalidationLog;
    use notify::EventKind;
    use notify::event::{CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind, RenameMode};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn walk_key() -> WalkKey {
        WalkKey {
            respect_gitignore: false,
            hidden: true,
            follow_symlinks: false,
        }
    }

    fn wait_for_watcher_ready(log: &InvalidationLog, probe: &Path, accepted_paths: &[&Path]) {
        let readiness_deadline = Instant::now() + Duration::from_secs(30);
        for attempt in 0_u64.. {
            std::fs::write(probe, attempt.to_string()).unwrap();
            std::thread::sleep(Duration::from_millis(25));
            let delta = log.since(0);
            let ready = match &delta.paths {
                None => true,
                Some(paths) => accepted_paths
                    .iter()
                    .any(|accepted| paths.iter().any(|path| path == *accepted)),
            };
            if ready {
                return;
            }
            if Instant::now() >= readiness_deadline {
                panic!(
                    "watcher did not become ready after repeated writes to {probe:?}; \
                     last delta: {delta:?}"
                );
            }
        }
    }

    fn wait_for_recorded_paths_since(
        log: &InvalidationLog,
        last_seen: u64,
        expected_paths: &[&Path],
    ) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let delta = log.since(last_seen);
            let found_all = delta.paths.as_ref().is_some_and(|paths| {
                expected_paths
                    .iter()
                    .all(|expected| paths.iter().any(|path| path == *expected))
            });
            if found_all {
                return delta.revision;
            }
            if Instant::now() >= deadline {
                panic!(
                    "watch callback did not record every expected path {expected_paths:?}; \
                     last delta: {delta:?}"
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_recorded_paths(log: &InvalidationLog, expected_paths: &[&Path]) {
        wait_for_recorded_paths_since(log, 0, expected_paths);
    }

    fn wait_for_log_quiescence(log: &InvalidationLog, mut revision: u64) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stable_since = Instant::now();
        loop {
            let current = log.since(revision).revision;
            if current != revision {
                revision = current;
                stable_since = Instant::now();
            } else if stable_since.elapsed() >= Duration::from_millis(250) {
                return revision;
            }
            if Instant::now() >= deadline {
                panic!("invalidation log did not become quiescent at revision {revision}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn resolved_paths(path: &Path, watched_trees: &[WatchedTree]) -> HashSet<PathBuf> {
        match expand_event_paths(path, watched_trees) {
            EventPathExpansion::Resolved(paths) => paths.into_iter().collect(),
            EventPathExpansion::Conservative(roots) => {
                panic!("expected resolved paths, got conservative roots {roots:?}")
            }
        }
    }

    #[test]
    fn event_paths_expand_from_canonical_identity_component_wise() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = dir.path().canonicalize().unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias_root = alias_dir.path().canonicalize().unwrap().join("alias");
        let trees = [WatchedTree {
            canonical_root: canonical_root.clone(),
            spellings: vec![canonical_root.clone(), alias_root.clone()],
            conservative: false,
        }];
        let canonical_event = canonical_root.join("src/a.rs");

        assert_eq!(
            resolved_paths(&canonical_event, &trees),
            HashSet::from([canonical_event, alias_root.join("src/a.rs")])
        );
        let sibling_dir = tempfile::tempdir().unwrap();
        let sibling_event = sibling_dir.path().canonicalize().unwrap().join("a.rs");
        assert_eq!(
            resolved_paths(&sibling_event, &trees),
            HashSet::from([sibling_event])
        );
    }

    #[test]
    fn event_path_expansion_deduplicates_identical_root_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let trees = [WatchedTree {
            canonical_root: root.clone(),
            spellings: vec![root.clone(), root.clone()],
            conservative: false,
        }];

        assert_eq!(resolved_paths(&root, &trees), HashSet::from([root]));
    }

    #[test]
    fn data_changes_are_recorded() {
        assert_eq!(
            event_effect(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            EventEffect {
                invalidate_content: true,
                invalidate_walk: false,
                record: true,
            }
        );
    }

    #[test]
    fn renames_invalidate_content_walks_and_are_recorded() {
        for mode in [
            RenameMode::From,
            RenameMode::To,
            RenameMode::Both,
            RenameMode::Any,
        ] {
            assert_eq!(
                event_effect(&EventKind::Modify(ModifyKind::Name(mode))),
                EventEffect {
                    invalidate_content: true,
                    invalidate_walk: true,
                    record: true,
                }
            );
        }
    }

    #[test]
    fn creates_and_removes_invalidate_content_walks_and_are_recorded() {
        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Remove(RemoveKind::Any),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let file = root.join("changed.rs");
            std::fs::write(&file, "cached").unwrap();
            let files = FileCache::new();
            files.get(&file).unwrap();
            let walks = WalkCache::new(1);
            walks.get(&root, walk_key());
            let log = InvalidationLog::new();
            assert!(
                !files.is_empty(),
                "{kind:?} must start with a warm file cache"
            );
            assert!(
                !walks.is_empty(),
                "{kind:?} must start with a warm walk cache"
            );

            process_event(
                event_effect(&kind),
                std::slice::from_ref(&file),
                &[],
                &files,
                &walks,
                &log,
            );

            assert!(files.is_empty(), "{kind:?} did not invalidate FileCache");
            assert!(walks.is_empty(), "{kind:?} did not invalidate WalkCache");
            assert_eq!(log.since(0).paths, Some(vec![file]), "{kind:?}");
        }
    }

    #[test]
    fn metadata_only_changes_are_not_recorded() {
        assert_eq!(
            event_effect(&EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::WriteTime
            ))),
            EventEffect {
                invalidate_content: false,
                invalidate_walk: false,
                record: false,
            }
        );
    }

    #[test]
    fn adding_the_same_root_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&root).unwrap();
        watch.add_root(&root).unwrap();

        assert_eq!(watch.watched.len(), 1);
    }

    #[test]
    fn adding_root_with_trailing_current_directory_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&root.join(".")).unwrap();

        assert_eq!(watch.watched.len(), 1);
    }

    #[test]
    fn adding_root_with_parent_directory_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&root.join("sub").join("..")).unwrap();

        assert_eq!(watch.watched.len(), 1);
    }

    #[test]
    fn adding_subdirectory_of_recursive_root_does_not_rewatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&sub).unwrap();

        assert_eq!(watch.watched.len(), 1);
        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[
                WatchedTree {
                    canonical_root: root.clone(),
                    spellings: vec![root],
                    conservative: false,
                },
                WatchedTree {
                    canonical_root: sub.clone(),
                    spellings: vec![sub],
                    conservative: false,
                },
            ]
        );
    }

    #[test]
    fn component_prefix_sibling_is_watched_separately() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let root = parent.join("repo");
        let sibling = parent.join("repo-other");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&sibling).unwrap();

        assert_eq!(watch.watched.len(), 2);
    }

    #[test]
    fn adding_parent_of_watched_root_still_watches_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let root = parent.join("repo");
        std::fs::create_dir(&root).unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&parent).unwrap();

        assert_eq!(watch.watched.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn adding_covered_symlink_alias_records_mapping_without_rewatching() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let second = tempfile::tempdir().unwrap();
        let alias = second.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&alias).unwrap();

        assert_eq!(watch.watched.len(), 1);
        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[WatchedTree {
                canonical_root: root.clone(),
                spellings: vec![root, alias],
                conservative: false,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_preserves_raw_symlink_parent_spelling_for_event_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        let sub = real.join("sub");
        let aliases = base.join("aliases");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(&aliases).unwrap();
        std::os::unix::fs::symlink(&sub, aliases.join("jump")).unwrap();
        let spelled_root = aliases.join("jump").join("..");
        assert_eq!(std::fs::canonicalize(&spelled_root).unwrap(), real);
        assert_eq!(super::lexical_normalize(&spelled_root), aliases);

        let watch = WatchHandle::start(
            &spelled_root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();
        let event = real.join("f");

        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[WatchedTree {
                canonical_root: real,
                spellings: vec![spelled_root.clone()],
                conservative: false,
            }]
        );
        assert!(
            resolved_paths(&event, watch.watched_trees.read().as_slice())
                .contains(&spelled_root.join("f"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_only_registration_expands_an_unregistered_symlink_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let canonical_event = root.join("canonical.rs");
        std::fs::write(&canonical_event, "").unwrap();
        let watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        let alias_dir = tempfile::tempdir().unwrap();
        let alias = alias_dir.path().canonicalize().unwrap().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let alias_event = alias.join("canonical.rs");

        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[WatchedTree {
                canonical_root: root.clone(),
                spellings: vec![root],
                conservative: false,
            }]
        );
        assert_eq!(
            resolved_paths(&alias_event, watch.watched_trees.read().as_slice()),
            HashSet::from([alias_event, canonical_event])
        );
    }

    #[cfg(unix)]
    #[test]
    fn registration_mapping_survives_alias_symlink_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias = alias_dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let watch = WatchHandle::start(
            &alias,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();
        std::fs::remove_file(&alias).unwrap();

        let canonical_event = root.join("after-alias-removal.rs");
        let alias_event = alias.join("after-alias-removal.rs");
        assert_eq!(
            resolved_paths(&canonical_event, watch.watched_trees.read().as_slice()),
            HashSet::from([canonical_event, alias_event])
        );
    }

    #[cfg(unix)]
    #[test]
    fn multiple_covered_aliases_expand_through_their_canonical_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let canonical_event = root.join("shared.rs");
        std::fs::write(&canonical_event, "").unwrap();
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        let alias_dir = tempfile::tempdir().unwrap();
        let alias_parent = alias_dir.path().canonicalize().unwrap();
        let alias_one = alias_parent.join("alias-one");
        let alias_two = alias_parent.join("alias-two");
        std::os::unix::fs::symlink(&root, &alias_one).unwrap();
        std::os::unix::fs::symlink(&root, &alias_two).unwrap();

        watch.add_root(&alias_one).unwrap();
        watch.add_root(&alias_two).unwrap();

        assert_eq!(watch.watched.len(), 1);
        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[WatchedTree {
                canonical_root: root.clone(),
                spellings: vec![root.clone(), alias_one.clone(), alias_two.clone()],
                conservative: false,
            }]
        );
        let alias_one_event = alias_one.join("shared.rs");
        assert_eq!(
            resolved_paths(&alias_one_event, watch.watched_trees.read().as_slice()),
            HashSet::from([
                alias_one_event,
                canonical_event,
                alias_two.join("shared.rs"),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_rename_source_expands_via_deepest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias_parent = alias_dir.path().canonicalize().unwrap();
        let alias_one = alias_parent.join("alias-one");
        let alias_two = alias_parent.join("alias-two");
        std::os::unix::fs::symlink(&root, &alias_one).unwrap();
        std::os::unix::fs::symlink(&root, &alias_two).unwrap();
        let trees = [WatchedTree {
            canonical_root: root.clone(),
            spellings: vec![root.clone(), alias_one.clone(), alias_two.clone()],
            conservative: false,
        }];
        let missing_source = alias_one.join("old.rs");
        assert!(!missing_source.exists());

        assert_eq!(
            resolved_paths(&missing_source, &trees),
            HashSet::from([
                missing_source,
                root.join("old.rs"),
                alias_two.join("old.rs"),
            ])
        );
    }

    #[test]
    fn event_paths_expand_for_every_matching_nested_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let aliases = tempfile::tempdir().unwrap();
        let alias_parent = aliases.path().canonicalize().unwrap();
        let root_alias = alias_parent.join("root-alias");
        let nested_alias = alias_parent.join("nested-alias");
        let trees = [
            WatchedTree {
                canonical_root: root.clone(),
                spellings: vec![root.clone(), root_alias.clone()],
                conservative: false,
            },
            WatchedTree {
                canonical_root: nested.clone(),
                spellings: vec![nested.clone(), nested_alias.clone()],
                conservative: false,
            },
        ];
        let event = nested.join("file.rs");

        assert_eq!(
            resolved_paths(&event, &trees),
            HashSet::from([
                event,
                root_alias.join("nested/file.rs"),
                nested_alias.join("file.rs"),
            ])
        );
    }

    #[test]
    fn event_path_expansion_reaches_fixed_point_across_trees() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let r = base.join("r");
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let trees = [
            WatchedTree {
                canonical_root: r.clone(),
                spellings: vec![r.clone(), a.clone()],
                conservative: false,
            },
            WatchedTree {
                canonical_root: a.clone(),
                spellings: vec![a.clone(), b.clone()],
                conservative: false,
            },
            WatchedTree {
                canonical_root: r.join("sub"),
                spellings: vec![r.join("sub"), r.join("s")],
                conservative: false,
            },
        ];
        let event = r.join("sub/f");

        assert_eq!(
            resolved_paths(&event, &trees),
            HashSet::from([
                event,
                r.join("s/f"),
                a.join("sub/f"),
                b.join("sub/f"),
                a.join("s/f"),
                b.join("s/f"),
            ])
        );
    }

    #[test]
    fn expansion_cap_overflow_is_conservative_instead_of_partially_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let aliases: Vec<PathBuf> = (0..63)
            .map(|index| base.join(format!("alias-{index}")))
            .collect();
        let mut spellings = vec![root.clone()];
        spellings.extend(aliases.iter().cloned());
        let trees = [
            WatchedTree {
                canonical_root: root.clone(),
                spellings,
                conservative: false,
            },
            WatchedTree {
                canonical_root: aliases[0].clone(),
                spellings: vec![aliases[0].clone(), base.join("composite")],
                conservative: false,
            },
        ];
        let event = root.join("f");
        let EventPathExpansion::Conservative(roots) = expand_event_paths(&event, &trees) else {
            panic!("overflow must never return a partial resolved expansion");
        };

        assert!(roots.contains(&root));
        for alias in &aliases {
            assert!(roots.contains(alias));
        }
        assert!(roots.contains(&base.join("composite")));
    }

    #[test]
    fn injected_expansion_cap_conservatively_invalidates_related_caches_and_records_roots() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let alias = base.join("alias");
        let composite = base.join("composite");
        for directory in [&root, &alias, &composite] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("f"), "cached").unwrap();
        }
        let trees = [
            WatchedTree {
                canonical_root: root.clone(),
                spellings: vec![root.clone(), alias.clone()],
                conservative: false,
            },
            WatchedTree {
                canonical_root: alias.clone(),
                spellings: vec![alias.clone(), composite.clone()],
                conservative: false,
            },
        ];
        let event = root.join("f");
        assert!(matches!(
            expand_event_paths_with_cap(&event, &trees, 1),
            EventPathExpansion::Conservative(_)
        ));
        let files = FileCache::new();
        let walks = WalkCache::new(1);
        for directory in [&root, &alias, &composite] {
            files.get(&directory.join("f")).unwrap();
            walks.get(directory, walk_key());
        }
        assert_eq!(files.len(), 3);
        assert_eq!(walks.len(), 3);
        let log = InvalidationLog::new();

        process_event_with_cap(
            event_effect(&EventKind::Create(CreateKind::Any)),
            &[event],
            &trees,
            &files,
            &walks,
            &log,
            1,
        );

        assert!(files.is_empty());
        assert!(walks.is_empty());
        // Conservative expansion is logged as a wipe: exact-path entries would
        // let consumers keep derived state for unlisted descendants.
        assert_eq!(log.since(0).paths, None);
    }

    #[test]
    fn no_existing_event_component_uses_conservative_lexical_fallback() {
        let path = Path::new("missing-component/../fallback.rs");

        assert_eq!(
            expand_event_paths(path, &[]),
            EventPathExpansion::Conservative(vec![PathBuf::from("fallback.rs")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn parent_dir_event_does_not_leave_canonical_target_stale() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("r");
        let links = base.join("links");
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::create_dir(&links).unwrap();
        std::os::unix::fs::symlink(root.join("dir"), links.join("jump")).unwrap();
        let target = root.join("f");
        std::fs::write(&target, "cached").unwrap();
        let files = FileCache::new();
        files.get(&target).unwrap();
        assert_eq!(files.len(), 1);
        let walks = WalkCache::new(1);
        let log = InvalidationLog::new();
        let event = links.join("jump/../f");

        process_event(
            event_effect(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            &[event],
            &[],
            &files,
            &walks,
            &log,
        );

        assert!(files.is_empty());
        let recorded = log.since(0).paths.unwrap();
        assert!(
            recorded.iter().any(|path| target.starts_with(path)),
            "expected the target or a covering ancestor, got {recorded:?}"
        );
    }

    #[test]
    fn conservative_parent_dir_suffix_invalidates_covering_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cached = root.join("cached.rs");
        std::fs::write(&cached, "cached").unwrap();
        let files = FileCache::new();
        files.get(&cached).unwrap();
        let walks = WalkCache::new(1);
        let log = InvalidationLog::new();
        let event = root.join("missing/../changed.rs");

        process_event(
            event_effect(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            &[event],
            &[],
            &files,
            &walks,
            &log,
        );

        assert!(files.is_empty());
        // The conservative arm records a wipe rather than the ancestor path.
        assert_eq!(log.since(0).paths, None);
    }

    #[cfg(unix)]
    #[test]
    fn equivalent_event_paths_are_recorded_once_per_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias = alias_dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let canonical_file = root.join("same.rs");
        std::fs::write(&canonical_file, "same").unwrap();
        let alias_file = alias.join("same.rs");
        let trees = [WatchedTree {
            canonical_root: root.clone(),
            spellings: vec![root, alias.clone()],
            conservative: false,
        }];
        let files = FileCache::new();
        let walks = WalkCache::new(1);
        let log = InvalidationLog::new();

        process_event(
            event_effect(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            &[canonical_file.clone(), alias_file.clone()],
            &trees,
            &files,
            &walks,
            &log,
        );

        let recorded = log.since(0).paths.unwrap();
        assert_eq!(
            recorded
                .iter()
                .filter(|path| **path == canonical_file)
                .count(),
            1
        );
        assert_eq!(
            recorded.iter().filter(|path| **path == alias_file).count(),
            1
        );
        assert_eq!(recorded.len(), 2);
    }

    #[test]
    fn adding_uncanonicalizable_covered_root_registers_a_conservative_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let spelling = root.join("ghost").join("..");
        assert!(std::fs::canonicalize(&spelling).is_err());
        let mut watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::new(InvalidationLog::new()),
        )
        .unwrap();

        watch.add_root(&spelling).unwrap();

        assert_eq!(watch.watched.len(), 1);
        assert_eq!(
            watch.watched_trees.read().as_slice(),
            &[
                WatchedTree {
                    canonical_root: root.clone(),
                    spellings: vec![root.clone()],
                    conservative: false,
                },
                WatchedTree {
                    canonical_root: root.clone(),
                    spellings: vec![spelling.clone()],
                    conservative: true,
                },
            ]
        );
        let EventPathExpansion::Conservative(roots) = expand_event_paths(
            &root.join("changed.rs"),
            watch.watched_trees.read().as_slice(),
        ) else {
            panic!("an event matching an uncanonicalizable tree must be conservative");
        };
        assert!(roots.contains(&root));
        assert!(roots.contains(&spelling));
    }

    #[test]
    fn watch_callback_records_changed_path_in_invalidation_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("watched.rs");
        std::fs::write(&file, "before\n").unwrap();
        let log = Arc::new(InvalidationLog::new());
        let _watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::clone(&log),
        )
        .unwrap();

        let probe = root.join(".watch-probe");
        wait_for_watcher_ready(&log, &probe, &[&probe]);

        std::fs::write(&file, "after\n").unwrap();

        wait_for_recorded_paths(&log, &[&file]);
    }

    #[cfg(unix)]
    #[test]
    fn watcher_records_alias_and_canonical_paths_for_create_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let second = tempfile::tempdir().unwrap();
        let alias = second.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let alias_file = alias.join("f.rs");
        let real_file = real.join("f.rs");
        std::fs::write(&alias_file, "stale before create\n").unwrap();
        let files = Arc::new(FileCache::new());
        files.get(&alias_file).unwrap();
        let walks = Arc::new(WalkCache::new(1));
        walks.get(&alias, walk_key());
        assert!(!files.is_empty());
        assert!(!walks.is_empty());
        std::fs::remove_file(&alias_file).unwrap();
        let log = Arc::new(InvalidationLog::new());
        let _watch = WatchHandle::start(
            &alias,
            Arc::clone(&files),
            Arc::clone(&walks),
            Arc::clone(&log),
        )
        .unwrap();

        let alias_probe = alias.join(".watch-probe");
        let real_probe = real.join(".watch-probe");
        wait_for_watcher_ready(&log, &alias_probe, &[&alias_probe, &real_probe]);
        wait_for_log_quiescence(&log, log.since(0).revision);

        walks.get(&alias, walk_key());
        assert!(!files.is_empty());
        assert!(!walks.is_empty());
        let before_create = log.since(0).revision;
        std::fs::write(&alias_file, "pub fn watched() {}\n").unwrap();
        let after_create = wait_for_log_quiescence(
            &log,
            wait_for_recorded_paths_since(&log, before_create, &[&alias_file, &real_file]),
        );
        assert!(
            files.is_empty(),
            "create event did not invalidate FileCache"
        );
        assert!(
            walks.is_empty(),
            "create event did not invalidate WalkCache"
        );
        let create_paths = log.since(before_create).paths.unwrap();
        assert!(create_paths.contains(&alias_file));
        assert!(create_paths.contains(&real_file));

        files.get(&alias_file).unwrap();
        walks.get(&alias, walk_key());
        assert!(!files.is_empty());
        assert!(!walks.is_empty());
        std::fs::remove_file(&alias_file).unwrap();
        wait_for_log_quiescence(
            &log,
            wait_for_recorded_paths_since(&log, after_create, &[&alias_file, &real_file]),
        );
        assert!(
            files.is_empty(),
            "remove event did not invalidate FileCache"
        );
        assert!(
            walks.is_empty(),
            "remove event did not invalidate WalkCache"
        );
        let remove_paths = log.since(after_create).paths.unwrap();
        assert!(remove_paths.contains(&alias_file));
        assert!(remove_paths.contains(&real_file));
    }

    #[test]
    fn watcher_records_root_spelling_passed_by_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let canonical = super::normalize_root(&root);
        let log = Arc::new(InvalidationLog::new());
        let _watch = WatchHandle::start(
            &root,
            Arc::new(FileCache::new()),
            Arc::new(WalkCache::new(1)),
            Arc::clone(&log),
        )
        .unwrap();

        let probe = root.join(".watch-probe");
        let canonical_probe = canonical.join(".watch-probe");
        wait_for_watcher_ready(&log, &probe, &[&probe, &canonical_probe]);

        let file = root.join("as-is.rs");
        std::fs::write(&file, "pub fn as_is() {}\n").unwrap();

        wait_for_recorded_paths(&log, &[&file]);
    }
}
