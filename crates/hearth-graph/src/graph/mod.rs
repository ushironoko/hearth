//! Incremental module graph storage and deterministic dependency queries.
//!
//! Import-extraction support is supplied explicitly to [`ModuleGraph::upsert_file`].
//! Callers should pass [`LanguageRegistry::supports_imports`][crate::LanguageRegistry::supports_imports]
//! for the analyzed path. Resolver liveness is sampled from [`ResolverSet`] at
//! resolution time and stored with the node, so queries need only `&self`.

use std::collections::VecDeque;

use compact_str::CompactString;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    FileAnalysis, ImportKind, RawImport, ResolutionCompleteness, ResolutionOutcome, Resolved,
    ResolverSet, UnresolvedReason,
};

/// The result of inserting or refreshing one analyzed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The path did not previously exist in the graph.
    Inserted,
    /// An analyzed node changed or a stub was promoted.
    Updated,
    /// The content and resolver generation were already current.
    Unchanged,
}

/// Accuracy attached to a dependency query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarantee {
    /// The structural completeness conditions for the query are satisfied.
    Exact,
    /// The graph may omit dependencies.
    Approximate,
}

impl Guarantee {
    fn weakest(self, other: Self) -> Self {
        if self == Self::Exact && other == Self::Exact {
            Self::Exact
        } else {
            Self::Approximate
        }
    }
}

/// State of one module-graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    /// Source for this node was analyzed.
    Analyzed {
        /// Caller-supplied content hash.
        content_hash: u64,
        /// Whether non-literal imports prevented complete extraction.
        has_opaque_imports: bool,
        /// Registered language name, when recognized.
        language: Option<CompactString>,
    },
    /// A resolved path that has not been analyzed yet.
    Stub,
}

/// Target stored on an import edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeTarget {
    /// Slot of another module node.
    Node(u32),
    /// Package or other dependency outside the module graph.
    External(CompactString),
    /// Import that could not be resolved.
    Unresolved(UnresolvedReason),
}

/// Owned target returned by graph queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeTargetOwned {
    /// Path of another module node.
    Path(CompactString),
    /// Package or other dependency outside the module graph.
    External(CompactString),
    /// Import that could not be resolved.
    Unresolved(UnresolvedReason),
}

/// One resolved import retained by the module graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportEdge {
    /// Import syntax and source location exactly as extracted.
    pub raw: RawImport,
    /// Resolution target.
    pub target: EdgeTarget,
}

/// One occupied module-graph slot.
#[derive(Debug, Clone)]
pub struct ModuleNode {
    /// Canonical path used as the graph key.
    pub path: CompactString,
    /// Whether this path is analyzed or only a resolution stub.
    pub state: NodeState,
    /// Outgoing imports in extraction order.
    pub out: Vec<ImportEdge>,
    /// Slots with at least one edge targeting this node.
    pub(crate) rdeps: FxHashSet<u32>,
    /// Deduplicated resolver configuration paths consulted by outgoing edges.
    pub config_dependencies: Vec<CompactString>,
    imports_supported: bool,
    imports_complete: bool,
    imports_analyzed: usize,
    resolver_live: bool,
    resolution_complete: bool,
    resolved_at: u64,
}

impl ModuleNode {
    fn stub(path: CompactString) -> Self {
        Self {
            path,
            state: NodeState::Stub,
            out: Vec::new(),
            rdeps: FxHashSet::default(),
            config_dependencies: Vec::new(),
            imports_supported: false,
            imports_complete: false,
            imports_analyzed: 0,
            resolver_live: false,
            resolution_complete: false,
            resolved_at: 0,
        }
    }

    /// Resolver generation at which this analyzed node's edges were produced.
    #[must_use]
    pub fn resolved_generation(&self) -> Option<u64> {
        matches!(self.state, NodeState::Analyzed { .. }).then_some(self.resolved_at)
    }

    /// Whether import extraction was available when this node was upserted.
    #[must_use]
    pub fn imports_supported(&self) -> bool {
        self.imports_supported
    }

    /// Whether every extracted import was resolved when this node was upserted.
    #[must_use]
    pub fn imports_complete(&self) -> bool {
        self.imports_complete
    }

    /// Whether the matching resolver was live when this node was resolved.
    #[must_use]
    pub fn resolver_live(&self) -> bool {
        self.resolver_live
    }

    /// Whether every outgoing edge was produced by a complete resolution.
    #[must_use]
    pub fn resolution_complete(&self) -> bool {
        self.resolution_complete
    }
}

/// One dependency edge materialized with paths instead of graph slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEdge {
    /// Importing file.
    pub from: CompactString,
    /// Owned resolution target.
    pub to: EdgeTargetOwned,
    /// Specifier exactly as written.
    pub specifier: CompactString,
    /// Syntactic import kind.
    pub kind: ImportKind,
    /// 1-based source line.
    pub line: u32,
    /// Byte range of the specifier.
    pub span: (u32, u32),
}

/// Files used to produce a query answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Number of traversed analyzed nodes.
    pub analyzed: u64,
    /// Number of traversed stub nodes.
    pub stubs: u64,
    /// Number of traversed analyzed nodes with opaque imports.
    pub opaque_files: u64,
    /// Traversed analyzed path/hash pairs, sorted by path.
    pub basis: Vec<(CompactString, u64)>,
}

/// Result of a forward- or reverse-dependency query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepsResult {
    /// Matching dependency edges.
    pub edges: Vec<DepEdge>,
    /// Structural accuracy of this answer.
    pub guarantee: Guarantee,
    /// Nodes traversed to produce this answer.
    pub coverage: Coverage,
}

/// Result of a bidirectional breadth-first traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighborhoodResult {
    /// Reached module paths, sorted lexicographically.
    pub nodes: Vec<CompactString>,
    /// Node-to-node edges induced by the reached paths.
    pub edges: Vec<DepEdge>,
    /// Weakest guarantee used by the traversal.
    pub guarantee: Guarantee,
    /// Reached nodes and their analyzed hashes.
    pub coverage: Coverage,
}

/// Slotted, incrementally maintained repository module graph.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    nodes: Vec<Option<ModuleNode>>,
    by_path: FxHashMap<CompactString, u32>,
    free: Vec<u32>,
    inexact_nodes: usize,
    generation: u64,
    universe_complete: bool,
    resolver_generation: u64,
}

impl ModuleGraph {
    /// Creates an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records whether the caller has supplied a complete source universe.
    pub fn set_universe_complete(&mut self, complete: bool) {
        if self.universe_complete != complete {
            self.universe_complete = complete;
            self.record_mutation();
        }
    }

    /// Returns the graph mutation generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the resolver configuration generation.
    #[must_use]
    pub fn resolver_generation(&self) -> u64 {
        self.resolver_generation
    }

    /// Returns whether an analyzed node has the supplied content hash.
    #[must_use]
    pub fn contains(&self, path: &str, hash: u64) -> bool {
        self.node(path).is_some_and(|node| {
            matches!(
                node.state,
                NodeState::Analyzed {
                    content_hash,
                    ..
                } if content_hash == hash
            )
        })
    }

    /// Returns an occupied node by path.
    #[must_use]
    pub fn node(&self, path: &str) -> Option<&ModuleNode> {
        let slot = *self.by_path.get(path)?;
        self.nodes.get(slot as usize)?.as_ref()
    }

    /// Sorted importer paths of `path` — a slot-history-independent view of
    /// the reverse-dependency set.
    pub fn rdeps_paths(&self, path: &str) -> Option<Vec<CompactString>> {
        let slot = *self.by_path.get(path)?;
        let node = self.occupied(slot);
        let mut paths: Vec<CompactString> = node
            .rdeps
            .iter()
            .map(|&importer| self.occupied(importer).path.clone())
            .collect();
        paths.sort_unstable();
        Some(paths)
    }

    /// Inserts or refreshes one file analysis.
    ///
    /// `imports_supported` should be computed from the same language registry
    /// that produced `analysis`.
    pub fn upsert_file(
        &mut self,
        analysis: &FileAnalysis,
        resolvers: &ResolverSet,
        imports_supported: bool,
    ) -> UpsertOutcome {
        self.upsert_file_bounded(analysis, resolvers, imports_supported, usize::MAX)
    }

    /// Inserts or refreshes one file while resolving at most `max_imports`.
    ///
    /// Bounded upserts deliberately leave a node structurally partial until a
    /// later full upsert supplies every extracted import. Repeating a narrower
    /// bounded upsert of unchanged content never downgrades a fuller node.
    pub fn upsert_file_bounded(
        &mut self,
        analysis: &FileAnalysis,
        resolvers: &ResolverSet,
        imports_supported: bool,
        max_imports: usize,
    ) -> UpsertOutcome {
        let existing = self.by_path.get(analysis.path.as_str()).copied();
        let imports_analyzed = analysis.imports.len().min(max_imports);
        let unchanged_content = existing.is_some_and(|slot| {
            matches!(
                self.occupied(slot).state,
                NodeState::Analyzed { content_hash, .. }
                    if content_hash == analysis.content_hash
            )
        });

        if unchanged_content
            && existing.is_some_and(|slot| self.occupied(slot).imports_analyzed > imports_analyzed)
        {
            return UpsertOutcome::Unchanged;
        }
        if existing.is_some_and(|slot| {
            let node = self.occupied(slot);
            matches!(
                node.state,
                NodeState::Analyzed {
                    content_hash,
                    ..
                } if content_hash == analysis.content_hash
            ) && node.resolved_at == self.resolver_generation
                && node.imports_analyzed >= imports_analyzed
        }) {
            return UpsertOutcome::Unchanged;
        }

        let analyzed_imports = &analysis.imports[..imports_analyzed];
        let resolutions = resolve_imports(resolvers, &analysis.path, analyzed_imports);
        let resolver_live =
            resolver_is_live(analysis.language.as_deref(), analyzed_imports, resolvers);
        let outcome = if existing.is_some() {
            UpsertOutcome::Updated
        } else {
            UpsertOutcome::Inserted
        };
        let slot =
            existing.unwrap_or_else(|| self.allocate(ModuleNode::stub(analysis.path.clone())));
        let was_exact = self.node_is_rdeps_exact(slot);
        let old_targets = self.node_targets(slot);
        let baseline_completeness = analysis
            .language
            .as_deref()
            .map_or(ResolutionCompleteness::Complete, |language| {
                resolvers.baseline_completeness(language)
            });
        let (edges, config_dependencies, resolution_complete) =
            self.materialize_resolutions(resolutions, baseline_completeness);
        let new_targets = targets_from_edges(&edges);

        self.update_rdeps(slot, &old_targets, &new_targets);
        let resolved_at = self.resolver_generation;
        let node = self.occupied_mut(slot);
        node.state = NodeState::Analyzed {
            content_hash: analysis.content_hash,
            has_opaque_imports: analysis.has_opaque_imports,
            language: analysis.language.clone(),
        };
        node.out = edges;
        node.config_dependencies = config_dependencies;
        node.imports_supported = imports_supported;
        node.imports_complete = imports_analyzed == analysis.imports.len();
        node.imports_analyzed = imports_analyzed;
        node.resolver_live = resolver_live;
        node.resolution_complete = resolution_complete;
        node.resolved_at = resolved_at;
        let is_exact = self.node_is_rdeps_exact(slot);
        self.record_exactness_transition(was_exact, is_exact);
        self.record_mutation();
        outcome
    }

    /// Removes an analyzed file.
    ///
    /// A referenced node is demoted to a stub in the same slot. Unreferenced
    /// nodes are freed, and outgoing reverse-dependency memberships are
    /// removed incrementally.
    pub fn remove_file(&mut self, path: &str) -> bool {
        let Some(&slot) = self.by_path.get(path) else {
            return false;
        };
        if matches!(self.occupied(slot).state, NodeState::Stub)
            && !self.occupied(slot).rdeps.is_empty()
        {
            return true;
        }

        let was_exact = self.node_is_rdeps_exact(slot);
        let old_targets = self.node_targets(slot);
        self.update_rdeps(slot, &old_targets, &FxHashSet::default());

        if self.occupied(slot).rdeps.is_empty() {
            self.free_slot(slot);
        } else {
            let node = self.occupied_mut(slot);
            node.state = NodeState::Stub;
            node.out.clear();
            node.config_dependencies.clear();
            node.imports_supported = false;
            node.imports_complete = false;
            node.imports_analyzed = 0;
            node.resolver_live = false;
            node.resolution_complete = false;
            node.resolved_at = 0;
            self.record_exactness_transition(was_exact, false);
        }
        self.record_mutation();
        true
    }

    /// Re-resolves every analyzed node against a new resolver generation.
    ///
    /// Reverse dependencies are adjusted by target-set diffs for each node;
    /// they are never rebuilt globally.
    pub fn reresolve_all(&mut self, resolvers: &ResolverSet) {
        self.resolver_generation += 1;
        self.inexact_nodes = self.by_path.len();
        let current_generation = self.resolver_generation;
        let jobs: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(slot, node)| {
                let node = node.as_ref()?;
                let NodeState::Analyzed { language, .. } = &node.state else {
                    return None;
                };
                Some((
                    slot as u32,
                    node.path.clone(),
                    node.out
                        .iter()
                        .map(|edge| edge.raw.clone())
                        .collect::<Vec<_>>(),
                    language.clone(),
                ))
            })
            .collect();

        for (slot, path, imports, language) in jobs {
            let resolutions = resolve_imports(resolvers, &path, &imports);
            let resolver_live = resolver_is_live(language.as_deref(), &imports, resolvers);
            let old_targets = self.node_targets(slot);
            let baseline_completeness = language
                .as_deref()
                .map_or(ResolutionCompleteness::Complete, |language| {
                    resolvers.baseline_completeness(language)
                });
            let (edges, config_dependencies, resolution_complete) =
                self.materialize_resolutions(resolutions, baseline_completeness);
            let new_targets = targets_from_edges(&edges);
            self.update_rdeps(slot, &old_targets, &new_targets);

            let node = self.occupied_mut(slot);
            node.out = edges;
            node.config_dependencies = config_dependencies;
            node.resolver_live = resolver_live;
            node.resolution_complete = resolution_complete;
            node.resolved_at = current_generation;
            let is_exact = self.node_is_rdeps_exact(slot);
            self.record_exactness_transition(false, is_exact);
        }
        self.record_mutation();
    }

    /// Advances only the resolver generation, leaving current edges stale.
    pub fn bump_resolver_generation(&mut self) {
        self.resolver_generation += 1;
        self.inexact_nodes = self.by_path.len();
        self.record_mutation();
    }

    /// Returns the deduplicated union of resolver configuration dependencies.
    #[must_use]
    pub fn config_dependencies(&self) -> Vec<CompactString> {
        let mut dependencies: Vec<_> = self
            .nodes
            .iter()
            .flatten()
            .flat_map(|node| node.config_dependencies.iter().cloned())
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect();
        dependencies.sort_unstable();
        dependencies
    }

    /// Iterates over occupied node paths in slot order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().flatten().map(|node| node.path.as_str())
    }

    /// Iterates over every stored edge with graph slots materialized as paths.
    pub fn edges(&self) -> impl Iterator<Item = DepEdge> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .flat_map(move |(source, node)| {
                let source =
                    u32::try_from(source).expect("module graph slot must fit in its u32 key");
                node.iter().flat_map(move |node| {
                    node.out
                        .iter()
                        .map(move |edge| self.owned_edge(source, edge))
                })
            })
    }

    /// Returns the total number of resolved, external, and unresolved edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.nodes.iter().flatten().map(|node| node.out.len()).sum()
    }

    /// Returns direct outgoing dependencies for `path`.
    #[must_use]
    pub fn deps(&self, path: &str) -> Option<DepsResult> {
        let slot = *self.by_path.get(path)?;
        let node = self.occupied(slot);
        let mut visited = FxHashSet::default();
        visited.insert(slot);
        let mut edges = Vec::with_capacity(node.out.len());
        for edge in &node.out {
            if let EdgeTarget::Node(target) = edge.target {
                visited.insert(target);
            }
            edges.push(self.owned_edge(slot, edge));
        }
        sort_edges(&mut edges);

        Some(DepsResult {
            edges,
            guarantee: self.deps_guarantee(slot),
            coverage: self.coverage(&visited),
        })
    }

    /// Reverse-dependency guarantee without materializing importer edges.
    #[must_use]
    pub fn rdeps_guarantee_for(&self, path: &str) -> Option<Guarantee> {
        self.by_path.get(path)?;
        Some(self.rdeps_guarantee())
    }

    /// Returns at most `limit` stored edges that directly target `path`.
    #[must_use]
    pub fn rdeps_bounded(&self, path: &str, limit: usize) -> Option<DepsResult> {
        let target = *self.by_path.get(path)?;
        let node = self.occupied(target);
        let mut visited = FxHashSet::default();
        visited.insert(target);
        let mut edges = Vec::new();
        'sources: for &source in &node.rdeps {
            let source_node = self.occupied(source);
            visited.insert(source);
            for edge in source_node
                .out
                .iter()
                .filter(|edge| edge.target == EdgeTarget::Node(target))
            {
                if edges.len() >= limit {
                    break 'sources;
                }
                edges.push(self.owned_edge(source, edge));
            }
        }
        sort_edges(&mut edges);
        Some(DepsResult {
            edges,
            guarantee: self.rdeps_guarantee(),
            coverage: self.coverage(&visited),
        })
    }

    /// Returns all stored edges that directly target `path`.
    #[must_use]
    pub fn rdeps(&self, path: &str) -> Option<DepsResult> {
        let target = *self.by_path.get(path)?;
        let node = self.occupied(target);
        let mut visited = FxHashSet::default();
        visited.insert(target);
        let mut edges = Vec::new();
        for &source in &node.rdeps {
            let source_node = self.occupied(source);
            visited.insert(source);
            edges.extend(
                source_node
                    .out
                    .iter()
                    .filter(|edge| edge.target == EdgeTarget::Node(target))
                    .map(|edge| self.owned_edge(source, edge)),
            );
        }
        sort_edges(&mut edges);

        Some(DepsResult {
            edges,
            guarantee: self.rdeps_guarantee(),
            coverage: self.coverage(&visited),
        })
    }

    /// Traverses both dependency directions to `depth` edges from `path`.
    #[must_use]
    pub fn neighborhood(&self, path: &str, depth: u32) -> Option<NeighborhoodResult> {
        let center = *self.by_path.get(path)?;
        let mut visited = FxHashSet::default();
        let mut queue = VecDeque::new();
        visited.insert(center);
        queue.push_back((center, 0_u32));

        while let Some((slot, distance)) = queue.pop_front() {
            if distance == depth {
                continue;
            }
            let node = self.occupied(slot);
            let neighbors = node
                .out
                .iter()
                .filter_map(|edge| match edge.target {
                    EdgeTarget::Node(target) => Some(target),
                    EdgeTarget::External(_) | EdgeTarget::Unresolved(_) => None,
                })
                .chain(node.rdeps.iter().copied())
                .collect::<Vec<_>>();
            for neighbor in neighbors {
                if visited.insert(neighbor) {
                    queue.push_back((neighbor, distance + 1));
                }
            }
        }

        let mut nodes: Vec<_> = visited
            .iter()
            .map(|&slot| self.occupied(slot).path.clone())
            .collect();
        nodes.sort_unstable();

        let mut edges = Vec::new();
        if depth != 0 {
            for &source in &visited {
                edges.extend(
                    self.occupied(source)
                        .out
                        .iter()
                        .filter(|edge| {
                            matches!(edge.target, EdgeTarget::Node(target) if visited.contains(&target))
                        })
                        .map(|edge| self.owned_edge(source, edge)),
                );
            }
        }
        sort_edges(&mut edges);

        let guarantee = visited
            .iter()
            .fold(Guarantee::Exact, |guarantee, &slot| {
                guarantee.weakest(self.deps_guarantee(slot))
            })
            .weakest(if depth == 0 {
                Guarantee::Exact
            } else {
                self.rdeps_guarantee()
            });

        Some(NeighborhoodResult {
            nodes,
            edges,
            guarantee,
            coverage: self.coverage(&visited),
        })
    }

    fn deps_guarantee(&self, slot: u32) -> Guarantee {
        let node = self.occupied(slot);
        let exact = matches!(
            node.state,
            NodeState::Analyzed {
                has_opaque_imports: false,
                ..
            }
        ) && node.imports_supported
            && node.imports_complete
            && node.resolver_live
            && node.resolution_complete
            // Edges resolved under an older resolver configuration may point
            // at the wrong targets until reresolve_all catches up.
            && node.resolved_at == self.resolver_generation;
        if exact {
            Guarantee::Exact
        } else {
            Guarantee::Approximate
        }
    }

    fn rdeps_guarantee(&self) -> Guarantee {
        let exact = self.universe_complete && self.inexact_nodes == 0;
        if exact {
            Guarantee::Exact
        } else {
            Guarantee::Approximate
        }
    }

    fn coverage(&self, slots: &FxHashSet<u32>) -> Coverage {
        let mut ordered: Vec<_> = slots.iter().map(|&slot| self.occupied(slot)).collect();
        ordered.sort_unstable_by(|left, right| left.path.cmp(&right.path));

        let mut coverage = Coverage::default();
        for node in ordered {
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

    fn materialize_resolutions(
        &mut self,
        resolutions: Vec<(RawImport, ResolutionOutcome)>,
        baseline_completeness: ResolutionCompleteness,
    ) -> (Vec<ImportEdge>, Vec<CompactString>, bool) {
        let mut dependencies = FxHashSet::default();
        let mut resolution_complete = baseline_completeness == ResolutionCompleteness::Complete;
        let edges = resolutions
            .into_iter()
            .map(|(raw, outcome)| {
                resolution_complete &= outcome.completeness == ResolutionCompleteness::Complete;
                dependencies.extend(outcome.dependencies);
                let target = match outcome.resolved {
                    Resolved::Path(path) => EdgeTarget::Node(self.ensure_stub(path)),
                    Resolved::External(package) => EdgeTarget::External(package),
                    Resolved::Unresolved(reason) => EdgeTarget::Unresolved(reason),
                };
                ImportEdge { raw, target }
            })
            .collect();
        let mut dependencies: Vec<_> = dependencies.into_iter().collect();
        dependencies.sort_unstable();
        (edges, dependencies, resolution_complete)
    }

    fn ensure_stub(&mut self, path: CompactString) -> u32 {
        self.by_path
            .get(path.as_str())
            .copied()
            .unwrap_or_else(|| self.allocate(ModuleNode::stub(path)))
    }

    fn allocate(&mut self, node: ModuleNode) -> u32 {
        let exact = rdeps_node_is_exact(&node, self.resolver_generation);
        let path = node.path.clone();
        let slot = if let Some(slot) = self.free.pop() {
            debug_assert!(self.nodes[slot as usize].is_none());
            self.nodes[slot as usize] = Some(node);
            slot
        } else {
            let slot =
                u32::try_from(self.nodes.len()).expect("module graph exhausted its u32 slot space");
            self.nodes.push(Some(node));
            slot
        };
        self.by_path.insert(path, slot);
        self.inexact_nodes += usize::from(!exact);
        slot
    }

    fn free_slot(&mut self, slot: u32) {
        let node = self.nodes[slot as usize]
            .take()
            .expect("slot to free must be occupied");
        if !rdeps_node_is_exact(&node, self.resolver_generation) {
            self.inexact_nodes -= 1;
        }
        let removed = self.by_path.remove(node.path.as_str());
        debug_assert_eq!(removed, Some(slot));
        self.free.push(slot);
    }

    fn prune_orphan_stub(&mut self, slot: u32) {
        let should_prune = self.nodes[slot as usize]
            .as_ref()
            .is_some_and(|node| matches!(node.state, NodeState::Stub) && node.rdeps.is_empty());
        if should_prune {
            self.free_slot(slot);
        }
    }

    fn node_targets(&self, slot: u32) -> FxHashSet<u32> {
        targets_from_edges(&self.occupied(slot).out)
    }

    fn update_rdeps(
        &mut self,
        source: u32,
        old_targets: &FxHashSet<u32>,
        new_targets: &FxHashSet<u32>,
    ) {
        for &target in new_targets.difference(old_targets) {
            self.occupied_mut(target).rdeps.insert(source);
        }
        let removed: Vec<_> = old_targets.difference(new_targets).copied().collect();
        for &target in &removed {
            self.occupied_mut(target).rdeps.remove(&source);
        }
        for target in removed {
            self.prune_orphan_stub(target);
        }
    }

    fn owned_edge(&self, source: u32, edge: &ImportEdge) -> DepEdge {
        let raw = &edge.raw;
        let to = match &edge.target {
            EdgeTarget::Node(target) => EdgeTargetOwned::Path(self.occupied(*target).path.clone()),
            EdgeTarget::External(package) => EdgeTargetOwned::External(package.clone()),
            EdgeTarget::Unresolved(reason) => EdgeTargetOwned::Unresolved(reason.clone()),
        };
        DepEdge {
            from: self.occupied(source).path.clone(),
            to,
            specifier: raw.specifier.clone(),
            kind: raw.kind,
            line: raw.line,
            span: raw.span,
        }
    }

    fn occupied(&self, slot: u32) -> &ModuleNode {
        self.nodes[slot as usize]
            .as_ref()
            .expect("graph edge must reference an occupied slot")
    }

    fn occupied_mut(&mut self, slot: u32) -> &mut ModuleNode {
        self.nodes[slot as usize]
            .as_mut()
            .expect("graph edge must reference an occupied slot")
    }

    fn node_is_rdeps_exact(&self, slot: u32) -> bool {
        rdeps_node_is_exact(self.occupied(slot), self.resolver_generation)
    }

    fn record_exactness_transition(&mut self, was_exact: bool, is_exact: bool) {
        match (was_exact, is_exact) {
            (false, true) => self.inexact_nodes -= 1,
            (true, false) => self.inexact_nodes += 1,
            (false, false) | (true, true) => {}
        }
    }

    fn record_mutation(&mut self) {
        self.generation += 1;
    }
}

fn rdeps_node_is_exact(node: &ModuleNode, resolver_generation: u64) -> bool {
    matches!(
        node.state,
        NodeState::Analyzed {
            has_opaque_imports: false,
            ..
        }
    ) && node.imports_supported
        && node.imports_complete
        && node.resolver_live
        && node.resolution_complete
        && node.resolved_at == resolver_generation
}

fn resolve_imports(
    resolvers: &ResolverSet,
    path: &str,
    imports: &[RawImport],
) -> Vec<(RawImport, ResolutionOutcome)> {
    imports
        .iter()
        .cloned()
        .map(|raw| {
            let outcome = resolvers.resolve(path, &raw);
            (raw, outcome)
        })
        .collect()
}

fn resolver_is_live(
    language: Option<&str>,
    imports: &[RawImport],
    resolvers: &ResolverSet,
) -> bool {
    match language {
        Some("rust") => resolvers.rust.is_some(),
        Some("typescript" | "tsx" | "javascript" | "jsx" | "vue") => resolvers.js.is_some(),
        Some(_) if !imports.is_empty() => imports.iter().all(|raw| match raw.kind {
            ImportKind::RustUse | ImportKind::RustMod => resolvers.rust.is_some(),
            _ => resolvers.js.is_some(),
        }),
        Some(_) | None => false,
    }
}

fn targets_from_edges(edges: &[ImportEdge]) -> FxHashSet<u32> {
    edges
        .iter()
        .filter_map(|edge| match edge.target {
            EdgeTarget::Node(target) => Some(target),
            EdgeTarget::External(_) | EdgeTarget::Unresolved(_) => None,
        })
        .collect()
}

fn sort_edges(edges: &mut [DepEdge]) {
    edges.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then(left.line.cmp(&right.line))
            .then(left.span.0.cmp(&right.span.0))
            .then(left.span.1.cmp(&right.span.1))
            .then(left.specifier.cmp(&right.specifier))
    });
}
