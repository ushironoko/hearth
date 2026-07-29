use std::sync::{Arc, RwLock};

use compact_str::CompactString;
use hearth_graph::graph::{EdgeTargetOwned, Guarantee, ModuleGraph, NodeState, UpsertOutcome};
use hearth_graph::{
    FileAnalysis, ImportKind, RawImport, ResolutionCompleteness, ResolutionOutcome, Resolve,
    Resolved, ResolverSet, UnresolvedReason,
};
use rustc_hash::FxHashMap;

#[derive(Default)]
struct MockData {
    targets: FxHashMap<CompactString, Resolved>,
    dependencies: FxHashMap<CompactString, Vec<CompactString>>,
    completeness: FxHashMap<CompactString, ResolutionCompleteness>,
}

#[derive(Clone, Default)]
struct MockState(Arc<RwLock<MockData>>);

impl MockState {
    fn with_targets(targets: impl IntoIterator<Item = (&'static str, Resolved)>) -> Self {
        let state = Self::default();
        {
            let mut data = state.0.write().expect("mock resolver lock poisoned");
            data.targets.extend(
                targets
                    .into_iter()
                    .map(|(specifier, resolved)| (specifier.into(), resolved)),
            );
        }
        state
    }

    fn set_target(&self, specifier: &str, resolved: Resolved) {
        self.0
            .write()
            .expect("mock resolver lock poisoned")
            .targets
            .insert(specifier.into(), resolved);
    }

    fn set_dependencies(&self, specifier: &str, dependencies: &[&str]) {
        self.0
            .write()
            .expect("mock resolver lock poisoned")
            .dependencies
            .insert(
                specifier.into(),
                dependencies.iter().copied().map(Into::into).collect(),
            );
    }

    fn set_completeness(&self, specifier: &str, completeness: ResolutionCompleteness) {
        self.0
            .write()
            .expect("mock resolver lock poisoned")
            .completeness
            .insert(specifier.into(), completeness);
    }

    fn resolvers(&self) -> ResolverSet {
        ResolverSet {
            js: Some(Box::new(MockResolver {
                state: self.clone(),
            })),
            rust: None,
        }
    }
}

struct MockResolver {
    state: MockState,
}

impl Resolve for MockResolver {
    fn resolve(&self, _from_file: &str, import: &RawImport) -> ResolutionOutcome {
        let data = self.state.0.read().expect("mock resolver lock poisoned");
        ResolutionOutcome {
            resolved: data
                .targets
                .get(import.specifier.as_str())
                .cloned()
                .unwrap_or(Resolved::Unresolved(UnresolvedReason::NotFound)),
            dependencies: data
                .dependencies
                .get(import.specifier.as_str())
                .cloned()
                .unwrap_or_default(),
            notes: Vec::new(),
            completeness: data
                .completeness
                .get(import.specifier.as_str())
                .copied()
                .unwrap_or(ResolutionCompleteness::Complete),
        }
    }

    fn clear_cache(&self) {}
}

fn raw(specifier: &str, line: u32) -> RawImport {
    let start = line * 10;
    RawImport {
        specifier: specifier.into(),
        kind: ImportKind::EsStatic,
        line,
        span: (start, start + u32::try_from(specifier.len()).unwrap()),
    }
}

fn analysis(path: &str, hash: u64, imports: &[&str], opaque: bool) -> FileAnalysis {
    FileAnalysis {
        path: path.into(),
        content_hash: hash,
        language: Some("typescript".into()),
        symbols: Vec::new(),
        imports: imports
            .iter()
            .enumerate()
            .map(|(index, specifier)| raw(specifier, index as u32 + 1))
            .collect(),
        has_opaque_imports: opaque,
    }
}

fn target_map(paths: &[&str]) -> MockState {
    MockState::with_targets(paths.iter().enumerate().map(|(index, path)| {
        (
            match index {
                0 => "p0",
                1 => "p1",
                2 => "p2",
                3 => "p3",
                4 => "p4",
                _ => unreachable!(),
            },
            Resolved::Path((*path).into()),
        )
    }))
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
}

#[test]
fn incremental_rdeps_match_a_scratch_rebuild_after_random_edits() {
    const PATHS: [&str; 5] = [
        "/repo/p0.ts",
        "/repo/p1.ts",
        "/repo/p2.ts",
        "/repo/p3.ts",
        "/repo/p4.ts",
    ];
    const SPECIFIERS: [&str; 5] = ["p0", "p1", "p2", "p3", "p4"];
    const SEEDS: [u64; 3] = [0x1234_5678, 0xcafe_babe, 0xdead_beef];

    let state = target_map(&PATHS);
    let resolvers = state.resolvers();
    for seed in SEEDS {
        let mut random = XorShift(seed);
        let mut graph = ModuleGraph::new();
        let mut files = FxHashMap::<CompactString, FileAnalysis>::default();

        for step in 0..80_u64 {
            let path_index = random.next() as usize % PATHS.len();
            let path = PATHS[path_index];
            if random.next().is_multiple_of(4) {
                graph.remove_file(path);
                files.remove(path);
            } else {
                let import_count = random.next() as usize % 4;
                let imports: Vec<_> = (0..import_count)
                    .map(|_| SPECIFIERS[random.next() as usize % SPECIFIERS.len()])
                    .collect();
                let file = analysis(path, step + 1, &imports, false);
                graph.upsert_file(&file, &resolvers, true);
                files.insert(path.into(), file);
            }

            let mut rebuilt = ModuleGraph::new();
            let mut ordered: Vec<_> = files.values().collect();
            ordered.sort_unstable_by(|left, right| left.path.cmp(&right.path));
            for file in ordered {
                rebuilt.upsert_file(file, &resolvers, true);
            }

            for path in PATHS {
                let incremental_edges = graph.rdeps(path).map(|result| result.edges);
                let rebuilt_edges = rebuilt.rdeps(path).map(|result| result.edges);
                assert_eq!(
                    incremental_edges, rebuilt_edges,
                    "seed={seed:#x}, step={step}, path={path}"
                );
            }
        }
    }
}

#[test]
fn resolved_paths_create_stubs_and_upsert_promotes_them_in_place() {
    let state = MockState::with_targets([("child", Resolved::Path("/repo/child.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();

    assert_eq!(
        graph.upsert_file(
            &analysis("/repo/parent.ts", 1, &["child"], false),
            &resolvers,
            true
        ),
        UpsertOutcome::Inserted
    );
    let stub = graph.node("/repo/child.ts").expect("resolved stub missing");
    assert_eq!(stub.state, NodeState::Stub);
    assert_eq!(graph.rdeps_paths("/repo/child.ts").unwrap().len(), 1);

    assert_eq!(
        graph.upsert_file(&analysis("/repo/child.ts", 2, &[], false), &resolvers, true),
        UpsertOutcome::Updated
    );
    let promoted = graph.node("/repo/child.ts").unwrap();
    assert!(matches!(promoted.state, NodeState::Analyzed { .. }));
    assert_eq!(graph.rdeps_paths("/repo/child.ts").unwrap().len(), 1);
    assert_eq!(graph.rdeps("/repo/child.ts").unwrap().edges.len(), 1);
}

#[test]
fn guarantee_matrix_covers_node_resolver_opaque_and_universe_state() {
    let live_state = MockState::with_targets([("query", Resolved::Path("/repo/query.ts".into()))]);
    let live_resolvers = live_state.resolvers();

    for analyzed in [false, true] {
        for resolver_present in [false, true] {
            for opaque in [false, true] {
                for universe_complete in [false, true] {
                    let mut graph = ModuleGraph::new();
                    if analyzed {
                        let absent = ResolverSet::default();
                        let resolvers = if resolver_present {
                            &live_resolvers
                        } else {
                            &absent
                        };
                        graph.upsert_file(
                            &analysis("/repo/query.ts", 1, &[], opaque),
                            resolvers,
                            true,
                        );
                    } else {
                        graph.upsert_file(
                            &analysis("/repo/importer.ts", 1, &["query"], opaque),
                            &live_resolvers,
                            true,
                        );
                    }
                    graph.set_universe_complete(universe_complete);

                    let expected_deps = if analyzed && resolver_present && !opaque {
                        Guarantee::Exact
                    } else {
                        Guarantee::Approximate
                    };
                    let expected_rdeps =
                        if analyzed && resolver_present && !opaque && universe_complete {
                            Guarantee::Exact
                        } else {
                            Guarantee::Approximate
                        };
                    assert_eq!(
                        graph.deps("/repo/query.ts").unwrap().guarantee,
                        expected_deps,
                        "deps: analyzed={analyzed}, resolver={resolver_present}, opaque={opaque}, universe={universe_complete}"
                    );
                    assert_eq!(
                        graph.rdeps("/repo/query.ts").unwrap().guarantee,
                        expected_rdeps,
                        "rdeps: analyzed={analyzed}, resolver={resolver_present}, opaque={opaque}, universe={universe_complete}"
                    );
                }
            }
        }
    }

    let mut graph = ModuleGraph::new();
    graph.upsert_file(
        &analysis("/repo/no-import-spec.go", 1, &[], false),
        &live_resolvers,
        false,
    );
    graph.set_universe_complete(true);
    assert_eq!(
        graph.deps("/repo/no-import-spec.go").unwrap().guarantee,
        Guarantee::Approximate
    );
    assert_eq!(
        graph.rdeps("/repo/no-import-spec.go").unwrap().guarantee,
        Guarantee::Approximate
    );
}

#[test]
fn partial_resolution_downgrades_deps_rdeps_and_neighborhood_guarantees() {
    let target = Resolved::Path("/repo/target.ts".into());
    let partial_state = MockState::with_targets([("target", target.clone())]);
    partial_state.set_completeness("target", ResolutionCompleteness::Partial);
    let mut partial_graph = ModuleGraph::new();
    let partial_resolvers = partial_state.resolvers();
    partial_graph.upsert_file(
        &analysis("/repo/importer.ts", 1, &["target"], false),
        &partial_resolvers,
        true,
    );
    partial_graph.upsert_file(
        &analysis("/repo/target.ts", 2, &[], false),
        &partial_resolvers,
        true,
    );
    partial_graph.set_universe_complete(true);

    assert_eq!(
        partial_graph.deps("/repo/importer.ts").unwrap().guarantee,
        Guarantee::Approximate
    );
    assert_eq!(
        partial_graph.rdeps("/repo/target.ts").unwrap().guarantee,
        Guarantee::Approximate
    );
    assert_eq!(
        partial_graph
            .neighborhood("/repo/importer.ts", 1)
            .unwrap()
            .guarantee,
        Guarantee::Approximate
    );

    let complete_state = MockState::with_targets([("target", target)]);
    let complete_resolvers = complete_state.resolvers();
    let mut complete_graph = ModuleGraph::new();
    complete_graph.upsert_file(
        &analysis("/repo/importer.ts", 1, &["target"], false),
        &complete_resolvers,
        true,
    );
    complete_graph.upsert_file(
        &analysis("/repo/target.ts", 2, &[], false),
        &complete_resolvers,
        true,
    );
    complete_graph.set_universe_complete(true);

    assert_eq!(
        complete_graph.deps("/repo/importer.ts").unwrap().guarantee,
        Guarantee::Exact
    );
    assert_eq!(
        complete_graph.rdeps("/repo/target.ts").unwrap().guarantee,
        Guarantee::Exact
    );
}

#[test]
fn every_returned_edge_preserves_its_raw_specifier() {
    let state = MockState::with_targets([
        ("./local?raw", Resolved::Path("/repo/local.ts".into())),
        (
            "@scope/package/subpath",
            Resolved::External("@scope/package".into()),
        ),
    ]);
    let resolvers = state.resolvers();
    let source = FileAnalysis {
        path: "/repo/source.ts".into(),
        content_hash: 1,
        language: Some("typescript".into()),
        symbols: Vec::new(),
        imports: vec![
            raw("./local?raw", 1),
            raw("@scope/package/subpath", 2),
            raw("./missing#fragment", 3),
        ],
        has_opaque_imports: false,
    };
    let mut graph = ModuleGraph::new();
    graph.upsert_file(&source, &resolvers, true);

    let result = graph.deps("/repo/source.ts").unwrap();
    assert_eq!(
        result
            .edges
            .iter()
            .map(|edge| edge.specifier.as_str())
            .collect::<Vec<_>>(),
        vec![
            "./local?raw",
            "@scope/package/subpath",
            "./missing#fragment"
        ]
    );
    assert!(matches!(
        result.edges[0].to,
        EdgeTargetOwned::Path(ref path) if path == "/repo/local.ts"
    ));
    assert!(matches!(
        result.edges[1].to,
        EdgeTargetOwned::External(ref package) if package == "@scope/package"
    ));
    assert!(matches!(
        result.edges[2].to,
        EdgeTargetOwned::Unresolved(UnresolvedReason::NotFound)
    ));
}

#[test]
fn neighborhood_honors_depth_and_walks_both_directions() {
    let state = MockState::with_targets([
        ("a", Resolved::Path("/repo/a.ts".into())),
        ("b", Resolved::Path("/repo/b.ts".into())),
        ("c", Resolved::Path("/repo/c.ts".into())),
    ]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    for file in [
        analysis("/repo/a.ts", 1, &["b"], false),
        analysis("/repo/b.ts", 2, &["c"], false),
        analysis("/repo/c.ts", 3, &[], false),
        analysis("/repo/d.ts", 4, &["a"], false),
    ] {
        graph.upsert_file(&file, &resolvers, true);
    }
    graph.set_universe_complete(true);

    let depth_zero = graph.neighborhood("/repo/a.ts", 0).unwrap();
    assert_eq!(depth_zero.nodes, vec![CompactString::from("/repo/a.ts")]);
    assert!(depth_zero.edges.is_empty());

    let depth_one = graph.neighborhood("/repo/a.ts", 1).unwrap();
    assert_eq!(
        depth_one.nodes,
        vec![
            CompactString::from("/repo/a.ts"),
            CompactString::from("/repo/b.ts"),
            CompactString::from("/repo/d.ts"),
        ]
    );
    assert_eq!(depth_one.edges.len(), 2);

    let beyond_diameter = graph.neighborhood("/repo/a.ts", 10).unwrap();
    assert_eq!(
        beyond_diameter.nodes,
        vec![
            CompactString::from("/repo/a.ts"),
            CompactString::from("/repo/b.ts"),
            CompactString::from("/repo/c.ts"),
            CompactString::from("/repo/d.ts"),
        ]
    );
    assert_eq!(beyond_diameter.edges.len(), 3);
    assert_eq!(beyond_diameter.guarantee, Guarantee::Exact);
}

#[test]
fn reresolve_all_updates_edges_and_restores_rdeps_exactness() {
    let state = MockState::with_targets([("target", Resolved::Path("/repo/b.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    for file in [
        analysis("/repo/a.ts", 1, &["target"], false),
        analysis("/repo/b.ts", 2, &[], false),
        analysis("/repo/c.ts", 3, &[], false),
    ] {
        graph.upsert_file(&file, &resolvers, true);
    }
    graph.set_universe_complete(true);
    assert_eq!(
        graph.rdeps("/repo/b.ts").unwrap().guarantee,
        Guarantee::Exact
    );
    assert_eq!(graph.rdeps("/repo/b.ts").unwrap().edges.len(), 1);

    state.set_target("target", Resolved::Path("/repo/c.ts".into()));
    let initial_resolver_generation = graph.resolver_generation();
    graph.bump_resolver_generation();
    assert_eq!(graph.resolver_generation(), initial_resolver_generation + 1);
    assert_eq!(
        graph.rdeps("/repo/b.ts").unwrap().guarantee,
        Guarantee::Approximate
    );

    graph.reresolve_all(&resolvers);
    assert_eq!(graph.resolver_generation(), initial_resolver_generation + 2);
    assert!(graph.rdeps("/repo/b.ts").unwrap().edges.is_empty());
    assert_eq!(graph.rdeps("/repo/c.ts").unwrap().edges.len(), 1);
    assert_eq!(
        graph.rdeps("/repo/c.ts").unwrap().guarantee,
        Guarantee::Exact
    );
}

#[test]
fn remove_demotes_a_referenced_node_and_reupsert_promotes_it() {
    let state = MockState::with_targets([("target", Resolved::Path("/repo/target.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(
        &analysis("/repo/target.ts", 1, &[], false),
        &resolvers,
        true,
    );
    graph.upsert_file(
        &analysis("/repo/importer.ts", 2, &["target"], false),
        &resolvers,
        true,
    );
    let incoming = graph.rdeps_paths("/repo/target.ts").unwrap();

    assert!(graph.remove_file("/repo/target.ts"));
    let demoted = graph.node("/repo/target.ts").unwrap();
    assert_eq!(demoted.state, NodeState::Stub);
    assert_eq!(graph.rdeps_paths("/repo/target.ts").unwrap(), incoming);
    assert!(!graph.contains("/repo/target.ts", 1));

    graph.upsert_file(
        &analysis("/repo/target.ts", 3, &[], false),
        &resolvers,
        true,
    );
    let promoted = graph.node("/repo/target.ts").unwrap();
    assert!(matches!(
        promoted.state,
        NodeState::Analyzed {
            content_hash: 3,
            ..
        }
    ));
    assert_eq!(graph.rdeps_paths("/repo/target.ts").unwrap(), incoming);
    assert_eq!(graph.rdeps("/repo/target.ts").unwrap().edges.len(), 1);
}

#[test]
fn config_dependencies_are_the_deduplicated_union_of_outcomes() {
    let state = MockState::with_targets([
        ("x", Resolved::Path("/repo/x.ts".into())),
        ("y", Resolved::Path("/repo/y.ts".into())),
        ("z", Resolved::External("z".into())),
    ]);
    state.set_dependencies("x", &["/repo/tsconfig.json", "/repo/package.json"]);
    state.set_dependencies("y", &["/repo/tsconfig.json", "/repo/base.json"]);
    state.set_dependencies("z", &["/repo/package.json"]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(
        &analysis("/repo/a.ts", 1, &["x", "y"], false),
        &resolvers,
        true,
    );
    graph.upsert_file(&analysis("/repo/b.ts", 2, &["z"], false), &resolvers, true);

    assert_eq!(
        graph.config_dependencies(),
        vec![
            CompactString::from("/repo/base.json"),
            CompactString::from("/repo/package.json"),
            CompactString::from("/repo/tsconfig.json"),
        ]
    );

    graph.upsert_file(&analysis("/repo/a.ts", 3, &["y"], false), &resolvers, true);
    graph.remove_file("/repo/b.ts");
    assert_eq!(
        graph.config_dependencies(),
        vec![
            CompactString::from("/repo/base.json"),
            CompactString::from("/repo/tsconfig.json"),
        ]
    );
}

#[test]
fn duplicate_edges_use_one_rdeps_membership_but_all_edges_are_returned() {
    let state = MockState::with_targets([("target", Resolved::Path("/repo/target.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(
        &analysis(
            "/repo/importer.ts",
            1,
            &["target", "target", "target"],
            false,
        ),
        &resolvers,
        true,
    );

    assert_eq!(graph.rdeps_paths("/repo/target.ts").unwrap().len(), 1);
    assert_eq!(graph.rdeps("/repo/target.ts").unwrap().edges.len(), 3);
}

#[test]
fn deps_degrade_after_resolver_generation_bump_and_recover_after_reresolve() {
    let state = MockState::with_targets([("b", Resolved::Path("/repo/b.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(&analysis("/repo/a.ts", 1, &["b"], false), &resolvers, true);
    graph.upsert_file(&analysis("/repo/b.ts", 1, &[], false), &resolvers, true);
    assert_eq!(
        graph.deps("/repo/a.ts").unwrap().guarantee,
        Guarantee::Exact
    );

    graph.bump_resolver_generation();
    assert_eq!(
        graph.deps("/repo/a.ts").unwrap().guarantee,
        Guarantee::Approximate
    );
    assert_eq!(
        graph.neighborhood("/repo/a.ts", 0).unwrap().guarantee,
        Guarantee::Approximate
    );

    graph.reresolve_all(&resolvers);
    assert_eq!(
        graph.deps("/repo/a.ts").unwrap().guarantee,
        Guarantee::Exact
    );
}

#[test]
fn retargeting_an_edge_to_external_drops_the_old_rdeps_entry() {
    let state = MockState::with_targets([("b", Resolved::Path("/repo/b.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(&analysis("/repo/a.ts", 1, &["b"], false), &resolvers, true);
    graph.upsert_file(&analysis("/repo/b.ts", 1, &[], false), &resolvers, true);
    assert_eq!(graph.rdeps_paths("/repo/b.ts").unwrap(), ["/repo/a.ts"]);

    // The same specifier now resolves to an external package.
    let external_state = MockState::with_targets([("b", Resolved::External("b-pkg".into()))]);
    let external = external_state.resolvers();
    graph.upsert_file(&analysis("/repo/a.ts", 2, &["b"], false), &external, true);
    assert!(graph.rdeps_paths("/repo/b.ts").unwrap().is_empty());
}

#[test]
fn removing_the_sole_edge_to_a_stub_prunes_it() {
    let state =
        MockState::with_targets([("stub-target", Resolved::Path("/repo/stub-target.ts".into()))]);
    let resolvers = state.resolvers();
    let mut graph = ModuleGraph::new();
    graph.upsert_file(
        &analysis("/repo/only.ts", 1, &["stub-target"], false),
        &resolvers,
        true,
    );
    assert!(graph.node("/repo/stub-target.ts").is_some());

    graph.upsert_file(&analysis("/repo/only.ts", 2, &[], false), &resolvers, true);
    assert!(graph.node("/repo/stub-target.ts").is_none());
}
