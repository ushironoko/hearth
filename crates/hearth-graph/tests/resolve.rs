#![cfg(feature = "resolve-js")]

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use hearth_graph::{
    FileAnalysis, ImportKind, JsResolveOptions, RawImport, ResolutionCompleteness,
    ResolutionOutcome, Resolved, ResolverSet, UnresolvedReason,
    graph::{Guarantee, ModuleGraph},
    js_resolver, js_resolver_preserving_symlinks, js_resolver_with_fs,
    resolve::FailedKind,
};
use oxc_resolver::{FileMetadata, FileSystem, ResolveError};
use tempfile::TempDir;

#[test]
fn resolves_relative_import_with_extension_probing() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/a.ts", "");
    let expected = fixture.write("src/util.ts", "export const value = 1;");
    let resolver = js_resolver(JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./util", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
}

#[cfg(unix)]
#[test]
fn js_resolution_can_preserve_symlink_target_spelling_for_discovery() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let canonical = fixture.write("src/real/dep.ts", "export const dep = true;");
    let alias = fixture.root.join("src/alias");
    symlink("real", &alias).unwrap();
    let import = raw_import("./alias/dep", ImportKind::EsStatic);

    let normal = js_resolver(JsResolveOptions::default()).resolve(path_str(&importer), &import);
    let discovery = js_resolver_preserving_symlinks(JsResolveOptions::default())
        .resolve(path_str(&importer), &import);

    assert_eq!(normal.resolved, Resolved::Path(compact_path(&canonical)));
    assert_eq!(
        discovery.resolved,
        Resolved::Path(compact_path(&alias.join("dep.ts")))
    );
}

#[cfg(unix)]
#[test]
fn preserving_symlinks_keeps_bare_workspace_packages_classified_as_paths() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let canonical = fixture.write("packages/pkg/index.js", "module.exports = {};");
    fixture.write(
        "packages/pkg/package.json",
        r#"{"name":"pkg","main":"index.js"}"#,
    );
    fs::create_dir_all(fixture.root.join("node_modules")).unwrap();
    let alias = fixture.root.join("node_modules/pkg");
    symlink("../packages/pkg", &alias).unwrap();
    let import = raw_import("pkg", ImportKind::EsStatic);

    let normal = js_resolver(JsResolveOptions::default()).resolve(path_str(&importer), &import);
    let discovery = js_resolver_preserving_symlinks(JsResolveOptions::default())
        .resolve(path_str(&importer), &import);

    assert_eq!(normal.resolved, Resolved::Path(compact_path(&canonical)));
    assert_eq!(
        discovery.resolved,
        Resolved::Path(compact_path(&alias.join("index.js")))
    );
}

#[test]
fn resolves_vue_importers_and_extensionless_vue_targets() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/App.vue", "");
    let expected_component = fixture.write("src/Component.vue", "<template />\n");
    let expected_helper = fixture.write("src/helper.ts", "export const helper = true;\n");
    let resolver = js_resolver(JsResolveOptions::default());

    let component = resolver.resolve(
        path_str(&importer),
        &raw_import("./Component", ImportKind::EsStatic),
    );
    let helper = resolver.resolve(
        path_str(&importer),
        &raw_import("./helper", ImportKind::EsStatic),
    );

    assert_eq!(
        component.resolved,
        Resolved::Path(compact_path(&expected_component))
    );
    assert_eq!(
        helper.resolved,
        Resolved::Path(compact_path(&expected_helper))
    );
}

#[test]
fn resolves_tsconfig_path_alias_and_tracks_the_config() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r##"{"compilerOptions":{"baseUrl":".","paths":{"#db/*":["db/*"]}}}"##,
    );
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("db/client.ts", "export const client = {};");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("#db/client", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    assert!(
        outcome
            .dependencies
            .iter()
            .any(|dependency| dependency.as_str() == path_str(&tsconfig))
    );
}

#[test]
fn tracks_configured_selected_and_extended_tsconfigs() {
    let fixture = Fixture::new();
    let configured = fixture.write(
        "tsconfig.json",
        r#"{"files":[],"references":[{"path":"packages/app"}]}"#,
    );
    let selected = fixture.write(
        "packages/app/tsconfig.json",
        r##"{
            "extends": ["../../configs/first.json", "../../configs/second.json"],
            "compilerOptions": {
                "baseUrl": "../..",
                "paths": {"#chain/*": ["shared/*"]}
            },
            "include": ["src/**/*"]
        }"##,
    );
    let first = fixture.write("configs/first.json", r#"{"extends":"./grandparent.json"}"#);
    let second = fixture.write(
        "configs/second.json",
        r#"{"compilerOptions":{"strict":true}}"#,
    );
    let grandparent = fixture.write(
        "configs/grandparent.json",
        r#"{"compilerOptions":{"allowJs":true}}"#,
    );
    let importer = fixture.write("packages/app/src/app.ts", "");
    let expected = fixture.write("shared/client.ts", "export const client = {};");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(configured.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("#chain/client", ImportKind::EsStatic),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::Unresolved(UnresolvedReason::NotFound)
    );
    // Project-reference fan-out is deliberately not traversed recursively:
    // expanding it before bounded tracking permits unbounded config work.
    assert_dependency(&outcome, &configured);
    for unexpanded in [selected, first, second, grandparent, expected] {
        assert!(
            !outcome
                .dependencies
                .iter()
                .any(|dependency| dependency.as_str() == path_str(&unexpanded))
        );
    }
}

#[test]
fn tracks_jsonc_tsconfig_extends_chain() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":"./config/base.json","include":["src/**/*"]}"#,
    );
    let base = fixture.write(
        "config/base.json",
        r#"{
            // line comment
            "extends": "./shared.json",
            /* block comment */
            "compilerOptions": {
                "strict": true,
            },
        }"#,
    );
    let shared = fixture.write(
        "config/shared.json",
        r#"{"compilerOptions":{"allowJs":true}}"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &base);
    assert_dependency(&outcome, &shared);
}

#[test]
fn tracks_missing_relative_tsconfig_extends_target() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":"./config/missing.json","include":["src/**/*"]}"#,
    );
    let missing = relative_path(&fixture.root, "config/missing.json");
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &missing);
}

#[test]
fn tracks_missing_absolute_tsconfig_extends_target() {
    let fixture = Fixture::new();
    let missing = relative_path(&fixture.root, "config/missing-absolute.json");
    let config = serde_json::json!({
        "extends": path_str(&missing),
        "include": ["src/**/*"],
    })
    .to_string();
    let tsconfig = fixture.write("tsconfig.json", &config);
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &missing);
}

#[test]
fn diamond_tsconfig_extends_is_not_reported_as_a_cycle() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":["./config/left.json","./config/right.json"]}"#,
    );
    fixture.write("config/left.json", r#"{"extends":"./shared.json"}"#);
    fixture.write("config/right.json", r#"{"extends":"./shared.json"}"#);
    let shared = fixture.write(
        "config/shared.json",
        r#"{"compilerOptions":{"strict":true}}"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &shared);
    assert!(
        outcome.notes.iter().all(|note| !note.contains("cycle")),
        "{outcome:?}"
    );
}

#[test]
fn tsconfig_extends_visit_budget_truncates_tracking() {
    let test = std::thread::Builder::new()
        .name("tsconfig-extends-visit-budget".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            const EXTENDED_CONFIGS: usize = 257;
            let fixture = Fixture::new();
            let mut chain = Vec::with_capacity(EXTENDED_CONFIGS);
            for level in 0..EXTENDED_CONFIGS {
                let contents = if level + 1 == EXTENDED_CONFIGS {
                    r#"{"compilerOptions":{"strict":true}}"#.to_owned()
                } else {
                    format!(r#"{{"extends":"./level{}.json"}}"#, level + 1)
                };
                chain.push(fixture.write(&format!("configs/level{level}.json"), &contents));
            }
            let tsconfig = fixture.write("tsconfig.json", r#"{"extends":"./configs/level0.json"}"#);
            let importer = fixture.write("src/app.ts", "");
            let expected = fixture.write("src/local.ts", "export const local = true;");
            let resolver = js_resolver(JsResolveOptions {
                tsconfig: Some(tsconfig.clone()),
                ..JsResolveOptions::default()
            });

            for _ in 0..2 {
                let outcome = resolver.resolve(
                    path_str(&importer),
                    &raw_import("./local", ImportKind::EsStatic),
                );

                assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
                assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
                assert!(
                    outcome
                        .notes
                        .iter()
                        .any(|note| note.contains("tsconfig extends visit budget")),
                    "{outcome:?}"
                );
                assert_dependency(&outcome, &tsconfig);
                assert_dependency(&outcome, &chain[254]);
            }
        })
        .expect("spawn tsconfig visit-budget test");
    test.join().expect("tsconfig visit-budget test panicked");
}

#[test]
fn oversized_tsconfig_extends_file_truncates_tracking() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write("tsconfig.json", r#"{"extends":"./config/oversized.json"}"#);
    let mut oversized = r#"{"compilerOptions":{"strict":true}}"#.to_owned();
    oversized.push_str(&" ".repeat(1024 * 1024));
    let oversized = fixture.write("config/oversized.json", &oversized);
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("src/local.ts", "export const local = true;");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert!(matches!(
        outcome.resolved,
        Resolved::Unresolved(UnresolvedReason::Failed {
            kind: hearth_graph::FailedKind::Config,
            ..
        })
    ));
    assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
    assert_dependency(&outcome, &oversized);
    let _ = expected;
}

#[test]
fn tsconfig_extends_entry_limit_tracks_the_bounded_prefix() {
    const EXTENDS_ENTRY_LIMIT: usize = 32;

    let fixture = Fixture::new();
    let mut specifiers = Vec::new();
    let mut configs = Vec::new();
    for index in 0..=EXTENDS_ENTRY_LIMIT {
        specifiers.push(format!("./config/base-{index}.json"));
        configs.push(fixture.write(
            &format!("config/base-{index}.json"),
            r#"{"compilerOptions":{"strict":true}}"#,
        ));
    }
    let tsconfig = fixture.write(
        "tsconfig.json",
        &serde_json::json!({"extends": specifiers}).to_string(),
    );
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("src/local.ts", "export const local = true;");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("tsconfig extends entry limit of 32")),
        "{outcome:?}"
    );
    for config in &configs[..EXTENDS_ENTRY_LIMIT] {
        assert_dependency(&outcome, config);
    }
}

#[test]
fn shared_tsconfig_diamond_consumes_one_visit_per_unique_config() {
    const DIAMOND_LAYERS: usize = 9;

    let fixture = Fixture::new();
    let mut configs = Vec::new();
    for layer in 0..DIAMOND_LAYERS {
        let contents = if layer + 1 == DIAMOND_LAYERS {
            r#"{"compilerOptions":{"strict":true}}"#.to_owned()
        } else {
            format!(
                r#"{{"extends":["./left-{}.json","./right-{}.json"]}}"#,
                layer + 1,
                layer + 1
            )
        };
        configs.push(fixture.write(&format!("config/left-{layer}.json"), &contents));
        configs.push(fixture.write(&format!("config/right-{layer}.json"), &contents));
    }
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":["./config/left-0.json","./config/right-0.json"]}"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("src/local.ts", "export const local = true;");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    assert_eq!(outcome.completeness, ResolutionCompleteness::Complete);
    assert!(
        outcome
            .notes
            .iter()
            .all(|note| !note.contains("tsconfig extends visit budget")),
        "{outcome:?}"
    );
    for config in std::iter::once(&tsconfig).chain(&configs) {
        assert_eq!(
            outcome
                .dependencies
                .iter()
                .filter(|dependency| dependency.as_str() == path_str(config))
                .count(),
            1,
            "duplicate dependency for {} in {:?}",
            config.display(),
            outcome.dependencies
        );
    }
}

#[test]
fn short_tsconfig_extends_chain_stays_complete_without_budget_note() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":"./config/base.json","include":["src/**/*"]}"#,
    );
    let base = fixture.write(
        "config/base.json",
        r#"{"extends":"./shared.json","compilerOptions":{"strict":true}}"#,
    );
    let shared = fixture.write(
        "config/shared.json",
        r#"{"compilerOptions":{"allowJs":true}}"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("src/local.ts", "export const local = true;");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    assert_eq!(outcome.completeness, ResolutionCompleteness::Complete);
    assert!(
        outcome
            .notes
            .iter()
            .all(|note| !note.contains("tsconfig extends visit budget")),
        "{outcome:?}"
    );
    for config in [tsconfig, base, shared] {
        assert_dependency(&outcome, &config);
    }
}

#[test]
fn unresolved_package_style_tsconfig_extends_is_a_note_not_a_dependency() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{"extends":"missing-shared-config","include":["src/**/*"]}"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &tsconfig);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.contains("missing-shared-config"))
    );
    assert!(
        outcome
            .dependencies
            .iter()
            .all(|dependency| !dependency.contains("missing-shared-config"))
    );
}

#[test]
fn resolves_conditional_package_exports_and_extracts_package_names() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    fixture.write(
        "node_modules/mypkg/package.json",
        r#"{"name":"mypkg","exports":{".":{"import":"./esm/index.mjs","require":"./cjs/index.cjs"}}}"#,
    );
    let esm_target = fixture.write(
        "node_modules/mypkg/esm/index.mjs",
        "export const mode = 'esm';",
    );
    let cjs_target = fixture.write("node_modules/mypkg/cjs/index.cjs", "exports.mode = 'cjs';");
    fixture.write(
        "node_modules/@scope/pkg/package.json",
        r#"{"name":"@scope/pkg","exports":{".":{"import":"./index.mjs"}}}"#,
    );
    fixture.write(
        "node_modules/@scope/pkg/index.mjs",
        "export const scoped = true;",
    );
    let resolver = js_resolver(JsResolveOptions::default());

    let scoped = resolver.resolve(
        path_str(&importer),
        &raw_import("@scope/pkg", ImportKind::EsStatic),
    );

    assert_eq!(scoped.resolved, Resolved::External("@scope/pkg".into()));

    for kind in [
        ImportKind::EsStatic,
        ImportKind::EsReexport,
        ImportKind::EsDynamic,
    ] {
        let package = resolver.resolve(path_str(&importer), &raw_import("mypkg", kind));

        assert_eq!(package.resolved, Resolved::External("mypkg".into()));
        assert_dependency(&package, &esm_target);
    }

    for kind in [ImportKind::CommonJs, ImportKind::TsImportRequire] {
        let package = resolver.resolve(path_str(&importer), &raw_import("mypkg", kind));

        assert_eq!(package.resolved, Resolved::External("mypkg".into()));
        assert_dependency(&package, &cjs_target);
    }
}

#[test]
fn package_json_dependency_survives_a_resolver_cache_hit() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let package_json = fixture.write(
        "node_modules/mypkg/package.json",
        r#"{"name":"mypkg","main":"index.js"}"#,
    );
    fixture.write(
        "node_modules/mypkg/index.js",
        "module.exports = { cached: true };",
    );
    let resolver = js_resolver(JsResolveOptions::default());
    let import = raw_import("mypkg", ImportKind::CommonJs);

    let first = resolver.resolve(path_str(&importer), &import);
    let cached = resolver.resolve(path_str(&importer), &import);

    assert_dependency(&first, &package_json);
    assert_dependency(&cached, &package_json);
}

#[test]
fn rejected_oversized_package_manifest_never_falls_back_to_index() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let package_json = fixture.write(
        "node_modules/oversized-package/package.json",
        &"x".repeat(1024 * 1024 + 1),
    );
    fixture.write(
        "node_modules/oversized-package/index.js",
        "module.exports = { unsafeFallback: true };",
    );
    let resolver = js_resolver(JsResolveOptions::default());
    let import = raw_import("oversized-package", ImportKind::CommonJs);

    for outcome in [
        resolver.resolve(path_str(&importer), &import),
        resolver.resolve(path_str(&importer), &import),
    ] {
        assert_dependency(&outcome, &package_json);
        assert_eq!(failed_kind(&outcome), FailedKind::Config);
        assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
    }
}

#[test]
fn package_import_referrer_dependency_survives_a_resolver_cache_hit() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let referrer_package_json =
        fixture.write("package.json", r##"{"imports":{"#cached":"cached-pkg"}}"##);
    fixture.write(
        "node_modules/cached-pkg/package.json",
        r#"{"name":"cached-pkg","main":"index.js"}"#,
    );
    fixture.write(
        "node_modules/cached-pkg/index.js",
        "module.exports = { cached: true };",
    );
    let resolver = js_resolver(JsResolveOptions::default());
    let import = raw_import("#cached", ImportKind::CommonJs);

    let first = resolver.resolve(path_str(&importer), &import);
    let cached = resolver.resolve(path_str(&importer), &import);

    assert_eq!(first.resolved, Resolved::External("cached-pkg".into()));
    assert_eq!(cached.resolved, first.resolved);
    assert_dependency(&first, &referrer_package_json);
    assert_dependency(&cached, &referrer_package_json);
}

#[test]
fn package_import_alias_classification_follows_the_resolution_target() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    fixture.write(
        "package.json",
        r##"{
            "imports": {
                "#external": "target-directory",
                "#workspace": "./src/workspace.js"
            }
        }"##,
    );
    fixture.write(
        "node_modules/target-directory/package.json",
        r#"{"name":"manifest-package-name","main":"index.js"}"#,
    );
    fixture.write(
        "node_modules/target-directory/index.js",
        "module.exports = {};",
    );
    let workspace = fixture.write("src/workspace.js", "export const workspace = true;");
    let resolver = js_resolver(JsResolveOptions::default());

    let external = resolver.resolve(
        path_str(&importer),
        &raw_import("#external", ImportKind::EsStatic),
    );
    let workspace_alias = resolver.resolve(
        path_str(&importer),
        &raw_import("#workspace", ImportKind::EsStatic),
    );

    assert_eq!(
        external.resolved,
        Resolved::External("manifest-package-name".into())
    );
    assert_eq!(
        workspace_alias.resolved,
        Resolved::Path(compact_path(&workspace))
    );
}

#[test]
fn relative_import_into_node_modules_is_a_path() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write(
        "node_modules/local-pkg/index.js",
        "export const local = true;",
    );
    let resolver = js_resolver(JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("../node_modules/local-pkg/index.js", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
}

#[test]
fn bare_tsconfig_alias_into_workspace_is_a_path() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {"workspace-alias": ["src/aliased.ts"]}
            }
        }"#,
    );
    let importer = fixture.write("src/app.ts", "");
    let expected = fixture.write("src/aliased.ts", "export const aliased = true;");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("workspace-alias", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
}

#[test]
fn reports_missing_relative_and_bare_specifiers() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions::default());

    for specifier in ["./does-not-exist", "totally-missing-pkg"] {
        let outcome = resolver.resolve(
            path_str(&importer),
            &raw_import(specifier, ImportKind::EsStatic),
        );
        assert_eq!(
            outcome.resolved,
            Resolved::Unresolved(UnresolvedReason::NotFound),
            "{specifier}"
        );
        assert_eq!(
            outcome.completeness,
            ResolutionCompleteness::Complete,
            "{specifier}"
        );
    }
}

#[test]
fn malformed_tsconfig_and_invalid_exports_are_failed_not_not_found() {
    let fixture = Fixture::new();
    let malformed_tsconfig = fixture.write("tsconfig.json", r#"{"compilerOptions":"#);
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(malformed_tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let malformed = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&malformed, &malformed_tsconfig);
    assert_eq!(failed_kind(&malformed), FailedKind::Config);
    assert_eq!(malformed.completeness, ResolutionCompleteness::Partial);

    let package_json = fixture.write(
        "node_modules/invalid-exports/package.json",
        r#"{
            "name": "invalid-exports",
            "exports": {".": "./index.js", "import": "./index.js"}
        }"#,
    );
    fixture.write(
        "node_modules/invalid-exports/index.js",
        "export const value = true;",
    );
    let resolver = js_resolver(JsResolveOptions::default());

    let invalid_exports = resolver.resolve(
        path_str(&importer),
        &raw_import("invalid-exports", ImportKind::EsStatic),
    );

    assert_dependency(&invalid_exports, &package_json);
    assert_eq!(failed_kind(&invalid_exports), FailedKind::Config);
    assert_eq!(
        invalid_exports.completeness,
        ResolutionCompleteness::Partial
    );
}

#[test]
fn missing_tsconfig_and_undefined_package_import_are_failed_not_not_found() {
    let fixture = Fixture::new();
    let missing_tsconfig = fixture.root.join("missing-tsconfig.json");
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(missing_tsconfig.clone()),
        ..JsResolveOptions::default()
    });

    let missing_config = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&missing_config, &missing_tsconfig);
    assert_eq!(failed_kind(&missing_config), FailedKind::Config);
    assert_eq!(missing_config.completeness, ResolutionCompleteness::Partial);

    let package_json = fixture.write(
        "package.json",
        r##"{"imports":{"#defined":"./src/defined.js"}}"##,
    );
    fixture.write("src/defined.js", "export const defined = true;");
    let resolver = js_resolver(JsResolveOptions::default());

    let undefined_import = resolver.resolve(
        path_str(&importer),
        &raw_import("#undefined", ImportKind::EsStatic),
    );

    assert_dependency(&undefined_import, &package_json);
    assert_eq!(failed_kind(&undefined_import), FailedKind::Other);
}

#[test]
fn circular_tsconfig_extends_is_failed_and_tracks_the_cycle() {
    let fixture = Fixture::new();
    let first = fixture.write(
        "tsconfig.json",
        r#"{"extends":"./config/base.json","include":["src/**/*"]}"#,
    );
    let second = fixture.write("config/base.json", r#"{"extends":"../tsconfig.json"}"#);
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(first.clone()),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    assert_dependency(&outcome, &first);
    assert_dependency(&outcome, &second);
    assert_eq!(failed_kind(&outcome), FailedKind::Config);
}

#[test]
fn relative_from_file_returns_failed() {
    let resolver = js_resolver(JsResolveOptions::default());

    let outcome = resolver.resolve("src/app.ts", &raw_import("./module", ImportKind::EsStatic));

    assert_eq!(
        outcome.resolved,
        Resolved::Unresolved(UnresolvedReason::Failed {
            kind: FailedKind::InvalidSpecifier,
            detail: "from_file must be absolute".into(),
        })
    );
}

#[test]
fn empty_specifier_is_an_invalid_specifier_failure() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions::default());

    let outcome = resolver.resolve(path_str(&importer), &raw_import("", ImportKind::EsStatic));

    assert_eq!(failed_kind(&outcome), FailedKind::InvalidSpecifier);
    assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
}

#[test]
fn filesystem_errors_are_partial_io_failures() {
    let root = virtual_root("io-error");
    let importer = relative_path(&root, "src/app.ts");
    let broken_link = relative_path(&root, "src/blocked.ts");
    let filesystem = MemoryFileSystem::with_files([
        (importer.clone(), String::new()),
        (broken_link.clone(), String::new()),
    ])
    .with_broken_symlink(broken_link);
    let resolver = js_resolver_with_fs(filesystem, JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./blocked", ImportKind::EsStatic),
    );

    assert_eq!(failed_kind(&outcome), FailedKind::Io);
    assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
}

#[test]
fn failed_js_resolution_downgrades_graph_deps_guarantee() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write("tsconfig.json", r#"{"compilerOptions":"#);
    let importer = fixture.write("src/app.ts", "");
    let resolvers = ResolverSet {
        js: Some(js_resolver(JsResolveOptions {
            tsconfig: Some(tsconfig),
            ..JsResolveOptions::default()
        })),
        rust: None,
    };
    let mut graph = ModuleGraph::new();
    let analysis = FileAnalysis {
        path: compact_path(&importer),
        content_hash: 1,
        language: Some("typescript".into()),
        symbols: Vec::new(),
        imports: vec![raw_import("./local", ImportKind::EsStatic)],
        has_opaque_imports: false,
    };

    graph.upsert_file(&analysis, &resolvers, true);

    assert_eq!(
        graph.deps(path_str(&importer)).unwrap().guarantee,
        Guarantee::Approximate
    );
}

#[test]
fn vue_graph_nodes_keep_the_javascript_resolver_live() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/App.vue", "");
    let expected = fixture.write("src/helper.ts", "export const helper = true;\n");
    let resolvers = ResolverSet {
        js: Some(js_resolver(JsResolveOptions::default())),
        rust: None,
    };
    let mut graph = ModuleGraph::new();
    let analysis = FileAnalysis {
        path: compact_path(&importer),
        content_hash: 1,
        language: Some("vue".into()),
        symbols: Vec::new(),
        imports: vec![raw_import("./helper", ImportKind::EsStatic)],
        has_opaque_imports: false,
    };

    graph.upsert_file(&analysis, &resolvers, true);

    let node = graph.node(path_str(&importer)).expect("Vue graph node");
    assert!(node.resolver_live());
    assert!(node.resolution_complete());
    assert_eq!(
        graph.deps(path_str(&importer)).unwrap().edges[0].to,
        hearth_graph::graph::EdgeTargetOwned::Path(compact_path(&expected))
    );
}

#[test]
fn resolves_with_an_in_memory_filesystem() {
    let root = virtual_root("relative-resolution");
    let importer = relative_path(&root, "src/app.ts");
    let expected = relative_path(&root, "src/util.ts");
    let filesystem = MemoryFileSystem::with_files([
        (importer.clone(), String::new()),
        (expected.clone(), "export const virtualValue = 1;".into()),
    ]);
    let resolver = js_resolver_with_fs(filesystem, JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./util", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
}

#[test]
fn injected_filesystem_resolves_a_virtual_tsconfig_alias() {
    let root = virtual_root("tsconfig-alias");
    let importer = relative_path(&root, "src/app.ts");
    let tsconfig = relative_path(&root, "tsconfig.json");
    let expected = relative_path(&root, "virtual/mod.ts");
    let filesystem = MemoryFileSystem::with_files([
        (importer.clone(), String::new()),
        (
            tsconfig.clone(),
            r##"{"compilerOptions":{"baseUrl":".","paths":{"#virtual/*":["virtual/*"]}}}"##.into(),
        ),
        (expected.clone(), "export const virtualValue = 1;".into()),
    ]);
    let resolver = js_resolver_with_fs(
        filesystem,
        JsResolveOptions {
            tsconfig: Some(tsconfig.clone()),
            ..JsResolveOptions::default()
        },
    );

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("#virtual/mod", ImportKind::EsStatic),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    assert_dependency(&outcome, &tsconfig);
}

#[test]
fn injected_filesystem_resolves_virtual_package_exports() {
    let root = virtual_root("package-exports");
    let importer = relative_path(&root, "src/app.ts");
    let package_json = relative_path(&root, "node_modules/virtual-package/package.json");
    let expected = relative_path(&root, "node_modules/virtual-package/dist/index.js");
    let filesystem = MemoryFileSystem::with_files([
        (importer.clone(), String::new()),
        (
            package_json.clone(),
            r#"{"name":"virtual-package","exports":"./dist/index.js"}"#.into(),
        ),
        (expected.clone(), "export const virtualValue = 1;".into()),
    ]);
    let resolver = js_resolver_with_fs(filesystem, JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("virtual-package", ImportKind::EsStatic),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::External("virtual-package".into())
    );
    assert_dependency(&outcome, &package_json);
    assert_dependency(&outcome, &expected);
}

#[test]
fn injected_filesystem_rejects_an_unreadable_package_manifest() {
    let root = virtual_root("unreadable-package-manifest");
    let importer = relative_path(&root, "src/app.ts");
    let package_json = relative_path(&root, "node_modules/unreadable-package/package.json");
    let fallback = relative_path(&root, "node_modules/unreadable-package/index.js");
    let filesystem = MemoryFileSystem::with_files([
        (importer.clone(), String::new()),
        (
            package_json.clone(),
            r#"{"name":"unreadable-package","main":"index.js"}"#.into(),
        ),
        (
            fallback,
            "module.exports = { unsafeFallback: true };".into(),
        ),
    ])
    .with_unreadable_file(package_json.clone());
    let resolver = js_resolver_with_fs(filesystem, JsResolveOptions::default());
    let import = raw_import("unreadable-package", ImportKind::CommonJs);

    for outcome in [
        resolver.resolve(path_str(&importer), &import),
        resolver.resolve(path_str(&importer), &import),
    ] {
        assert_dependency(&outcome, &package_json);
        assert_eq!(failed_kind(&outcome), FailedKind::Config);
        assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
    }
}

#[test]
fn clear_cache_reloads_a_rewritten_tsconfig() {
    let fixture = Fixture::new();
    let tsconfig = fixture.write(
        "tsconfig.json",
        r##"{"compilerOptions":{"baseUrl":".","paths":{"#x/*":["a/*"]}}}"##,
    );
    let importer = fixture.write("src/app.ts", "");
    let first_target = fixture.write("a/mod.ts", "export const source = 'a';");
    let second_target = fixture.write(
        "b/mod.ts",
        "export const source = 'b-with-a-different-size';",
    );
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig.clone()),
        ..JsResolveOptions::default()
    });
    let import = raw_import("#x/mod", ImportKind::EsStatic);

    let first = resolver.resolve(path_str(&importer), &import);
    assert_eq!(first.resolved, Resolved::Path(compact_path(&first_target)));

    fs::write(
        &tsconfig,
        r##"{"compilerOptions":{"baseUrl":".","paths":{"#x/*":["b/*"]}}}"##,
    )
    .expect("rewrite tsconfig");
    let still_cached = resolver.resolve(path_str(&importer), &import);
    assert_eq!(
        still_cached.resolved,
        Resolved::Path(compact_path(&first_target))
    );

    resolver.clear_cache();
    let reloaded = resolver.resolve(path_str(&importer), &import);
    assert_eq!(
        reloaded.resolved,
        Resolved::Path(compact_path(&second_target))
    );
}

#[test]
fn resolver_set_reports_unsupported_when_the_responsible_resolver_is_missing() {
    let resolvers = ResolverSet::default();

    let rust = resolvers.resolve(
        "/workspace/src/lib.rs",
        &raw_import("crate::module", ImportKind::RustUse),
    );
    let javascript = resolvers.resolve(
        "/workspace/src/app.ts",
        &raw_import("./module", ImportKind::EsStatic),
    );

    assert_eq!(
        rust.resolved,
        Resolved::Unresolved(UnresolvedReason::Unsupported)
    );
    assert!(rust.dependencies.is_empty());
    assert_eq!(
        javascript.resolved,
        Resolved::Unresolved(UnresolvedReason::Unsupported)
    );
    assert!(javascript.dependencies.is_empty());
}

struct Fixture {
    _tempdir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let root =
            resolver_path(fs::canonicalize(tempdir.path()).expect("canonicalize temp directory"));
        Self {
            _tempdir: tempdir,
            root,
        }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture directory");
        fs::write(&path, contents).expect("write fixture file");
        resolver_path(fs::canonicalize(path).expect("canonicalize fixture file"))
    }
}

fn raw_import(specifier: &str, kind: ImportKind) -> RawImport {
    RawImport {
        specifier: specifier.into(),
        kind,
        line: 1,
        span: (0, specifier.len() as u32),
    }
}

#[cfg(windows)]
fn resolver_path(path: PathBuf) -> PathBuf {
    let path = path.to_str().expect("fixture path must be UTF-8");
    if let Some(path) = path
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| path.strip_prefix(r"\\.\UNC\"))
    {
        PathBuf::from(format!(r"\\{path}"))
    } else if let Some(path) = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
    {
        PathBuf::from(path)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(not(windows))]
fn resolver_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
#[test]
fn resolver_fixture_paths_match_windows_resolver_representation() {
    assert_eq!(
        resolver_path(PathBuf::from(r"\\?\C:\workspace\src\app.ts")),
        PathBuf::from(r"C:\workspace\src\app.ts")
    );
    assert_eq!(
        resolver_path(PathBuf::from(r"\\?\UNC\server\share\app.ts")),
        PathBuf::from(r"\\server\share\app.ts")
    );
    assert_eq!(
        relative_path(Path::new(r"C:\workspace"), "src/app.ts"),
        PathBuf::from(r"C:\workspace\src\app.ts")
    );
}

fn relative_path(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
    }
    path
}

fn virtual_root(name: &str) -> PathBuf {
    let root = std::env::current_dir()
        .expect("current directory must be available")
        .join(format!(".hearth-virtual-{name}"));
    assert!(root.is_absolute(), "virtual root must be absolute");
    root
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path must be UTF-8")
}

fn compact_path(path: &Path) -> compact_str::CompactString {
    path_str(path).into()
}

fn assert_dependency(outcome: &ResolutionOutcome, expected: &Path) {
    assert!(
        outcome
            .dependencies
            .iter()
            .any(|dependency| dependency.as_str() == path_str(expected)),
        "missing dependency {} in {:?}",
        expected.display(),
        outcome.dependencies
    );
}

fn failed_kind(outcome: &ResolutionOutcome) -> FailedKind {
    let Resolved::Unresolved(UnresolvedReason::Failed { kind, .. }) = &outcome.resolved else {
        panic!("expected failed resolution, got {outcome:?}");
    };
    *kind
}

#[derive(Default)]
struct MemoryFileSystem {
    files: HashMap<PathBuf, String>,
    directories: HashSet<PathBuf>,
    broken_symlinks: HashSet<PathBuf>,
    unreadable_files: HashSet<PathBuf>,
}

impl MemoryFileSystem {
    fn with_files(files: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let files: HashMap<_, _> = files.into_iter().collect();
        let directories = files
            .keys()
            .flat_map(|path| path.ancestors().skip(1).map(Path::to_path_buf))
            .collect();
        Self {
            files,
            directories,
            broken_symlinks: HashSet::new(),
            unreadable_files: HashSet::new(),
        }
    }

    fn with_unreadable_file(mut self, path: PathBuf) -> Self {
        self.unreadable_files.insert(path);
        self
    }

    fn with_broken_symlink(mut self, path: PathBuf) -> Self {
        self.broken_symlinks.insert(path);
        self
    }

    fn metadata_for(&self, path: &Path) -> io::Result<FileMetadata> {
        if self.files.contains_key(path) {
            Ok(FileMetadata::new(true, false, false))
        } else if self.directories.contains(path) {
            Ok(FileMetadata::new(false, true, false))
        } else {
            Err(not_found(path))
        }
    }
}

impl FileSystem for MemoryFileSystem {
    fn new() -> Self {
        Self::default()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.unreadable_files.contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("virtual path is unreadable: {}", path.display()),
            ));
        }
        self.files
            .get(path)
            .map(|contents| contents.as_bytes().to_vec())
            .ok_or_else(|| not_found(path))
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        if self.unreadable_files.contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("virtual path is unreadable: {}", path.display()),
            ));
        }
        self.files.get(path).cloned().ok_or_else(|| not_found(path))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata_for(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        if self.broken_symlinks.contains(path) {
            Ok(FileMetadata::new(false, false, true))
        } else {
            self.metadata_for(path)
        }
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        Err(not_found(path).into())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.broken_symlinks.contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("virtual symlink target is unreadable: {}", path.display()),
            ));
        }
        self.metadata_for(path)?;
        Ok(path.to_path_buf())
    }
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("virtual path does not exist: {}", path.display()),
    )
}

#[test]
fn tracks_extends_chains_deeper_than_eight_levels() {
    let fixture = Fixture::new();
    // A 10-level single-extends chain: tsconfig -> level0 -> ... -> level9.
    let mut chain_paths = Vec::new();
    for level in 0..10 {
        let contents = if level == 9 {
            r#"{"compilerOptions":{"strict":true}}"#.to_owned()
        } else {
            format!(r#"{{"extends":"./level{}.json"}}"#, level + 1)
        };
        chain_paths.push(fixture.write(&format!("configs/level{level}.json"), &contents));
    }
    let tsconfig = fixture.write("tsconfig.json", r#"{"extends":"./configs/level0.json"}"#);
    let importer = fixture.write("src/app.ts", "");
    let resolver = js_resolver(JsResolveOptions {
        tsconfig: Some(tsconfig),
        ..JsResolveOptions::default()
    });

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("./local", ImportKind::EsStatic),
    );

    for path in &chain_paths {
        assert_dependency(&outcome, path);
    }
}

#[test]
fn external_package_without_manifest_name_uses_directory_name() {
    let fixture = Fixture::new();
    // The installed package's manifest has no "name" field; the alias must
    // still classify as External under the node_modules directory name.
    fixture.write(
        "node_modules/actual-dep/package.json",
        r#"{"main":"./lib.js"}"#,
    );
    fixture.write("node_modules/actual-dep/lib.js", "");
    fixture.write(
        "package.json",
        r##"{"name":"app","imports":{"#dep":"actual-dep"}}"##,
    );
    let importer = fixture.write("src/app.js", "");
    let resolver = js_resolver(JsResolveOptions::default());

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("#dep", ImportKind::EsStatic),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::External("actual-dep".into()),
        "expected directory-derived package name, got {:?}",
        outcome.resolved
    );
}
