//! The `graph` tool adapter.
//!
//! `hearth-graph` owns parsing and symbol-index semantics without ambient I/O.
//! This module connects it to the resident engine's walk/file caches and keeps
//! one incrementally revalidated index per root.

use crate::grep::grep_cancellable;
use crate::util::resolve_path;
use compact_str::CompactString;
use dashmap::DashMap;
use hearth_core::cache::WalkKey;
use hearth_core::{CancelToken, Engine, profile};
use hearth_graph::graph::{
    Coverage, DepEdge, EdgeTargetOwned, Guarantee, ModuleGraph, ModuleNode, NodeState,
};
use hearth_graph::{
    CancelSignal, FileAnalysis, FileSymbols, ImportKind, JsResolveOptions, LanguageRegistry,
    MAX_SYMBOLS_PER_FILE, ParserPool, ResolverSet, RustResolveOptions, Symbol, SymbolIndex,
    SymbolKind, UnresolvedReason, analyze_source, js_resolver, rust_resolver,
};
use hearth_proto::{
    GraphBasisEntry, GraphCoverage, GraphDefinitionsResult, GraphDepEdge, GraphDepsResult,
    GraphGuarantee, GraphLanguageStatus, GraphMeta, GraphNeighborhoodResult, GraphNode, GraphOp,
    GraphOutlineResult, GraphOutput, GraphParams, GraphRdepEntry, GraphRdepsResult, GraphResult,
    GraphSearchResult, GraphStatusResult, GraphSymbol, GraphSymbolsResult, GraphUnresolvedImport,
    GrepMode, GrepParams, ToolError, ToolResult, content_hash_hex,
};
use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use xxhash_rust::xxh3::Xxh3;

const MAX_GRAPH_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GRAPH_ROOTS: usize = 16;
const MAX_RDEPS_REPAIR: usize = 256;

/// Run a graph query without a live cancellation token.
pub fn graph(engine: &Engine, params: &GraphParams) -> ToolResult<GraphResult> {
    graph_cancellable(engine, params, &CancelToken::none())
}

/// Run a cancellable graph query.
pub fn graph_cancellable(
    engine: &Engine,
    params: &GraphParams,
    cancel: &CancelToken,
) -> ToolResult<GraphResult> {
    profile!("tool.graph", {
        cancel.check()?;
        let root_path = resolve_path(engine, &params.root);
        let metadata = std::fs::metadata(&root_path)
            .map_err(|_| ToolError::not_found(root_path.display().to_string()))?;
        if !metadata.is_dir() {
            return Err(ToolError::invalid(format!(
                "graph root is not a directory: {}",
                root_path.display()
            ))
            .with_path(root_path.display().to_string()));
        }

        let graph_state = engine.extension::<GraphState>();
        let root = graph_state.root(&root_path);
        if matches!(params.op, GraphOp::Status) {
            return Ok(status_query(engine, &root_path, &root));
        }

        let answer = query_ready_root(engine, &graph_state, &root_path, &root, params, cancel)?;
        if let GraphOp::Rdeps {
            path,
            depth,
            verify: true,
        } = &params.op
            && matches!(
                &answer.result.output,
                GraphOutput::Rdeps(result) if !result.verified
            )
        {
            return verify_rdeps_query(
                engine,
                &graph_state,
                &root_path,
                &root,
                params,
                path,
                *depth,
                answer.freshness,
                cancel,
            );
        }
        Ok(answer.result)
    })
}

/// Drop every graph root associated with `engine`.
///
/// Engine extensions outlive the ordinary file and walk caches, so cache
/// clearing must detach these roots explicitly.
pub fn graph_clear(engine: &Engine) -> u64 {
    let state = engine.extension::<GraphState>();
    let dropped = state.roots.len() as u64;
    state.roots.clear();
    dropped
}

struct GraphState {
    registry: Arc<LanguageRegistry>,
    roots: DashMap<PathBuf, Arc<RootGraph>>,
    access_clock: AtomicU64,
}

impl Default for GraphState {
    fn default() -> Self {
        // The registry is frozen by ownership: after construction only shared
        // references are published through this private engine extension.
        Self {
            registry: Arc::new(LanguageRegistry::bundled()),
            roots: DashMap::new(),
            access_clock: AtomicU64::new(1),
        }
    }
}

impl GraphState {
    fn root(&self, path: &Path) -> Arc<RootGraph> {
        let root = Arc::clone(
            self.roots
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(RootGraph::new(path)))
                .value(),
        );
        self.touch(&root);
        self.evict_roots(path, &root);
        root
    }

    fn touch(&self, root: &RootGraph) {
        let stamp = self.access_clock.fetch_add(1, Ordering::Relaxed);
        if let Some(mut sweep) = root.sweep.try_lock() {
            sweep.last_access = stamp;
        }
    }

    fn evict_roots(&self, current_path: &Path, current_root: &Arc<RootGraph>) {
        while self.roots.len() > MAX_GRAPH_ROOTS {
            // Inspect every candidate: a busy sweep may temporarily push the
            // map over the cap, but any later query retries the eviction.
            let victim = self
                .roots
                .iter()
                .filter(|entry| {
                    entry.key().as_path() != current_path
                        && !Arc::ptr_eq(entry.value(), current_root)
                })
                .filter_map(|entry| {
                    let last_access = entry.value().sweep.try_lock()?.last_access;
                    Some((entry.key().clone(), Arc::clone(entry.value()), last_access))
                })
                .min_by_key(|(_, _, last_access)| *last_access);
            let Some((path, root, _)) = victim else {
                break;
            };
            self.roots
                .remove_if(&path, |_, candidate| Arc::ptr_eq(candidate, &root));
        }
    }
}

struct RootGraph {
    state: RwLock<RootState>,
    sweep: Mutex<SweepMeta>,
    rdeps_flights: Mutex<FxHashMap<RdepsFlightKey, Arc<RdepsFlight>>>,
}

impl RootGraph {
    fn new(root: &Path) -> Self {
        Self {
            state: RwLock::new(RootState::new(root)),
            sweep: Mutex::new(SweepMeta::default()),
            rdeps_flights: Mutex::new(FxHashMap::default()),
        }
    }
}

struct RootState {
    phase: RootPhase,
    index: SymbolIndex,
    graph: ModuleGraph,
    resolvers: ResolverSet,
    records: FxHashMap<CompactString, StatRecord>,
    config_records: FxHashMap<CompactString, Option<StatRecord>>,
    supported_universe: FxHashSet<CompactString>,
    rust_crate_roots: Vec<CompactString>,
    last_seen_invalidation: u64,
    graph_generation: u64,
    components: ComponentsCache,
    counters: RootCounters,
    languages: Vec<GraphLanguageStatus>,
    last_sweep_at: Option<Instant>,
    build_duration_us: Option<u64>,
}

impl RootState {
    fn new(root: &Path) -> Self {
        let tsconfig = root.join("tsconfig.json");
        let mut config_records = FxHashMap::default();
        config_records.insert(
            CompactString::from(tsconfig.to_string_lossy().as_ref()),
            stat_record(&tsconfig),
        );
        Self {
            phase: RootPhase::Uninitialized,
            index: SymbolIndex::new(),
            graph: ModuleGraph::new(),
            resolvers: root_resolvers(&tsconfig, &[]),
            records: FxHashMap::default(),
            config_records,
            supported_universe: FxHashSet::default(),
            rust_crate_roots: Vec::new(),
            last_seen_invalidation: 0,
            graph_generation: 0,
            components: ComponentsCache::default(),
            counters: RootCounters::default(),
            languages: Vec::new(),
            last_sweep_at: None,
            build_duration_us: None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RdepsFlightKey {
    target: CompactString,
    resolver_generation: u64,
    graph_generation: u64,
    hidden: bool,
    respect_gitignore: bool,
    follow_symlinks: bool,
}

#[derive(Clone)]
struct RdepsRepairOutcome {
    approximate_entries: Vec<GraphRdepEntry>,
    completed: bool,
    repair_truncated: bool,
    generation_changed: bool,
}

struct RdepsFlight {
    result: Mutex<Option<ToolResult<RdepsRepairOutcome>>>,
    ready: Condvar,
}

impl RdepsFlight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

#[derive(Default)]
struct SweepMeta {
    last_walk_sweep: Option<SweepStamp>,
    last_view_sweep: Option<SweepStamp>,
    last_access: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SweepKey {
    Walk(WalkKey),
    Files {
        hash: u64,
        paths: Arc<Vec<CompactString>>,
    },
}

struct SweepStamp {
    key: SweepKey,
    at: Instant,
    excluded: FxHashSet<CompactString>,
}

impl SweepMeta {
    fn stamp(&self, key: &SweepKey) -> Option<&SweepStamp> {
        match key {
            SweepKey::Walk(_) => self.last_walk_sweep.as_ref(),
            SweepKey::Files { .. } => self.last_view_sweep.as_ref(),
        }
        .filter(|stamp| &stamp.key == key)
    }

    fn record(&mut self, key: SweepKey, at: Instant, excluded: FxHashSet<CompactString>) {
        let is_walk = matches!(&key, SweepKey::Walk(_));
        let stamp = Some(SweepStamp { key, at, excluded });
        if is_walk {
            self.last_walk_sweep = stamp;
        } else {
            self.last_view_sweep = stamp;
        }
    }
}

enum RootPhase {
    Uninitialized,
    Building,
    Ready { generation: u64 },
    Failed(String),
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct StatRecord {
    mtime_ns: i128,
    size: u64,
}

#[derive(Clone, Copy, Default)]
struct ComponentsCache {
    graph_generation: u64,
    components: u64,
}

#[derive(Clone, Copy, Default)]
struct RootCounters {
    universe_files: u64,
    unsupported_files: u64,
    oversize_files: u64,
    revalidated_files: u64,
    reindexed_files: u64,
    failed_files: u64,
    walk_cache_hit: bool,
}

#[derive(Clone, Copy)]
struct Freshness {
    swept: bool,
    guarantee: GraphGuarantee,
}

struct ReadyAnswer {
    result: GraphResult,
    freshness: Freshness,
}

fn query_ready_root(
    engine: &Engine,
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    cancel: &CancelToken,
) -> ToolResult<ReadyAnswer> {
    let sweep_key = sweep_key(root_path, params);
    let ready = {
        let state = root.state.read();
        matches!(state.phase, RootPhase::Ready { .. })
    };

    if ready {
        let Some(mut sweep) = root.sweep.try_lock() else {
            return ready_answer(
                root_path,
                &root.state.read(),
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Approximate,
                },
                None,
            );
        };
        sweep.last_access = graph_state.access_clock.fetch_add(1, Ordering::Relaxed);

        let still_ready = {
            let state = root.state.read();
            matches!(state.phase, RootPhase::Ready { .. })
        };
        let reusable_excluded = still_ready
            .then(|| {
                params.max_stale_ms.and_then(|max_stale_ms| {
                    sweep
                        .stamp(&sweep_key)
                        .filter(|stamp| elapsed_ms(stamp.at.elapsed()) <= max_stale_ms)
                        .map(|stamp| stamp.excluded.clone())
                })
            })
            .flatten();
        if let Some(excluded) = reusable_excluded {
            return ready_answer(
                root_path,
                &root.state.read(),
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Exact,
                },
                Some(&excluded),
            );
        }

        return sweep_and_answer(
            engine,
            graph_state,
            root_path,
            root,
            params,
            cancel,
            &mut sweep,
            false,
        );
    }

    // Polling keeps the cold-build barrier cancellable. A loser can only
    // acquire it after the winner has published Ready or restored the phase.
    let mut observed_busy = false;
    let mut sweep = loop {
        if let Some(sweep) = root.sweep.try_lock_for(Duration::from_millis(10)) {
            break sweep;
        }
        if !observed_busy {
            run_graph_test_hook(root_path, GraphTestPoint::ColdWaitObserved);
            observed_busy = true;
        }
        cancel.check()?;
    };
    sweep.last_access = graph_state.access_clock.fetch_add(1, Ordering::Relaxed);
    run_graph_test_hook(root_path, GraphTestPoint::ColdWaitLockAcquired);
    cancel.check()?;
    let phase_after_wait = {
        let state = root.state.read();
        match &state.phase {
            RootPhase::Ready { .. } => 0,
            RootPhase::Uninitialized => 1,
            RootPhase::Building => 2,
            RootPhase::Failed(_) => 3,
        }
    };
    if phase_after_wait == 0
        && let Some(excluded) = sweep.stamp(&sweep_key).map(|stamp| stamp.excluded.clone())
    {
        return ready_answer(
            root_path,
            &root.state.read(),
            params,
            Freshness {
                swept: false,
                guarantee: GraphGuarantee::Exact,
            },
            Some(&excluded),
        );
    }

    sweep_and_answer(
        engine,
        graph_state,
        root_path,
        root,
        params,
        cancel,
        &mut sweep,
        phase_after_wait != 0,
    )
}

#[allow(clippy::too_many_arguments)]
fn sweep_and_answer(
    engine: &Engine,
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    cancel: &CancelToken,
    sweep: &mut SweepMeta,
    cold: bool,
) -> ToolResult<ReadyAnswer> {
    let started = Instant::now();
    let mut cold_guard = cold.then(|| ColdBuildGuard::new(root));
    if cold {
        run_graph_test_hook(root_path, GraphTestPoint::ColdBuildStarted);
    }

    let snapshot = sweep_snapshot(root_path, &root.state.read());
    let delta = match build_sweep_delta(
        engine,
        &graph_state.registry,
        root_path,
        params,
        cancel,
        snapshot,
    ) {
        Ok(delta) => delta,
        Err(error) => {
            if let Some(guard) = cold_guard.as_mut() {
                guard.fail(&error);
            }
            return Err(error);
        }
    };
    cancel.check()?;

    run_graph_test_hook(root_path, GraphTestPoint::BeforePublish);
    let Some(attached_root) = graph_state.roots.get(root_path) else {
        if cold {
            return Err(ToolError::internal(
                "graph root was cleared or evicted during its initial build; retry",
            ));
        }
        return ready_answer(
            root_path,
            &root.state.read(),
            params,
            Freshness {
                swept: false,
                guarantee: GraphGuarantee::Approximate,
            },
            None,
        );
    };
    if !Arc::ptr_eq(attached_root.value(), root) {
        if cold {
            return Err(ToolError::internal(
                "graph root was replaced during its initial build; retry",
            ));
        }
        return ready_answer(
            root_path,
            &root.state.read(),
            params,
            Freshness {
                swept: false,
                guarantee: GraphGuarantee::Approximate,
            },
            None,
        );
    }

    let published_at = Instant::now();
    let SweepDelta {
        upserts,
        removes,
        records,
        mut config_records,
        config_changed,
        supported_universe,
        universe_complete,
        invalidation_revision,
        counters,
        excluded,
    } = delta;
    let rust_crate_roots = rust_crate_roots(root_path, &supported_universe);
    {
        // The map guard fences graph_clear/eviction through the complete
        // publication. Readers see either the old or the new generation.
        let mut state = root.state.write();
        let resolver_inputs_changed = config_changed || state.rust_crate_roots != rust_crate_roots;
        {
            let RootState {
                graph,
                resolvers,
                index,
                ..
            } = &mut *state;
            for relative in &removes {
                index.remove(relative);
                graph.remove_file(absolute_graph_path(root_path, relative).as_str());
            }
            for upsert in upserts {
                let SweepUpsert { relative, analysis } = upsert;
                let imports_supported = graph_state
                    .registry
                    .supports_imports(Path::new(analysis.path.as_str()));
                graph.upsert_file(&analysis, resolvers, imports_supported);
                index.upsert(
                    FileSymbols {
                        path: relative,
                        content_hash: analysis.content_hash,
                        symbols: analysis.symbols,
                    },
                    graph_state.registry.generation(),
                );
            }
            if resolver_inputs_changed {
                *resolvers = root_resolvers(&root_path.join("tsconfig.json"), &rust_crate_roots);
                graph.bump_resolver_generation();
                graph.reresolve_all(resolvers);
            }
            graph.set_universe_complete(universe_complete);
        }
        let config_dependencies = tracked_config_dependencies(root_path, &state.graph);
        config_records
            .retain(|dependency, _| config_dependencies.binary_search(dependency).is_ok());
        for dependency in config_dependencies {
            config_records
                .entry(dependency.clone())
                .or_insert_with(|| stat_record(Path::new(dependency.as_str())));
        }
        state.records = records;
        state.config_records = config_records;
        state.supported_universe = supported_universe;
        state.rust_crate_roots = rust_crate_roots;
        state.last_seen_invalidation = invalidation_revision;
        state.counters = counters;
        state.graph_generation = state.graph_generation.saturating_add(1);
        let generation = state.graph_generation;
        let components = component_count(root_path, &state.index, &state.graph);
        state.components = ComponentsCache {
            graph_generation: generation,
            components,
        };
        state.languages = language_statuses(&state.index, &graph_state.registry);
        state.last_sweep_at = Some(published_at);
        if cold {
            state.build_duration_us = Some(saturating_u64(started.elapsed().as_micros()));
        }
        state.phase = RootPhase::Ready { generation };
    }
    drop(attached_root);
    if let Some(guard) = cold_guard.as_mut() {
        guard.finish();
    }
    let sweep_key = sweep_key(root_path, params);
    sweep.record(sweep_key.clone(), published_at, excluded);

    let excluded = &sweep
        .stamp(&sweep_key)
        .expect("the completed sweep was just recorded")
        .excluded;
    ready_answer(
        root_path,
        &root.state.read(),
        params,
        Freshness {
            swept: true,
            guarantee: GraphGuarantee::Exact,
        },
        Some(excluded),
    )
}

struct ColdBuildGuard<'a> {
    root: &'a RootGraph,
    armed: bool,
}

impl<'a> ColdBuildGuard<'a> {
    fn new(root: &'a RootGraph) -> Self {
        root.state.write().phase = RootPhase::Building;
        Self { root, armed: true }
    }

    fn fail(&mut self, error: &ToolError) {
        self.root.state.write().phase = if error.is_cancelled() {
            RootPhase::Uninitialized
        } else {
            RootPhase::Failed(error.message.clone())
        };
        self.armed = false;
    }

    fn finish(&mut self) {
        self.armed = false;
    }
}

impl Drop for ColdBuildGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.root.state.write().phase = RootPhase::Uninitialized;
        }
    }
}

struct SweepSnapshot {
    records: FxHashMap<CompactString, StatRecord>,
    config_records: FxHashMap<CompactString, Option<StatRecord>>,
    config_dependencies: Vec<CompactString>,
    last_seen_invalidation: u64,
    indexed_hashes: FxHashMap<CompactString, u64>,
    indexed_paths: FxHashSet<CompactString>,
}

fn sweep_snapshot(root: &Path, state: &RootState) -> SweepSnapshot {
    let indexed_hashes = state
        .index
        .paths()
        .filter_map(|path| {
            state
                .index
                .file_hash(path)
                .map(|hash| (CompactString::from(path), hash))
        })
        .collect();
    let indexed_paths = state.index.paths().map(CompactString::from).collect();
    SweepSnapshot {
        records: state.records.clone(),
        config_records: state.config_records.clone(),
        config_dependencies: tracked_config_dependencies(root, &state.graph),
        last_seen_invalidation: state.last_seen_invalidation,
        indexed_hashes,
        indexed_paths,
    }
}

struct SweepUpsert {
    relative: CompactString,
    analysis: FileAnalysis,
}

struct SweepDelta {
    upserts: Vec<SweepUpsert>,
    removes: FxHashSet<CompactString>,
    records: FxHashMap<CompactString, StatRecord>,
    config_records: FxHashMap<CompactString, Option<StatRecord>>,
    config_changed: bool,
    supported_universe: FxHashSet<CompactString>,
    universe_complete: bool,
    invalidation_revision: u64,
    counters: RootCounters,
    excluded: FxHashSet<CompactString>,
}

fn build_sweep_delta(
    engine: &Engine,
    registry: &LanguageRegistry,
    root: &Path,
    params: &GraphParams,
    cancel: &CancelToken,
    snapshot: SweepSnapshot,
) -> ToolResult<SweepDelta> {
    cancel.check()?;
    let invalidations = engine
        .invalidations()
        .since(snapshot.last_seen_invalidation);
    let force_reindex = invalidations.paths.is_none();
    let config_invalidated = match &invalidations.paths {
        None => !snapshot.config_dependencies.is_empty(),
        Some(paths) => paths.iter().any(|path| {
            snapshot
                .config_dependencies
                .iter()
                .any(|dependency| path == Path::new(dependency.as_str()))
        }),
    };
    let mut config_changed = config_invalidated;
    let mut config_records = FxHashMap::default();
    for dependency in &snapshot.config_dependencies {
        let current = stat_record(Path::new(dependency.as_str()));
        if snapshot.config_records.get(dependency.as_str()) != Some(&current) {
            config_changed = true;
        }
        config_records.insert(dependency.clone(), current);
    }
    let mut records = snapshot.records;
    match invalidations.paths {
        None => records.clear(),
        Some(paths) => {
            for path in paths {
                if let Some(relative) = relative_path(root, &path) {
                    records.remove(relative.as_str());
                }
            }
        }
    }

    let caller_universe = !params.files.is_empty();
    let (universe, walk_cache_hit) = if caller_universe {
        let mut paths: Vec<PathBuf> = params
            .files
            .iter()
            .map(|path| {
                let path = Path::new(path);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                }
            })
            .collect();
        paths.sort_unstable();
        paths.dedup();
        (paths, false)
    } else {
        let key = WalkKey {
            respect_gitignore: params.respect_gitignore,
            hidden: params.hidden,
            follow_symlinks: params.follow_symlinks,
        };
        engine.watch_root(root);
        let (entry, hit) = engine.walks().get(root, key);
        (entry.files.as_ref().clone(), hit)
    };

    let mut relative_universe = FxHashSet::default();
    let mut upserts = Vec::new();
    let mut removes = FxHashSet::default();
    let mut excluded = FxHashSet::default();
    let mut supported_universe = FxHashSet::default();
    let mut parser_pool = ParserPool::new(registry);
    let mut counters = RootCounters {
        universe_files: universe.len() as u64,
        walk_cache_hit,
        ..RootCounters::default()
    };
    let trust = engine.stat_free(root);

    for path in &universe {
        cancel.check()?;
        let relative = relative_path(root, path).ok_or_else(|| {
            ToolError::invalid(format!(
                "graph universe path is outside root: {}",
                path.display()
            ))
            .with_path(path.display().to_string())
        })?;
        relative_universe.insert(relative.clone());

        if !registry.supports_symbols(path) {
            counters.unsupported_files += 1;
            records.remove(relative.as_str());
            if caller_universe {
                excluded.insert(relative);
            }
            continue;
        }
        supported_universe.insert(relative.clone());
        counters.revalidated_files += 1;

        let loaded = engine
            .files()
            .get_bounded_trusting(path, MAX_GRAPH_FILE_BYTES, trust);
        let entry = match loaded {
            Ok(Some((entry, _))) if entry.size <= MAX_GRAPH_FILE_BYTES => entry,
            Ok(Some(_)) | Ok(None) => {
                counters.oversize_files += 1;
                records.remove(relative.as_str());
                if caller_universe {
                    excluded.insert(relative);
                } else {
                    removes.insert(relative);
                }
                continue;
            }
            Err(_) => {
                counters.failed_files += 1;
                records.remove(relative.as_str());
                if caller_universe {
                    excluded.insert(relative);
                } else {
                    removes.insert(relative);
                }
                continue;
            }
        };

        let record = StatRecord {
            mtime_ns: entry.mtime_ns,
            size: entry.size,
        };
        let stat_matches = records.get(relative.as_str()).is_some_and(|previous| {
            previous.mtime_ns == record.mtime_ns && previous.size == record.size
        });
        let hash = entry.content_hash();
        let hash_matches = snapshot.indexed_hashes.get(relative.as_str()) == Some(&hash);
        records.insert(relative.clone(), record);

        if stat_matches && hash_matches {
            continue;
        }
        if hash_matches && !force_reindex {
            continue;
        }

        let Some(source) = entry.as_str() else {
            counters.failed_files += 1;
            records.remove(relative.as_str());
            if caller_universe {
                excluded.insert(relative);
            } else {
                removes.insert(relative);
            }
            continue;
        };
        let cancel_signal = || cancel.is_cancelled();
        if CancelSignal::is_cancelled(&cancel_signal) {
            return Err(ToolError::cancelled());
        }
        let absolute = CompactString::from(path.to_string_lossy().as_ref());
        let analysis = analyze_source(source, absolute.as_str(), hash, &mut parser_pool);
        if CancelSignal::is_cancelled(&cancel_signal) {
            return Err(ToolError::cancelled());
        }
        upserts.push(SweepUpsert { relative, analysis });
        counters.reindexed_files += 1;
    }

    if !caller_universe {
        for path in snapshot.indexed_paths {
            if !relative_universe.contains(path.as_str()) {
                records.remove(path.as_str());
                removes.insert(path);
            }
        }
    }

    Ok(SweepDelta {
        upserts,
        removes,
        records,
        config_records,
        config_changed,
        supported_universe,
        universe_complete: !caller_universe,
        invalidation_revision: invalidations.revision,
        counters,
        excluded,
    })
}

fn answer_query(
    root: &Path,
    state: &RootState,
    params: &GraphParams,
    freshness: Freshness,
    excluded: Option<&FxHashSet<CompactString>>,
) -> ToolResult<GraphResult> {
    match &state.phase {
        RootPhase::Ready { .. } => {}
        RootPhase::Failed(message) => return Err(ToolError::internal(message.clone())),
        RootPhase::Building => return Err(ToolError::internal("graph root is still building")),
        RootPhase::Uninitialized => {
            return Err(ToolError::internal("graph root is not initialized"));
        }
    }
    let filter = query_filter(root, &params.files, excluded)?;
    let (output, graph_guarantee) = match &params.op {
        GraphOp::Symbols { path } => (
            GraphOutput::Symbols(file_symbols_result(root, state, path, &filter, false)?),
            None,
        ),
        GraphOp::Outline { path } => (
            GraphOutput::Outline(file_outline_result(root, state, path, &filter)?),
            None,
        ),
        GraphOp::Search { query, limit } => (
            GraphOutput::Search(search_result(root, state, query, *limit, &filter)),
            None,
        ),
        GraphOp::Definitions { name, limit } => (
            GraphOutput::Definitions(definitions_result(root, state, name, *limit, &filter)),
            None,
        ),
        GraphOp::Deps { path, depth } => {
            let (result, guarantee) =
                deps_result(root, state, path, *depth, params.include_basis, &filter)?;
            (GraphOutput::Deps(result), Some(guarantee))
        }
        GraphOp::Rdeps {
            path,
            depth,
            verify: _,
        } => {
            let (mut result, guarantee) =
                rdeps_result(root, state, path, *depth, params.include_basis, &filter)?;
            let freshness_guarantee = graph_meta(root, state, freshness).guarantee;
            result.verified =
                guarantee == Guarantee::Exact && freshness_guarantee == GraphGuarantee::Exact;
            (GraphOutput::Rdeps(result), Some(guarantee))
        }
        GraphOp::Neighborhood { path, depth } => {
            let (result, guarantee) =
                neighborhood_result(root, state, path, *depth, params.include_basis, &filter)?;
            (GraphOutput::Neighborhood(result), Some(guarantee))
        }
        GraphOp::Status => {
            return Err(ToolError::internal(
                "status query unexpectedly entered the graph build path",
            ));
        }
    };

    let mut meta = graph_meta(root, state, freshness);
    if let Some(guarantee) = graph_guarantee {
        meta.guarantee = weakest_graph_guarantee(meta.guarantee, wire_guarantee(guarantee));
    }
    Ok(GraphResult { meta, output })
}

fn ready_answer(
    root: &Path,
    state: &RootState,
    params: &GraphParams,
    freshness: Freshness,
    excluded: Option<&FxHashSet<CompactString>>,
) -> ToolResult<ReadyAnswer> {
    answer_query(root, state, params, freshness, excluded)
        .map(|result| ReadyAnswer { result, freshness })
}

struct TraversedEdges {
    edges: Vec<(DepEdge, Guarantee)>,
    guarantee: Guarantee,
    coverage: Coverage,
}

fn deps_result(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    depth: u32,
    include_basis: bool,
    filter: &Option<FxHashSet<CompactString>>,
) -> ToolResult<(GraphDepsResult, Guarantee)> {
    let (absolute, node) = query_graph_node(root, state, requested_path, filter)?;
    let traversed = traverse_deps(&state.graph, absolute.as_str(), depth)
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    for (edge, guarantee) in traversed.edges {
        match map_dep_edge(root, edge, guarantee) {
            MappedDepEdge::Resolved(edge) => edges.push(edge),
            MappedDepEdge::Unresolved(import) => unresolved.push(import),
        }
    }
    Ok((
        GraphDepsResult {
            node,
            edges,
            unresolved,
            coverage: graph_coverage(root, traversed.coverage, include_basis),
        },
        traversed.guarantee,
    ))
}

fn rdeps_result(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    depth: u32,
    include_basis: bool,
    filter: &Option<FxHashSet<CompactString>>,
) -> ToolResult<(GraphRdepsResult, Guarantee)> {
    let (absolute, node) = query_graph_node(root, state, requested_path, filter)?;
    let traversed = traverse_rdeps(&state.graph, absolute.as_str(), depth)
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    let mut importers = Vec::with_capacity(traversed.edges.len());
    for (edge, guarantee) in traversed.edges {
        let importer = state
            .graph
            .node(edge.from.as_str())
            .and_then(|node| graph_node(root, &state.index, node))
            .ok_or_else(|| ToolError::not_found(edge.from.to_string()))?;
        importers.push(GraphRdepEntry {
            node: importer,
            specifier: Some(edge.specifier.to_string()),
            line: u64::from(edge.line),
            guarantee: wire_guarantee(guarantee),
        });
    }
    sort_rdep_entries(&mut importers);
    Ok((
        GraphRdepsResult {
            node,
            importers,
            verified: false,
            coverage: graph_coverage(root, traversed.coverage, include_basis),
        },
        traversed.guarantee,
    ))
}

fn neighborhood_result(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    depth: u32,
    include_basis: bool,
    filter: &Option<FxHashSet<CompactString>>,
) -> ToolResult<(GraphNeighborhoodResult, Guarantee)> {
    let (absolute, center) = query_graph_node(root, state, requested_path, filter)?;
    let result = state
        .graph
        .neighborhood(absolute.as_str(), depth)
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    let nodes = result
        .nodes
        .iter()
        .filter_map(|path| state.graph.node(path))
        .filter_map(|node| graph_node(root, &state.index, node))
        .collect();
    let edges = result
        .edges
        .into_iter()
        .filter_map(|edge| {
            let owner_guarantee = state
                .graph
                .deps(edge.from.as_str())
                .map_or(result.guarantee, |deps| deps.guarantee);
            match map_dep_edge(root, edge, owner_guarantee) {
                MappedDepEdge::Resolved(edge) => Some(edge),
                MappedDepEdge::Unresolved(_) => None,
            }
        })
        .collect();
    Ok((
        GraphNeighborhoodResult {
            center,
            nodes,
            edges,
            coverage: graph_coverage(root, result.coverage, include_basis),
        },
        result.guarantee,
    ))
}

fn traverse_deps(graph: &ModuleGraph, path: &str, depth: u32) -> Option<TraversedEdges> {
    graph.node(path)?;
    let mut reached = FxHashSet::default();
    reached.insert(CompactString::from(path));
    if depth == 0 {
        // Even an empty answer carries the queried node's structural
        // guarantee — a constant Exact would mislabel Rust/opaque nodes.
        let guarantee = graph.deps(path)?.guarantee;
        return Some(TraversedEdges {
            edges: Vec::new(),
            guarantee,
            coverage: coverage_for_paths(graph, &reached),
        });
    }

    let mut queue = VecDeque::from([(CompactString::from(path), 0_u32)]);
    let mut expanded = FxHashSet::default();
    let mut edges = Vec::new();
    let mut guarantee = Guarantee::Exact;
    while let Some((current, distance)) = queue.pop_front() {
        if !expanded.insert(current.clone()) {
            continue;
        }
        let deps = graph.deps(current.as_str())?;
        guarantee = weakest_guarantee(guarantee, deps.guarantee);
        for edge in deps.edges {
            if let EdgeTargetOwned::Path(target) = &edge.to {
                reached.insert(target.clone());
                if distance + 1 < depth {
                    queue.push_back((target.clone(), distance + 1));
                }
            }
            edges.push((edge, deps.guarantee));
        }
    }
    Some(TraversedEdges {
        edges,
        guarantee,
        coverage: coverage_for_paths(graph, &reached),
    })
}

fn traverse_rdeps(graph: &ModuleGraph, path: &str, depth: u32) -> Option<TraversedEdges> {
    graph.node(path)?;
    let mut reached = FxHashSet::default();
    reached.insert(CompactString::from(path));
    if depth == 0 {
        // Reverse-dependency exactness is a root-wide property; an empty
        // answer still must not claim more than the store can prove.
        let guarantee = graph.rdeps(path)?.guarantee;
        return Some(TraversedEdges {
            edges: Vec::new(),
            guarantee,
            coverage: coverage_for_paths(graph, &reached),
        });
    }

    let mut queue = VecDeque::from([(CompactString::from(path), 0_u32)]);
    let mut expanded = FxHashSet::default();
    let mut edges = Vec::new();
    let mut guarantee = Guarantee::Exact;
    while let Some((current, distance)) = queue.pop_front() {
        if !expanded.insert(current.clone()) {
            continue;
        }
        let rdeps = graph.rdeps(current.as_str())?;
        guarantee = weakest_guarantee(guarantee, rdeps.guarantee);
        for edge in rdeps.edges {
            reached.insert(edge.from.clone());
            if distance + 1 < depth {
                queue.push_back((edge.from.clone(), distance + 1));
            }
            edges.push((edge, rdeps.guarantee));
        }
    }
    Some(TraversedEdges {
        edges,
        guarantee,
        coverage: coverage_for_paths(graph, &reached),
    })
}

fn coverage_for_paths(graph: &ModuleGraph, paths: &FxHashSet<CompactString>) -> Coverage {
    let mut ordered: Vec<_> = paths.iter().collect();
    ordered.sort_unstable();
    let mut coverage = Coverage::default();
    for path in ordered {
        let Some(node) = graph.node(path) else {
            continue;
        };
        match &node.state {
            NodeState::Analyzed {
                content_hash,
                has_opaque_imports,
                ..
            } => {
                coverage.analyzed += 1;
                coverage.opaque_files += u64::from(*has_opaque_imports);
                coverage.basis.push((node.path.clone(), *content_hash));
            }
            NodeState::Stub => coverage.stubs += 1,
        }
    }
    coverage
}

fn graph_coverage(root: &Path, coverage: Coverage, include_basis: bool) -> GraphCoverage {
    GraphCoverage {
        analyzed: coverage.analyzed,
        stubs: coverage.stubs,
        basis: if include_basis {
            coverage
                .basis
                .into_iter()
                .map(|(path, hash)| GraphBasisEntry {
                    path: absolute_graph_path(root, path.as_str()).to_string(),
                    content_hash_hex: content_hash_hex(hash),
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

fn query_graph_node(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    filter: &Option<FxHashSet<CompactString>>,
) -> ToolResult<(CompactString, GraphNode)> {
    let (absolute, relative) = query_path(root, requested_path)?;
    if filter
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(relative.as_str()))
    {
        return Err(ToolError::not_found(absolute.display().to_string()));
    }
    let absolute = CompactString::from(absolute.to_string_lossy().as_ref());
    let module = state
        .graph
        .node(absolute.as_str())
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    if !matches!(module.state, NodeState::Analyzed { .. }) {
        return Err(ToolError::not_found(absolute.to_string()));
    }
    let node = graph_node(root, &state.index, module)
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    Ok((absolute, node))
}

fn graph_node(root: &Path, index: &SymbolIndex, node: &ModuleNode) -> Option<GraphNode> {
    let absolute = absolute_graph_path(root, node.path.as_str());
    let relative = relative_path(root, Path::new(absolute.as_str()));
    let (content_hash, language, indexed) = match &node.state {
        NodeState::Analyzed {
            content_hash,
            language,
            ..
        } => (
            *content_hash,
            language.as_ref().map(ToString::to_string),
            true,
        ),
        NodeState::Stub => (
            relative
                .as_ref()
                .and_then(|path| index.file_hash(path.as_str()))
                .unwrap_or(0),
            None,
            false,
        ),
    };
    let prefix = relative
        .as_ref()
        .map_or_else(|| absolute.as_str(), CompactString::as_str);
    Some(GraphNode {
        path: absolute.to_string(),
        node_id: node_id(prefix, content_hash),
        language,
        indexed,
    })
}

enum MappedDepEdge {
    Resolved(GraphDepEdge),
    Unresolved(GraphUnresolvedImport),
}

fn map_dep_edge(root: &Path, edge: DepEdge, guarantee: Guarantee) -> MappedDepEdge {
    let to = match edge.to {
        EdgeTargetOwned::Path(path) => absolute_graph_path(root, path.as_str()).to_string(),
        EdgeTargetOwned::External(package) => package.to_string(),
        EdgeTargetOwned::Unresolved(reason) => {
            return MappedDepEdge::Unresolved(GraphUnresolvedImport {
                specifier: edge.specifier.to_string(),
                line: u64::from(edge.line),
                reason: unresolved_reason(&reason),
            });
        }
    };
    MappedDepEdge::Resolved(GraphDepEdge {
        from: absolute_graph_path(root, edge.from.as_str()).to_string(),
        to,
        specifier: edge.specifier.to_string(),
        kind: import_kind(edge.kind).to_owned(),
        line: u64::from(edge.line),
        guarantee: wire_guarantee(guarantee),
    })
}

fn unresolved_reason(reason: &UnresolvedReason) -> String {
    // The reason is fixed wire vocabulary; resolver-specific diagnostics stay
    // out of it so environment-dependent strings never become the contract.
    match reason {
        UnresolvedReason::NotFound => "not found".to_owned(),
        UnresolvedReason::Unsupported => "unsupported".to_owned(),
        UnresolvedReason::Failed { kind, .. } => match kind {
            hearth_graph::FailedKind::Config => "config error".to_owned(),
            hearth_graph::FailedKind::Io => "io error".to_owned(),
            hearth_graph::FailedKind::InvalidSpecifier => "invalid specifier".to_owned(),
            hearth_graph::FailedKind::Other => "resolver error".to_owned(),
        },
    }
}

fn import_kind(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::EsStatic => "import",
        ImportKind::EsReexport => "reexport",
        ImportKind::EsDynamic => "dynamic",
        ImportKind::CommonJs => "require",
        ImportKind::TsImportRequire => "tsrequire",
        ImportKind::RustUse => "use",
        ImportKind::RustMod => "mod",
    }
}

fn weakest_guarantee(left: Guarantee, right: Guarantee) -> Guarantee {
    if left == Guarantee::Exact && right == Guarantee::Exact {
        Guarantee::Exact
    } else {
        Guarantee::Approximate
    }
}

fn wire_guarantee(guarantee: Guarantee) -> GraphGuarantee {
    match guarantee {
        Guarantee::Exact => GraphGuarantee::Exact,
        Guarantee::Approximate => GraphGuarantee::Approximate,
    }
}

fn weakest_graph_guarantee(left: GraphGuarantee, right: GraphGuarantee) -> GraphGuarantee {
    if left == GraphGuarantee::Exact && right == GraphGuarantee::Exact {
        GraphGuarantee::Exact
    } else {
        GraphGuarantee::Approximate
    }
}

fn rdeps_flight_key(
    state: &RootState,
    target: CompactString,
    params: &GraphParams,
) -> RdepsFlightKey {
    // `files` scopes the final graph query, but repair always greps the whole
    // root and approximate_rdep_entry searches only the individual hit path.
    RdepsFlightKey {
        target,
        resolver_generation: state.graph.resolver_generation(),
        graph_generation: state.graph.generation(),
        hidden: params.hidden,
        respect_gitignore: params.respect_gitignore,
        follow_symlinks: params.follow_symlinks,
    }
}

fn rdeps_node_is_structurally_exact(graph: &ModuleGraph, path: &str) -> bool {
    let Some(node) = graph.node(path) else {
        return false;
    };
    matches!(
        node.state,
        NodeState::Analyzed {
            has_opaque_imports: false,
            ..
        }
    ) && node.imports_supported()
        && node.resolver_live()
        && node.resolution_complete()
        && node.resolved_generation() == Some(graph.resolver_generation())
}

fn sort_rdep_entries(entries: &mut [GraphRdepEntry]) {
    entries.sort_unstable_by(|left, right| {
        left.node
            .path
            .cmp(&right.node.path)
            .then(left.specifier.cmp(&right.specifier))
            .then(left.line.cmp(&right.line))
    });
}

#[allow(clippy::too_many_arguments)]
fn verify_rdeps_query(
    engine: &Engine,
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    requested_path: &str,
    depth: u32,
    freshness: Freshness,
    cancel: &CancelToken,
) -> ToolResult<GraphResult> {
    let (target, _) = query_path(root_path, requested_path)?;
    let target = CompactString::from(target.to_string_lossy().as_ref());
    let key = rdeps_flight_key(&root.state.read(), target.clone(), params);
    let (flight, leader) = {
        let mut flights = root.rdeps_flights.lock();
        if let Some(flight) = flights.get(&key) {
            (Arc::clone(flight), false)
        } else {
            let flight = Arc::new(RdepsFlight::new());
            flights.insert(key.clone(), Arc::clone(&flight));
            (flight, true)
        }
    };

    let outcome = if leader {
        let outcome = run_rdeps_repair(
            engine,
            graph_state,
            root_path,
            root,
            params,
            target.as_str(),
            cancel,
        );
        *flight.result.lock() = Some(outcome.clone());
        flight.ready.notify_all();
        if outcome.is_err() {
            let mut flights = root.rdeps_flights.lock();
            if flights
                .get(&key)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &flight))
            {
                flights.remove(&key);
            }
        }
        outcome?
    } else {
        let mut result = flight.result.lock();
        loop {
            if let Some(outcome) = result.as_ref() {
                break outcome.clone()?;
            }
            flight
                .ready
                .wait_for(&mut result, Duration::from_millis(10));
            cancel.check()?;
        }
    };
    if outcome.generation_changed {
        cancel.check()?;
    }

    let state = root.state.read();
    let filter = query_filter(root_path, &params.files, None)?;
    let (mut result, guarantee) = rdeps_result(
        root_path,
        &state,
        requested_path,
        depth,
        params.include_basis,
        &filter,
    )?;
    let approximate_paths: FxHashSet<_> = outcome
        .approximate_entries
        .iter()
        .map(|entry| entry.node.path.as_str())
        .collect();
    result
        .importers
        .retain(|entry| !approximate_paths.contains(entry.node.path.as_str()));
    result.importers.extend(outcome.approximate_entries);
    sort_rdep_entries(&mut result.importers);

    let mut meta = graph_meta(root_path, &state, freshness);
    meta.guarantee = weakest_graph_guarantee(meta.guarantee, wire_guarantee(guarantee));
    if result
        .importers
        .iter()
        .any(|entry| entry.guarantee == GraphGuarantee::Approximate)
    {
        meta.guarantee = GraphGuarantee::Approximate;
    }
    meta.repair_truncated = outcome.repair_truncated;
    let structural_exact = guarantee == Guarantee::Exact && meta.guarantee == GraphGuarantee::Exact;
    result.verified = structural_exact || outcome.completed;

    Ok(GraphResult {
        meta,
        output: GraphOutput::Rdeps(result),
    })
}

fn run_rdeps_repair(
    engine: &Engine,
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    target: &str,
    cancel: &CancelToken,
) -> ToolResult<RdepsRepairOutcome> {
    let needles = rdeps_needles(Path::new(target));
    let pattern = needles
        .iter()
        .map(|needle| regex::escape(needle))
        .collect::<Vec<_>>()
        .join("|");
    let globs = registry_globs(&graph_state.registry);
    let grep_params = GrepParams {
        pattern: format!("(?:{pattern})"),
        path: root_path.display().to_string(),
        mode: GrepMode::FilesWithMatches,
        globs,
        max_count: Some(1),
        hidden: params.hidden,
        respect_gitignore: params.respect_gitignore,
        follow_symlinks: params.follow_symlinks,
        ..GrepParams::default()
    };
    let grep = grep_cancellable(engine, &grep_params, cancel)?;
    let mut parser_pool = ParserPool::new(&graph_state.registry);
    let mut approximate_entries = Vec::new();
    let mut repaired = 0_usize;
    let mut repair_truncated = grep.limit_reached;
    let mut generation_changed = false;
    let trust = engine.stat_free(root_path);

    for hit in grep.files {
        cancel.check()?;
        let path = PathBuf::from(&hit.path);
        let Some(relative) = relative_path(root_path, &path) else {
            continue;
        };
        let absolute = absolute_graph_path(root_path, path.to_string_lossy().as_ref());
        let loaded = engine
            .files()
            .get_bounded_trusting(&path, MAX_GRAPH_FILE_BYTES, trust);
        let current_hash = match &loaded {
            Ok(Some((entry, _))) if entry.size <= MAX_GRAPH_FILE_BYTES => {
                Some(entry.content_hash())
            }
            Ok(Some(_)) | Ok(None) | Err(_) => None,
        };
        let confirmation = current_hash.and_then(|hash| {
            let state = root.state.read();
            if !state
                .index
                .contains(relative.as_str(), hash, graph_state.registry.generation())
                || !state.graph.contains(absolute.as_str(), hash)
            {
                return None;
            }
            Some(rdeps_node_is_structurally_exact(
                &state.graph,
                absolute.as_str(),
            ))
        });
        match confirmation {
            Some(true) => continue,
            Some(false) => {
                // Re-publishing identical non-exact analysis cannot improve
                // it, so preserve the repair budget and surface grep evidence.
                approximate_entries.push(approximate_rdep_entry(
                    engine,
                    root_path,
                    root,
                    &grep_params.pattern,
                    &path,
                    params,
                    cancel,
                )?);
                continue;
            }
            None => {}
        }
        if repaired == MAX_RDEPS_REPAIR {
            repair_truncated = true;
            break;
        }
        repaired += 1;

        let entry = match loaded {
            Ok(Some((entry, _))) if entry.size <= MAX_GRAPH_FILE_BYTES => entry,
            Ok(Some(_)) | Ok(None) | Err(_) => {
                approximate_entries.push(approximate_rdep_entry(
                    engine,
                    root_path,
                    root,
                    &grep_params.pattern,
                    &path,
                    params,
                    cancel,
                )?);
                continue;
            }
        };
        let Some(source) = entry.as_str() else {
            approximate_entries.push(approximate_rdep_entry(
                engine,
                root_path,
                root,
                &grep_params.pattern,
                &path,
                params,
                cancel,
            )?);
            continue;
        };
        let analysis = analyze_source(
            source,
            absolute.as_str(),
            entry.content_hash(),
            &mut parser_pool,
        );
        if analysis.language.is_none() {
            approximate_entries.push(approximate_rdep_entry(
                engine,
                root_path,
                root,
                &grep_params.pattern,
                &path,
                params,
                cancel,
            )?);
            continue;
        }
        generation_changed |= publish_rdeps_repair(
            graph_state,
            root_path,
            root,
            relative,
            analysis,
            StatRecord {
                mtime_ns: entry.mtime_ns,
                size: entry.size,
            },
        )?;
        if !rdeps_node_is_structurally_exact(&root.state.read().graph, absolute.as_str()) {
            approximate_entries.push(approximate_rdep_entry(
                engine,
                root_path,
                root,
                &grep_params.pattern,
                &path,
                params,
                cancel,
            )?);
        }
    }

    Ok(RdepsRepairOutcome {
        approximate_entries,
        completed: !repair_truncated,
        repair_truncated,
        generation_changed,
    })
}

fn rdeps_needles(target: &Path) -> Vec<String> {
    let mut needles = Vec::new();
    if let Some(stem) = target.file_stem().and_then(|stem| stem.to_str()) {
        if stem == "index"
            && let Some(parent) = target
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
        {
            needles.push(parent.to_owned());
        }
        needles.push(stem.to_owned());
    }
    needles.sort_unstable();
    needles.dedup();
    needles
}

fn registry_globs(registry: &LanguageRegistry) -> Vec<String> {
    let mut globs: Vec<_> = registry
        .iter()
        .flat_map(|(_, spec)| spec.extensions.iter())
        .map(|extension| format!("*.{extension}"))
        .collect();
    globs.sort_unstable();
    globs.dedup();
    globs
}

#[allow(clippy::too_many_arguments)]
fn approximate_rdep_entry(
    engine: &Engine,
    root_path: &Path,
    root: &RootGraph,
    pattern: &str,
    path: &Path,
    params: &GraphParams,
    cancel: &CancelToken,
) -> ToolResult<GraphRdepEntry> {
    let line = first_grep_hit_line(engine, pattern, path, params, cancel)?;
    let state = root.state.read();
    let absolute = absolute_graph_path(root_path, path.to_string_lossy().as_ref());
    let node = state
        .graph
        .node(absolute.as_str())
        .and_then(|node| graph_node(root_path, &state.index, node))
        .unwrap_or_else(|| {
            let relative = relative_path(root_path, path);
            let hash = relative
                .as_ref()
                .and_then(|relative| state.index.file_hash(relative.as_str()))
                .unwrap_or(0);
            let prefix = relative
                .as_ref()
                .map_or_else(|| absolute.as_str(), CompactString::as_str);
            GraphNode {
                path: absolute.to_string(),
                node_id: node_id(prefix, hash),
                language: None,
                indexed: false,
            }
        });
    Ok(GraphRdepEntry {
        node,
        specifier: None,
        line,
        guarantee: GraphGuarantee::Approximate,
    })
}

fn first_grep_hit_line(
    engine: &Engine,
    pattern: &str,
    path: &Path,
    params: &GraphParams,
    cancel: &CancelToken,
) -> ToolResult<u64> {
    let grep = grep_cancellable(
        engine,
        &GrepParams {
            pattern: pattern.to_owned(),
            path: path.display().to_string(),
            mode: GrepMode::Content,
            max_count: Some(1),
            hidden: params.hidden,
            respect_gitignore: params.respect_gitignore,
            follow_symlinks: params.follow_symlinks,
            ..GrepParams::default()
        },
        cancel,
    )?;
    Ok(grep
        .files
        .iter()
        .flat_map(|file| file.lines.iter())
        .find(|line| line.is_match)
        .map_or(0, |line| line.line_number))
}

fn publish_rdeps_repair(
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    relative: CompactString,
    analysis: FileAnalysis,
    record: StatRecord,
) -> ToolResult<bool> {
    let Some(attached_root) = graph_state.roots.get(root_path) else {
        return Err(ToolError::internal(
            "graph root was cleared or evicted during rdeps repair; retry",
        ));
    };
    if !Arc::ptr_eq(attached_root.value(), root) {
        return Err(ToolError::internal(
            "graph root was replaced during rdeps repair; retry",
        ));
    }

    let hash = analysis.content_hash;
    let mut state = root.state.write();
    if state
        .index
        .contains(relative.as_str(), hash, graph_state.registry.generation())
        && state.graph.contains(analysis.path.as_str(), hash)
    {
        return Ok(false);
    }
    {
        let RootState {
            graph,
            resolvers,
            index,
            ..
        } = &mut *state;
        let imports_supported = graph_state
            .registry
            .supports_imports(Path::new(analysis.path.as_str()));
        graph.upsert_file(&analysis, resolvers, imports_supported);
        index.upsert(
            FileSymbols {
                path: relative.clone(),
                content_hash: hash,
                symbols: analysis.symbols,
            },
            graph_state.registry.generation(),
        );
    }
    state.records.insert(relative, record);
    state.graph_generation = state.graph_generation.saturating_add(1);
    let generation = state.graph_generation;
    state.components = ComponentsCache {
        graph_generation: generation,
        components: component_count(root_path, &state.index, &state.graph),
    };
    state.languages = language_statuses(&state.index, &graph_state.registry);
    state.phase = RootPhase::Ready { generation };
    Ok(true)
}

fn file_symbols_result(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    filter: &Option<FxHashSet<CompactString>>,
    _outline: bool,
) -> ToolResult<GraphSymbolsResult> {
    let (absolute, relative) = query_path(root, requested_path)?;
    if filter
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(relative.as_str()))
    {
        return Err(ToolError::not_found(absolute.display().to_string()));
    }
    let hash = state
        .index
        .file_hash(relative.as_str())
        .ok_or_else(|| ToolError::not_found(absolute.display().to_string()))?;
    let symbols = state
        .index
        .file_symbols(relative.as_str())
        .ok_or_else(|| ToolError::not_found(absolute.display().to_string()))?;
    Ok(GraphSymbolsResult {
        path: absolute.display().to_string(),
        node_id: node_id(relative.as_str(), hash),
        symbols: symbols
            .iter()
            .map(|symbol| graph_symbol(root, relative.as_str(), hash, symbol))
            .collect(),
        truncated: symbols.len() >= MAX_SYMBOLS_PER_FILE,
    })
}

fn file_outline_result(
    root: &Path,
    state: &RootState,
    requested_path: &str,
    filter: &Option<FxHashSet<CompactString>>,
) -> ToolResult<GraphOutlineResult> {
    let symbols = file_symbols_result(root, state, requested_path, filter, true)?;
    Ok(GraphOutlineResult {
        path: symbols.path,
        node_id: symbols.node_id,
        symbols: symbols.symbols,
        truncated: symbols.truncated,
    })
}

fn search_result(
    root: &Path,
    state: &RootState,
    query: &str,
    limit: u64,
    filter: &Option<FxHashSet<CompactString>>,
) -> GraphSearchResult {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let fetch_limit = if filter.is_some() {
        usize::MAX
    } else {
        limit.saturating_add(1)
    };
    let mut symbols: Vec<GraphSymbol> = state
        .index
        .search(query, fetch_limit)
        .into_iter()
        .filter(|found| {
            filter
                .as_ref()
                .is_none_or(|allowed| allowed.contains(found.path))
        })
        .filter_map(|found| {
            let hash = state.index.file_hash(found.path)?;
            Some(graph_symbol(root, found.path, hash, found.symbol))
        })
        .collect();
    let limit_reached = symbols.len() > limit;
    symbols.truncate(limit);
    GraphSearchResult {
        symbols,
        limit_reached,
    }
}

fn definitions_result(
    root: &Path,
    state: &RootState,
    name: &str,
    limit: u64,
    filter: &Option<FxHashSet<CompactString>>,
) -> GraphDefinitionsResult {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut symbols: Vec<GraphSymbol> = state
        .index
        .definitions(name)
        .into_iter()
        .filter(|found| {
            filter
                .as_ref()
                .is_none_or(|allowed| allowed.contains(found.path))
        })
        .filter_map(|found| {
            let hash = state.index.file_hash(found.path)?;
            Some(graph_symbol(root, found.path, hash, found.symbol))
        })
        .collect();
    let limit_reached = symbols.len() > limit;
    symbols.truncate(limit);
    GraphDefinitionsResult {
        symbols,
        limit_reached,
    }
}

fn graph_symbol(root: &Path, relative: &str, hash: u64, symbol: &Symbol) -> GraphSymbol {
    GraphSymbol {
        name: symbol.name.to_string(),
        kind: symbol_kind(symbol.kind).to_owned(),
        path: root.join(relative).display().to_string(),
        node_id: node_id(relative, hash),
        line: u64::from(symbol.line),
        column: u64::from(symbol.column),
        end_line: None,
        end_column: None,
        start_byte: Some(u64::from(symbol.def_start)),
        end_byte: Some(u64::from(symbol.def_end)),
        depth: u32::from(symbol.depth),
    }
}

fn symbol_kind(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Module => "module",
        SymbolKind::Macro => "macro",
        SymbolKind::Constant => "constant",
        SymbolKind::Type => "type",
        SymbolKind::Field => "field",
        SymbolKind::Property => "property",
        SymbolKind::Heading => "heading",
    }
}

fn node_id(relative: &str, hash: u64) -> String {
    format!("{relative}@{}", content_hash_hex(hash))
}

fn absolute_graph_path(root: &Path, path: &str) -> CompactString {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    CompactString::from(absolute.to_string_lossy().as_ref())
}

fn root_resolvers(tsconfig: &Path, rust_crate_roots: &[CompactString]) -> ResolverSet {
    ResolverSet {
        js: Some(js_resolver(JsResolveOptions {
            tsconfig: tsconfig.is_file().then(|| tsconfig.to_path_buf()),
            ..JsResolveOptions::default()
        })),
        rust: Some(rust_resolver(RustResolveOptions {
            crate_roots: rust_crate_roots.to_vec(),
        })),
    }
}

fn rust_crate_roots(
    root: &Path,
    supported_universe: &FxHashSet<CompactString>,
) -> Vec<CompactString> {
    let mut roots: Vec<CompactString> = supported_universe
        .iter()
        .filter(|relative| {
            let path = Path::new(relative.as_str());
            matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs")
            )
        })
        .map(|relative| absolute_graph_path(root, relative.as_str()))
        .collect();
    roots.sort_unstable_by(|left, right| {
        let left_is_src = Path::new(left.as_str())
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "src");
        let right_is_src = Path::new(right.as_str())
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "src");
        right_is_src.cmp(&left_is_src).then_with(|| left.cmp(right))
    });
    roots
}

fn tracked_config_dependencies(root: &Path, graph: &ModuleGraph) -> Vec<CompactString> {
    let mut dependencies = graph.config_dependencies();
    dependencies.push(CompactString::from(
        root.join("tsconfig.json").to_string_lossy().as_ref(),
    ));
    dependencies.sort_unstable();
    dependencies.dedup();
    dependencies
}

fn stat_record(path: &Path) -> Option<StatRecord> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
        Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
    };
    Some(StatRecord {
        mtime_ns,
        size: metadata.len(),
    })
}

fn component_count(root: &Path, index: &SymbolIndex, graph: &ModuleGraph) -> u64 {
    let mut vertices = FxHashSet::default();
    vertices.extend(index.paths().map(|path| absolute_graph_path(root, path)));
    vertices.extend(graph.paths().map(|path| absolute_graph_path(root, path)));
    let mut vertices: Vec<_> = vertices.into_iter().collect();
    vertices.sort_unstable();
    let by_path: FxHashMap<_, _> = vertices
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), index))
        .collect();
    let mut parents: Vec<usize> = (0..vertices.len()).collect();
    let mut ranks = vec![0_u8; vertices.len()];

    for edge in graph.edges() {
        let EdgeTargetOwned::Path(target) = edge.to else {
            continue;
        };
        let source = absolute_graph_path(root, edge.from.as_str());
        let target = absolute_graph_path(root, target.as_str());
        let (Some(&source), Some(&target)) = (by_path.get(&source), by_path.get(&target)) else {
            continue;
        };
        union_components(&mut parents, &mut ranks, source, target);
    }

    let mut components = FxHashSet::default();
    for vertex in 0..vertices.len() {
        components.insert(find_component(&mut parents, vertex));
    }
    components.len() as u64
}

fn find_component(parents: &mut [usize], mut vertex: usize) -> usize {
    let mut root = vertex;
    while parents[root] != root {
        root = parents[root];
    }
    while parents[vertex] != vertex {
        let parent = parents[vertex];
        parents[vertex] = root;
        vertex = parent;
    }
    root
}

fn union_components(parents: &mut [usize], ranks: &mut [u8], left: usize, right: usize) {
    let left = find_component(parents, left);
    let right = find_component(parents, right);
    if left == right {
        return;
    }
    match ranks[left].cmp(&ranks[right]) {
        std::cmp::Ordering::Less => parents[left] = right,
        std::cmp::Ordering::Greater => parents[right] = left,
        std::cmp::Ordering::Equal => {
            parents[right] = left;
            ranks[left] = ranks[left].saturating_add(1);
        }
    }
}

fn graph_meta(root: &Path, state: &RootState, freshness: Freshness) -> GraphMeta {
    // Universe files the sweep could not index (unreadable or over the size
    // cap) may hold symbols the answer cannot see, so their presence caps the
    // guarantee at Approximate regardless of freshness. Unsupported languages
    // stay Exact-compatible: they are documented scope, not missing data.
    let guarantee = if state.counters.failed_files > 0 || state.counters.oversize_files > 0 {
        GraphGuarantee::Approximate
    } else {
        freshness.guarantee
    };
    GraphMeta {
        guarantee,
        root: root.display().to_string(),
        universe_files: state.counters.universe_files,
        indexed_files: state.index.file_count() as u64,
        unsupported_files: state.counters.unsupported_files,
        oversize_files: state.counters.oversize_files,
        revalidated_files: state.counters.revalidated_files,
        reindexed_files: state.counters.reindexed_files,
        swept: freshness.swept,
        sweep_age_ms: sweep_age(state.last_sweep_at).unwrap_or(0),
        walk_cache_hit: state.counters.walk_cache_hit,
        repair_truncated: false,
    }
}

fn status_query(engine: &Engine, root_path: &Path, root: &RootGraph) -> GraphResult {
    let sweep = root.sweep.try_lock();
    let sweep_busy = sweep.is_none();
    let Some(state) = root.state.try_read() else {
        return empty_status(root_path, true, GraphGuarantee::Approximate);
    };

    let (built, phase_building, generation) = match &state.phase {
        RootPhase::Uninitialized => (false, false, None),
        RootPhase::Building => (false, true, None),
        RootPhase::Ready { generation } => (true, false, Some(*generation)),
        RootPhase::Failed(_) => (false, false, None),
    };
    let building = phase_building || sweep_busy;
    let components = generation
        .filter(|generation| *generation == state.components.graph_generation)
        .map_or(0, |_| state.components.components);
    let missing_supported = state
        .supported_universe
        .iter()
        .filter(|path| state.index.file_hash(path.as_str()).is_none())
        .count() as u64;
    let pending_files = missing_supported.saturating_sub(
        state
            .counters
            .oversize_files
            .saturating_add(state.counters.failed_files),
    );
    let stale_files = stale_file_count(engine, root_path, &state);
    let status = GraphStatusResult {
        built,
        building,
        universe_files: state.counters.universe_files,
        indexed_files: state.index.file_count() as u64,
        unsupported_files: state.counters.unsupported_files,
        oversize_files: state.counters.oversize_files,
        pending_files,
        stale_files,
        failed_files: state.counters.failed_files,
        symbols: state.index.symbol_count() as u64,
        edges: state.graph.edge_count() as u64,
        components,
        languages: state.languages.clone(),
        last_sweep_ms_ago: state.last_sweep_at.map(|at| elapsed_ms(at.elapsed())),
        build_duration_us: state.build_duration_us,
    };
    let guarantee = if building || pending_files > 0 || stale_files > 0 {
        GraphGuarantee::Approximate
    } else {
        GraphGuarantee::Exact
    };
    GraphResult {
        meta: graph_meta(
            root_path,
            &state,
            Freshness {
                swept: false,
                guarantee,
            },
        ),
        output: GraphOutput::Status(status),
    }
}

fn stale_file_count(engine: &Engine, root: &Path, state: &RootState) -> u64 {
    let invalidations = engine.invalidations().since(state.last_seen_invalidation);
    let Some(paths) = invalidations.paths else {
        return state.index.file_count() as u64;
    };
    paths
        .into_iter()
        .filter_map(|path| {
            let absolute = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            relative_path(root, &absolute)
        })
        .filter(|path| state.index.file_hash(path.as_str()).is_some())
        .collect::<FxHashSet<_>>()
        .len() as u64
}

fn empty_status(root: &Path, building: bool, guarantee: GraphGuarantee) -> GraphResult {
    GraphResult {
        meta: GraphMeta {
            guarantee,
            root: root.display().to_string(),
            universe_files: 0,
            indexed_files: 0,
            unsupported_files: 0,
            oversize_files: 0,
            revalidated_files: 0,
            reindexed_files: 0,
            swept: false,
            sweep_age_ms: 0,
            walk_cache_hit: false,
            repair_truncated: false,
        },
        output: GraphOutput::Status(GraphStatusResult {
            built: false,
            building,
            universe_files: 0,
            indexed_files: 0,
            unsupported_files: 0,
            oversize_files: 0,
            pending_files: 0,
            stale_files: 0,
            failed_files: 0,
            symbols: 0,
            edges: 0,
            components: 0,
            languages: Vec::new(),
            last_sweep_ms_ago: None,
            build_duration_us: None,
        }),
    }
}

fn language_statuses(index: &SymbolIndex, registry: &LanguageRegistry) -> Vec<GraphLanguageStatus> {
    let mut counts: FxHashMap<CompactString, (u64, u64)> = FxHashMap::default();
    for path in index.paths() {
        let Some(spec) = registry
            .for_path(Path::new(path))
            .and_then(|language| registry.get(language))
        else {
            continue;
        };
        let entry = counts.entry(spec.name.clone()).or_default();
        entry.0 += 1;
        entry.1 += index
            .file_symbols(path)
            .map_or(0, |symbols| symbols.len() as u64);
    }
    let mut statuses: Vec<_> = counts
        .into_iter()
        .map(|(language, (files, symbols))| GraphLanguageStatus {
            language: language.to_string(),
            files,
            symbols,
        })
        .collect();
    statuses.sort_by(|left, right| left.language.cmp(&right.language));
    statuses
}

fn query_filter(
    root: &Path,
    files: &[String],
    excluded: Option<&FxHashSet<CompactString>>,
) -> ToolResult<Option<FxHashSet<CompactString>>> {
    if files.is_empty() {
        return Ok(None);
    }
    files
        .iter()
        .map(|path| {
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            relative_path(root, &absolute).ok_or_else(|| {
                ToolError::invalid(format!(
                    "graph universe path is outside root: {}",
                    absolute.display()
                ))
                .with_path(absolute.display().to_string())
            })
        })
        .collect::<ToolResult<FxHashSet<_>>>()
        .map(|mut allowed| {
            if let Some(excluded) = excluded {
                allowed.retain(|path| !excluded.contains(path));
            }
            Some(allowed)
        })
}

fn sweep_key(root: &Path, params: &GraphParams) -> SweepKey {
    if params.files.is_empty() {
        return SweepKey::Walk(WalkKey {
            respect_gitignore: params.respect_gitignore,
            hidden: params.hidden,
            follow_symlinks: params.follow_symlinks,
        });
    }

    let mut paths: Vec<CompactString> = params
        .files
        .iter()
        .map(|path| {
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            CompactString::from(absolute.to_string_lossy().as_ref())
        })
        .collect();
    paths.sort_unstable();
    paths.dedup();
    let mut hash = Xxh3::new();
    for path in &paths {
        hash.update(&(path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
    }
    SweepKey::Files {
        hash: hash.digest(),
        paths: Arc::new(paths),
    }
}

/// Synchronization points compiled only for deterministic graph tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphTestPoint {
    ColdBuildStarted,
    ColdWaitObserved,
    ColdWaitLockAcquired,
    BeforePublish,
}

#[cfg(test)]
type GraphTestHook = Arc<dyn Fn(GraphTestPoint) + Send + Sync>;

#[cfg(test)]
fn graph_test_hooks() -> &'static Mutex<FxHashMap<PathBuf, GraphTestHook>> {
    static HOOKS: OnceLock<Mutex<FxHashMap<PathBuf, GraphTestHook>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(FxHashMap::default()))
}

#[cfg(test)]
fn run_graph_test_hook(root: &Path, point: GraphTestPoint) {
    let hook = graph_test_hooks().lock().get(root).cloned();
    if let Some(hook) = hook {
        hook(point);
    }
}

#[cfg(not(test))]
fn run_graph_test_hook(_root: &Path, _point: GraphTestPoint) {}

#[cfg(test)]
fn graph_test_set_hook(root: &Path, hook: Option<GraphTestHook>) {
    let mut hooks = graph_test_hooks().lock();
    if let Some(hook) = hook {
        hooks.insert(root.to_path_buf(), hook);
    } else {
        hooks.remove(root);
    }
}

#[cfg(test)]
fn graph_test_root_count(engine: &Engine) -> usize {
    engine.extension::<GraphState>().roots.len()
}

fn query_path(root: &Path, requested: &str) -> ToolResult<(PathBuf, CompactString)> {
    let path = Path::new(requested);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let relative = relative_path(root, &absolute).ok_or_else(|| {
        ToolError::invalid(format!(
            "graph query path is outside root: {}",
            absolute.display()
        ))
        .with_path(absolute.display().to_string())
    })?;
    Ok((absolute, relative))
}

fn relative_path(root: &Path, path: &Path) -> Option<CompactString> {
    path.strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| CompactString::from(relative.to_string_lossy().as_ref()))
}

fn sweep_age(last_sweep: Option<Instant>) -> Option<u64> {
    last_sweep.map(|at| elapsed_ms(at.elapsed()))
}

fn elapsed_ms(duration: Duration) -> u64 {
    saturating_u64(duration.as_millis())
}

fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        GraphTestHook, GraphTestPoint, MAX_GRAPH_ROOTS, RootState, SweepKey, SweepMeta, graph,
        graph_cancellable, graph_clear, graph_test_root_count, graph_test_set_hook,
        rdeps_flight_key,
    };
    use compact_str::CompactString;
    use hearth_core::{CancelToken, Engine, EngineConfig};
    use hearth_proto::{ErrorKind, GraphOp, GraphOutput, GraphParams};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Instant;

    fn engine() -> Engine {
        Engine::new(EngineConfig {
            enable_watch: false,
            enable_optimizer: false,
            ..EngineConfig::default()
        })
    }

    fn seed(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("test file has a parent")).unwrap();
        std::fs::write(path, "pub fn symbol() {}\n").unwrap();
    }

    fn query(root: &Path) -> GraphParams {
        GraphParams::new(
            root.display().to_string(),
            GraphOp::Search {
                query: "symbol".into(),
                limit: 10,
            },
        )
    }

    use std::path::Path;

    #[test]
    fn cold_loser_deterministically_observes_building() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "src/lib.rs");
        let engine = engine();
        let build_started = Arc::new(Barrier::new(2));
        let release_build = Arc::new(Barrier::new(2));
        let loser_waiting = Arc::new(Barrier::new(2));
        let hook: GraphTestHook = {
            let build_started = Arc::clone(&build_started);
            let release_build = Arc::clone(&release_build);
            let loser_waiting = Arc::clone(&loser_waiting);
            Arc::new(move |point| match point {
                GraphTestPoint::ColdBuildStarted => {
                    build_started.wait();
                    release_build.wait();
                }
                GraphTestPoint::ColdWaitObserved => {
                    loser_waiting.wait();
                }
                GraphTestPoint::ColdWaitLockAcquired => {}
                GraphTestPoint::BeforePublish => {}
            })
        };
        graph_test_set_hook(dir.path(), Some(hook));

        let first_engine = engine.clone();
        let first_query = query(dir.path());
        let first = std::thread::spawn(move || graph(&first_engine, &first_query));
        build_started.wait();

        let second_engine = engine.clone();
        let second_query = query(dir.path());
        let second = std::thread::spawn(move || graph(&second_engine, &second_query));
        loser_waiting.wait();
        release_build.wait();

        assert!(first.join().unwrap().is_ok());
        assert!(second.join().unwrap().is_ok());
        graph_test_set_hook(dir.path(), None);
    }

    #[test]
    fn cold_loser_observes_cancel_latched_after_acquiring_sweep_lock() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "src/lib.rs");
        let engine = engine();
        let cancel = CancelToken::new();
        let build_started = Arc::new(Barrier::new(2));
        let release_build = Arc::new(Barrier::new(2));
        let loser_waiting = Arc::new(Barrier::new(2));
        let observed_busy = Arc::new(AtomicBool::new(false));
        let hook: GraphTestHook = {
            let cancel = cancel.clone();
            let build_started = Arc::clone(&build_started);
            let release_build = Arc::clone(&release_build);
            let loser_waiting = Arc::clone(&loser_waiting);
            let observed_busy = Arc::clone(&observed_busy);
            Arc::new(move |point| match point {
                GraphTestPoint::ColdBuildStarted => {
                    build_started.wait();
                    release_build.wait();
                }
                GraphTestPoint::ColdWaitObserved => {
                    observed_busy.store(true, Ordering::Release);
                    loser_waiting.wait();
                }
                GraphTestPoint::ColdWaitLockAcquired => {
                    if observed_busy.load(Ordering::Acquire) {
                        cancel.cancel();
                    }
                }
                GraphTestPoint::BeforePublish => {}
            })
        };
        graph_test_set_hook(dir.path(), Some(hook));

        let winner_engine = engine.clone();
        let winner_query = query(dir.path());
        let winner = std::thread::spawn(move || graph(&winner_engine, &winner_query));
        build_started.wait();

        let loser_engine = engine.clone();
        let loser_query = query(dir.path());
        let loser_cancel = cancel.clone();
        let loser = std::thread::spawn(move || {
            graph_cancellable(&loser_engine, &loser_query, &loser_cancel)
        });
        loser_waiting.wait();
        release_build.wait();

        assert!(winner.join().unwrap().is_ok());
        assert_eq!(
            loser.join().unwrap().unwrap_err().kind,
            ErrorKind::Cancelled
        );

        let follow_up = graph(&engine, &query(dir.path())).unwrap();
        let GraphOutput::Search(search) = follow_up.output else {
            panic!("expected a search result");
        };
        assert_eq!(search.symbols.len(), 1);
        assert_eq!(search.symbols[0].name, "symbol");
        graph_test_set_hook(dir.path(), None);
    }

    #[test]
    fn files_sweep_stamp_rejects_a_forced_hash_collision() {
        let mut sweep = SweepMeta::default();
        let first = SweepKey::Files {
            hash: 7,
            paths: Arc::new(vec![CompactString::from("/root/a.rs")]),
        };
        let colliding = SweepKey::Files {
            hash: 7,
            paths: Arc::new(vec![CompactString::from("/root/b.rs")]),
        };

        sweep.record(first.clone(), Instant::now(), Default::default());

        assert!(sweep.stamp(&colliding).is_none());
        assert!(sweep.stamp(&first).is_some());
    }

    #[test]
    fn rdeps_flight_key_covers_every_repair_walk_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let state = RootState::new(dir.path());
        let mut params = GraphParams::new(
            dir.path().display().to_string(),
            GraphOp::Rdeps {
                path: "src/target.ts".into(),
                depth: 1,
                verify: true,
            },
        );
        let target = CompactString::from("/root/src/target.ts");
        let baseline = rdeps_flight_key(&state, target.clone(), &params);

        params.hidden = true;
        assert_ne!(baseline, rdeps_flight_key(&state, target.clone(), &params));
        params.hidden = false;
        params.respect_gitignore = false;
        assert_ne!(baseline, rdeps_flight_key(&state, target.clone(), &params));
        params.respect_gitignore = true;
        params.follow_symlinks = true;
        assert_ne!(baseline, rdeps_flight_key(&state, target, &params));
    }

    #[test]
    fn busy_root_overshoot_converges_on_the_next_query() {
        let parent = tempfile::tempdir().unwrap();
        let engine = engine();
        let release = Arc::new(Barrier::new(MAX_GRAPH_ROOTS + 2));
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut handles = Vec::new();
        let mut roots = Vec::new();

        for index in 0..=MAX_GRAPH_ROOTS {
            let root = parent.path().join(format!("root-{index}"));
            seed(&root, "src/lib.rs");
            roots.push(root.clone());
            let hook: GraphTestHook = {
                let entered_tx = entered_tx.clone();
                let release = Arc::clone(&release);
                Arc::new(move |point| {
                    if point == GraphTestPoint::ColdBuildStarted {
                        entered_tx.send(()).unwrap();
                        release.wait();
                    }
                })
            };
            graph_test_set_hook(&root, Some(hook));
            let query = query(&root);
            let thread_engine = engine.clone();
            handles.push(std::thread::spawn(move || graph(&thread_engine, &query)));
            entered_rx.recv().unwrap();
        }

        assert_eq!(graph_test_root_count(&engine), MAX_GRAPH_ROOTS + 1);
        release.wait();
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        for root in &roots {
            graph_test_set_hook(root, None);
        }

        graph(&engine, &query(roots.last().unwrap())).unwrap();
        assert_eq!(graph_test_root_count(&engine), MAX_GRAPH_ROOTS);
        assert_eq!(graph_clear(&engine), MAX_GRAPH_ROOTS as u64);
    }

    #[test]
    fn clear_before_publish_rejects_the_detached_root() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "src/lib.rs");
        let engine = engine();
        let before_publish = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let hook: GraphTestHook = {
            let before_publish = Arc::clone(&before_publish);
            let release = Arc::clone(&release);
            Arc::new(move |point| {
                if point == GraphTestPoint::BeforePublish {
                    before_publish.wait();
                    release.wait();
                }
            })
        };
        graph_test_set_hook(dir.path(), Some(hook));

        let thread_engine = engine.clone();
        let thread_query = query(dir.path());
        let handle = std::thread::spawn(move || graph(&thread_engine, &thread_query));
        before_publish.wait();
        assert_eq!(graph_clear(&engine), 1);
        release.wait();

        assert!(handle.join().unwrap().is_err());
        assert_eq!(graph_test_root_count(&engine), 0);
        graph_test_set_hook(dir.path(), None);
    }
}
