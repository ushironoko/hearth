#![cfg(feature = "resolve-js")]

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use hearth_graph::{
    ImportKind, JsResolveOptions, RawImport, ResolutionOutcome, Resolved, ResolverSet,
    UnresolvedReason, js_resolver, js_resolver_with_fs, resolve::FailedKind,
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

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&expected)));
    for config in [configured, selected, first, second, grandparent] {
        assert_dependency(&outcome, &config);
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
    let missing = fixture.root.join("config/missing.json");
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
    let missing = fixture.root.join("config/missing-absolute.json");
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

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "from_file must be absolute")]
fn relative_from_file_triggers_debug_assert() {
    let resolver = js_resolver(JsResolveOptions::default());

    let _ = resolver.resolve("src/app.ts", &raw_import("./module", ImportKind::EsStatic));
}

#[cfg(not(debug_assertions))]
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
}

#[test]
fn resolves_with_an_in_memory_filesystem() {
    let root = Path::new("/virtual-hearth-test-root");
    let importer = root.join("src/app.ts");
    let expected = root.join("src/util.ts");
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
        let root = fs::canonicalize(tempdir.path()).expect("canonicalize temp directory");
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
        fs::canonicalize(path).expect("canonicalize fixture file")
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
}

impl MemoryFileSystem {
    fn with_files(files: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let files: HashMap<_, _> = files.into_iter().collect();
        let directories = files
            .keys()
            .flat_map(|path| path.ancestors().skip(1).map(Path::to_path_buf))
            .collect();
        Self { files, directories }
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
        self.files
            .get(path)
            .map(|contents| contents.as_bytes().to_vec())
            .ok_or_else(|| not_found(path))
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(|| not_found(path))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata_for(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata_for(path)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        Err(not_found(path).into())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
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
