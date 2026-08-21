# Why the release pipeline is shaped this way

Background for `release-napi`. Read this when changing the pipeline, adding a
target, or deciding whether a failure is a bug or a designed refusal. For the
procedure itself, see `../SKILL.md`.

The npm package ships **prebuilt** native addons: a consumer installs it and
gets a working binary without a Rust toolchain. So a release is a fan-out build
across every declared target, an assembly step, and a publish — all driven by
one tag.

## Layout

`@hearthdev/napi` is the package a consumer depends on. It contains the
generated loader (`index.js`), the type declarations (`index.d.ts`), and nothing
native. The binaries live in one package per platform, listed as optional
dependencies so npm installs only the one that matches:

| Rust target                 | npm package                       |
| --------------------------- | --------------------------------- |
| `aarch64-apple-darwin`      | `@hearthdev/napi-darwin-arm64`    |
| `x86_64-apple-darwin`       | `@hearthdev/napi-darwin-x64`      |
| `x86_64-unknown-linux-gnu`  | `@hearthdev/napi-linux-x64-gnu`   |
| `aarch64-unknown-linux-gnu` | `@hearthdev/napi-linux-arm64-gnu` |

**`x86_64-apple-darwin` has an end date.** Apple dropped the architecture, and
GitHub removes Intel macOS runners when the macOS 15 image retires in autumn
2027. Before then this target has to be dropped or moved to a cross-compile.
(The `macos-13` image it originally used was already retired in December 2025 —
a job pointed at a retired label does not fail, it queues forever, so a stuck
build job is worth checking against the runner-images deprecation notices.)

The target list lives in `crates/hearth-napi/package.json` under `napi.targets`.
Adding a target means adding it there, adding a matching runner to
`.github/workflows/release.yml`, extending the map in
`scripts/verify-release-artifacts.sh`, and running `pnpm run create-npm-dirs`
to generate the new platform package.

`index.js` and `index.d.ts` are **committed** as the reviewable public API
surface, and CI fails if they are out of date with the Rust source. A release
has one necessary generated variation: `napi build` embeds the root package
version in `index.js`, so each matrix build applies the tag version first and
transports its generated bindings. The verify job requires all target copies to
be byte-identical before staging one ESM loader into the release tree, and also
requires every generated `index.d.ts` to match the committed declaration file.

## Versioning

One version number covers the root package and every platform package. It comes
from the git tag:

```
napi-v<major>.<minor>.<patch>
```

The release workflow strips the `napi-v` prefix and runs `npm version` on the
root package before each matrix build so the generated loader embeds that
version. The verify job applies the same root version and runs `napi version`
to propagate it to `npm/*`. Nothing needs hand-editing — do not bump
`package.json` before tagging, or the tag is no longer the single source of
truth.

Rust crate versions are independent of the npm package version. `hearth-graph`
carries its own version in `crates/hearth-graph/Cargo.toml` and is published
separately to crates.io; the other workspace Rust crates are not currently
published.

## The jobs

**build** — applies the tag-derived root version, builds every target on a
native runner, smoke-tests each fresh binary, and transports each target's
binary plus generated ESM loader and declarations to verification.

**verify** — downloads the artifacts, applies the tag's version everywhere,
byte-compares the generated bindings from all targets, stages the common
loader, and lays the binaries out into `npm/*` (`napi artifacts`). It then
writes the root package's `optionalDependencies` (`napi pre-publish`), asserts
every declared target has a binary (`scripts/verify-release-artifacts.sh`), and
checks that the root,
loader guards, optional dependencies, and platform manifests carry exactly one
version (`scripts/verify-napi-release-versions.mjs`). `napi artifacts` leaves
assembly copies of the addons in the root, so verify removes them before it
packs the root and Linux x64 platform packages separately. It installs both
into a scratch directory with strict version checking enabled and runs the
smoke and contract suites against the *installed* copy on Node and Bun. It
uploads the verified tree.

**publish** — gated on the `npm-publish` environment, and the only job with
`id-token: write`. It publishes the tree the previous job verified, rather than
rebuilding, so what ships is what was tested. Platform packages go first, then
it waits for them to be resolvable before publishing the root. A fourth job
creates the GitHub release with generated notes.

**diagnose-oidc** — a `workflow_dispatch`-only probe that skips the build and
reports, per package, whether the registry will exchange this workflow's OIDC
token. It exists because `npm publish` cannot say why authentication failed.

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
- No token configured for the registry — not even an empty one. `setup-node`'s
  `registry-url` is deliberately **not** set on the publish job, because it
  writes `//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}` into a scratch
  `.npmrc`. With no token to substitute, npm still reads that line as "already
  authenticated", skips the OIDC exchange entirely, and publishes as an
  anonymous user. The registry rejects that with **404 on the PUT** rather than
  401, so the symptom reads like a missing package rather than a failed login.
  Each package's `publishConfig.registry` points npm at the public registry
  instead. A step asserts both preconditions — no configured token, and an OIDC
  endpoint in the environment — before the first publish.

**Provenance is automatic.** npm generates and publishes attestations for every
trusted-publishing release, so `--provenance` is neither passed nor needed.
Consumers verify with `npm audit signatures`.

The publish job runs in a GitHub **environment** (`npm-publish`) with required
reviewers, and its deployment policy permits only `napi-v*` tags. A human
approves before the OIDC token is minted, and the environment narrows the
identity npm will accept. That policy is also why the diagnostic probe cannot be
dispatched from `main`.

### One-time setup per package

npm requires a package to **already exist** before a trusted publisher can be
configured for it — unlike PyPI, there is no "pending publisher" for a name that
has never been published. So each package needs one bootstrap publish, done
once, by a human:

```bash
cd crates/hearth-napi
pnpm run build                      # produces the local platform's addon
pnpm exec napi pre-publish -t npm --skip-optional-publish

npm login                           # with 2FA
# `./` is required: npm reads a bare `a/b` as the GitHub shorthand `owner/repo`
# and tries to clone it, so `npm publish npm/darwin-arm64` fails with a git error.
for dir in npm/*/; do npm publish "./$dir" --access public; done
npm publish --access public
```

With 2FA set to `auth-and-writes`, each publish prompts for its own OTP; the
codes rotate every 30 seconds, so they cannot be reused across the five.

npm forces `latest` onto a package's **first** version regardless of `--tag`, so
the placeholder is briefly what `npm install` resolves to. The next real release
moves `latest`.

Then bind each package to this repository and workflow — through
*npmjs.com → package → Settings → Trusted publisher*, or from the CLI with
npm 11.15.0+:

```bash
for pkg in @hearthdev/napi @hearthdev/napi-darwin-arm64 @hearthdev/napi-darwin-x64 \
           @hearthdev/napi-linux-x64-gnu @hearthdev/napi-linux-arm64-gnu; do
  npm trust github "$pkg" \
    --repository ushironoko/hearth \
    --workflow release.yml \
    --environment npm-publish
done
```

Leave *Environment name* set to `npm-publish`. Blank means "any environment in
that workflow", which discards the approval gate as a constraint on who can
publish.

**npm does not validate the configuration when it is saved.** A package can look
configured in the UI and still refuse the exchange, and a five-package setup can
silently end up applied to only some of them — this is what broke the first
0.1.0 release. Verify with `npm trust list <pkg>`, or with the diagnostic probe.

After that every release is tokenless; the bootstrap is never repeated,
including for new versions. Adding a *new platform package* later repeats only
the bootstrap for that one package.

> Trusted publisher configurations created after 20 May 2026 must explicitly
> select which actions they allow. `npm publish` is the only one this pipeline
> needs.

## Why the publish order matters

The root package's `optionalDependencies` name the platform packages, so they
must exist first. But "published" and "resolvable" are not the same moment:
npm's publish endpoint returns before the new version is visible on every
replica. A brand-new package name took about three and a half minutes to become
readable during the 0.0.1 bootstrap.

Publishing the root package into that window produces a release that installs
*sometimes*. Worse, npm skips an optional dependency it cannot resolve **without
failing the install** — so the symptom is not a clear error at install time, it
is a missing addon at `require()` time, for whichever users happened to resolve
against a replica that had not caught up.

`scripts/await-npm-availability.sh` closes that window: after publishing the
platform packages it polls `npm view <pkg>@<version>` until each one answers,
and only then is the root package published. It gives up after five minutes per
package rather than hanging a release forever.

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

## Runtime support

The suites run on Node 22 and 24 and on current Bun in CI. `engines.node` is
`>=18`; the addon targets N-API 8, so older runtimes that implement it will
load, but only the tested versions are supported.
