//! Graph adapter contract: lazy publication, cache coherence, caller-provided
//! universes, deterministic Stage-A queries, and the cold-build barrier.

mod common;

use common::{abs, engine, seed, trusting_engine, watching_engine};
use hearth_core::CancelToken;
use hearth_proto::{
    ErrorKind, GraphGuarantee, GraphOp, GraphOutput, GraphParams, GraphPrefetchParams, GraphResult,
    GraphStatusResult, ReadParams, Request, Response, WriteParams,
};
use hearth_tools::{
    dispatch, graph, graph_cancellable, graph_clear, graph_prefetch, graph_prefetch_cancellable,
    read, write,
};
#[cfg(unix)]
use std::ffi::OsStr;
use std::fs::{FileTimes, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
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
fn prefetch_warms_only_seeds_and_direct_imports_without_ignore_or_walk_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let seed_path = seed(
        &root,
        "src/seed.ts",
        concat!(
            "import './.direct';\n",
            "import './missing';\n",
            "import 'external-package';\n",
            "export const seedValue = true;\n",
        ),
    );
    seed(
        &root,
        "src/.direct.ts",
        "import './deep';\nexport const directValue = true;\n",
    );
    let deep_path = seed(&root, "src/deep.ts", "export const deepValue = true;\n");
    seed(
        &root,
        "src/unrelated.ts",
        "export const unrelatedValue = true;\n",
    );
    std::fs::write(root.join(".gitignore"), "src/.direct.ts\n").unwrap();
    std::fs::write(root.join(".ignore"), "src/.direct.ts\n").unwrap();
    std::fs::write(root.join(".rgignore"), "src/.direct.ts\n").unwrap();
    std::fs::create_dir_all(root.join("node_modules/external-package")).unwrap();
    std::fs::write(
        root.join("node_modules/external-package/package.json"),
        r#"{"name":"external-package","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("node_modules/external-package/index.js"),
        "module.exports = {};\n",
    )
    .unwrap();

    let eng = engine(&root);
    let result = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(
            root.display().to_string(),
            vec![seed_path.clone(), seed_path],
        ),
    )
    .unwrap();

    assert_eq!(result.seeds_indexed, 1);
    assert_eq!(result.targets_warmed, 1);
    assert_eq!(result.skips.duplicate_seeds, 1);
    assert_eq!(result.skips.unresolved, 1);
    assert_eq!(result.skips.external, 1);
    assert_eq!(result.skips.ignored, 0);
    assert_eq!(eng.files().len(), 2, "deep and unrelated files stay cold");
    assert!(
        eng.walks().is_empty(),
        "prefetch must not populate WalkCache"
    );

    let current = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert!(!status(&current).built);
    assert_eq!(status(&current).indexed_files, 2);
    assert_eq!(status(&current).languages[0].files, 2);
    assert_eq!(status(&current).components, 0);
    assert_eq!(current.meta.guarantee, GraphGuarantee::Approximate);

    let direct_path = root.join("src/.direct.ts").display().to_string();
    let mut upgrade = params(
        &root,
        GraphOp::Deps {
            path: direct_path.clone(),
            depth: 1,
        },
    );
    upgrade.files = vec![direct_path, deep_path];
    let upgraded = graph(&eng, &upgrade).unwrap();
    assert_eq!(dep_targets(&upgraded).len(), 1);
    assert!(eng.walks().is_empty(), "an explicit upgrade must not walk");
}

#[test]
fn prefetch_infers_cold_rust_crate_roots_without_reading_the_root_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    seed(&root, "src/lib.rs", "mod shared;\n");
    let feature = seed(
        &root,
        "src/nested/feature.rs",
        "use crate::shared;\npub fn feature() {}\n",
    );
    seed(&root, "src/shared.rs", "mod deep;\npub fn shared() {}\n");
    seed(&root, "src/shared/deep.rs", "pub fn deep() {}\n");
    let eng = engine(&root);

    let result = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![feature]),
    )
    .unwrap();

    assert_eq!(result.targets_warmed, 1);
    assert_eq!(eng.files().len(), 2, "lib.rs and deep.rs must stay cold");
    assert!(eng.walks().is_empty());
}

#[test]
fn prefetch_infers_rust_roots_under_parent_component_and_nested_bin_layouts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("discarded")).unwrap();
    seed(&root, "src/lib.rs", "mod shared;\n");
    let library_seed = seed(
        &root,
        "src/nested/feature.rs",
        "use crate::shared;\npub fn feature() {}\n",
    );
    seed(&root, "src/shared.rs", "pub fn shared() {}\n");
    let spelled_root = root.join("discarded").join("..");
    let eng = engine(&root);

    let library = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(spelled_root.display().to_string(), vec![library_seed]),
    )
    .unwrap();
    assert_eq!(library.targets_warmed, 1);

    seed(&root, "src/bin/tool/main.rs", "mod shared;\n");
    let binary_seed = seed(
        &root,
        "src/bin/tool/worker.rs",
        "use crate::shared;\npub fn worker() {}\n",
    );
    seed(
        &root,
        "src/bin/tool/shared.rs",
        "pub fn binary_shared() {}\n",
    );
    let binary = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![binary_seed]),
    )
    .unwrap();
    assert_eq!(binary.targets_warmed, 1);
}

#[test]
fn prefetch_prunes_deleted_rust_crate_roots_before_request_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let crate_root = root.join("src/lib.rs");
    seed(&root, "src/lib.rs", "mod shared;\n");
    let feature = seed(
        &root,
        "src/nested/feature.rs",
        "use crate::shared;\npub fn feature() {}\n",
    );
    seed(&root, "src/shared.rs", "pub fn shared() {}\n");
    let eng = engine(&root);
    let options = GraphPrefetchParams::new(root.display().to_string(), vec![feature]);

    assert_eq!(graph_prefetch(&eng, &options).unwrap().targets_warmed, 1);
    std::fs::remove_file(crate_root).unwrap();
    let without_root = graph_prefetch(&eng, &options).unwrap();

    assert_eq!(without_root.targets_discovered, 0);
    assert_eq!(without_root.targets_warmed, 0);
    assert_eq!(without_root.skips.unresolved, 1);
}

#[test]
fn prefetch_refreshes_root_and_extended_js_config_only_when_stats_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        "import { value } from '@lib/value';\nexport { value };\n",
    );
    seed(&root, "a/value.ts", "export const value = 'a';\n");
    seed(&root, "longer-b/value.ts", "export const value = 'b';\n");
    seed(&root, "third/value.ts", "export const value = 'c';\n");
    let base = seed(
        &root,
        "base.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["a/*"]}}}"#,
    );
    let config = seed(&root, "tsconfig.json", r#"{"extends":"./base.json"}"#);
    let eng = engine(&root);
    let options = GraphPrefetchParams::new(root.display().to_string(), vec![importer]);

    let first = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(first.targets_warmed, 1);

    std::fs::write(
        &base,
        r#"{"compilerOptions": {"baseUrl":".","paths":{"@lib/*":["longer-b/*"]}}}"#,
    )
    .unwrap();
    let extended_change = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(extended_change.targets_warmed, 1);
    assert!(extended_change.graph_updates >= 1);

    std::fs::write(&config, r#"{"extends":"./third.json"}"#).unwrap();
    seed(
        &root,
        "third.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["third/*"]}}}"#,
    );
    let root_change = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(root_change.targets_warmed, 1);
    assert!(root_change.graph_updates >= 1);

    let unchanged = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(unchanged.targets_warmed, 1);
    assert_eq!(unchanged.graph_updates, 0);
    assert!(unchanged.cache_hits >= 2);
    assert!(eng.walks().is_empty());
}

#[test]
fn prefetch_honors_explicit_config_invalidation_when_stat_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(&root, "src/app.ts", "import '#dep';\n");
    seed(&root, "a/dep.ts", "export const value = 'a';\n");
    let second_target = seed(&root, "b/dep.ts", "export const value = 'b';\n");
    let config = root.join("tsconfig.json");
    std::fs::write(
        &config,
        r##"{"compilerOptions":{"baseUrl":".","paths":{"#dep":["a/dep.ts"]}}}"##,
    )
    .unwrap();
    let original_mtime = std::fs::metadata(&config).unwrap().modified().unwrap();
    let eng = engine(&root);
    let options = GraphPrefetchParams::new(root.display().to_string(), vec![importer]);

    assert_eq!(graph_prefetch(&eng, &options).unwrap().targets_warmed, 1);
    std::fs::write(
        &config,
        r##"{"compilerOptions":{"baseUrl":".","paths":{"#dep":["b/dep.ts"]}}}"##,
    )
    .unwrap();
    OpenOptions::new()
        .write(true)
        .open(&config)
        .unwrap()
        .set_times(FileTimes::new().set_modified(original_mtime))
        .unwrap();
    eng.invalidate_path(&config);

    let refreshed = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(refreshed.targets_warmed, 1);
    assert!(refreshed.graph_updates >= 1);
    assert!(
        read(&eng, &ReadParams::new(second_target))
            .unwrap()
            .cache_hit
    );
}

#[test]
fn prefetch_prunes_obsolete_resolver_dependencies_before_stat_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = root.join("src/app.ts");
    seed(&root, "src/app.ts", "import 'package-a';\n");
    let package_a = root.join("node_modules/package-a/package.json");
    seed(
        &root,
        "node_modules/package-a/package.json",
        r#"{"name":"package-a","main":"index.js"}"#,
    );
    seed(
        &root,
        "node_modules/package-a/index.js",
        "module.exports = {};\n",
    );
    seed(
        &root,
        "node_modules/package-b/package.json",
        r#"{"name":"package-b","main":"index.js"}"#,
    );
    seed(
        &root,
        "node_modules/package-b/index.js",
        "module.exports = {};\n",
    );
    let eng = engine(&root);
    let options = GraphPrefetchParams::new(
        root.display().to_string(),
        vec![importer.display().to_string()],
    );

    assert_eq!(graph_prefetch(&eng, &options).unwrap().graph_updates, 1);
    std::fs::write(&importer, "import 'package-b';\n").unwrap();
    eng.invalidate_path(&importer);
    assert_eq!(graph_prefetch(&eng, &options).unwrap().graph_updates, 1);

    std::fs::write(package_a, r#"{"name":"package-a","main":"different.js"}"#).unwrap();
    let unchanged = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(unchanged.graph_updates, 0);
}

#[test]
fn prefetch_reduced_limits_are_clamped_and_report_each_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let first = seed(
        &root,
        "src/first.ts",
        "import './one';\nimport './two';\nexport const first = true;\n",
    );
    let second = seed(&root, "src/second.ts", "export const second = true;\n");
    seed(&root, "src/one.ts", "export const one = true;\n");
    seed(&root, "src/two.ts", "export const two = true;\n");

    let mut seed_limited =
        GraphPrefetchParams::new(root.display().to_string(), vec![first.clone(), second]);
    seed_limited.max_seeds = Some(1);
    let seed_result = graph_prefetch(&engine(&root), &seed_limited).unwrap();
    assert_eq!(seed_result.seeds_processed, 1);
    assert_eq!(seed_result.skips.seed_limit, 1);
    assert!(seed_result.truncated);

    let mut import_limited =
        GraphPrefetchParams::new(root.display().to_string(), vec![first.clone()]);
    import_limited.max_targets_per_seed = Some(1);
    let import_result = graph_prefetch(&engine(&root), &import_limited).unwrap();
    assert_eq!(import_result.imports_examined, 1);
    assert_eq!(import_result.targets_warmed, 1);
    assert_eq!(import_result.skips.target_limit, 1);
    assert!(import_result.truncated);

    let mut target_limited =
        GraphPrefetchParams::new(root.display().to_string(), vec![first.clone()]);
    target_limited.max_targets = Some(1);
    let target_result = graph_prefetch(&engine(&root), &target_limited).unwrap();
    assert_eq!(target_result.imports_examined, 2);
    assert_eq!(target_result.targets_discovered, 2);
    assert_eq!(target_result.targets_warmed, 1);
    assert_eq!(target_result.skips.target_limit, 1);
    assert!(target_result.truncated);

    let mut file_limited =
        GraphPrefetchParams::new(root.display().to_string(), vec![first.clone()]);
    file_limited.max_file_bytes = Some(8);
    let file_result = graph_prefetch(&engine(&root), &file_limited).unwrap();
    assert_eq!(file_result.skips.oversize, 1);
    assert!(file_result.truncated);

    let mut total_limited = GraphPrefetchParams::new(root.display().to_string(), vec![first]);
    total_limited.max_total_bytes = Some(8);
    let total_result = graph_prefetch(&engine(&root), &total_limited).unwrap();
    assert_eq!(total_result.skips.byte_limit, 1);
    assert!(total_result.truncated);
}

#[test]
fn prefetch_does_not_validate_seed_paths_beyond_the_admitted_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let admitted = seed(&root, "admitted.ts", "export const admitted = true;\n");
    let mut options = GraphPrefetchParams::new(
        root.display().to_string(),
        vec![admitted, "x".repeat(64 * 1024 + 1)],
    );
    options.max_seeds = Some(1);

    let result = graph_prefetch(&engine(&root), &options).unwrap();

    assert_eq!(result.seeds_indexed, 1);
    assert_eq!(result.skips.seed_limit, 1);
    assert!(result.truncated);
}

#[test]
fn prefetch_hard_caps_bound_seeds_imports_targets_file_bytes_and_total_bytes() {
    const FILE_CAP: usize = 2 * 1024 * 1024;
    const TOTAL_CAP: u64 = 16 * 1024 * 1024;

    let seed_dir = tempfile::tempdir().unwrap();
    let seed_root = seed_dir.path().canonicalize().unwrap();
    let seeds: Vec<_> = (0..33)
        .map(|index| seed(&seed_root, &format!("seed-{index}.ts"), ""))
        .collect();
    let mut seed_options = GraphPrefetchParams::new(seed_root.display().to_string(), seeds);
    seed_options.max_seeds = Some(u64::MAX);
    let seed_result = graph_prefetch(&engine(&seed_root), &seed_options).unwrap();
    assert_eq!(seed_result.seeds_indexed, 32);
    assert_eq!(seed_result.skips.seed_limit, 1);
    assert!(seed_result.truncated);

    let import_dir = tempfile::tempdir().unwrap();
    let import_root = import_dir.path().canonicalize().unwrap();
    let mut source = String::new();
    for index in 0..65 {
        source.push_str(&format!("import './target-{index}';\n"));
        seed(&import_root, &format!("target-{index}.ts"), "");
    }
    let importer = seed(&import_root, "importer.ts", &source);
    let mut import_options =
        GraphPrefetchParams::new(import_root.display().to_string(), vec![importer]);
    import_options.max_targets_per_seed = Some(u64::MAX);
    let import_result = graph_prefetch(&engine(&import_root), &import_options).unwrap();
    assert_eq!(import_result.imports_examined, 64);
    assert_eq!(import_result.targets_warmed, 64);
    assert_eq!(import_result.skips.target_limit, 1);
    assert!(import_result.truncated);

    let target_dir = tempfile::tempdir().unwrap();
    let target_root = target_dir.path().canonicalize().unwrap();
    let mut importers = Vec::new();
    let mut target = 0usize;
    for seed_index in 0..5 {
        let count = if seed_index == 4 { 1 } else { 64 };
        let mut source = String::new();
        for _ in 0..count {
            source.push_str(&format!("import './target-{target}';\n"));
            seed(&target_root, &format!("target-{target}.ts"), "");
            target += 1;
        }
        importers.push(seed(
            &target_root,
            &format!("importer-{seed_index}.ts"),
            &source,
        ));
    }
    let mut target_options = GraphPrefetchParams::new(target_root.display().to_string(), importers);
    target_options.max_targets = Some(u64::MAX);
    let target_result = graph_prefetch(&engine(&target_root), &target_options).unwrap();
    assert_eq!(target_result.targets_discovered, 257);
    assert_eq!(target_result.targets_warmed, 256);
    assert_eq!(target_result.skips.target_limit, 1);
    assert!(target_result.truncated);

    let file_dir = tempfile::tempdir().unwrap();
    let file_root = file_dir.path().canonicalize().unwrap();
    let oversize = seed(&file_root, "oversize.ts", &"x".repeat(FILE_CAP + 1));
    let mut file_options =
        GraphPrefetchParams::new(file_root.display().to_string(), vec![oversize]);
    file_options.max_file_bytes = Some(u64::MAX);
    let file_result = graph_prefetch(&engine(&file_root), &file_options).unwrap();
    assert_eq!(file_result.skips.oversize, 1);
    assert_eq!(file_result.source_bytes, 0);
    assert!(file_result.truncated);

    let total_dir = tempfile::tempdir().unwrap();
    let total_root = total_dir.path().canonicalize().unwrap();
    let exact_cap_source = format!("/*{}*/", "x".repeat(FILE_CAP - 4));
    let mut total_files: Vec<_> = (0..8)
        .map(|index| seed(&total_root, &format!("large-{index}.ts"), &exact_cap_source))
        .collect();
    total_files.push(seed(&total_root, "after-cap.ts", "x"));
    let mut total_options = GraphPrefetchParams::new(total_root.display().to_string(), total_files);
    total_options.max_total_bytes = Some(u64::MAX);
    let total_result = graph_prefetch(&engine(&total_root), &total_options).unwrap();
    assert_eq!(total_result.source_bytes, TOTAL_CAP);
    assert_eq!(total_result.seeds_indexed, 8);
    assert_eq!(total_result.skips.byte_limit, 1);
    assert!(total_result.truncated);
}

#[test]
fn prefetch_full_node_reuse_obeys_import_cap_without_downgrading_exact_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/importer.ts",
        "import './one';\nimport './two';\nimport './three';\n",
    );
    let one = seed(&root, "src/one.ts", "export const one = 1;\n");
    let two = seed(&root, "src/two.ts", "export const two = 2;\n");
    let three = seed(&root, "src/three.ts", "export const three = 3;\n");
    let eng = engine(&root);
    let mut full = params(
        &root,
        GraphOp::Deps {
            path: importer.clone(),
            depth: 1,
        },
    );
    full.files = vec![importer.clone(), one, two, three];
    assert_eq!(dep_targets(&graph(&eng, &full).unwrap()).len(), 3);

    let mut limited = GraphPrefetchParams::new(root.display().to_string(), vec![importer]);
    limited.max_targets_per_seed = Some(1);
    let result = graph_prefetch(&eng, &limited).unwrap();
    assert_eq!(result.imports_examined, 1);
    assert_eq!(result.targets_warmed, 1);
    assert_eq!(result.skips.target_limit, 2);
    assert_eq!(result.graph_updates, 0);

    full.max_stale_ms = Some(u64::MAX);
    let preserved = graph(&eng, &full).unwrap();
    assert!(!preserved.meta.swept);
    assert_eq!(preserved.meta.guarantee, GraphGuarantee::Exact);
    assert_eq!(dep_targets(&preserved).len(), 3);
}

#[test]
fn prefetch_later_seed_can_admit_a_target_rejected_by_an_earlier_import_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let first = seed(
        &root,
        "first.ts",
        "import './first-only';\nimport './shared';\n",
    );
    let second = seed(&root, "second.ts", "import './shared';\n");
    seed(&root, "first-only.ts", "");
    seed(&root, "shared.ts", "");
    let mut options = GraphPrefetchParams::new(root.display().to_string(), vec![first, second]);
    options.max_targets_per_seed = Some(1);

    let result = graph_prefetch(&engine(&root), &options).unwrap();

    assert_eq!(result.imports_examined, 2);
    assert_eq!(result.targets_warmed, 2);
    assert_eq!(result.skips.target_limit, 1);
    assert_eq!(result.skips.duplicate_targets, 0);
}

#[test]
fn prefetch_trust_cache_reuses_stale_source_until_explicit_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = seed(&root, "seed.rs", "pub fn original() {}\n");
    let eng = trusting_engine(&root);
    let options = GraphPrefetchParams::new(root.display().to_string(), vec![file.clone()]);

    assert_eq!(graph_prefetch(&eng, &options).unwrap().graph_updates, 1);
    std::fs::write(&file, "pub fn changed_with_a_different_size() {}\n").unwrap();
    let stale = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(stale.cache_hits, 1);
    assert_eq!(stale.graph_updates, 0);

    eng.invalidate_path(Path::new(&file));
    let refreshed = graph_prefetch(&eng, &options).unwrap();
    assert_eq!(refreshed.graph_updates, 1);
}

#[test]
fn prefetch_real_insert_invalidates_reusable_sweep_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    seed(&root, "old.rs", "pub fn old() {}\n");
    let eng = engine(&root);
    let mut search = params(
        &root,
        GraphOp::Search {
            query: String::new(),
            limit: 20,
        },
    );
    graph(&eng, &search).unwrap();

    let inserted = seed(&root, "new.rs", "pub fn new() {}\n");
    eng.invalidate_path(Path::new(&inserted));
    let result = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![inserted]),
    )
    .unwrap();
    assert_eq!(result.graph_updates, 1);

    search.max_stale_ms = Some(u64::MAX);
    let refreshed = graph(&eng, &search).unwrap();
    assert!(refreshed.meta.swept);
    assert_eq!(refreshed.meta.indexed_files, 2);
}

#[test]
fn prefetch_same_content_stat_change_updates_records_and_invalidates_sweep_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = seed(&root, "same.rs", "pub fn same() {}\n");
    let eng = engine(&root);
    let mut search = params(
        &root,
        GraphOp::Search {
            query: String::new(),
            limit: 20,
        },
    );
    graph(&eng, &search).unwrap();

    let original_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(FileTimes::new().set_modified(original_mtime + Duration::from_secs(5)))
        .unwrap();
    let result = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![file.clone()]),
    )
    .unwrap();
    assert_eq!(result.graph_updates, 1);

    search.max_stale_ms = Some(u64::MAX);
    assert!(graph(&eng, &search).unwrap().meta.swept);
    assert_eq!(
        graph_prefetch(
            &eng,
            &GraphPrefetchParams::new(root.display().to_string(), vec![file]),
        )
        .unwrap()
        .graph_updates,
        0
    );
}

#[test]
fn prefetch_rejects_missing_non_regular_unsupported_and_non_utf8_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("directory.rs")).unwrap();
    let unsupported = seed(&root, "plain.txt", "plain text\n");
    let invalid = root.join("invalid.rs");
    std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
    let result = graph_prefetch(
        &engine(&root),
        &GraphPrefetchParams::new(
            root.display().to_string(),
            vec![
                "missing.rs".into(),
                "directory.rs".into(),
                unsupported,
                invalid.display().to_string(),
            ],
        ),
    )
    .unwrap();

    assert_eq!(result.skips.missing, 1);
    assert_eq!(result.skips.unsupported, 2);
    assert_eq!(result.skips.non_utf8, 1);
}

#[cfg(unix)]
#[test]
fn prefetch_skips_a_source_beneath_a_non_utf8_absolute_root_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(OsStr::from_bytes(b"root-\xff"));
    if std::fs::create_dir(&root).is_err() {
        return;
    }
    std::fs::write(root.join("seed.rs"), "pub fn seed() {}\n").unwrap();

    let mut options = GraphPrefetchParams::new(".", vec!["seed.rs".into()]);
    options.follow_symlinks = true;
    let result = graph_prefetch(&engine(&root), &options).unwrap();

    assert_eq!(result.seeds_processed, 1);
    assert_eq!(result.seeds_indexed, 0);
    assert_eq!(result.skips.non_utf8, 1);
}

#[test]
fn prefetch_honors_a_pre_cancelled_request_without_warming() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = seed(&root, "seed.ts", "export const seedValue = true;\n");
    let eng = engine(&root);
    let cancel = CancelToken::new();
    cancel.cancel();

    let error = graph_prefetch_cancellable(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![path]),
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(eng.files().is_empty());
}

#[cfg(unix)]
#[test]
fn prefetch_canonical_containment_blocks_root_and_symlink_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let root = root.canonicalize().unwrap();
    let outside_file = seed(&outside, "outside.rs", "pub fn outside() {}\n");
    symlink(&outside_file, root.join("escape.rs")).unwrap();

    let direct = graph_prefetch(
        &engine(&root),
        &GraphPrefetchParams::new(root.display().to_string(), vec![outside_file]),
    )
    .unwrap();
    assert_eq!(direct.skips.root_escaping, 1);

    let blocked_link = graph_prefetch(
        &engine(&root),
        &GraphPrefetchParams::new(root.display().to_string(), vec!["escape.rs".into()]),
    )
    .unwrap();
    assert_eq!(blocked_link.skips.symlink, 1);

    let mut followed =
        GraphPrefetchParams::new(root.display().to_string(), vec!["escape.rs".into()]);
    followed.follow_symlinks = true;
    let escaped_link = graph_prefetch(&engine(&root), &followed).unwrap();
    assert_eq!(escaped_link.skips.root_escaping, 1);

    let invalid_name = OsStr::from_bytes(b"invalid-\xff.rs");
    let invalid_path = root.join(invalid_name);
    if std::fs::write(&invalid_path, "pub fn invalid_name() {}\n").is_err() {
        return;
    }
    symlink(&invalid_path, root.join("utf8-link.rs")).unwrap();
    let mut non_utf8 =
        GraphPrefetchParams::new(root.display().to_string(), vec!["utf8-link.rs".into()]);
    non_utf8.follow_symlinks = true;
    let non_utf8_result = graph_prefetch(&engine(&root), &non_utf8).unwrap();
    assert_eq!(non_utf8_result.skips.non_utf8, 1);
}

#[cfg(unix)]
#[test]
fn prefetch_rejects_raw_seed_symlinks_even_when_parent_components_hide_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
    seed(&root, "seed.rs", "pub fn lexical_seed() {}\n");
    let resolved_seed = seed(&root, "nested/seed.rs", "pub fn resolved_seed() {}\n");
    symlink("nested/deeper", root.join("step")).unwrap();

    let blocked = graph_prefetch(
        &engine(&root),
        &GraphPrefetchParams::new(root.display().to_string(), vec!["step/../seed.rs".into()]),
    )
    .unwrap();

    assert_eq!(blocked.seeds_processed, 0);
    assert_eq!(blocked.skips.symlink, 1);

    let mut followed =
        GraphPrefetchParams::new(root.display().to_string(), vec!["step/../seed.rs".into()]);
    followed.follow_symlinks = true;
    let eng = engine(&root);
    let warmed = graph_prefetch(&eng, &followed).unwrap();
    assert_eq!(warmed.seeds_indexed, 1);
    assert!(
        read(&eng, &ReadParams::new(resolved_seed))
            .unwrap()
            .cache_hit
    );
}

#[cfg(unix)]
#[test]
fn prefetch_rejects_a_symlink_in_the_explicit_root_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let real_root = base.join("real-root");
    std::fs::create_dir(&real_root).unwrap();
    seed(&real_root, "seed.rs", "pub fn seed() {}\n");
    let linked_root = base.join("linked-root");
    symlink(&real_root, &linked_root).unwrap();

    let blocked = graph_prefetch(
        &engine(&base),
        &GraphPrefetchParams::new(linked_root.display().to_string(), vec!["seed.rs".into()]),
    )
    .unwrap();

    assert_eq!(blocked.seeds_processed, 0);
    assert_eq!(blocked.skips.symlink, 1);

    let canonical_seed = real_root
        .join("seed.rs")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let canonical_spelling = graph_prefetch(
        &engine(&base),
        &GraphPrefetchParams::new(linked_root.display().to_string(), vec![canonical_seed]),
    )
    .unwrap();
    assert_eq!(canonical_spelling.seeds_processed, 0);
    assert_eq!(canonical_spelling.skips.symlink, 1);

    let canonical_importer = seed(&real_root, "app.ts", "import './target';\n");
    seed(&real_root, "target.ts", "export const target = true;\n");
    let mut followed =
        GraphPrefetchParams::new(linked_root.display().to_string(), vec![canonical_importer]);
    followed.follow_symlinks = true;
    let eng = engine(&base);
    let warmed = graph_prefetch(&eng, &followed).unwrap();
    assert_eq!(warmed.targets_warmed, 1);

    let linked_importer = linked_root.join("app.ts").display().to_string();
    let linked_target = linked_root.join("target.ts").display().to_string();
    let mut deps = GraphParams::new(
        linked_root.display().to_string(),
        GraphOp::Deps {
            path: linked_importer.clone(),
            depth: 1,
        },
    );
    deps.files = vec![linked_importer, linked_target.clone()];
    assert_eq!(dep_targets(&graph(&eng, &deps).unwrap()), [linked_target]);

    let real_parent = base.join("real-parent");
    std::fs::create_dir_all(real_parent.join("subdir")).unwrap();
    seed(&real_parent, "subdir/nested.rs", "pub fn nested() {}\n");
    let linked_parent = base.join("linked-parent");
    symlink(&real_parent, &linked_parent).unwrap();
    let intermediate = graph_prefetch(
        &engine(&base),
        &GraphPrefetchParams::new(
            linked_parent.join("subdir").display().to_string(),
            vec!["nested.rs".into()],
        ),
    )
    .unwrap();
    assert_eq!(intermediate.seeds_processed, 0);
    assert_eq!(intermediate.skips.symlink, 1);

    let canonical_importer = seed(
        &real_parent,
        "subdir/app.ts",
        "import { target } from './target';\nexport { target };\n",
    );
    seed(
        &real_parent,
        "subdir/target.ts",
        "export const target = true;\n",
    );
    let intermediate_root = linked_parent.join("subdir");
    let mut followed = GraphPrefetchParams::new(
        intermediate_root.display().to_string(),
        vec![canonical_importer],
    );
    followed.follow_symlinks = true;
    let eng = engine(&base);
    let warmed = graph_prefetch(&eng, &followed).unwrap();
    assert_eq!(warmed.targets_warmed, 1);

    let linked_importer = intermediate_root.join("app.ts").display().to_string();
    let linked_target = intermediate_root.join("target.ts").display().to_string();
    let mut deps = GraphParams::new(
        intermediate_root.display().to_string(),
        GraphOp::Deps {
            path: linked_importer.clone(),
            depth: 1,
        },
    );
    deps.files = vec![linked_importer, linked_target.clone()];
    assert_eq!(dep_targets(&graph(&eng, &deps).unwrap()), [linked_target]);
}

#[test]
fn prefetch_accepts_a_root_spelling_with_parent_components() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("discarded")).unwrap();
    let seed_path = seed(&root, "seed.rs", "pub fn seed() {}\n");
    let spelled_root = root.join("discarded").join("..");
    let eng = engine(&root);

    let result = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(spelled_root.display().to_string(), vec!["seed.rs".into()]),
    )
    .unwrap();

    assert_eq!(result.seeds_indexed, 1);
    assert_eq!(result.skips.symlink, 0);
    assert!(
        read(&eng, &ReadParams::new(seed_path)).unwrap().cache_hit,
        "prefetch and ordinary reads must share the normalized cache key"
    );
}

#[test]
fn prefetch_partial_targets_upgrade_under_a_root_with_parent_components() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("discarded")).unwrap();
    let importer = seed(&root, "src/app.ts", "import './direct';\n");
    let direct = seed(&root, "src/direct.ts", "import './deep';\n");
    let deep = seed(&root, "src/deep.ts", "export const deep = true;\n");
    let spelled_root = root.join("discarded").join("..");
    let eng = engine(&root);

    let prefetched = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(spelled_root.display().to_string(), vec![importer]),
    )
    .unwrap();
    assert_eq!(prefetched.targets_warmed, 1);

    let mut upgrade = GraphParams::new(
        spelled_root.display().to_string(),
        GraphOp::Deps {
            path: direct.clone(),
            depth: 1,
        },
    );
    upgrade.files = vec![direct, deep.clone()];
    let upgraded = graph(&eng, &upgrade).unwrap();
    assert!(upgraded.meta.swept);
    assert_eq!(dep_targets(&upgraded), [deep.as_str()]);
}

#[cfg(unix)]
#[test]
fn prefetch_discovery_rejects_symlink_spelling_without_changing_resident_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        "import { dep } from './alias/dep';\nexport { dep };\n",
    );
    let canonical = seed(&root, "src/real/dep.ts", "export const dep = true;\n");
    symlink("real", root.join("src/alias")).unwrap();
    let eng = engine(&root);

    let blocked = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![importer.clone()]),
    )
    .unwrap();
    assert_eq!(blocked.targets_discovered, 1);
    assert_eq!(blocked.targets_warmed, 0);
    assert_eq!(blocked.skips.symlink, 1);

    let mut build = params(
        &root,
        GraphOp::Deps {
            path: importer.clone(),
            depth: 1,
        },
    );
    build.files = vec![importer.clone(), canonical.clone()];
    assert_eq!(
        dep_targets(&graph(&eng, &build).unwrap()),
        [canonical.as_str()]
    );

    let mut followed = GraphPrefetchParams::new(root.display().to_string(), vec![importer]);
    followed.follow_symlinks = true;
    let warmed = graph_prefetch(&eng, &followed).unwrap();
    assert_eq!(warmed.targets_warmed, 1);
}

#[cfg(unix)]
#[test]
fn prefetch_treats_a_symlinked_workspace_package_as_an_in_root_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        "import { value } from 'workspace-pkg';\nexport { value };\n",
    );
    seed(
        &root,
        "packages/workspace-pkg/index.js",
        "export const value = true;\n",
    );
    seed(
        &root,
        "packages/workspace-pkg/package.json",
        r#"{"name":"workspace-pkg","main":"index.js"}"#,
    );
    std::fs::create_dir(root.join("node_modules")).unwrap();
    symlink(
        "../packages/workspace-pkg",
        root.join("node_modules/workspace-pkg"),
    )
    .unwrap();
    let eng = engine(&root);

    let blocked = graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![importer.clone()]),
    )
    .unwrap();
    assert_eq!(blocked.targets_discovered, 1);
    assert_eq!(blocked.targets_warmed, 0);
    assert_eq!(blocked.skips.symlink, 1);
    assert_eq!(blocked.skips.external, 0);

    let mut followed = GraphPrefetchParams::new(root.display().to_string(), vec![importer]);
    followed.follow_symlinks = true;
    let warmed = graph_prefetch(&eng, &followed).unwrap();
    assert_eq!(warmed.targets_discovered, 1);
    assert_eq!(warmed.targets_warmed, 1);
    assert_eq!(warmed.skips.external, 0);
}

#[test]
fn prefetch_keeps_cold_status_unbuilt_and_preserves_an_existing_ready_phase() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let first = seed(&root, "first.rs", "pub fn first() {}\n");
    let eng = engine(&root);

    graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![first.clone()]),
    )
    .unwrap();
    let cold = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert!(!status(&cold).built);
    assert!(!cold.meta.swept);
    assert_eq!(cold.meta.guarantee, GraphGuarantee::Approximate);

    let mut build = params(
        &root,
        GraphOp::Search {
            query: String::new(),
            limit: 10,
        },
    );
    build.files = vec![first];
    assert!(graph(&eng, &build).unwrap().meta.swept);
    assert!(status(&graph(&eng, &params(&root, GraphOp::Status)).unwrap()).built);

    let second = seed(&root, "second.rs", "pub fn second() {}\n");
    graph_prefetch(
        &eng,
        &GraphPrefetchParams::new(root.display().to_string(), vec![second]),
    )
    .unwrap();
    let dirty = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert!(status(&dirty).built);
    assert_eq!(dirty.meta.guarantee, GraphGuarantee::Approximate);
}

#[test]
fn vue_sfc_symbols_and_dependencies_flow_through_the_graph_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/dep.ts", "export const dep = true;\n");
    let component = seed(
        &root,
        "src/App.vue",
        "<template><div /></template>\n\
<script setup lang=\"ts\">\n\
import { dep } from \"./dep\";\n\
export function vueEntry() { return dep; }\n\
</script>\n",
    );
    let eng = engine(&root);

    let outline = graph(
        &eng,
        &params(
            &root,
            GraphOp::Outline {
                path: component.clone(),
            },
        ),
    )
    .unwrap();
    assert_eq!(symbol_names(&outline), ["vueEntry"]);
    assert_eq!(outline.meta.unsupported_files, 0);

    let deps = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: component,
                depth: 1,
            },
        ),
    )
    .unwrap();
    assert_eq!(dep_targets(&deps), [target]);
    assert_eq!(deps.meta.guarantee, GraphGuarantee::Exact);
}

#[test]
fn hostile_graph_scalars_and_vectors_are_rejected_before_build() {
    let dir = tempfile::tempdir().unwrap();
    let eng = engine(dir.path());

    let mut params = GraphParams::new(
        dir.path().display().to_string(),
        GraphOp::Deps {
            path: "src/main.rs".into(),
            depth: u32::MAX,
        },
    );
    assert_eq!(
        graph(&eng, &params).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    params.op = GraphOp::Search {
        query: "x".into(),
        limit: u64::MAX,
    };
    assert_eq!(
        graph(&eng, &params).unwrap_err().kind,
        ErrorKind::InvalidInput
    );

    params.op = GraphOp::Status;
    params.files = vec!["x.rs".into(); 100_001];
    assert_eq!(
        graph(&eng, &params).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
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
fn alias_root_reindexes_after_a_canonical_hearth_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let file = seed(&outside, "lib.rs", "pub fn original_alias_symbol() {}\n");
    symlink(&outside, repo.join("link")).unwrap();

    let alias_root = repo.join("link");
    let eng = trusting_engine(&repo);
    let query = || {
        graph(
            &eng,
            &params(
                &alias_root,
                GraphOp::Symbols {
                    path: "lib.rs".to_string(),
                },
            ),
        )
        .unwrap()
    };

    assert_eq!(symbol_names(&query()), ["original_alias_symbol"]);
    write(
        &eng,
        &WriteParams::new(&file, "pub fn refreshed_alias_symbol() {}\n"),
    )
    .unwrap();
    let refreshed = query();
    assert_eq!(symbol_names(&refreshed), ["refreshed_alias_symbol"]);
    assert_eq!(refreshed.meta.reindexed_files, 1);

    std::fs::create_dir(outside.join("dir")).unwrap();
    symlink(outside.join("dir"), repo.join("step")).unwrap();
    let parent_spelling = repo.join("step").join("..").join("lib.rs");
    write(
        &eng,
        &WriteParams::new(
            parent_spelling.display().to_string(),
            "pub fn parent_alias_symbol() {}\n",
        ),
    )
    .unwrap();
    let parent_refreshed = query();
    assert_eq!(symbol_names(&parent_refreshed), ["parent_alias_symbol"]);
    assert_eq!(parent_refreshed.meta.reindexed_files, 1);
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
fn oversized_dependency_is_a_path_identified_zero_hash_stub() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/a.ts",
        "import { b } from \"./b\";\nexport const a = b;\n",
    );
    let oversize_source = "x".repeat(2 * 1024 * 1024 + 1);
    let target = seed(&root, "src/b.ts", &oversize_source);
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Neighborhood {
                path: importer,
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Neighborhood(neighborhood) = result.output else {
        panic!("expected neighborhood");
    };
    let stub = neighborhood
        .nodes
        .iter()
        .find(|node| node.path == target)
        .expect("oversized dependency must remain visible as a stub");

    assert!(!stub.indexed);
    assert_eq!(stub.node_id, "src/b.ts@0000000000000000");
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
fn dependency_edges_expose_joinable_node_ids_and_discriminate_external_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.ts",
        concat!(
            "import { value } from \"./value\";\n",
            "import React from \"react\";\n",
            "export const result = [value, React];\n"
        ),
    );
    let target = seed(&root, "src/value.ts", "export const value = 1;\n");
    seed(
        &root,
        "node_modules/react/package.json",
        "{\"name\":\"react\",\"main\":\"index.js\"}\n",
    );
    seed(&root, "node_modules/react/index.js", "export default {};\n");
    let eng = engine(&root);

    let deps_result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Deps {
                path: importer.clone(),
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Deps(deps) = &deps_result.output else {
        panic!("expected deps");
    };
    let workspace_edge = deps
        .edges
        .iter()
        .find(|edge| edge.to == target)
        .expect("workspace dependency edge");
    assert_eq!(workspace_edge.from_node_id, deps.node.node_id);
    assert_eq!(workspace_edge.to_kind, "path");
    assert!(workspace_edge.to_node_id.is_some());

    let external_edge = deps
        .edges
        .iter()
        .find(|edge| edge.specifier == "react")
        .expect("external dependency edge");
    assert_eq!(external_edge.from_node_id, deps.node.node_id);
    assert_eq!(external_edge.to, "react");
    assert_eq!(external_edge.to_kind, "external");
    assert_eq!(external_edge.to_node_id, None);

    let neighborhood_result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Neighborhood {
                path: importer,
                depth: 1,
            },
        ),
    )
    .unwrap();
    let GraphOutput::Neighborhood(neighborhood) = &neighborhood_result.output else {
        panic!("expected neighborhood");
    };
    let edge = neighborhood
        .edges
        .iter()
        .find(|edge| edge.to == target)
        .expect("workspace neighborhood edge");
    let from = neighborhood
        .nodes
        .iter()
        .find(|node| node.path == edge.from)
        .expect("edge source node");
    let to = neighborhood
        .nodes
        .iter()
        .find(|node| node.path == edge.to)
        .expect("edge target node");
    assert_eq!(edge.from_node_id, from.node_id);
    assert_eq!(edge.to_node_id.as_deref(), Some(to.node_id.as_str()));
    assert_eq!(edge.to_kind, "path");
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
fn files_view_deps_excludes_out_of_view_targets_and_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let a = seed(
        &root,
        "src/a.ts",
        concat!(
            "import { b } from \"./b\";\n",
            "import React from \"react\";\n",
            "export const a = [b, React];\n"
        ),
    );
    let b = seed(
        &root,
        "src/b.ts",
        "import { c } from \"./c\";\nexport const b = c;\n",
    );
    let c = seed(&root, "src/c.ts", "export const c = 1;\n");
    seed(
        &root,
        "node_modules/react/package.json",
        "{\"name\":\"react\",\"main\":\"index.js\"}\n",
    );
    seed(&root, "node_modules/react/index.js", "export default {};\n");
    let eng = engine(&root);
    let op = GraphOp::Deps {
        path: a.clone(),
        depth: 2,
    };

    let initial = graph(&eng, &params(&root, op.clone())).unwrap();
    assert_eq!(initial.meta.guarantee, GraphGuarantee::Exact);
    assert!(dep_targets(&initial).contains(&c.as_str()));

    let mut view_query = params(&root, op);
    view_query.files = vec![a.clone(), b.clone()];
    view_query.include_basis = true;
    let result = graph(&eng, &view_query).unwrap();
    let GraphOutput::Deps(deps) = &result.output else {
        panic!("expected deps");
    };

    assert!(result.meta.swept);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert!(dep_targets(&result).contains(&b.as_str()));
    assert!(dep_targets(&result).contains(&"react"));
    assert!(!dep_targets(&result).contains(&c.as_str()));
    assert!(deps.edges.iter().all(|edge| edge.to != c));
    let external = deps
        .edges
        .iter()
        .find(|edge| edge.to == "react")
        .expect("an external edge from an in-view owner stays visible");
    assert_eq!(external.from, a);
    assert_eq!(external.to_node_id, None);
    assert_eq!(external.to_kind, "external");
    assert_eq!(deps.coverage.analyzed, 2);
    assert_eq!(deps.coverage.stubs, 0);
    let basis_paths: Vec<_> = deps
        .coverage
        .basis
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(basis_paths, [a.as_str(), b.as_str()]);
}

#[test]
fn files_view_deps_stays_exact_when_every_reached_node_is_revalidated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let a = seed(
        &root,
        "src/a.ts",
        "import { b } from \"./b\";\nexport const a = b;\n",
    );
    let b = seed(
        &root,
        "src/b.ts",
        "import { c } from \"./c\";\nexport const b = c;\n",
    );
    let c = seed(&root, "src/c.ts", "export const c = 1;\n");
    let eng = engine(&root);
    let op = GraphOp::Deps { path: a, depth: 2 };

    graph(&eng, &params(&root, op.clone())).unwrap();

    let mut view_query = params(&root, op);
    view_query.files = vec!["src/a.ts".into(), "src/b.ts".into(), "src/c.ts".into()];
    let result = graph(&eng, &view_query).unwrap();
    let GraphOutput::Deps(deps) = &result.output else {
        panic!("expected deps");
    };

    assert!(result.meta.swept);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Exact);
    assert_eq!(dep_targets(&result), [b.as_str(), c.as_str()]);
    assert!(
        deps.edges
            .iter()
            .all(|edge| edge.guarantee == GraphGuarantee::Exact)
    );
}

#[test]
fn files_view_rdeps_excludes_out_of_view_importers_and_coverage_after_full_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = 1;\n");
    let allowed = seed(
        &root,
        "src/allowed.ts",
        "import { targetValue } from \"./target\";\nexport const allowed = targetValue;\n",
    );
    let excluded = seed(
        &root,
        "src/excluded.ts",
        "import { targetValue } from \"./target\";\nexport const excluded = targetValue;\n",
    );
    let eng = engine(&root);
    graph(
        &eng,
        &params(
            &root,
            GraphOp::Search {
                query: String::new(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    let mut query = params(
        &root,
        GraphOp::Rdeps {
            path: target.clone(),
            depth: 1,
            verify: true,
        },
    );
    query.files = vec![target.clone(), allowed.clone()];
    query.include_basis = true;
    let result = graph(&eng, &query).unwrap();
    let GraphOutput::Rdeps(rdeps) = &result.output else {
        panic!("expected rdeps");
    };

    let importer_paths: Vec<_> = rdeps
        .importers
        .iter()
        .map(|entry| entry.node.path.as_str())
        .collect();
    assert_eq!(importer_paths, [allowed.as_str()]);
    assert!(
        rdeps
            .importers
            .iter()
            .all(|entry| entry.node.path != excluded)
    );
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert_eq!(rdeps.coverage.analyzed, 2);
    assert_eq!(rdeps.coverage.stubs, 0);
    let basis_paths: Vec<_> = rdeps
        .coverage
        .basis
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(basis_paths, [allowed.as_str(), target.as_str()]);
}

#[test]
fn files_view_neighborhood_does_not_expand_through_an_out_of_view_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let center = seed(
        &root,
        "src/center.ts",
        concat!(
            "import { allowed } from \"./allowed\";\n",
            "import { bridge } from \"./bridge\";\n",
            "export const center = allowed + bridge;\n"
        ),
    );
    let allowed = seed(&root, "src/allowed.ts", "export const allowed = 1;\n");
    let bridge = seed(
        &root,
        "src/bridge.ts",
        "import { distant } from \"./distant\";\nexport const bridge = distant;\n",
    );
    let distant = seed(&root, "src/distant.ts", "export const distant = 2;\n");
    let eng = engine(&root);
    graph(
        &eng,
        &params(
            &root,
            GraphOp::Search {
                query: String::new(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    let mut query = params(
        &root,
        GraphOp::Neighborhood {
            path: center.clone(),
            depth: 2,
        },
    );
    query.files = vec![center.clone(), allowed.clone(), distant.clone()];
    query.include_basis = true;
    let result = graph(&eng, &query).unwrap();
    let GraphOutput::Neighborhood(neighborhood) = &result.output else {
        panic!("expected neighborhood");
    };

    let node_paths: Vec<_> = neighborhood
        .nodes
        .iter()
        .map(|node| node.path.as_str())
        .collect();
    assert_eq!(node_paths, [allowed.as_str(), center.as_str()]);
    assert!(neighborhood.nodes.iter().all(|node| node.path != bridge));
    assert!(neighborhood.nodes.iter().all(|node| node.path != distant));
    assert_eq!(neighborhood.edges.len(), 1);
    assert_eq!(neighborhood.edges[0].from, center);
    assert_eq!(neighborhood.edges[0].to, allowed);
    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert_eq!(neighborhood.coverage.analyzed, 2);
    assert_eq!(neighborhood.coverage.stubs, 0);
    let basis_paths: Vec<_> = neighborhood
        .coverage
        .basis
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(basis_paths, [allowed.as_str(), center.as_str()]);
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
fn files_entry_that_escapes_root_is_rejected_before_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let outside = seed(
        dir.path(),
        "outside.rs",
        "pub fn outside_root_symbol() {}\n",
    );
    let eng = engine(&root);
    let mut query = params(
        &root,
        GraphOp::Search {
            query: "outside_root_symbol".into(),
            limit: 20,
        },
    );
    query.files = vec!["../outside.rs".into()];

    let error = graph(&eng, &query).unwrap_err();

    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(
        error.path.as_deref(),
        Some(root.join("../outside.rs").display().to_string().as_str())
    );
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "pub fn outside_root_symbol() {}\n"
    );
    let current = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert_eq!(current.meta.indexed_files, 0);
    assert_eq!(status(&current).symbols, 0);
}

#[test]
fn absolute_files_entry_outside_root_is_rejected_before_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let outside = seed(
        dir.path(),
        "outside.rs",
        "pub fn absolute_outside_root_symbol() {}\n",
    );
    let eng = engine(&root);
    let mut query = params(
        &root,
        GraphOp::Search {
            query: "absolute_outside_root_symbol".into(),
            limit: 20,
        },
    );
    query.files = vec![outside.clone()];

    let error = graph(&eng, &query).unwrap_err();

    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(error.path.as_deref(), Some(outside.as_str()));
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "pub fn absolute_outside_root_symbol() {}\n"
    );
    let current = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert_eq!(current.meta.indexed_files, 0);
    assert_eq!(status(&current).symbols, 0);
}

#[test]
fn relative_and_absolute_query_paths_outside_root_are_rejected_before_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let outside = seed(
        dir.path(),
        "outside.rs",
        "pub fn escaped_query_symbol() {}\n",
    );
    let eng = engine(&root);

    for requested in ["../outside.rs".to_owned(), outside.clone()] {
        let error = graph(&eng, &params(&root, GraphOp::Symbols { path: requested })).unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "pub fn escaped_query_symbol() {}\n"
    );
    let current = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert_eq!(current.meta.indexed_files, 0);
    assert_eq!(status(&current).symbols, 0);
}

#[cfg(unix)]
#[test]
fn files_entry_symlinks_must_resolve_inside_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let outside = seed(
        dir.path(),
        "outside/secret.ts",
        "export function outsideRootSymbol() {}\n",
    );
    let escaping_link = root.join("inside.ts");
    std::os::unix::fs::symlink("../outside/secret.ts", &escaping_link).unwrap();
    let eng = engine(&root);
    let mut query = params(
        &root,
        GraphOp::Search {
            query: "outsideRootSymbol".into(),
            limit: 20,
        },
    );
    query.files = vec!["inside.ts".into()];

    let error = graph(&eng, &query).unwrap_err();

    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(
        error.path.as_deref(),
        Some(escaping_link.display().to_string().as_str())
    );
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "export function outsideRootSymbol() {}\n"
    );

    seed(
        &root,
        "src/real.ts",
        "export function insideRootSymbol() {}\n",
    );
    std::os::unix::fs::symlink("src/real.ts", root.join("alias.ts")).unwrap();
    query.files = vec!["alias.ts".into()];
    query.op = GraphOp::Search {
        query: "insideRootSymbol".into(),
        limit: 20,
    };

    let result = graph(&eng, &query).unwrap();

    assert_eq!(symbol_names(&result), ["insideRootSymbol"]);
}

#[test]
fn absolute_files_entry_with_internal_parent_segment_stays_inside_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = seed(&root, "src/lib.rs", "pub fn normalized_symbol() {}\n");
    let mut query = params(&root, GraphOp::Symbols { path: file });
    query.files = vec![
        root.join("src")
            .join("..")
            .join("src/lib.rs")
            .display()
            .to_string(),
    ];

    let result = graph(&engine(&root), &query).unwrap();

    assert_eq!(symbol_names(&result), ["normalized_symbol"]);
    assert_eq!(result.meta.indexed_files, 1);
}

#[cfg(unix)]
#[test]
fn supported_non_utf8_walk_entry_demotes_the_guarantee() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).unwrap();
    seed(&root, "src/a.ts", "export function validUtf8Symbol() {}\n");
    let non_utf8 = source_dir.join(OsStr::from_bytes(b"\xff\xfe.ts"));
    if std::fs::write(&non_utf8, "export function hiddenSymbol() {}\n").is_err() {
        return;
    }
    let root = root.canonicalize().unwrap();
    let eng = engine(&root);

    let result = graph(
        &eng,
        &params(
            &root,
            GraphOp::Search {
                query: "Symbol".into(),
                limit: 20,
            },
        ),
    )
    .unwrap();

    assert_eq!(result.meta.guarantee, GraphGuarantee::Approximate);
    assert_eq!(result.meta.unsupported_files, 0);
    assert_eq!(result.meta.indexed_files, 1);
    assert_eq!(symbol_names(&result), ["validUtf8Symbol"]);
    let current = graph(&eng, &params(&root, GraphOp::Status)).unwrap();
    assert_eq!(status(&current).failed_files, 1);
    assert_eq!(status(&current).unsupported_files, 0);
    assert_eq!(status(&current).symbols, 1);
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
        2,
        "repair must not index a grep hit outside the files view"
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
fn rdeps_grep_candidate_cap_marks_verification_incomplete() {
    const GREP_CANDIDATES: usize = 1030;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let target = seed(&root, "src/target.ts", "export const targetValue = true;\n");
    for index in 0..GREP_CANDIDATES {
        let contents = if index == 0 {
            "import \"./target\";\nexport const importer = true;\n"
        } else {
            "// target\n"
        };
        seed(&root, &format!("src/candidate-{index:04}.ts"), contents);
    }
    let eng = engine(&root);

    graph(
        &eng,
        &params(
            &root,
            GraphOp::Search {
                query: "target".into(),
                limit: 10,
            },
        ),
    )
    .unwrap();

    let mut query = params(
        &root,
        GraphOp::Rdeps {
            path: target.clone(),
            depth: 1,
            verify: true,
        },
    );
    query.files = vec![target];
    let result = graph(&eng, &query).unwrap();
    let GraphOutput::Rdeps(rdeps) = result.output else {
        panic!("expected rdeps");
    };

    assert!(result.meta.repair_truncated);
    assert!(!rdeps.verified);
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
fn jsconfig_paths_resolve_and_root_config_transitions_invalidate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let importer = seed(
        &root,
        "src/app.js",
        "import { value } from \"@lib/x\";\nexport { value };\n",
    );
    let js_target = seed(&root, "src/x.js", "export const value = \"js\";\n");
    let edited_target = seed(
        &root,
        "alternate/x.js",
        "export const value = \"edited-jsconfig\";\n",
    );
    let ts_target = seed(
        &root,
        "typescript/x.js",
        "export const value = \"tsconfig-wins\";\n",
    );
    let jsconfig = seed(
        &root,
        "jsconfig.json",
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@lib/*\":[\"src/*\"]}}}\n",
    );
    let eng = engine(&root);
    let query = params(
        &root,
        GraphOp::Deps {
            path: importer,
            depth: 1,
        },
    );

    let initial = graph(&eng, &query).unwrap();
    assert_eq!(initial.meta.guarantee, GraphGuarantee::Exact);
    assert!(dep_targets(&initial).contains(&js_target.as_str()));

    let old_size = std::fs::metadata(&jsconfig).unwrap().len();
    std::fs::write(
        &jsconfig,
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@lib/*\":[\"alternate/*\"]}}}\n",
    )
    .unwrap();
    assert_ne!(std::fs::metadata(&jsconfig).unwrap().len(), old_size);
    let edited = graph(&eng, &query).unwrap();
    assert!(dep_targets(&edited).contains(&edited_target.as_str()));
    assert!(!dep_targets(&edited).contains(&js_target.as_str()));

    let tsconfig = seed(
        &root,
        "tsconfig.json",
        "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"@lib/*\":[\"typescript/*\"]}}}\n",
    );
    let preferred = graph(&eng, &query).unwrap();
    assert!(dep_targets(&preferred).contains(&ts_target.as_str()));
    assert!(!dep_targets(&preferred).contains(&edited_target.as_str()));

    std::fs::remove_file(tsconfig).unwrap();
    let fallback = graph(&eng, &query).unwrap();
    assert!(dep_targets(&fallback).contains(&edited_target.as_str()));

    std::fs::remove_file(jsconfig).unwrap();
    let removed = graph(&eng, &query).unwrap();
    assert!(dep_targets(&removed).is_empty());
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
