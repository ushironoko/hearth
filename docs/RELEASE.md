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

3. The `Release @hearth/napi` workflow then runs three jobs:

   **build** — every target on a native runner, smoke-testing each fresh binary.

   **verify** — downloads the artifacts, lays them out into `npm/*`
   (`napi artifacts`), applies the tag's version everywhere, writes the root
   package's `optionalDependencies` (`napi pre-publish`), asserts every declared
   target has a binary (`scripts/verify-release-artifacts.sh`), then packs the
   tarball, installs it into a scratch directory, and runs the smoke and
   contract suites against the *installed* copy on Node and Bun. It uploads the
   verified tree.

   **publish** — gated on the `npm-publish` environment, and the only job with
   `id-token: write`. It publishes the tree the previous job verified, rather
   than rebuilding, so what ships is what was tested. Platform packages go
   first, then it **waits for them to be resolvable** before publishing the
   root — see below. Finally a fourth job creates the GitHub release with
   generated notes.

## Authentication: no tokens

Publishing uses **npm trusted publishing** (OIDC). The workflow does not read an
npm token, and there is no `NPM_TOKEN` secret to create, rotate, or leak. Each
publish authenticates with a short-lived credential minted for that specific
workflow run, which npm matches against a publisher configuration recorded on
the package itself.

Requirements, all already in the workflow:

- `permissions: id-token: write` on the publish job (and nothing more than
  `contents: read` besides).
- npm **11.5.1 or later**. The workflow uses the npm bundled with the Node it
  installs — one fewer unpinned fetch on the path to a publish than
  `npm install -g npm@…` would be — and asserts the version rather than
  assuming it.
- Node 22.14 or later.
- No `NODE_AUTH_TOKEN`. `registry-url` is still set, because that is what points
  npm at the public registry.

**Provenance is automatic.** npm generates and publishes attestations for every
trusted-publishing release, so `--provenance` is neither passed nor needed.
Consumers verify with:

```bash
npm audit signatures
```

The publish job also runs in a GitHub **environment** (`npm-publish`). That is
worth configuring with required reviewers in *Settings → Environments*: it means
a human approves before the OIDC token is minted, and it narrows the identity
npm will accept.

### One-time setup per package

npm requires a package to **already exist** before a trusted publisher can be
configured for it — unlike PyPI, there is no "pending publisher" for a name that
has never been published. So each of the five packages needs one bootstrap
publish, done once, by a human:

```bash
cd crates/hearth-napi
pnpm run build                      # produces the local platform's addon
pnpm exec napi pre-publish -t npm --skip-optional-publish

npm login                           # with 2FA
for dir in npm/*/; do npm publish "$dir" --access public; done
npm publish --access public
```

Then bind each package to this repository and workflow — either through
*npmjs.com → package → Settings → Trusted publisher*, or from the CLI with
npm 11.15.0+:

```bash
for pkg in @hearth/napi @hearth/napi-darwin-arm64 @hearth/napi-darwin-x64 \
           @hearth/napi-linux-x64-gnu @hearth/napi-linux-arm64-gnu; do
  npm trust github "$pkg" \
    --repository ushironoko/hearth \
    --workflow release.yml \
    --environment npm-publish
done
```

Check it took with `npm trust list <pkg>`. After that every release is
tokenless; the bootstrap is never repeated, including for new versions.

Adding a *new platform package* later repeats only the bootstrap for that one
package.

> Trusted publisher configurations created after 20 May 2026 must explicitly
> select which actions they allow. `npm publish` is the only one this pipeline
> needs.

## Why the publish order matters

The root package's `optionalDependencies` name the platform packages, so they
must exist first. But "published" and "resolvable" are not the same moment: npm's
publish endpoint returns before the new version is visible on every replica.

Publishing the root package into that window produces a release that installs
*sometimes*. Worse, npm skips an optional dependency it cannot resolve **without
failing the install** — so the symptom is not a clear error at install time, it
is a missing addon at `require()` time, for whichever users happened to resolve
against a replica that had not caught up.

`scripts/await-npm-availability.sh` closes that window: after publishing the
platform packages it polls `npm view <pkg>@<version>` until each one answers,
and only then is the root package published. It gives up after five minutes
rather than hanging a release forever.

`scripts/verify-release-artifacts.sh` guards the related failure one step
earlier. It is not enough for the addon to be on disk in `npm/<platform>/`: a
wrong `files` entry would produce a package that installs fine and then cannot
load. So it packs each platform package and asserts the `.node` is actually
inside the tarball.

## Supply-chain posture

The controls this pipeline relies on, and why:

| control | what it stops |
|---|---|
| **Actions pinned to commit SHAs** (`uses: owner/action@<sha> # vX.Y.Z`) | A tag like `v4` is mutable — whoever controls the action repository can repoint it at new code, and every workflow picks that up silently. A SHA cannot be repointed. |
| **Dependabot with a cooldown** (`.github/dependabot.yml`) | The other half of pinning: SHAs that are never updated are SHAs that never get security fixes. The cooldown means a release is not adopted the same day it lands, which is when a compromised one is most likely still undetected. |
| **`permissions: {}` at workflow level** | A compromised step inherits nothing. Each job re-grants only what it needs, and only the publish job ever holds `id-token: write`. |
| **OIDC instead of a long-lived token** | There is no credential at rest to exfiltrate from a secret store, a log, or a malicious dependency's postinstall. |
| **GitHub environment on the publish job** | Puts a human approval gate in front of the only job that can publish, and scopes the OIDC subject. |
| **`persist-credentials: false` on checkout** | Keeps the job's git token out of `.git/config`, where a later step or an uploaded artifact could pick it up. |
| **No `${{ }}` interpolation inside `run:`** | Closes the template-injection path where context data becomes shell code. Values reach scripts through `env:` instead. |
| **zizmor in CI** | Static analysis over these workflows, so a future edit that reintroduces any of the above fails review rather than shipping. |
| **No caching at all in the release workflow** | A cache is written by other workflows on other refs, so restoring one while building the bytes that get published would let a branch influence a release. CI still caches, but never writes one from a pull request. |
| **Publish job consumes the verified tree** | The artifact that was smoke-tested is the artifact that is published, rather than a rebuild that could differ. |
| **`gh release create` rather than a release action** | The one job holding `contents: write` runs a tool already on the runner, instead of granting write access to third-party code. |

Deliberately **not** used: a runner-hardening agent such as `harden-runner`. It
would add an always-on external service into every job, and the higher-value
controls here — SHA pinning, OIDC, least privilege, and the approval gate — do
not depend on it. That is a judgement call, not an oversight; revisit it if this
repository starts handling anything more sensitive than a public build.

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
