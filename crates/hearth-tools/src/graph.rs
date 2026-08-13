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
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use xxhash_rust::xxh3::Xxh3;

const MAX_GRAPH_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GRAPH_ROOTS: usize = 16;
const MAX_GRAPH_RESIDENT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_ACTIVE_BUILDS: usize = 2;
const MAX_RDEPS_REPAIR: usize = 256;
const MAX_RDEPS_GREP_CANDIDATES: u64 = 1024;
const MAX_SWEEP_PUBLISH_REBUILDS: usize = 3;
const MAX_GRAPH_FILES: usize = 100_000;
const MAX_GRAPH_PATH_BYTES: usize = 64 * 1024;
const MAX_GRAPH_QUERY_BYTES: usize = 1024 * 1024;
const MAX_GRAPH_DEPTH: u32 = 64;
const MAX_GRAPH_RESULTS: u64 = 100_000;
const MAX_GRAPH_BUILD_BYTES: usize = 256 * 1024 * 1024;

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
        validate_graph_params(params)?;
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
        let pin = graph_state.root(&root_path);
        let root = &pin.root;
        if matches!(params.op, GraphOp::Status) {
            return Ok(status_query(engine, &root_path, root));
        }

        let answer = query_ready_root(engine, &graph_state, &root_path, root, params, cancel)?;
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
                root,
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

fn validate_graph_params(params: &GraphParams) -> ToolResult<()> {
    if params.root.len() > MAX_GRAPH_PATH_BYTES {
        return Err(ToolError::invalid("graph root path exceeds 64 KiB"));
    }
    if params.files.len() > MAX_GRAPH_FILES {
        return Err(ToolError::invalid(
            "graph files view exceeds 100000 entries",
        ));
    }
    if params
        .files
        .iter()
        .any(|path| path.len() > MAX_GRAPH_PATH_BYTES)
    {
        return Err(ToolError::invalid("graph file path exceeds 64 KiB"));
    }
    let (text_len, depth, limit) = match &params.op {
        GraphOp::Symbols { path } | GraphOp::Outline { path } => (path.len(), 0, 0),
        GraphOp::Search { query, limit } => (query.len(), 0, *limit),
        GraphOp::Definitions { name, limit } => (name.len(), 0, *limit),
        GraphOp::Deps { path, depth }
        | GraphOp::Rdeps { path, depth, .. }
        | GraphOp::Neighborhood { path, depth } => (path.len(), *depth, 0),
        GraphOp::Status => (0, 0, 0),
    };
    if text_len > MAX_GRAPH_QUERY_BYTES {
        return Err(ToolError::invalid("graph query text exceeds 1 MiB"));
    }
    if depth > MAX_GRAPH_DEPTH {
        return Err(ToolError::invalid("graph depth exceeds 64"));
    }
    if limit > MAX_GRAPH_RESULTS {
        return Err(ToolError::invalid("graph result limit exceeds 100000"));
    }
    Ok(())
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
    active_builds: Mutex<usize>,
}

impl Default for GraphState {
    fn default() -> Self {
        // The registry is frozen by ownership: after construction only shared
        // references are published through this private engine extension.
        Self {
            registry: Arc::new(LanguageRegistry::bundled()),
            roots: DashMap::new(),
            access_clock: AtomicU64::new(1),
            active_builds: Mutex::new(0),
        }
    }
}

impl GraphState {
    fn root(self: &Arc<Self>, path: &Path) -> RootQueryPin {
        let root = {
            let entry = self
                .roots
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(RootGraph::new(path)));
            let root = Arc::clone(entry.value());
            root.active_queries.fetch_add(1, Ordering::SeqCst);
            root
        };
        self.touch(&root);
        run_graph_test_hook(path, GraphTestPoint::RootPinned);
        self.evict_roots();
        RootQueryPin {
            graph_state: Arc::clone(self),
            root,
        }
    }

    fn touch(&self, root: &RootGraph) {
        let stamp = self.access_clock.fetch_add(1, Ordering::Relaxed);
        if let Some(mut sweep) = root.sweep.try_lock() {
            sweep.last_access = stamp;
        }
    }

    fn evict_roots(&self) {
        while self.roots.len() > MAX_GRAPH_ROOTS
            || self.estimated_bytes() > MAX_GRAPH_RESIDENT_BYTES
        {
            // Inspect every candidate: a busy sweep may temporarily push the
            // map over the cap, but any later query retries the eviction.
            let victim = self
                .roots
                .iter()
                .filter(|entry| entry.value().active_queries.load(Ordering::SeqCst) == 0)
                .filter_map(|entry| {
                    let last_access = entry.value().sweep.try_lock()?.last_access;
                    Some((entry.key().clone(), Arc::clone(entry.value()), last_access))
                })
                .min_by_key(|(_, _, last_access)| *last_access);
            let Some((path, root, _)) = victim else {
                break;
            };
            let removed = self.roots.remove_if(&path, |_, candidate| {
                Arc::ptr_eq(candidate, &root)
                    && candidate.active_queries.load(Ordering::SeqCst) == 0
            });
            if removed.is_none() {
                break;
            }
        }
    }

    fn reserve_build(self: &Arc<Self>) -> ToolResult<GraphBuildPermit> {
        let mut active = self.active_builds.lock();
        if *active >= MAX_GRAPH_ACTIVE_BUILDS {
            return Err(ToolError::invalid("graph build capacity is exhausted"));
        }
        *active += 1;
        Ok(GraphBuildPermit {
            graph_state: Arc::clone(self),
        })
    }

    fn estimated_bytes(&self) -> u64 {
        self.roots
            .iter()
            .map(|entry| entry.value().estimated_bytes())
            .sum()
    }
}

struct GraphBuildPermit {
    graph_state: Arc<GraphState>,
}

impl Drop for GraphBuildPermit {
    fn drop(&mut self) {
        let mut active = self.graph_state.active_builds.lock();
        *active = active.saturating_sub(1);
    }
}

struct RootQueryPin {
    graph_state: Arc<GraphState>,
    root: Arc<RootGraph>,
}

impl Drop for RootQueryPin {
    fn drop(&mut self) {
        self.root.active_queries.fetch_sub(1, Ordering::SeqCst);
        // A cold build may push resident bytes over budget after root()'s
        // admission-time eviction. Retry once the query unpins the built root.
        self.graph_state.evict_roots();
    }
}

struct RootGraph {
    state: RwLock<RootState>,
    sweep: Mutex<SweepMeta>,
    // Flights only merge concurrent verifiers of the same key. They are a
    // rendezvous and must never serve as a result cache.
    rdeps_flights: Mutex<FxHashMap<RdepsFlightKey, Arc<RdepsFlight>>>,
    active_queries: AtomicU64,
}

impl RootGraph {
    fn new(root: &Path) -> Self {
        Self {
            state: RwLock::new(RootState::new(root)),
            sweep: Mutex::new(SweepMeta::default()),
            rdeps_flights: Mutex::new(FxHashMap::default()),
            active_queries: AtomicU64::new(0),
        }
    }

    fn estimated_bytes(&self) -> u64 {
        let state = self.state.read();
        state.estimated_bytes()
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
        let jsconfig = root.join("jsconfig.json");
        let mut config_records = FxHashMap::default();
        for config in [&tsconfig, &jsconfig] {
            config_records.insert(
                CompactString::from(config.to_string_lossy().as_ref()),
                stat_record(config),
            );
        }
        Self {
            phase: RootPhase::Uninitialized,
            index: SymbolIndex::new(),
            graph: ModuleGraph::new(),
            resolvers: root_resolvers(root, &[]),
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

    fn estimated_bytes(&self) -> u64 {
        let paths = self
            .records
            .keys()
            .chain(self.config_records.keys())
            .chain(self.supported_universe.iter())
            .map(|path| path.len() as u64)
            .sum::<u64>();
        let nodes = self.graph.paths().count() as u64;
        let edges = self.graph.edge_count() as u64;
        let symbols = self.index.symbol_count() as u64;
        let files = self.index.file_count() as u64;
        // Conservative structural accounting. String capacities and allocator
        // metadata vary by platform, so multiply logical payloads rather than
        // claiming exact RSS. The hard root-count cap remains a second bound.
        paths
            .saturating_mul(3)
            .saturating_add(nodes.saturating_mul(512))
            .saturating_add(edges.saturating_mul(256))
            .saturating_add(symbols.saturating_mul(512))
            .saturating_add(files.saturating_mul(256))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RdepsFlightKey {
    target: CompactString,
    resolver_generation: u64,
    graph_generation: u64,
    caller_view: Option<Arc<Vec<CompactString>>>,
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
    view_excluded: bool,
}

struct RdepsGrepCandidates {
    candidates: Vec<RdepsGrepCandidate>,
    limit_reached: bool,
}

struct RdepsGrepCandidate {
    path: PathBuf,
    line: u64,
}

struct PreparedRdepsRepair {
    candidate: RdepsGrepCandidate,
    relative: CompactString,
    analysis: FileAnalysis,
    record: StatRecord,
}

struct PublishedRdepsRepair {
    candidate: RdepsGrepCandidate,
    absolute: CompactString,
    outcome: RepairPublish,
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
    sweep_at: Option<Instant>,
}

struct ReadyAnswer {
    result: GraphResult,
    freshness: Freshness,
}

fn query_ready_root(
    engine: &Engine,
    graph_state: &Arc<GraphState>,
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
            let state = root.state.read();
            return ready_answer(
                root_path,
                &state,
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Approximate,
                    sweep_at: state.last_sweep_at,
                },
                None,
            );
        };
        sweep.last_access = graph_state.access_clock.fetch_add(1, Ordering::Relaxed);

        let still_ready = {
            let state = root.state.read();
            matches!(state.phase, RootPhase::Ready { .. })
        };
        let reusable_stamp = still_ready
            .then(|| {
                params.max_stale_ms.and_then(|max_stale_ms| {
                    sweep
                        .stamp(&sweep_key)
                        .filter(|stamp| elapsed_ms(stamp.at.elapsed()) <= max_stale_ms)
                        .map(|stamp| (stamp.excluded.clone(), stamp.at))
                })
            })
            .flatten();
        if let Some((excluded, sweep_at)) = reusable_stamp {
            return ready_answer(
                root_path,
                &root.state.read(),
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Exact,
                    sweep_at: Some(sweep_at),
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
        && let Some((excluded, sweep_at)) = sweep
            .stamp(&sweep_key)
            .map(|stamp| (stamp.excluded.clone(), stamp.at))
    {
        return ready_answer(
            root_path,
            &root.state.read(),
            params,
            Freshness {
                swept: false,
                guarantee: GraphGuarantee::Exact,
                sweep_at: Some(sweep_at),
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
    graph_state: &Arc<GraphState>,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    cancel: &CancelToken,
    sweep: &mut SweepMeta,
    cold: bool,
) -> ToolResult<ReadyAnswer> {
    let started = Instant::now();
    let _build_permit = graph_state.reserve_build()?;
    let mut cold_guard = cold.then(|| ColdBuildGuard::new(root));
    if cold {
        run_graph_test_hook(root_path, GraphTestPoint::ColdBuildStarted);
    }
    let mut rebuilds = 0;
    let (excluded, published_at) = loop {
        let snapshot = sweep_snapshot(root_path, &root.state.read());
        let snapshot_epoch = snapshot.graph_generation;
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
            let state = root.state.read();
            return ready_answer(
                root_path,
                &state,
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Approximate,
                    sweep_at: state.last_sweep_at,
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
            let state = root.state.read();
            return ready_answer(
                root_path,
                &state,
                params,
                Freshness {
                    swept: false,
                    guarantee: GraphGuarantee::Approximate,
                    sweep_at: state.last_sweep_at,
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

        // The map guard fences graph_clear/eviction through the complete
        // publication. Readers see either the old or the new generation.
        let mut state = root.state.write();
        if state.graph_generation != snapshot_epoch {
            // A concurrent rdeps repair published between our snapshot and
            // now, so the delta was computed against a stale world.
            drop(state);
            drop(attached_root);
            rebuilds += 1;
            if rebuilds > MAX_SWEEP_PUBLISH_REBUILDS {
                if cold {
                    return Err(ToolError::internal(
                        "graph sweep publication kept losing the rebase race; retry",
                    ));
                }
                let state = root.state.read();
                return ready_answer(
                    root_path,
                    &state,
                    params,
                    Freshness {
                        swept: false,
                        guarantee: GraphGuarantee::Approximate,
                        sweep_at: state.last_sweep_at,
                    },
                    None,
                );
            }
            continue;
        }

        let resolver_inputs_changed = config_changed || state.rust_crate_roots != rust_crate_roots;
        let index_changed = !upserts.is_empty() || !removes.is_empty();
        let topology_changed = index_changed || resolver_inputs_changed;
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
                *resolvers = root_resolvers(root_path, &rust_crate_roots);
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
        let components = if topology_changed || state.components.graph_generation != snapshot_epoch
        {
            component_count(root_path, &state.index, &state.graph)
        } else {
            state.components.components
        };
        state.components = ComponentsCache {
            graph_generation: generation,
            components,
        };
        if index_changed {
            state.languages = language_statuses(&state.index, &graph_state.registry);
        }
        state.last_sweep_at = Some(published_at);
        if cold {
            state.build_duration_us = Some(saturating_u64(started.elapsed().as_micros()));
        }
        state.phase = RootPhase::Ready { generation };
        drop(state);
        drop(attached_root);
        break (excluded, published_at);
    };

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
            sweep_at: Some(published_at),
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
    graph_generation: u64,
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
        graph_generation: state.graph_generation,
        indexed_hashes,
        indexed_paths,
    }
}

fn analysis_bytes(analysis: &FileAnalysis) -> usize {
    analysis
        .path
        .len()
        .saturating_add(
            analysis
                .language
                .as_ref()
                .map_or(0, |language| language.len()),
        )
        .saturating_add(
            analysis
                .symbols
                .iter()
                .map(|symbol| symbol.name.len().saturating_add(size_of::<Symbol>()))
                .sum::<usize>(),
        )
        .saturating_add(
            analysis
                .imports
                .iter()
                .map(|import| import.specifier.len().saturating_add(size_of_val(import)))
                .sum::<usize>(),
        )
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
            .map(|path| caller_universe_path(root, path).map(|(path, _)| path))
            .collect::<ToolResult<_>>()?;
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
        if entry.files.len() > MAX_GRAPH_FILES {
            return Err(ToolError::invalid(
                "graph implicit universe exceeds 100000 files",
            ));
        }
        (entry.files.as_ref().clone(), hit)
    };

    if universe
        .iter()
        .map(|path| path.as_os_str().len())
        .sum::<usize>()
        > MAX_GRAPH_QUERY_BYTES.saturating_mul(64)
    {
        return Err(ToolError::invalid(
            "graph universe path bytes exceed 64 MiB",
        ));
    }

    let mut relative_universe = FxHashSet::default();
    let mut upserts = Vec::new();
    let mut removes = FxHashSet::default();
    let mut excluded = FxHashSet::default();
    let mut supported_universe = FxHashSet::default();
    let mut parser_pool = ParserPool::new(registry);
    let mut build_bytes = 0usize;
    let mut counters = RootCounters {
        universe_files: universe.len() as u64,
        walk_cache_hit,
        ..RootCounters::default()
    };
    let trust = engine.stat_free(root);

    for path in &universe {
        cancel.check()?;
        let (path, relative) = match classify_relative_path(root, path) {
            RelativePath::Inside { path, relative } => (path, relative),
            RelativePath::OutsideRoot => return Err(graph_universe_outside_root(path)),
            RelativePath::NonUtf8 => {
                if registry.supports_symbols(path) {
                    counters.failed_files += 1;
                } else {
                    counters.unsupported_files += 1;
                }
                continue;
            }
        };
        relative_universe.insert(relative.clone());

        if !registry.supports_symbols(&path) {
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
            .get_bounded_trusting(&path, MAX_GRAPH_FILE_BYTES, trust);
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
        let absolute = CompactString::from(
            path.to_str()
                .expect("classified graph universe paths are UTF-8"),
        );
        let analysis = analyze_source(source, absolute.as_str(), hash, &mut parser_pool);
        let analysis_bytes = analysis_bytes(&analysis);
        if analysis_bytes > MAX_GRAPH_BUILD_BYTES.saturating_sub(build_bytes) {
            return Err(ToolError::invalid("graph build exceeds byte limit"));
        }
        build_bytes += analysis_bytes;
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
    let caller_view = query_filter(root, &params.files, None)?;
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
            let (result, guarantee) = deps_result(
                root,
                state,
                path,
                *depth,
                params.include_basis,
                &filter,
                caller_view.as_ref(),
            )?;
            if result.edges.len().saturating_add(result.unresolved.len())
                > MAX_GRAPH_RESULTS as usize
                || result.coverage.basis.len() > MAX_GRAPH_RESULTS as usize
            {
                return Err(ToolError::invalid("graph dependency result exceeds limit"));
            }
            (GraphOutput::Deps(result), Some(guarantee))
        }
        GraphOp::Rdeps {
            path,
            depth,
            verify: _,
        } => {
            let (mut result, guarantee) = rdeps_result(
                root,
                state,
                path,
                *depth,
                params.include_basis,
                &filter,
                caller_view.as_ref(),
            )?;
            if result.importers.len() > MAX_GRAPH_RESULTS as usize
                || result.coverage.basis.len() > MAX_GRAPH_RESULTS as usize
            {
                return Err(ToolError::invalid(
                    "graph reverse dependency result exceeds limit",
                ));
            }
            let freshness_guarantee = graph_meta(root, state, freshness).guarantee;
            result.verified =
                guarantee == Guarantee::Exact && freshness_guarantee == GraphGuarantee::Exact;
            (GraphOutput::Rdeps(result), Some(guarantee))
        }
        GraphOp::Neighborhood { path, depth } => {
            let (result, guarantee) = neighborhood_result(
                root,
                state,
                path,
                *depth,
                params.include_basis,
                &filter,
                caller_view.as_ref(),
            )?;
            if result.nodes.len().saturating_add(result.edges.len()) > MAX_GRAPH_RESULTS as usize
                || result.coverage.basis.len() > MAX_GRAPH_RESULTS as usize
            {
                return Err(ToolError::invalid(
                    "graph neighborhood result exceeds limit",
                ));
            }
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

struct TraversedNeighborhood {
    nodes: Vec<CompactString>,
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
    caller_view: Option<&FxHashSet<CompactString>>,
) -> ToolResult<(GraphDepsResult, Guarantee)> {
    let (absolute, node) = query_graph_node(root, state, requested_path, filter)?;
    let traversed = traverse_deps(&state.graph, root, absolute.as_str(), depth, caller_view)
        .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    for (edge, guarantee) in traversed.edges {
        match map_dep_edge(root, state, edge, guarantee) {
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
    caller_view: Option<&FxHashSet<CompactString>>,
) -> ToolResult<(GraphRdepsResult, Guarantee)> {
    let (absolute, node) = query_graph_node(root, state, requested_path, filter)?;
    let traversed = traverse_rdeps(&state.graph, root, absolute.as_str(), depth, caller_view)
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
    caller_view: Option<&FxHashSet<CompactString>>,
) -> ToolResult<(GraphNeighborhoodResult, Guarantee)> {
    let (absolute, center) = query_graph_node(root, state, requested_path, filter)?;
    let traversed =
        traverse_neighborhood(&state.graph, root, absolute.as_str(), depth, caller_view)
            .ok_or_else(|| ToolError::not_found(absolute.to_string()))?;
    let nodes = traversed
        .nodes
        .iter()
        .filter_map(|path| state.graph.node(path))
        .filter_map(|node| graph_node(root, &state.index, node))
        .collect();
    let edges = traversed
        .edges
        .into_iter()
        .filter_map(
            |(edge, guarantee)| match map_dep_edge(root, state, edge, guarantee) {
                MappedDepEdge::Resolved(edge) => Some(edge),
                MappedDepEdge::Unresolved(_) => None,
            },
        )
        .collect();
    Ok((
        GraphNeighborhoodResult {
            center,
            nodes,
            edges,
            coverage: graph_coverage(root, traversed.coverage, include_basis),
        },
        traversed.guarantee,
    ))
}

fn traverse_deps(
    graph: &ModuleGraph,
    root: &Path,
    path: &str,
    depth: u32,
    caller_view: Option<&FxHashSet<CompactString>>,
) -> Option<TraversedEdges> {
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
    let mut escaped_view = false;
    while let Some((current, distance)) = queue.pop_front() {
        if !expanded.insert(current.clone()) {
            continue;
        }
        if graph_path_escapes_view(root, current.as_str(), caller_view) {
            escaped_view = true;
            continue;
        }
        let deps = graph.deps(current.as_str())?;
        guarantee = weakest_guarantee(guarantee, deps.guarantee);
        for edge in deps.edges {
            if edges.len() >= MAX_GRAPH_RESULTS as usize {
                return None;
            }
            if let EdgeTargetOwned::Path(target) = &edge.to {
                if graph_path_escapes_view(root, target.as_str(), caller_view) {
                    escaped_view = true;
                    continue;
                }
                reached.insert(target.clone());
                if distance + 1 < depth {
                    queue.push_back((target.clone(), distance + 1));
                }
            }
            edges.push((edge, deps.guarantee));
        }
    }
    if escaped_view {
        guarantee = Guarantee::Approximate;
    }
    Some(TraversedEdges {
        edges,
        guarantee,
        coverage: coverage_for_paths(graph, &reached),
    })
}

fn traverse_rdeps(
    graph: &ModuleGraph,
    root: &Path,
    path: &str,
    depth: u32,
    caller_view: Option<&FxHashSet<CompactString>>,
) -> Option<TraversedEdges> {
    graph.node(path)?;
    let mut reached = FxHashSet::default();
    reached.insert(CompactString::from(path));
    if depth == 0 {
        // Reverse-dependency exactness is a root-wide property; an empty
        // answer still must not claim more than the store can prove.
        let guarantee = graph.rdeps_guarantee_for(path)?;
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
    let mut escaped_view = false;
    while let Some((current, distance)) = queue.pop_front() {
        if !expanded.insert(current.clone()) {
            continue;
        }
        if graph_path_escapes_view(root, current.as_str(), caller_view) {
            escaped_view = true;
            continue;
        }
        let remaining = MAX_GRAPH_RESULTS as usize - edges.len();
        let rdeps = graph.rdeps_bounded(current.as_str(), remaining.saturating_add(1))?;
        if rdeps.edges.len() > remaining {
            return None;
        }
        guarantee = weakest_guarantee(guarantee, rdeps.guarantee);
        for edge in rdeps.edges {
            if edges.len() >= MAX_GRAPH_RESULTS as usize {
                return None;
            }
            if graph_path_escapes_view(root, edge.from.as_str(), caller_view) {
                escaped_view = true;
                continue;
            }
            reached.insert(edge.from.clone());
            if distance + 1 < depth {
                queue.push_back((edge.from.clone(), distance + 1));
            }
            edges.push((edge, rdeps.guarantee));
        }
    }
    if escaped_view {
        guarantee = Guarantee::Approximate;
    }
    Some(TraversedEdges {
        edges,
        guarantee,
        coverage: coverage_for_paths(graph, &reached),
    })
}

fn traverse_neighborhood(
    graph: &ModuleGraph,
    root: &Path,
    path: &str,
    depth: u32,
    caller_view: Option<&FxHashSet<CompactString>>,
) -> Option<TraversedNeighborhood> {
    graph.node(path)?;
    let mut reached = FxHashSet::default();
    reached.insert(CompactString::from(path));
    let mut queue = VecDeque::from([(CompactString::from(path), 0_u32)]);
    let mut escaped_view = false;

    while let Some((current, distance)) = queue.pop_front() {
        if distance == depth {
            continue;
        }
        if graph_path_escapes_view(root, current.as_str(), caller_view) {
            escaped_view = true;
            continue;
        }

        let deps = graph.deps(current.as_str())?;
        for edge in deps.edges {
            let EdgeTargetOwned::Path(target) = edge.to else {
                continue;
            };
            if graph_path_escapes_view(root, target.as_str(), caller_view) {
                escaped_view = true;
                continue;
            }
            if reached.len() >= MAX_GRAPH_RESULTS as usize {
                return None;
            }
            if reached.insert(target.clone()) {
                queue.push_back((target, distance + 1));
            }
        }

        let remaining = MAX_GRAPH_RESULTS as usize - reached.len();
        let rdeps = graph.rdeps_bounded(current.as_str(), remaining.saturating_add(1))?;
        if rdeps.edges.len() > remaining {
            return None;
        }
        for edge in rdeps.edges {
            if graph_path_escapes_view(root, edge.from.as_str(), caller_view) {
                escaped_view = true;
                continue;
            }
            if reached.len() >= MAX_GRAPH_RESULTS as usize {
                return None;
            }
            if reached.insert(edge.from.clone()) {
                queue.push_back((edge.from, distance + 1));
            }
        }
    }

    let mut nodes: Vec<_> = reached.iter().cloned().collect();
    nodes.sort_unstable();
    let mut edges = Vec::new();
    let mut guarantee = Guarantee::Exact;
    for source in &nodes {
        let deps = graph.deps(source.as_str())?;
        guarantee = weakest_guarantee(guarantee, deps.guarantee);
        if depth == 0 {
            continue;
        }
        if edges.len() >= MAX_GRAPH_RESULTS as usize {
            return None;
        }
        let remaining = MAX_GRAPH_RESULTS as usize - edges.len();
        edges.extend(
            deps.edges
                .into_iter()
                .filter_map(|edge| {
                    let EdgeTargetOwned::Path(target) = &edge.to else {
                        return None;
                    };
                    reached
                        .contains(target.as_str())
                        .then_some((edge, deps.guarantee))
                })
                .take(remaining),
        );
    }
    if depth != 0 {
        guarantee = weakest_guarantee(guarantee, graph.rdeps_guarantee_for(path)?);
    }
    if escaped_view {
        guarantee = Guarantee::Approximate;
    }

    Some(TraversedNeighborhood {
        nodes,
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
    let absolute = CompactString::from(
        absolute
            .to_str()
            .expect("classified graph query paths are UTF-8"),
    );
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

fn map_dep_edge(
    root: &Path,
    state: &RootState,
    edge: DepEdge,
    guarantee: Guarantee,
) -> MappedDepEdge {
    let from_node_id = graph_node_id(root, state, edge.from.as_str())
        .expect("dependency edge source must refer to a graph node");
    let (to, to_node_id, to_kind) = match edge.to {
        EdgeTargetOwned::Path(path) => (
            absolute_graph_path(root, path.as_str()).to_string(),
            Some(
                graph_node_id(root, state, path.as_str())
                    .expect("dependency edge target must refer to a graph node"),
            ),
            "path",
        ),
        EdgeTargetOwned::External(package) => (package.to_string(), None, "external"),
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
        from_node_id,
        to,
        to_node_id,
        to_kind: to_kind.to_owned(),
        specifier: edge.specifier.to_string(),
        kind: import_kind(edge.kind).to_owned(),
        line: u64::from(edge.line),
        guarantee: wire_guarantee(guarantee),
    })
}

fn graph_node_id(root: &Path, state: &RootState, path: &str) -> Option<String> {
    state
        .graph
        .node(path)
        .and_then(|node| graph_node(root, &state.index, node))
        .map(|node| node.node_id)
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

fn graph_path_escapes_view(
    root: &Path,
    path: &str,
    caller_view: Option<&FxHashSet<CompactString>>,
) -> bool {
    let Some(caller_view) = caller_view else {
        return false;
    };
    relative_path(root, Path::new(path))
        .is_none_or(|relative| !caller_view.contains(relative.as_str()))
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
    caller_view: Option<&FxHashSet<CompactString>>,
) -> RdepsFlightKey {
    let caller_view = caller_view.map(|view| {
        let mut paths: Vec<_> = view.iter().cloned().collect();
        paths.sort_unstable();
        Arc::new(paths)
    });
    RdepsFlightKey {
        target,
        resolver_generation: state.graph.resolver_generation(),
        graph_generation: state.graph.generation(),
        caller_view,
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
    graph_state: &Arc<GraphState>,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    requested_path: &str,
    depth: u32,
    freshness: Freshness,
    cancel: &CancelToken,
) -> ToolResult<GraphResult> {
    let caller_view = query_filter(root_path, &params.files, None)?;
    let (target, target_relative) = query_path(root_path, requested_path)?;
    if caller_view
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(target_relative.as_str()))
    {
        return Err(ToolError::not_found(target.display().to_string()));
    }
    let target = CompactString::from(target.to_string_lossy().as_ref());
    let mut outcome = loop {
        let key = rdeps_flight_key(
            &root.state.read(),
            target.clone(),
            params,
            caller_view.as_ref(),
        );
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

        if leader {
            let outcome = run_rdeps_repair(
                engine,
                graph_state,
                root_path,
                root,
                params,
                target.as_str(),
                caller_view.as_ref(),
                cancel,
            );
            {
                let mut flights = root.rdeps_flights.lock();
                if flights
                    .get(&key)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &flight))
                {
                    flights.remove(&key);
                }
            }
            *flight.result.lock() = Some(outcome.clone());
            flight.ready.notify_all();
            break outcome?;
        } else {
            run_graph_test_hook(root_path, GraphTestPoint::RdepsFollowerWaiting);
            let mut result = flight.result.lock();
            let outcome = loop {
                if let Some(outcome) = result.as_ref() {
                    break outcome.clone();
                }
                flight
                    .ready
                    .wait_for(&mut result, Duration::from_millis(10));
                cancel.check()?;
            };
            drop(result);
            match outcome {
                Err(error) if error.is_cancelled() => continue,
                outcome => break outcome?,
            }
        }
    };
    if outcome.generation_changed {
        cancel.check()?;
    }

    let state = root.state.read();
    let (mut result, guarantee) = rdeps_result(
        root_path,
        &state,
        requested_path,
        depth,
        params.include_basis,
        &caller_view,
        caller_view.as_ref(),
    )?;
    outcome.approximate_entries.retain(|entry| {
        !graph_path_escapes_view(root_path, entry.node.path.as_str(), caller_view.as_ref())
    });
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
    if outcome.view_excluded {
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

#[allow(clippy::too_many_arguments)]
fn run_rdeps_repair(
    engine: &Engine,
    graph_state: &Arc<GraphState>,
    root_path: &Path,
    root: &Arc<RootGraph>,
    params: &GraphParams,
    target: &str,
    caller_view: Option<&FxHashSet<CompactString>>,
    cancel: &CancelToken,
) -> ToolResult<RdepsRepairOutcome> {
    run_graph_test_hook(root_path, GraphTestPoint::RdepsRepairStarted);
    let _repair_permit = graph_state.reserve_build()?;
    let RdepsGrepCandidates {
        candidates,
        limit_reached,
    } = rdeps_grep_candidates(engine, graph_state, root_path, params, target, cancel)?;
    let mut parser_pool = ParserPool::new(&graph_state.registry);
    let mut approximate_entries = Vec::new();
    let mut prepared = Vec::new();
    let mut repaired = 0_usize;
    let mut repair_bytes = 0usize;
    let mut repair_truncated = limit_reached;
    let mut generation_changed = false;
    let mut view_excluded = false;
    let trust = engine.stat_free(root_path);

    for candidate in candidates {
        cancel.check()?;
        let Some(relative) = relative_path(root_path, &candidate.path) else {
            continue;
        };
        if caller_view.is_some_and(|allowed| !allowed.contains(relative.as_str())) {
            view_excluded = true;
            continue;
        }
        let absolute = absolute_graph_path(
            root_path,
            candidate
                .path
                .to_str()
                .expect("relative graph repair paths are UTF-8"),
        );
        let loaded =
            engine
                .files()
                .get_bounded_trusting(&candidate.path, MAX_GRAPH_FILE_BYTES, trust);
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
                approximate_entries.push(approximate_rdep_entry(root_path, root, &candidate));
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
                approximate_entries.push(approximate_rdep_entry(root_path, root, &candidate));
                continue;
            }
        };
        let Some(source) = entry.as_str() else {
            approximate_entries.push(approximate_rdep_entry(root_path, root, &candidate));
            continue;
        };
        let analysis = analyze_source(
            source,
            absolute.as_str(),
            entry.content_hash(),
            &mut parser_pool,
        );
        let bytes = analysis_bytes(&analysis);
        if bytes > MAX_GRAPH_BUILD_BYTES.saturating_sub(repair_bytes) {
            repair_truncated = true;
            break;
        }
        repair_bytes += bytes;
        if analysis.language.is_none() {
            approximate_entries.push(approximate_rdep_entry(root_path, root, &candidate));
            continue;
        }
        prepared.push(PreparedRdepsRepair {
            candidate,
            relative,
            analysis,
            record: StatRecord {
                mtime_ns: entry.mtime_ns,
                size: entry.size,
            },
        });
    }

    let published = publish_rdeps_repairs(graph_state, root_path, root, prepared)?;
    for PublishedRdepsRepair {
        candidate,
        absolute,
        outcome,
    } in published
    {
        match outcome {
            RepairPublish::Published => generation_changed = true,
            RepairPublish::AlreadyCurrent => {}
            RepairPublish::InputStale => {
                // The analyzed bytes no longer describe the on-disk file.
                // Publishing them could insert a node no sweep is obliged to
                // remove. Skip grep evidence for a file that may be gone and
                // report the repair as incomplete.
                repair_truncated = true;
                continue;
            }
        }
        if !rdeps_node_is_structurally_exact(&root.state.read().graph, absolute.as_str()) {
            approximate_entries.push(approximate_rdep_entry(root_path, root, &candidate));
        }
    }

    Ok(RdepsRepairOutcome {
        approximate_entries,
        completed: !repair_truncated,
        repair_truncated,
        generation_changed,
        view_excluded,
    })
}

fn rdeps_grep_candidates(
    engine: &Engine,
    graph_state: &GraphState,
    root_path: &Path,
    params: &GraphParams,
    target: &str,
    cancel: &CancelToken,
) -> ToolResult<RdepsGrepCandidates> {
    let needles = rdeps_needles(Path::new(target));
    let pattern = needles
        .iter()
        .map(|needle| regex::escape(needle))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!("(?:{pattern})");
    let globs = registry_globs(&graph_state.registry);

    let grep = grep_cancellable(
        engine,
        &GrepParams {
            pattern,
            path: root_path.display().to_string(),
            mode: GrepMode::Content,
            globs,
            max_count: Some(1),
            max_total_count: Some(MAX_RDEPS_GREP_CANDIDATES),
            hidden: params.hidden,
            respect_gitignore: params.respect_gitignore,
            follow_symlinks: params.follow_symlinks,
            ..GrepParams::default()
        },
        cancel,
    )?;
    Ok(RdepsGrepCandidates {
        candidates: grep
            .files
            .into_iter()
            .filter_map(|hit| {
                let line = hit.lines.iter().find(|line| line.is_match)?.line_number;
                Some(RdepsGrepCandidate {
                    path: PathBuf::from(hit.path),
                    line,
                })
            })
            .collect(),
        limit_reached: grep.limit_reached,
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

fn approximate_rdep_entry(
    root_path: &Path,
    root: &RootGraph,
    candidate: &RdepsGrepCandidate,
) -> GraphRdepEntry {
    let state = root.state.read();
    let absolute = absolute_graph_path(
        root_path,
        candidate
            .path
            .to_str()
            .expect("relative graph repair paths are UTF-8"),
    );
    let node = state
        .graph
        .node(absolute.as_str())
        .and_then(|node| graph_node(root_path, &state.index, node))
        .unwrap_or_else(|| {
            let relative = relative_path(root_path, &candidate.path);
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
    GraphRdepEntry {
        node,
        specifier: None,
        line: candidate.line,
        guarantee: GraphGuarantee::Approximate,
    }
}

enum RepairPublish {
    Published,
    AlreadyCurrent,
    InputStale,
}

fn publish_rdeps_repairs(
    graph_state: &GraphState,
    root_path: &Path,
    root: &Arc<RootGraph>,
    repairs: Vec<PreparedRdepsRepair>,
) -> ToolResult<Vec<PublishedRdepsRepair>> {
    if repairs.is_empty() {
        return Ok(Vec::new());
    }
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

    let mut state = root.state.write();
    let mut published = Vec::with_capacity(repairs.len());
    let mut generation_changed = false;
    for repair in repairs {
        let PreparedRdepsRepair {
            candidate,
            relative,
            analysis,
            record,
        } = repair;
        let absolute = analysis.path.clone();
        let hash = analysis.content_hash;
        let outcome = if stat_record(Path::new(analysis.path.as_str())) != Some(record) {
            RepairPublish::InputStale
        } else if state
            .index
            .contains(relative.as_str(), hash, graph_state.registry.generation())
            && state.graph.contains(analysis.path.as_str(), hash)
        {
            RepairPublish::AlreadyCurrent
        } else {
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
            generation_changed = true;
            RepairPublish::Published
        };
        published.push(PublishedRdepsRepair {
            candidate,
            absolute,
            outcome,
        });
    }
    if generation_changed {
        let generation = state.graph_generation;
        state.components = ComponentsCache {
            graph_generation: generation,
            components: component_count(root_path, &state.index, &state.graph),
        };
        state.languages = language_statuses(&state.index, &graph_state.registry);
        state.phase = RootPhase::Ready { generation };
    }
    Ok(published)
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
    let fetch_limit = limit.saturating_add(1);
    let found = match filter {
        Some(allowed) => state
            .index
            .search_filtered(query, fetch_limit, |path| allowed.contains(path)),
        None => state.index.search(query, fetch_limit),
    };
    let mut symbols: Vec<GraphSymbol> = found
        .into_iter()
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
        .take(limit.saturating_add(1))
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

fn root_resolvers(root: &Path, rust_crate_roots: &[CompactString]) -> ResolverSet {
    let tsconfig = root.join("tsconfig.json");
    let jsconfig = root.join("jsconfig.json");
    let configured_js = if tsconfig.is_file() {
        Some(tsconfig)
    } else if jsconfig.is_file() {
        Some(jsconfig)
    } else {
        None
    };
    ResolverSet {
        js: Some(js_resolver(JsResolveOptions {
            tsconfig: configured_js,
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
    dependencies.extend(
        ["tsconfig.json", "jsconfig.json"]
            .map(|name| CompactString::from(root.join(name).to_string_lossy().as_ref())),
    );
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
        sweep_age_ms: sweep_age(freshness.sweep_at).unwrap_or(0),
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
                sweep_at: state.last_sweep_at,
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
        .map(|path| caller_universe_path(root, path).map(|(_, relative)| relative))
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
    RootPinned,
    RdepsRepairStarted,
    RdepsFollowerWaiting,
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
    match classify_relative_path(root, &absolute) {
        RelativePath::Inside { path, relative } => Ok((path, relative)),
        RelativePath::OutsideRoot | RelativePath::NonUtf8 => Err(ToolError::invalid(format!(
            "graph query path is outside root: {}",
            absolute.display()
        ))
        .with_path(absolute.display().to_string())),
    }
}

fn relative_path(root: &Path, path: &Path) -> Option<CompactString> {
    match classify_relative_path(root, path) {
        RelativePath::Inside { relative, .. } => Some(relative),
        RelativePath::OutsideRoot | RelativePath::NonUtf8 => None,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum RelativePath {
    OutsideRoot,
    NonUtf8,
    Inside {
        path: PathBuf,
        relative: CompactString,
    },
}

fn classify_relative_path(root: &Path, path: &Path) -> RelativePath {
    let Some(root) = lexical_normalize(root) else {
        return RelativePath::OutsideRoot;
    };
    let Some(path) = lexical_normalize(path) else {
        return RelativePath::OutsideRoot;
    };
    let Ok(relative) = path.strip_prefix(&root) else {
        return RelativePath::OutsideRoot;
    };
    if relative.as_os_str().is_empty() {
        return RelativePath::OutsideRoot;
    }
    let (Some(_), Some(relative)) = (path.to_str(), relative.to_str()) else {
        return RelativePath::NonUtf8;
    };
    let relative = CompactString::from(relative);
    RelativePath::Inside { path, relative }
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    return None;
                }
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Some(normalized)
}

fn caller_universe_path(root: &Path, supplied: &str) -> ToolResult<(PathBuf, CompactString)> {
    let supplied = Path::new(supplied);
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        root.join(supplied)
    };
    match classify_relative_path(root, &candidate) {
        RelativePath::Inside { path, relative } => {
            if let Ok(canonical_path) = std::fs::canonicalize(&path) {
                let canonical_root = std::fs::canonicalize(root).map_err(|error| {
                    ToolError::internal(format!(
                        "could not canonicalize graph root {}: {error}",
                        root.display()
                    ))
                    .with_path(root.display().to_string())
                })?;
                if !canonical_path.starts_with(&canonical_root) {
                    return Err(graph_universe_outside_root(&candidate));
                }
            }
            Ok((path, relative))
        }
        RelativePath::OutsideRoot | RelativePath::NonUtf8 => {
            Err(graph_universe_outside_root(&candidate))
        }
    }
}

fn graph_universe_outside_root(path: &Path) -> ToolError {
    ToolError::invalid(format!(
        "graph universe path is outside root: {}",
        path.display()
    ))
    .with_path(path.display().to_string())
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
        GraphState, GraphTestHook, GraphTestPoint, MAX_GRAPH_ROOTS, RootState, SweepKey, SweepMeta,
        graph, graph_cancellable, graph_clear, graph_test_root_count, graph_test_set_hook,
        rdeps_flight_key,
    };
    #[cfg(unix)]
    use super::{RelativePath, classify_relative_path};
    use compact_str::CompactString;
    use hearth_core::{CancelToken, Engine, EngineConfig};
    use hearth_proto::{ErrorKind, GraphGuarantee, GraphOp, GraphOutput, GraphParams, GraphResult};
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

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

    fn rdeps_query(root: &Path, target: &Path, files: &[&Path]) -> GraphParams {
        let mut params = GraphParams::new(
            root.display().to_string(),
            GraphOp::Rdeps {
                path: target.display().to_string(),
                depth: 1,
                verify: true,
            },
        );
        params.files = files
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        params
    }

    fn rdeps_importer_paths(result: &GraphResult) -> Vec<String> {
        let GraphOutput::Rdeps(rdeps) = &result.output else {
            panic!("expected rdeps");
        };
        rdeps
            .importers
            .iter()
            .map(|entry| entry.node.path.clone())
            .collect()
    }

    fn root_graph(engine: &Engine, root: &Path) -> Arc<super::RootGraph> {
        let graph_state = engine.extension::<GraphState>();
        let entry = graph_state
            .roots
            .get(root)
            .expect("graph root was published");
        Arc::clone(entry.value())
    }

    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_relative_path_is_classified_without_a_lossy_key() {
        let root = Path::new("/tmp/hearth-graph-relative-path-test");
        let path = root.join(OsStr::from_bytes(b"a\xff.rs"));

        assert_eq!(classify_relative_path(root, &path), RelativePath::NonUtf8);
    }

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
                _ => {}
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
        let loser_result = second.join().unwrap().unwrap();
        assert_eq!(loser_result.meta.guarantee, GraphGuarantee::Exact);
        assert_eq!(loser_result.meta.universe_files, 1);
        assert_eq!(loser_result.meta.indexed_files, 1);
        let GraphOutput::Search(search) = &loser_result.output else {
            panic!("expected a search result");
        };
        assert_eq!(search.symbols.len(), 1);
        assert_eq!(search.symbols[0].name, "symbol");
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
                GraphTestPoint::ColdWaitLockAcquired if observed_busy.load(Ordering::Acquire) => {
                    cancel.cancel();
                }
                GraphTestPoint::BeforePublish => {}
                _ => {}
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
    fn walk_snapshot_reuse_reports_the_walk_stamp_age() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        seed(&root, "src/lib.rs");
        let engine = engine();

        graph(&engine, &query(&root)).unwrap();

        let mut view_query = query(&root);
        view_query.files = vec![root.join("src/lib.rs").display().to_string()];
        graph(&engine, &view_query).unwrap();

        let graph_state = engine.extension::<GraphState>();
        let root_graph = graph_state
            .roots
            .get(&root)
            .expect("graph root was published");
        let old_walk_stamp = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("test instant supports a ten-second lookback");
        root_graph
            .sweep
            .lock()
            .last_walk_sweep
            .as_mut()
            .expect("walk sweep stamp exists")
            .at = old_walk_stamp;
        drop(root_graph);

        let mut reused_query = query(&root);
        reused_query.max_stale_ms = Some(u64::MAX);
        let reused = graph(&engine, &reused_query).unwrap();

        assert!(!reused.meta.swept);
        assert_eq!(reused.meta.guarantee, GraphGuarantee::Exact);
        assert!(
            reused.meta.sweep_age_ms >= 9_000,
            "walk reuse reported the newer files-view sweep age: {}ms",
            reused.meta.sweep_age_ms
        );
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
        let baseline = rdeps_flight_key(&state, target.clone(), &params, None);

        params.hidden = true;
        assert_ne!(
            baseline,
            rdeps_flight_key(&state, target.clone(), &params, None)
        );
        params.hidden = false;
        params.respect_gitignore = false;
        assert_ne!(
            baseline,
            rdeps_flight_key(&state, target.clone(), &params, None)
        );
        params.respect_gitignore = true;
        params.follow_symlinks = true;
        assert_ne!(
            baseline,
            rdeps_flight_key(&state, target.clone(), &params, None)
        );
        params.follow_symlinks = false;
        let caller_view = rustc_hash::FxHashSet::from_iter([
            CompactString::from("src/target.ts"),
            CompactString::from("src/importer.ts"),
        ]);
        assert_ne!(
            baseline,
            rdeps_flight_key(&state, target, &params, Some(&caller_view))
        );
    }

    #[test]
    fn completed_rdeps_flight_is_not_reused_after_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("src/target.ts");
        let importer = root.join("src/importer.ts");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "export const targetValue = true;\n").unwrap();
        std::fs::write(
            &importer,
            "import \"./target\";\nexport const importerProbe = true;\n",
        )
        .unwrap();
        let engine = engine();
        let params = rdeps_query(&root, &target, &[&target, &importer]);
        let repair_count = Arc::new(AtomicUsize::new(0));
        let hook: GraphTestHook = {
            let repair_count = Arc::clone(&repair_count);
            Arc::new(move |point| {
                if point == GraphTestPoint::RdepsRepairStarted {
                    repair_count.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        graph_test_set_hook(&root, Some(hook));

        let first = graph(&engine, &params).unwrap();
        assert_eq!(
            rdeps_importer_paths(&first),
            [importer.display().to_string()]
        );
        let root_graph = root_graph(&engine, &root);
        assert!(root_graph.rdeps_flights.lock().is_empty());

        let second = graph(&engine, &params).unwrap();
        assert_eq!(
            rdeps_importer_paths(&second),
            [importer.display().to_string()]
        );
        assert!(root_graph.rdeps_flights.lock().is_empty());

        std::fs::write(
            &importer,
            "export const importerProbe = true;\n// still mentions target\n",
        )
        .unwrap();
        engine.invalidate_path(&importer);

        let third = graph(&engine, &params).unwrap();
        assert!(rdeps_importer_paths(&third).is_empty());
        assert_eq!(repair_count.load(Ordering::SeqCst), 3);
        assert!(root_graph.rdeps_flights.lock().is_empty());
        graph_test_set_hook(&root, None);
    }

    #[test]
    fn concurrent_rdeps_verify_queries_share_one_repair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("src/target.ts");
        let importer = root.join("src/importer.ts");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "export const targetValue = true;\n").unwrap();
        std::fs::write(
            &importer,
            "import \"./target\";\nexport const importerProbe = true;\n",
        )
        .unwrap();
        let engine = engine();
        let params = rdeps_query(&root, &target, &[&target, &importer]);
        let leader_in_repair = Arc::new(Barrier::new(2));
        let release_leader = Arc::new(Barrier::new(2));
        let follower_waiting = Arc::new(Barrier::new(2));
        let repair_count = Arc::new(AtomicUsize::new(0));
        let hook: GraphTestHook = {
            let leader_in_repair = Arc::clone(&leader_in_repair);
            let release_leader = Arc::clone(&release_leader);
            let follower_waiting = Arc::clone(&follower_waiting);
            let repair_count = Arc::clone(&repair_count);
            Arc::new(move |point| match point {
                GraphTestPoint::RdepsRepairStarted
                    if repair_count.fetch_add(1, Ordering::SeqCst) == 0 =>
                {
                    leader_in_repair.wait();
                    release_leader.wait();
                }
                GraphTestPoint::RdepsFollowerWaiting => {
                    follower_waiting.wait();
                }
                _ => {}
            })
        };
        graph_test_set_hook(&root, Some(hook));

        let leader_engine = engine.clone();
        let leader_params = params.clone();
        let leader = std::thread::spawn(move || graph(&leader_engine, &leader_params));
        leader_in_repair.wait();

        let follower_engine = engine.clone();
        let follower_params = params.clone();
        let follower = std::thread::spawn(move || graph(&follower_engine, &follower_params));
        follower_waiting.wait();
        release_leader.wait();

        let leader_result = leader.join().unwrap().unwrap();
        let follower_result = follower.join().unwrap().unwrap();
        let expected = [importer.display().to_string()];
        assert_eq!(rdeps_importer_paths(&leader_result), expected);
        assert_eq!(rdeps_importer_paths(&follower_result), expected);
        assert_eq!(repair_count.load(Ordering::SeqCst), 1);
        assert!(root_graph(&engine, &root).rdeps_flights.lock().is_empty());
        graph_test_set_hook(&root, None);
    }

    #[test]
    fn cancelled_leader_does_not_cancel_followers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("src/target.ts");
        let importer = root.join("src/importer.ts");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "export const targetValue = true;\n").unwrap();
        std::fs::write(
            &importer,
            "import \"./target\";\nexport const importerProbe = true;\n",
        )
        .unwrap();
        let engine = engine();
        let params = rdeps_query(&root, &target, &[&target, &importer]);
        let leader_cancel = CancelToken::new();
        let leader_in_repair = Arc::new(Barrier::new(2));
        let release_leader = Arc::new(Barrier::new(2));
        let follower_waiting = Arc::new(Barrier::new(2));
        let repair_count = Arc::new(AtomicUsize::new(0));
        let hook: GraphTestHook = {
            let leader_in_repair = Arc::clone(&leader_in_repair);
            let release_leader = Arc::clone(&release_leader);
            let follower_waiting = Arc::clone(&follower_waiting);
            let repair_count = Arc::clone(&repair_count);
            Arc::new(move |point| match point {
                GraphTestPoint::RdepsRepairStarted
                    if repair_count.fetch_add(1, Ordering::SeqCst) == 0 =>
                {
                    leader_in_repair.wait();
                    release_leader.wait();
                }
                GraphTestPoint::RdepsFollowerWaiting => {
                    follower_waiting.wait();
                }
                _ => {}
            })
        };
        graph_test_set_hook(&root, Some(hook));

        let leader_engine = engine.clone();
        let leader_params = params.clone();
        let leader_token = leader_cancel.clone();
        let leader = std::thread::spawn(move || {
            graph_cancellable(&leader_engine, &leader_params, &leader_token)
        });
        leader_in_repair.wait();

        let follower_engine = engine.clone();
        let follower_params = params.clone();
        let follower = std::thread::spawn(move || graph(&follower_engine, &follower_params));
        follower_waiting.wait();
        leader_cancel.cancel();
        release_leader.wait();

        assert_eq!(
            leader.join().unwrap().unwrap_err().kind,
            ErrorKind::Cancelled
        );
        let follower_result = follower.join().unwrap().unwrap();
        assert_eq!(
            rdeps_importer_paths(&follower_result),
            [importer.display().to_string()]
        );
        assert_eq!(repair_count.load(Ordering::SeqCst), 2);
        assert!(root_graph(&engine, &root).rdeps_flights.lock().is_empty());
        graph_test_set_hook(&root, None);
    }

    #[test]
    fn sweep_publication_rebases_over_concurrent_repair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("src/target.ts");
        let importer = root.join("src/ximport.ts");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "export const targetValue = true;\n").unwrap();
        let engine = engine();
        graph(
            &engine,
            &GraphParams::new(
                root.display().to_string(),
                GraphOp::Search {
                    query: "ximportProbe".into(),
                    limit: 10,
                },
            ),
        )
        .unwrap();

        let sweep_paused = Arc::new(Barrier::new(2));
        let release_sweep = Arc::new(Barrier::new(2));
        let before_publish_count = Arc::new(AtomicUsize::new(0));
        let hook: GraphTestHook = {
            let sweep_paused = Arc::clone(&sweep_paused);
            let release_sweep = Arc::clone(&release_sweep);
            let before_publish_count = Arc::clone(&before_publish_count);
            Arc::new(move |point| {
                if point == GraphTestPoint::BeforePublish
                    && before_publish_count.fetch_add(1, Ordering::SeqCst) == 0
                {
                    sweep_paused.wait();
                    release_sweep.wait();
                }
            })
        };
        graph_test_set_hook(&root, Some(hook));

        let sweep_engine = engine.clone();
        let sweep_params = GraphParams::new(
            root.display().to_string(),
            GraphOp::Search {
                query: "ximportProbe".into(),
                limit: 10,
            },
        );
        let sweep = std::thread::spawn(move || graph(&sweep_engine, &sweep_params));
        sweep_paused.wait();

        std::fs::write(
            &importer,
            "import \"./target\";\nexport function ximportProbe() {}\n",
        )
        .unwrap();
        engine.invalidate_path(&importer);
        let repaired = graph(&engine, &rdeps_query(&root, &target, &[])).unwrap();
        assert_eq!(
            rdeps_importer_paths(&repaired),
            [importer.display().to_string()]
        );

        std::fs::remove_file(&importer).unwrap();
        engine.invalidate_path(&importer);
        release_sweep.wait();

        let swept = sweep.join().unwrap().unwrap();
        let GraphOutput::Search(search) = swept.output else {
            panic!("expected search");
        };
        assert!(search.symbols.is_empty());

        let follow_up = graph(
            &engine,
            &GraphParams::new(
                root.display().to_string(),
                GraphOp::Search {
                    query: "ximportProbe".into(),
                    limit: 10,
                },
            ),
        )
        .unwrap();
        let GraphOutput::Search(search) = follow_up.output else {
            panic!("expected search");
        };
        assert!(search.symbols.is_empty());
        assert_eq!(follow_up.meta.guarantee, GraphGuarantee::Exact);
        graph_test_set_hook(&root, None);
    }

    #[test]
    fn graph_build_reservations_are_bounded_and_released() {
        let state = Arc::new(GraphState::default());
        let first = state.reserve_build().unwrap();
        let second = state.reserve_build().unwrap();
        let error = match state.reserve_build() {
            Ok(_) => panic!("third graph build reservation must be rejected"),
            Err(error) => error,
        };
        assert!(error.message.contains("capacity"));

        drop(first);
        let replacement = state.reserve_build().unwrap();
        drop(second);
        drop(replacement);
        assert_eq!(*state.active_builds.lock(), 0);
    }

    #[test]
    fn concurrent_insertions_preserve_the_hard_root_cap() {
        let parent = tempfile::tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        let engine = engine();
        let mut old_root_paths = Vec::new();
        for index in 0..15 {
            let root = parent.join(format!("root-{index}"));
            std::fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            graph(
                &engine,
                &GraphParams::new(root.display().to_string(), GraphOp::Status),
            )
            .unwrap();
            old_root_paths.push(root);
        }
        let old_roots: Vec<_> = old_root_paths
            .iter()
            .map(|root| root_graph(&engine, root))
            .collect();
        let sweep_guards: Vec<_> = old_roots.iter().map(|root| root.sweep.lock()).collect();

        let root_a = parent.join("root-a");
        let root_b = parent.join("root-b");
        seed(&root_a, "src/lib.rs");
        seed(&root_b, "src/lib.rs");
        let root_a = root_a.canonicalize().unwrap();
        let root_b = root_b.canonicalize().unwrap();
        let both_pinned = Arc::new(Barrier::new(2));
        for root in [&root_a, &root_b] {
            let both_pinned = Arc::clone(&both_pinned);
            let hook: GraphTestHook = Arc::new(move |point| {
                if point == GraphTestPoint::RootPinned {
                    both_pinned.wait();
                }
            });
            graph_test_set_hook(root, Some(hook));
        }

        let engine_a = engine.clone();
        let query_a = query(&root_a);
        let thread_a = std::thread::spawn(move || graph(&engine_a, &query_a));
        let engine_b = engine.clone();
        let query_b = query(&root_b);
        let thread_b = std::thread::spawn(move || graph(&engine_b, &query_b));

        assert!(thread_a.join().unwrap().is_ok());
        assert!(thread_b.join().unwrap().is_ok());
        assert!(graph_test_root_count(&engine) <= MAX_GRAPH_ROOTS);

        graph_test_set_hook(&root_a, None);
        graph_test_set_hook(&root_b, None);
        drop(sweep_guards);
        graph(&engine, &query(&root_a)).unwrap();
        assert_eq!(graph_test_root_count(&engine), MAX_GRAPH_ROOTS);
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
