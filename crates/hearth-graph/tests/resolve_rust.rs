#![cfg(feature = "resolve-rust")]

use std::{
    fs,
    path::{Path, PathBuf},
};

use hearth_graph::{
    ImportKind, RawImport, ResolutionCompleteness, ResolutionOutcome, Resolved, RustResolveOptions,
    UnresolvedReason, rust_resolver,
};
use tempfile::TempDir;

#[test]
fn rust_mod_resolves_file_and_directory_forms_from_root_and_leaf_files() {
    let fixture = Fixture::new();
    let lib = fixture.write("src/lib.rs", "");
    let root_file = fixture.write("src/foo.rs", "");
    let root_directory_parent = fixture.write("src/nested/mod.rs", "");
    let root_directory = fixture.write("src/nested/foo/mod.rs", "");
    let leaf_file_parent = fixture.write("src/x.rs", "");
    let leaf_file = fixture.write("src/x/foo.rs", "");
    let leaf_directory_parent = fixture.write("src/y.rs", "");
    let leaf_directory = fixture.write("src/y/foo/mod.rs", "");
    let resolver = resolver([]);

    for (from, expected) in [
        (&lib, &root_file),
        (&root_directory_parent, &root_directory),
        (&leaf_file_parent, &leaf_file),
        (&leaf_directory_parent, &leaf_directory),
    ] {
        let outcome = resolver.resolve(path_str(from), &raw_import("foo", ImportKind::RustMod));
        assert_eq!(
            outcome.resolved,
            Resolved::Path(compact_path(expected)),
            "from {}",
            from.display()
        );
        assert_partial(&outcome);
    }
}

#[test]
fn crate_root_items_fall_back_to_the_crate_root_file() {
    let fixture = Fixture::new();
    let root = fixture.write("src/lib.rs", "pub struct RootItem;\n");
    let importer = fixture.write("src/a.rs", "use crate::RootItem;\n");
    let resolver = resolver([&root]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("crate::RootItem", ImportKind::RustUse),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&root)));
    assert_partial(&outcome);
}

#[test]
fn crate_use_walks_mod_rs_and_file_modules() {
    let mod_fixture = Fixture::new();
    let mod_root = mod_fixture.write("src/lib.rs", "");
    let mod_importer = mod_fixture.write("src/importer.rs", "");
    mod_fixture.write("src/a/mod.rs", "");
    let mod_target = mod_fixture.write("src/a/b.rs", "");
    let mod_resolver = resolver([&mod_root]);

    let through_mod = mod_resolver.resolve(
        path_str(&mod_importer),
        &raw_import("crate::a::b", ImportKind::RustUse),
    );
    assert_eq!(
        through_mod.resolved,
        Resolved::Path(compact_path(&mod_target))
    );
    assert_partial(&through_mod);

    let file_fixture = Fixture::new();
    let file_root = file_fixture.write("src/lib.rs", "");
    let file_importer = file_fixture.write("src/importer.rs", "");
    file_fixture.write("src/a.rs", "");
    let file_target = file_fixture.write("src/a/b/mod.rs", "");
    let file_resolver = resolver([&file_root]);

    let through_file = file_resolver.resolve(
        path_str(&file_importer),
        &raw_import("crate::a::b", ImportKind::RustUse),
    );
    assert_eq!(
        through_file.resolved,
        Resolved::Path(compact_path(&file_target))
    );
    assert_partial(&through_file);
}

#[test]
fn crate_use_returns_deepest_file_when_a_later_segment_is_missing() {
    let fixture = Fixture::new();
    let root = fixture.write("src/lib.rs", "");
    let importer = fixture.write("src/importer.rs", "");
    let partial = fixture.write("src/a.rs", "");
    let resolver = resolver([&root]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("crate::a::b", ImportKind::RustUse),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&partial)));
    assert_dependency(&outcome, &fixture.path("src/a/b.rs"));
    assert_dependency(&outcome, &fixture.path("src/a/b/mod.rs"));
    assert_partial(&outcome);
}

#[test]
fn nearest_ancestor_crate_root_wins() {
    let fixture = Fixture::new();
    let outer_root = fixture.write("lib.rs", "");
    fixture.write("a.rs", "");
    let inner_root = fixture.write("packages/app/src/lib.rs", "");
    let importer = fixture.write("packages/app/src/importer.rs", "");
    let inner_target = fixture.write("packages/app/src/a.rs", "");
    let resolver = resolver([&outer_root, &inner_root]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("crate::a", ImportKind::RustUse),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::Path(compact_path(&inner_target))
    );
    assert_eq!(outcome.dependencies, [compact_path(&inner_target)]);
    assert_partial(&outcome);
}

#[test]
fn self_and_super_resolve_from_mod_and_leaf_styles() {
    let fixture = Fixture::new();
    let mod_file = fixture.write("src/outer/mod.rs", "");
    let mod_child = fixture.write("src/outer/child.rs", "");
    let sibling = fixture.write("src/sibling.rs", "");
    let leaf_file = fixture.write("src/x.rs", "");
    let leaf_child = fixture.write("src/x/child/mod.rs", "");
    let resolver = resolver([]);

    for (from, specifier, expected) in [
        (&mod_file, "self::child", &mod_child),
        (&mod_file, "super::sibling", &sibling),
        (&leaf_file, "self::child", &leaf_child),
        (&leaf_file, "super::sibling", &sibling),
    ] {
        let outcome = resolver.resolve(path_str(from), &raw_import(specifier, ImportKind::RustUse));
        assert_eq!(
            outcome.resolved,
            Resolved::Path(compact_path(expected)),
            "{specifier} from {}",
            from.display()
        );
        assert_partial(&outcome);
    }
}

#[test]
fn multi_level_super_falls_back_to_each_ancestor_module_file() {
    let fixture = Fixture::new();
    let root = fixture.write("src/lib.rs", "pub struct RootItem;\n");
    let outer = fixture.write("src/outer.rs", "pub struct OuterItem;\n");
    let inner = fixture.write("src/outer/inner.rs", "pub struct InnerItem;\n");
    let importer = fixture.write("src/outer/inner/leaf.rs", "");
    let resolver = resolver([&root]);

    for (specifier, expected) in [
        ("super::InnerItem", &inner),
        ("super::super::OuterItem", &outer),
        ("super::super::super::RootItem", &root),
    ] {
        let outcome = resolver.resolve(
            path_str(&importer),
            &raw_import(specifier, ImportKind::RustUse),
        );
        assert_eq!(
            outcome.resolved,
            Resolved::Path(compact_path(expected)),
            "{specifier}"
        );
        assert_partial(&outcome);
    }
}

#[test]
fn super_from_a_crate_root_is_not_found() {
    let fixture = Fixture::new();
    let lib = fixture.write("src/lib.rs", "");
    fixture.write("sibling.rs", "");
    let resolver = resolver([&lib]);

    let outcome = resolver.resolve(
        path_str(&lib),
        &raw_import("super::sibling", ImportKind::RustUse),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::Unresolved(UnresolvedReason::NotFound)
    );
    assert!(outcome.dependencies.is_empty());
    assert_partial(&outcome);
}

#[test]
fn bare_rust_use_is_external() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/lib.rs", "");
    let resolver = resolver([]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("serde::Serialize", ImportKind::RustUse),
    );

    assert_eq!(outcome.resolved, Resolved::External("serde".into()));
    assert_eq!(
        outcome.dependencies,
        [
            compact_path(&fixture.path("src/serde.rs")),
            compact_path(&fixture.path("src/serde/mod.rs")),
        ]
    );
    assert_partial(&outcome);
}

#[test]
fn bare_rust_use_prefers_a_local_sibling_module() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/lib.rs", "");
    let target = fixture.write("src/foo.rs", "pub struct X;\n");
    let resolver = resolver([]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("foo::X", ImportKind::RustUse),
    );

    assert_eq!(outcome.resolved, Resolved::Path(compact_path(&target)));
    assert_eq!(
        outcome.dependencies,
        [
            compact_path(&target),
            compact_path(&fixture.path("src/foo/mod.rs")),
            compact_path(&fixture.path("src/foo/X.rs")),
            compact_path(&fixture.path("src/foo/X/mod.rs")),
        ]
    );
    assert_partial(&outcome);
}

#[test]
fn missing_rust_module_tracks_both_candidates() {
    let fixture = Fixture::new();
    let importer = fixture.write("src/lib.rs", "");
    let resolver = resolver([]);

    let outcome = resolver.resolve(
        path_str(&importer),
        &raw_import("missing", ImportKind::RustMod),
    );

    assert_eq!(
        outcome.resolved,
        Resolved::Unresolved(UnresolvedReason::NotFound)
    );
    assert_eq!(
        outcome.dependencies,
        [
            compact_path(&fixture.path("src/missing.rs")),
            compact_path(&fixture.path("src/missing/mod.rs")),
        ]
    );
    assert_partial(&outcome);
}

#[test]
fn src_bin_file_is_its_own_crate_root_for_crate_and_mod_paths() {
    let fixture = Fixture::new();
    let workspace_root = fixture.write("src/lib.rs", "");
    let tool = fixture.write("src/bin/tool.rs", "pub struct RootItem;\nmod x;\n");
    let x = fixture.write("src/bin/x.rs", "pub struct Item;\n");
    let resolver = resolver([&workspace_root]);

    let root_item = resolver.resolve(
        path_str(&tool),
        &raw_import("crate::RootItem", ImportKind::RustUse),
    );
    assert_eq!(root_item.resolved, Resolved::Path(compact_path(&tool)));
    assert_partial(&root_item);

    let crate_child = resolver.resolve(
        path_str(&tool),
        &raw_import("crate::x::Item", ImportKind::RustUse),
    );
    assert_eq!(crate_child.resolved, Resolved::Path(compact_path(&x)));
    assert_partial(&crate_child);

    let mod_child = resolver.resolve(path_str(&tool), &raw_import("x", ImportKind::RustMod));
    assert_eq!(mod_child.resolved, Resolved::Path(compact_path(&x)));
    assert_partial(&mod_child);
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
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture directory");
        fs::write(&path, contents).expect("write fixture file");
        fs::canonicalize(path).expect("canonicalize fixture file")
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

fn resolver<const N: usize>(crate_roots: [&PathBuf; N]) -> Box<dyn hearth_graph::Resolve> {
    rust_resolver(RustResolveOptions {
        crate_roots: crate_roots
            .into_iter()
            .map(|path| compact_path(path))
            .collect(),
    })
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

fn assert_partial(outcome: &ResolutionOutcome) {
    assert_eq!(outcome.completeness, ResolutionCompleteness::Partial);
}
