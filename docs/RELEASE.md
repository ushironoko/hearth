# Releasing `@hearth/napi`

The npm package ships **prebuilt** native addons: a consumer installs it and
gets a working binary without a Rust toolchain. That means a release is a
fan-out build across every declared target, an assembly step, and a publish —
all driven by one tag.

## Layout

`@hearth/napi` is the package a consumer depends on. It contains the generated
loader (`index.js`), the type declarations (`index.d.ts`), and nothing native.
The binaries live in one package per platform, listed as optional dependencies
so npm installs only the one that matches:

| Rust target                 | npm package                    |
| --------------------------- | ------------------------------ |
| `aarch64-apple-darwin`      | `@hearth/napi-darwin-arm64`    |
| `x86_64-apple-darwin`       | `@hearth/napi-darwin-x64`      |
| `x86_64-unknown-linux-gnu`  | `@hearth/napi-linux-x64-gnu`   |
| `aarch64-unknown-linux-gnu` | `@hearth/napi-linux-arm64-gnu` |

The target list lives in `crates/hearth-napi/package.json` under `napi.targets`.
Adding a target means adding it there, adding a matching runner to
`.github/workflows/release.yml`, extending the map in
`scripts/verify-release-artifacts.sh`, and running `pnpm run create-npm-dirs`
to generate the new platform package.

`index.js` and `index.d.ts` are **committed**, not generated at publish time:
they are the package's public API surface, so a change to them belongs in a
diff a reviewer sees. CI fails if they are out of date with the Rust source.

## Versioning

One version number covers the root package and every platform package. It comes
from the git tag:

```
napi-v<major>.<minor>.<patch>
```

The release workflow strips the `napi-v` prefix, runs `npm version` on the root
package and `napi version` to propagate it to `npm/*`. Nothing else needs
editing — do not hand-bump `package.json` before tagging, or the workflow will
be setting the version it already has (harmless, but it means the tag is no
longer the single source of truth).

The Rust crates carry their own `version` in the workspace `Cargo.toml` and are
released independently; they are not published to crates.io today.

## Cutting a release

1. Make sure `main` is green, and that the bindings are current:

   ```bash
   pnpm --filter @hearth/napi run build
   git diff --exit-code -- crates/hearth-napi/index.js crates/hearth-napi/index.d.ts
   ```

2. Tag and push:

   ```bash
   git tag napi-v0.2.0
   git push origin napi-v0.2.0
   ```

3. The `Release @hearth/napi` workflow then:
   - builds every target on a native runner and smoke-tests each fresh binary;
   - downloads the artifacts and lays them out into `npm/*` (`napi artifacts`);
   - applies the tag's version everywhere;
   - asserts every declared target has a binary
     (`scripts/verify-release-artifacts.sh`) — a missing one fails the release
     rather than shipping a package that installs and then cannot load;
   - packs the tarball, installs it into a scratch directory, and runs the smoke
     and contract suites against the *installed* copy on both Node and Bun;
   - publishes each platform package, then the root package.

   Platform packages go first on purpose: the root package's
   `optionalDependencies` name them, so publishing the root first leaves a
   window where an install resolves to versions that do not exist.

## Provenance

Publishing runs with `id-token: write` and `--provenance`, so npm records a
signed attestation linking each tarball to the workflow run and commit that
produced it. Consumers can check it with:

```bash
npm audit signatures
```

`publishConfig.provenance` is also set in `package.json`, so a manual
`npm publish` from CI keeps the attestation even if the flag is dropped.

## Credentials

The workflow reads `NPM_TOKEN` from repository secrets — an npm **automation**
token with publish rights on the `@hearth` scope. Creating that token and
adding it to the repository is a maintainer action and the one step this
pipeline cannot do for itself. Everything else is in the repo.

## Dry runs

`workflow_dispatch` with `dry_run: true` (the default) builds every target,
assembles the packages, and runs all verification, but publishes nothing. It
writes the `npm pack` contents listing to the run summary, which is the fastest
way to check what a release *would* ship.

To do the same locally for the current platform only:

```bash
pnpm --filter @hearth/napi run build
bash scripts/verify-tarball.sh
```

That prints a `HEARTH_ENTRY=` line; point the suites at it:

```bash
HEARTH_ENTRY=… node crates/hearth-napi/__test__/contract.mjs
HEARTH_ENTRY=… bun  crates/hearth-napi/__test__/contract.mjs
```

## Runtime support

The suites run on Node 22 and 24 and on current Bun in CI. `engines.node` is
`>=18`; the addon targets N-API 8, so older runtimes that implement it will
load, but only the tested versions are supported.
