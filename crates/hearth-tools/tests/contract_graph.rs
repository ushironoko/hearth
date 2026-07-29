//! Graph adapter contract: lazy publication, cache coherence, caller-provided
//! universes, deterministic Stage-A queries, and the cold-build barrier.

mod common;

use common::{abs, engine, seed, trusting_engine, watching_engine};
use hearth_core::CancelToken;
use hearth_proto::{
    ErrorKind, GraphGuarantee, GraphOp, GraphOutput, GraphParams, GraphResult, GraphStatusResult,
    Request, Response, WriteParams,
};
use hearth_tools::{dispatch, graph, graph_cancellable, graph_clear, write};
use std::fs::{FileTimes, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn params(root: &Path, op: GraphOp) -> GraphParams {
    GraphParams::new(root.display().to_string(), op)
}

fn symbol_names(result: &GraphResult) -> Vec<&str> {
    match &result.output {
        GraphOutput::Symbols(result) => result
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect(),
        GraphOutput::Outline(result) => result
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect(),
        GraphOutput::Search(result) => result
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect(),
        GraphOutput::Definitions(result) => result
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect(),
        other => panic!("expected a symbol-bearing result, got {other:?}"),
    }
}

fn sorted_symbol_names(result: &GraphResult) -> Vec<&str> {
    let mut names = symbol_names(result);
    names.sort_unstable();
    names
}

fn status(result: &GraphResult) -> &GraphStatusResult {
    match &result.output {
        GraphOutput::Status(status) => status,
        other => panic!("expected status, got {other:?}"),
    }
}

fn dep_targets(result: &GraphResult) -> Vec<&str> {
    match &result.output {
        GraphOutput::Deps(result) => result.edges.iter().map(|edge| edge.to.as_str()).collect(),
        other => panic!("expected deps, got {other:?}"),
    }
}

#[test]
fn verification_01_lazy_build_transitions_status_from_unbuilt_to_built() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "src/lib.rs", "pub fn lazy_symbol() {}\n");
    let eng = engine(dir.path());

    let before = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert!(!status(&before).built);
    assert!(!status(&before).building);

    let built = graph(
        &eng,
        &params(dir.path(), GraphOp::Symbols { path: file.clone() }),
    )
    .unwrap();
    assert_eq!(symbol_names(&built), ["lazy_symbol"]);
    assert!(built.meta.swept);
    assert_eq!(built.meta.indexed_files, 1);

    let after = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert!(status(&after).built);
    assert!(!status(&after).building);
    assert_eq!(status(&after).indexed_files, 1);
}

#[test]
fn verification_02_out_of_band_size_change_reindexes_exactly_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let changed = seed(dir.path(), "src/changed.rs", "pub fn old_name() {}\n");
    seed(dir.path(), "src/stable.rs", "pub fn stable_name() {}\n");
    let eng = engine(dir.path());

    graph(
        &eng,
        &params(
            dir.path(),
            GraphOp::Search {
                query: "name".into(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    let original_len = std::fs::metadata(&changed).unwrap().len();
    let replacement = "pub fn replacement_name_with_different_size() {}\n";
    assert_ne!(
        replacement.len() as u64,
        original_len,
        "the fixture must change the byte size, or the stat gate is untested"
    );
    std::fs::write(&changed, replacement).unwrap();
    let refreshed = graph(
        &eng,
        &params(
            dir.path(),
            GraphOp::Symbols {
                path: changed.clone(),
            },
        ),
    )
    .unwrap();

    assert_eq!(refreshed.meta.reindexed_files, 1);
    assert_eq!(
        symbol_names(&refreshed),
        ["replacement_name_with_different_size"]
    );
}

#[test]
fn verification_03_trust_cache_waits_for_invalidation_but_hearth_write_is_immediate() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "src/lib.rs", "pub fn original_symbol() {}\n");
    let eng = trusting_engine(dir.path());
    let query = || {
        graph(
            &eng,
            &params(dir.path(), GraphOp::Symbols { path: file.clone() }),
        )
        .unwrap()
    };

    assert_eq!(symbol_names(&query()), ["original_symbol"]);

    std::fs::write(&file, "pub fn external_change_with_different_size() {}\n").unwrap();
    assert_eq!(
        symbol_names(&query()),
        ["original_symbol"],
        "trustCache deliberately serves the resident FileCache entry"
    );

    eng.invalidate_path(Path::new(&file));
    assert_eq!(
        symbol_names(&query()),
        ["external_change_with_different_size"]
    );

    write(
        &eng,
        &WriteParams::new(&file, "pub fn hearth_write_is_immediate() {}\n"),
    )
    .unwrap();
    assert_eq!(symbol_names(&query()), ["hearth_write_is_immediate"]);
}

#[test]
fn verification_04_oversize_importer_is_approximate_rdeps_grep_backstop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = true;\n");
    let mut oversize_source = "import \"./target\";\n".to_owned();
    while oversize_source.len() <= 2 * 1024 * 1024 {
        oversize_source.push_str("// padding keeps this importer above the graph parse limit\n");
    }
    let importer = seed(&root, "src/oversize.ts", &oversize_source);
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Rdeps {
                path: target,
                depth: 1,
                verify: true,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Rdeps(rdeps) = result.output else {
        panic!("expected rdeps");
    };

    assert_eq!(result.meta.oversize_files, 1);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert!(
        rdeps
            .importers
            .iter()
            .any(|entry| entry.node.path == importer
                && entry.guarantee == GraphGuarantee::Approximate)
    );
}

#[test]
fn verification_05_hearth_write_immediately_adds_new_rdeps_importer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = true;\n");
    let original_importer = seed(
        &root,
        "src/original.ts",
        "import \"./target\";\nexport const original = true;\n",
    );
    let new_importer = abs(&root, "src/new_importer.ts");
    let eng = engine(&root);
    let query = params(
        &root,
        GraphOp::Rdeps {
            path: target,
            depth: 1,
            verify: true,
        },
    );

    let first = graph(&eng, &query).unwrap();
    let GraphOutput::Rdeps(first_rdeps) = first.output else {
        panic!("expected rdeps");
    };
    assert_eq!(
        first_rdeps
            .importers
            .iter()
            .map(|entry| entry.node.path.as_str())
            .collect::<Vec<_>>(),
        [original_importer.as_str()]
    );

    write(
        &eng,
        &WriteParams::new(
            &new_importer,
            "import \"./target\";\nexport const observed = true;\n",
        ),
    )
    .unwrap();

    let refreshed = graph(&eng, &query).unwrap();
    let GraphOutput::Rdeps(refreshed_rdeps) = refreshed.output else {
        panic!("expected rdeps");
    };
    let importer_paths: Vec<_> = refreshed_rdeps
        .importers
        .iter()
        .map(|entry| entry.node.path.as_str())
        .collect();
    assert!(importer_paths.contains(&original_importer.as_str()));
    assert!(importer_paths.contains(&new_importer.as_str()));
}

#[test]
fn verification_06_pre_latched_cancel_token_returns_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "src/lib.rs", "pub fn never_indexed() {}\n");
    let eng = engine(dir.path());
    let cancel = CancelToken::new();
    cancel.cancel();

    let err = graph_cancellable(
        &eng,
        &params(
            dir.path(),
            GraphOp::Search {
                query: "never".into(),
                limit: 20,
            },
        ),
        &cancel,
    )
    .unwrap_err();

    assert_eq!(err.kind, ErrorKind::Cancelled);
}

#[test]
fn verification_07_closed_deps_fixture_synthesizes_exact_guarantees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        concat!(
            "import { first } from \"./first\";\n",
            "import { second } from \"./second\";\n",
            "export const total = first + second;\n"
        ),
    );
    let first_target = seed(&root, "src/first.ts", "export const first = 1;\n");
    let second_target = seed(&root, "src/second.ts", "export const second = 2;\n");
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: importer,
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Deps(deps) = &result.output else {
        panic!("expected deps");
    };

    assert_eq!(deps.edges.len(), 2);
    assert!(deps.unresolved.is_empty());
    assert!(deps.edges.iter().any(|edge| edge.to == first_target));
    assert!(deps.edges.iter().any(|edge| edge.to == second_target));
    assert!(
        deps.edges
            .iter()
            .all(|edge| edge.guarantee == GraphGuarantee::Exact)
    );
    assert_eq!(result.meta.indexed_files, 3);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Exact);
}

#[test]
fn rust_only_root_resolves_mod_edges_with_best_effort_edges_and_approximate_guarantee() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/main.rs",
        "mod util;\nuse crate::util::run;\nfn main() { run(); }\n",
    );
    let target = seed(&root, "src/util.rs", "pub fn run() {}\n");
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: importer,
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Deps(deps) = &result.output else {
        panic!("expected deps");
    };

    assert_eq!(deps.edges.len(), 2);
    assert!(
        deps.edges
            .iter()
            .any(|edge| edge.kind == "mod" && edge.to == target)
    );
    assert!(
        deps.edges
            .iter()
            .any(|edge| edge.kind == "use" && edge.to == target)
    );
    assert!(
        deps.edges
            .iter()
            .all(|edge| edge.guarantee == GraphGuarantee::Approximate)
    );
    assert!(deps.unresolved.is_empty());
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
}

#[test]
fn undeclared_rust_sibling_module_never_claims_an_exact_guarantee() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/lib.rs",
        "use serde::Serialize;\npub fn deserialize() {}\n",
    );
    let undeclared = seed(&root, "src/serde.rs", "pub struct Serialize;\n");
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: importer,
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Deps(deps) = &result.output else {
        panic!("expected deps");
    };

    let resolved = deps
        .edges
        .iter()
        .find(|edge| edge.specifier == "serde::Serialize");
    let unresolved = deps
        .unresolved
        .iter()
        .any(|import| import.specifier == "serde::Serialize");
    assert!(
        resolved.is_some() || unresolved,
        "the undeclared sibling may resolve best-effort or remain unresolved"
    );
    if let Some(edge) = resolved {
        assert_eq!(edge.to, undeclared);
        assert_eq!(edge.guarantee, GraphGuarantee::Approximate);
    }
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
}

#[test]
fn verification_08_files_universe_indexes_only_the_caller_subset() {
    let dir = tempfile::tempdir().unwrap();
    let included = seed(
        dir.path(),
        "src/included.rs",
        "pub fn included_symbol() {}\n",
    );
    seed(
        dir.path(),
        "src/excluded.rs",
        "pub fn excluded_symbol() {}\n",
    );
    let eng = engine(dir.path());

    let mut subset = params(
        dir.path(),
        GraphOp::Search {
            query: "symbol".into(),
            limit: 20,
        },
    );
    subset.files = vec![included];
    let result = graph(&eng, &subset).unwrap();

    assert_eq!(result.meta.universe_files, 1);
    assert_eq!(result.meta.indexed_files, 1);
    assert_eq!(symbol_names(&result), ["included_symbol"]);

    let current = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert_eq!(status(&current).indexed_files, 1);
    assert_eq!(status(&current).universe_files, 1);
}

#[test]
fn verification_09_fresh_engines_produce_identical_results() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "src/a.rs", "pub fn deterministic_alpha() {}\n");
    seed(dir.path(), "src/b.rs", "pub struct DeterministicBeta;\n");
    let query = params(
        dir.path(),
        GraphOp::Search {
            query: "deterministic".into(),
            limit: 20,
        },
    );

    let first = graph(&engine(dir.path()), &query).unwrap();
    let second = graph(&engine(dir.path()), &query).unwrap();

    assert_eq!(first.output, second.output);
    assert_eq!(first.meta.universe_files, second.meta.universe_files);
    assert_eq!(first.meta.indexed_files, second.meta.indexed_files);
    assert_eq!(first.meta.unsupported_files, second.meta.unsupported_files);
}

#[test]
fn verification_10_max_stale_window_reuses_exact_snapshot_without_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "src/lib.rs", "pub fn reusable_snapshot() {}\n");
    let eng = engine(dir.path());

    graph(
        &eng,
        &params(dir.path(), GraphOp::Symbols { path: file.clone() }),
    )
    .unwrap();

    let mut reused = params(dir.path(), GraphOp::Symbols { path: file });
    reused.max_stale_ms = Some(u64::MAX);
    let result = graph(&eng, &reused).unwrap();

    assert!(!result.meta.swept);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Exact);
}

#[test]
fn verification_10b_subset_freshness_never_covers_the_walk_universe() {
    let dir = tempfile::tempdir().unwrap();
    let included = seed(dir.path(), "src/included.rs", "pub fn included() {}\n");
    seed(
        dir.path(),
        "src/also_present.rs",
        "pub fn also_present() {}\n",
    );
    let eng = engine(dir.path());

    let mut subset = params(
        dir.path(),
        GraphOp::Search {
            query: String::new(),
            limit: 20,
        },
    );
    subset.files = vec![included];
    graph(&eng, &subset).unwrap();

    let mut full = params(
        dir.path(),
        GraphOp::Search {
            query: String::new(),
            limit: 20,
        },
    );
    full.max_stale_ms = Some(u64::MAX);
    let result = graph(&eng, &full).unwrap();

    assert!(result.meta.swept);
    assert_eq!(result.meta.indexed_files, 2);
}

#[test]
fn wipe_discards_all_stat_records_on_a_built_root() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "src/a.rs", "pub fn wipe_alpha() {}\n");
    seed(dir.path(), "src/b.rs", "pub fn wipe_beta() {}\n");
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Search {
            query: "wipe".into(),
            limit: 20,
        },
    );

    let initial = graph(&eng, &query).unwrap();
    assert_eq!(initial.meta.reindexed_files, 2);
    assert_eq!(sorted_symbol_names(&initial), ["wipe_alpha", "wipe_beta"]);

    let unchanged = graph(&eng, &query).unwrap();
    assert!(unchanged.meta.swept);
    assert_eq!(unchanged.meta.reindexed_files, 0);
    assert_eq!(sorted_symbol_names(&unchanged), ["wipe_alpha", "wipe_beta"]);

    eng.invalidations().record_wipe();
    let rebuilt = graph(&eng, &query).unwrap();
    assert!(rebuilt.meta.swept);
    assert_eq!(rebuilt.meta.reindexed_files, 2);
    assert_eq!(sorted_symbol_names(&rebuilt), ["wipe_alpha", "wipe_beta"]);
}

#[test]
fn same_files_set_stamp_reuses_after_sorting_and_deduplication() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path(), "src/a.rs", "pub fn set_alpha() {}\n");
    let b = seed(dir.path(), "src/b.rs", "pub fn set_beta() {}\n");
    let eng = engine(dir.path());
    let op = GraphOp::Search {
        query: "set".into(),
        limit: 20,
    };

    let mut first = params(dir.path(), op.clone());
    first.files = vec![a.clone(), b.clone()];
    first.max_stale_ms = Some(u64::MAX);
    let first_result = graph(&eng, &first).unwrap();
    assert!(first_result.meta.swept);
    assert_eq!(
        sorted_symbol_names(&first_result),
        ["set_alpha", "set_beta"]
    );

    let mut reordered = params(dir.path(), op.clone());
    reordered.files = vec![b, a.clone(), a.clone()];
    reordered.max_stale_ms = Some(u64::MAX);
    let reused = graph(&eng, &reordered).unwrap();
    assert!(!reused.meta.swept);
    assert_eq!(reused.meta.guarantee, GraphGuarantee::Exact);
    assert_eq!(sorted_symbol_names(&reused), ["set_alpha", "set_beta"]);

    let mut different = params(dir.path(), op);
    different.files = vec![a];
    different.max_stale_ms = Some(u64::MAX);
    let refreshed = graph(&eng, &different).unwrap();
    assert!(refreshed.meta.swept);
    assert_eq!(symbol_names(&refreshed), ["set_alpha"]);
}

#[test]
fn walk_sweep_publish_removes_a_deleted_file() {
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "src/retained.rs",
        "pub fn retained_symbol() {}\n",
    );
    let deleted = seed(dir.path(), "src/deleted.rs", "pub fn deleted_symbol() {}\n");
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Search {
            query: "symbol".into(),
            limit: 20,
        },
    );

    let initial = graph(&eng, &query).unwrap();
    assert_eq!(initial.meta.indexed_files, 2);
    assert_eq!(
        symbol_names(&initial),
        ["deleted_symbol", "retained_symbol"]
    );

    std::fs::remove_file(&deleted).unwrap();
    eng.invalidate_path(Path::new(&deleted));
    let refreshed = graph(&eng, &query).unwrap();

    assert!(refreshed.meta.swept);
    assert_eq!(refreshed.meta.indexed_files, 1);
    assert_eq!(symbol_names(&refreshed), ["retained_symbol"]);
}

#[cfg(unix)]
#[test]
fn walk_sweep_read_failure_removes_the_file_and_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "src/readable.rs",
        "pub fn readable_symbol() {}\n",
    );
    let unreadable = seed(
        dir.path(),
        "src/unreadable.rs",
        "pub fn unreadable_symbol() {}\n",
    );
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Search {
            query: "symbol".into(),
            limit: 20,
        },
    );

    let initial = graph(&eng, &query).unwrap();
    assert_eq!(initial.meta.indexed_files, 2);

    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
    eng.invalidate_path(Path::new(&unreadable));
    let refreshed = graph(&eng, &query);
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let refreshed = refreshed.unwrap();

    assert!(refreshed.meta.swept);
    assert_eq!(refreshed.meta.indexed_files, 1);
    assert_eq!(symbol_names(&refreshed), ["readable_symbol"]);
    let current = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert_eq!(status(&current).failed_files, 1);
}

#[test]
fn verification_11_unsupported_extensions_are_counted_without_error() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), "src/lib.rs", "pub fn supported_symbol() {}\n");
    seed(
        dir.path(),
        "src/styles.css",
        ".unsupported { color: red; }\n",
    );
    let eng = engine(dir.path());

    let result = graph(
        &eng,
        &params(
            dir.path(),
            GraphOp::Search {
                query: "supported".into(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    assert_eq!(result.meta.universe_files, 2);
    assert_eq!(result.meta.indexed_files, 1);
    assert_eq!(result.meta.unsupported_files, 1);
    assert_eq!(symbol_names(&result), ["supported_symbol"]);
}

#[test]
fn verification_12_clear_caches_detaches_roots_and_the_next_query_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "src/lib.rs", "pub fn rebuilt_symbol() {}\n");
    let eng = engine(dir.path());

    graph(
        &eng,
        &params(dir.path(), GraphOp::Symbols { path: file.clone() }),
    )
    .unwrap();
    assert_eq!(graph_clear(&eng), 1);
    let cleared = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert!(!status(&cleared).built);

    graph(&eng, &params(dir.path(), GraphOp::Symbols { path: file })).unwrap();
    assert!(status(&graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap()).built);

    assert!(matches!(
        dispatch(&eng, Request::ClearCaches),
        Response::Invalidate(_)
    ));
    let dispatch_cleared = graph(&eng, &params(dir.path(), GraphOp::Status)).unwrap();
    assert!(!status(&dispatch_cleared).built);
}

#[test]
fn verification_13_rdeps_repair_excludes_comment_only_false_positive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = true;\n");
    let importer = seed(
        &root,
        "src/importer.ts",
        "import \"./target\";\nexport const imported = true;\n",
    );
    let comment_only = seed(
        &root,
        "src/comment_only.ts",
        "// target appears here, but this file has no import statement.\nexport const unrelated = true;\n",
    );
    let eng = engine(&root);

    let op = GraphOp::Rdeps {
        path: target.clone(),
        depth: 1,
        verify: true,
    };
    let mut repair_query = params(&root, op.clone());
    repair_query.files = vec![target, importer.clone()];
    let repaired = graph(&eng, &repair_query).unwrap();
    let GraphOutput::Rdeps(repaired_rdeps) = repaired.output else {
        panic!("expected rdeps");
    };
    assert!(repaired_rdeps.verified);
    // The repair query runs under a files view, so universe files outside the
    // view were not revalidated by it — Approximate is the honest label here,
    // pinned so the mid-repair state stays observed. Exactness is asserted on
    // the full-universe query below.
    assert_eq!(repaired.meta.guarantee, GraphGuarantee::Approximate);
    assert_eq!(
        status(&graph(&eng, &params(&root, GraphOp::Status)).unwrap()).indexed_files,
        3,
        "the grep hit omitted from the subset must be analyzed by repair"
    );

    let result = graph(&eng, &params(&root, op)).unwrap();
    let GraphOutput::Rdeps(rdeps) = result.output else {
        panic!("expected rdeps");
    };

    assert_eq!(result.meta.guarantee, GraphGuarantee::Exact);
    assert!(rdeps.verified);
    assert!(
        rdeps
            .importers
            .iter()
            .any(|entry| entry.node.path == importer)
    );
    assert!(
        rdeps
            .importers
            .iter()
            .all(|entry| entry.node.path != comment_only)
    );
}

#[test]
fn verification_14_files_subset_query_never_destroys_existing_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let included = seed(
        dir.path(),
        "src/included.rs",
        "pub fn included_symbol() {}\n",
    );
    let retained = seed(
        dir.path(),
        "src/retained.rs",
        "pub fn retained_symbol() {}\n",
    );
    let eng = engine(dir.path());

    graph(
        &eng,
        &params(
            dir.path(),
            GraphOp::Search {
                query: "symbol".into(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    std::fs::write(&included, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    let mut subset = params(
        dir.path(),
        GraphOp::Search {
            query: "included".into(),
            limit: 20,
        },
    );
    subset.files = vec![included];
    let subset_result = graph(&eng, &subset).unwrap();
    assert_eq!(subset_result.meta.oversize_files, 1);

    let mut retained_query = params(
        dir.path(),
        GraphOp::Symbols {
            path: retained.clone(),
        },
    );
    retained_query.max_stale_ms = Some(u64::MAX);
    let result = graph(&eng, &retained_query).unwrap();

    assert!(!result.meta.swept);
    // The subset sweep discovered an unindexable (oversize) file in this
    // root, and a known-unindexable file caps every answer at Approximate —
    // the retained index entries are still served, but not labeled Exact.
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert_eq!(symbol_names(&result), ["retained_symbol"]);
    assert_eq!(result.meta.indexed_files, 2);
}

#[test]
fn verification_15_same_size_same_mtime_edit_requires_invalidation_log_path() {
    let dir = tempfile::tempdir().unwrap();
    let file = seed(dir.path(), "src/lib.rs", "pub fn alpha() {}\n");
    let eng = engine(dir.path());
    let query = || {
        graph(
            &eng,
            &params(dir.path(), GraphOp::Symbols { path: file.clone() }),
        )
        .unwrap()
    };

    assert_eq!(symbol_names(&query()), ["alpha"]);
    let original_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

    let mut handle = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&file)
        .unwrap();
    handle.write_all(b"pub fn omega() {}\n").unwrap();
    handle.flush().unwrap();
    handle
        .set_times(FileTimes::new().set_modified(original_mtime))
        .unwrap();
    drop(handle);

    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        original_mtime
    );
    assert_eq!(
        symbol_names(&query()),
        ["alpha"],
        "the reverse stat gate intentionally cannot detect same-size same-mtime edits"
    );

    eng.invalidate_path(Path::new(&file));
    assert_eq!(symbol_names(&query()), ["omega"]);
}

#[test]
fn verification_15b_same_size_same_mtime_rename_invalidates_file_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let source = seed(&root, "src/a.rs", "pub fn source_symbol() {}\n");
    let target = seed(&root, "src/b.rs", "pub fn target_symbol() {}\n");
    let eng = watching_engine(&root);
    eng.watch_root(&root);

    let probe = root.join(".watch-probe");
    let readiness_deadline = Instant::now() + Duration::from_secs(30);
    for attempt in 0_u64.. {
        std::fs::write(&probe, attempt.to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        let delta = eng.invalidations().since(0);
        let ready = match &delta.paths {
            None => true,
            Some(paths) => paths.iter().any(|path| path == &probe),
        };
        if ready {
            break;
        }
        if Instant::now() >= readiness_deadline {
            panic!(
                "watcher did not become ready after repeated writes to {probe:?}; \
                 last delta: {delta:?}"
            );
        }
    }

    let query = |path: &str| {
        graph(
            &eng,
            &params(
                &root,
                GraphOp::Symbols {
                    path: path.to_owned(),
                },
            ),
        )
        .unwrap()
    };

    assert_eq!(symbol_names(&query(&source)), ["source_symbol"]);
    assert_eq!(symbol_names(&query(&target)), ["target_symbol"]);
    assert_eq!(
        std::fs::metadata(&source).unwrap().len(),
        std::fs::metadata(&target).unwrap().len(),
        "fixture contents must have identical byte lengths"
    );
    let target_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();

    std::fs::rename(&source, &target).unwrap();
    let renamed = OpenOptions::new().write(true).open(&target).unwrap();
    renamed
        .set_times(FileTimes::new().set_modified(target_mtime))
        .unwrap();
    drop(renamed);

    assert_eq!(
        std::fs::metadata(&target).unwrap().modified().unwrap(),
        target_mtime
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = query(&target);
        let names = symbol_names(&result);
        if names == ["source_symbol"] {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "watcher did not invalidate the stale b.rs FileCache entry after rename; \
                 last symbols: {names:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn verification_15c_alias_root_same_size_same_mtime_rename_invalidates_file_cache() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap();
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    seed(&real, "src/a.rs", "pub fn source_symbol() {}\n");
    seed(&real, "src/b.rs", "pub fn target_symbol() {}\n");
    let source = alias.join("src/a.rs").display().to_string();
    let target = alias.join("src/b.rs").display().to_string();
    let eng = watching_engine(&alias);
    eng.watch_root(&alias);

    let probe = alias.join(".watch-probe");
    let real_probe = real.join(".watch-probe");
    let readiness_deadline = Instant::now() + Duration::from_secs(30);
    for attempt in 0_u64.. {
        std::fs::write(&probe, attempt.to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        let delta = eng.invalidations().since(0);
        let ready = match &delta.paths {
            None => true,
            Some(paths) => paths
                .iter()
                .any(|path| path == &probe || path == &real_probe),
        };
        if ready {
            break;
        }
        if Instant::now() >= readiness_deadline {
            panic!(
                "watcher did not become ready after repeated writes to {probe:?}; \
                 last delta: {delta:?}"
            );
        }
    }

    let query = |path: &str| {
        graph(
            &eng,
            &params(
                &alias,
                GraphOp::Symbols {
                    path: path.to_owned(),
                },
            ),
        )
        .unwrap()
    };

    assert_eq!(symbol_names(&query(&source)), ["source_symbol"]);
    assert_eq!(symbol_names(&query(&target)), ["target_symbol"]);
    assert_eq!(
        std::fs::metadata(&source).unwrap().len(),
        std::fs::metadata(&target).unwrap().len(),
        "fixture contents must have identical byte lengths"
    );
    let target_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();

    std::fs::rename(real.join("src/a.rs"), real.join("src/b.rs")).unwrap();
    let renamed = OpenOptions::new().write(true).open(&target).unwrap();
    renamed
        .set_times(FileTimes::new().set_modified(target_mtime))
        .unwrap();
    drop(renamed);

    assert_eq!(
        std::fs::metadata(&target).unwrap().modified().unwrap(),
        target_mtime
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = query(&target);
        let names = symbol_names(&result);
        if names == ["source_symbol"] {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "watcher did not invalidate the alias-keyed b.rs FileCache entry after rename; \
                 last symbols: {names:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn verification_17_tsconfig_paths_change_reresolves_unchanged_importer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        "import { value } from \"@lib/thing\";\nexport { value };\n",
    );
    let first_target = seed(&root, "a/thing.ts", "export const value = \"a\";\n");
    let second_target = seed(&root, "b/thing.ts", "export const value = \"b\";\n");
    let config = seed(
        &root,
        "tsconfig.json",
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@lib/*\":[\"./a/*\"]}}}\n",
    );
    let first_target = std::fs::canonicalize(first_target)
        .unwrap()
        .display()
        .to_string();
    let second_target = std::fs::canonicalize(second_target)
        .unwrap()
        .display()
        .to_string();
    let eng = engine(&root);
    let query = params(
        &root,
        GraphOp::Deps {
            path: importer,
            depth: 1,
        },
    );

    let first = graph(&eng, &query).unwrap();
    let GraphOutput::Deps(first_deps) = &first.output else {
        panic!("expected deps");
    };
    assert_eq!(first.meta.guarantee, GraphGuarantee::Exact);
    assert!(
        first_deps
            .edges
            .iter()
            .any(|edge| edge.to == first_target && edge.guarantee == GraphGuarantee::Exact)
    );

    let old_size = std::fs::metadata(&config).unwrap().len();
    std::fs::write(
        &config,
        "{\"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@lib/*\": [\"./b/*\"] } }}\n",
    )
    .unwrap();
    assert_ne!(std::fs::metadata(&config).unwrap().len(), old_size);

    // Resolver-config detection and reresolution are synchronous inside the
    // detecting query, so the public API cannot observe an intermediate
    // Approximate phase.
    let refreshed = graph(&eng, &query).unwrap();
    let GraphOutput::Deps(refreshed_deps) = &refreshed.output else {
        panic!("expected deps");
    };
    assert_eq!(refreshed.meta.guarantee, GraphGuarantee::Exact);
    assert!(
        refreshed_deps
            .edges
            .iter()
            .any(|edge| edge.to == second_target && edge.guarantee == GraphGuarantee::Exact)
    );
    assert!(
        refreshed_deps
            .edges
            .iter()
            .all(|edge| edge.to != first_target)
    );
}

#[test]
fn verification_18_any_opaque_import_makes_rdeps_approximate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = true;\n");
    let importer = seed(
        &root,
        "src/importer.ts",
        "import \"./target\";\nexport const imported = true;\n",
    );
    seed(
        &root,
        "src/opaque.ts",
        concat!(
            "declare function pick(): string;\n",
            "export async function load() {\n",
            "  const m = await import(pick());\n",
            "  return m;\n",
            "}\n"
        ),
    );
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Rdeps {
                path: target,
                depth: 1,
                verify: true,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Rdeps(rdeps) = result.output else {
        panic!("expected rdeps");
    };

    assert!(
        rdeps
            .importers
            .iter()
            .any(|entry| entry.node.path == importer)
    );
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
}

#[test]
fn verification_19_concurrent_cold_queries_wait_for_first_publication() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..128 {
        seed(
            dir.path(),
            &format!("src/cold_{index:03}.rs"),
            &format!("pub fn cold_symbol_{index:03}() {{}}\n"),
        );
    }
    let eng = engine(dir.path());
    let barrier = Arc::new(Barrier::new(3));
    let query = params(
        dir.path(),
        GraphOp::Search {
            query: "cold_symbol".into(),
            limit: 256,
        },
    );

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let eng = eng.clone();
            let barrier = Arc::clone(&barrier);
            let query = query.clone();
            std::thread::spawn(move || {
                barrier.wait();
                graph(&eng, &query).unwrap()
            })
        })
        .collect();
    barrier.wait();

    for handle in handles {
        let result = handle.join().unwrap();
        assert_eq!(
            symbol_names(&result).len(),
            128,
            "a Building root must never be observed as a valid empty repository"
        );
    }

    assert_eq!(
        abs(dir.path(), "src/cold_000.rs"),
        dir.path().join("src/cold_000.rs").display().to_string()
    );
}

#[test]
fn resolver_config_extends_edit_reresolves_without_explicit_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let importer = seed(
        dir.path(),
        "src/app.ts",
        "import { value } from \"#x/mod\";\nexport { value };\n",
    );
    let first_target = seed(dir.path(), "a/mod.ts", "export const value = \"a\";\n");
    let second_target = seed(
        dir.path(),
        "longer-b/mod.ts",
        "export const value = \"b\";\n",
    );
    let first_target = std::fs::canonicalize(first_target)
        .unwrap()
        .display()
        .to_string();
    let second_target = std::fs::canonicalize(second_target)
        .unwrap()
        .display()
        .to_string();
    seed(
        dir.path(),
        "tsconfig.json",
        "{\"extends\":\"./tsconfig.base.json\"}\n",
    );
    let base = seed(
        dir.path(),
        "tsconfig.base.json",
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"#x/*\":[\"a/*\"]}}}\n",
    );
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Deps {
            path: importer,
            depth: 1,
        },
    );

    let first = graph(&eng, &query).unwrap();
    assert!(dep_targets(&first).contains(&first_target.as_str()));

    std::fs::write(
        base,
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"#x/*\":[\"longer-b/*\"]}}}\n",
    )
    .unwrap();
    let refreshed = graph(&eng, &query).unwrap();
    let targets = dep_targets(&refreshed);
    assert!(targets.contains(&second_target.as_str()));
    assert!(!targets.contains(&first_target.as_str()));
}

#[test]
fn creating_previously_missing_root_tsconfig_enables_alias_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let importer = seed(
        dir.path(),
        "src/app.ts",
        "import { configured } from \"workspace-alias\";\nexport { configured };\n",
    );
    let target = seed(
        dir.path(),
        "configured/target.ts",
        "export const configured = true;\n",
    );
    let target = std::fs::canonicalize(target).unwrap().display().to_string();
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Deps {
            path: importer,
            depth: 1,
        },
    );

    let first = graph(&eng, &query).unwrap();
    assert!(dep_targets(&first).is_empty());

    seed(
        dir.path(),
        "tsconfig.json",
        concat!(
            "{\"compilerOptions\":{\"baseUrl\":\".\",",
            "\"paths\":{\"workspace-alias\":[\"configured/target.ts\"]}}}\n"
        ),
    );
    let refreshed = graph(&eng, &query).unwrap();
    assert!(dep_targets(&refreshed).contains(&target.as_str()));
}

#[test]
fn verified_rdeps_keep_present_opaque_grep_hits_as_approximate() {
    let dir = tempfile::tempdir().unwrap();
    let target = seed(
        dir.path(),
        "src/target.ts",
        "export const targetValue = true;\n",
    );
    let opaque = seed(
        dir.path(),
        "src/opaque.ts",
        "const targetPath = \"./target\";\nvoid import(targetPath);\n",
    );
    let mixed = seed(
        dir.path(),
        "src/mixed.ts",
        "import \"./target\";\nconst runtimePath = \"./other\";\nvoid import(runtimePath);\n",
    );
    let eng = engine(dir.path());
    let query = params(
        dir.path(),
        GraphOp::Rdeps {
            path: target,
            depth: 1,
            verify: true,
        },
    );

    let result = graph(&eng, &query).unwrap();
    let GraphOutput::Rdeps(rdeps) = result.output else {
        panic!("expected rdeps");
    };
    assert!(rdeps.verified);
    assert_eq!(rdeps.importers.len(), 2);
    assert!(
        rdeps
            .importers
            .iter()
            .any(|entry| entry.node.path == opaque)
    );
    assert!(rdeps.importers.iter().any(|entry| entry.node.path == mixed));
    assert!(
        rdeps
            .importers
            .iter()
            .all(|entry| entry.guarantee == GraphGuarantee::Approximate)
    );
    assert!(
        rdeps
            .importers
            .iter()
            .all(|entry| entry.specifier.is_none())
    );
}

#[test]
fn depth_zero_queries_carry_structural_guarantees_not_constant_exact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/main.rs",
        "mod util;\nuse crate::util::run;\nfn main() { run(); }\n",
    );
    seed(&root, "src/util.rs", "pub fn run() {}\n");
    let eng = engine(&root);

    // A Rust-containing root can never prove exactness, even for an empty
    // depth-0 answer — the old constant-Exact early return mislabeled this.
    let deps = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: importer.clone(),
                depth: 0,
            },
        ),
    )
    .unwrap();
    assert_eq!(deps.meta.guarantee, GraphGuarantee::Approximate);

    let rdeps = graph(
        &eng,
        &params(
            &root,
            GraphOp::Rdeps {
                path: importer,
                depth: 0,
                verify: false,
            },
        ),
    )
    .unwrap();
    assert_eq!(rdeps.meta.guarantee, GraphGuarantee::Approximate);
}
