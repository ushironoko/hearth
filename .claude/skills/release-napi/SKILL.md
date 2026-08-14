---
name: release-napi
description: "Cut and verify a release of the @hearthdev/napi npm packages. Use when the user asks to release, publish, cut a version, or tag napi-v*; when a `Release @hearthdev/napi` workflow run fails; or when an npm publish fails with ENEEDAUTH, a 404 on PUT, or any trusted-publishing/OIDC error."
---

# Overview

Releases are tag-driven and tokenless. Pushing a `napi-v*` tag builds four
targets, verifies the assembled packages, and stops at a human approval gate;
approving publishes five packages to npm with provenance and cuts a GitHub
release.

This file is the procedure and the failure handling.
`references/design.md` explains *why* the pipeline is shaped this way — read it
before changing the pipeline, adding a target, or concluding that a refusal is a
bug. Between them they are the single source of truth for releasing; there is no
separate release doc.

The bootstrap publish and the per-package trusted-publisher configuration are
**one-time setup that is already done**. Do not repeat them for a new version —
only for a newly added platform package.

## Constraints

- Tagging and pushing are outward-facing: get explicit approval before either,
  per the repository's contribution rules.
- **Never hand-edit a `version` field.** The tag is the only source of truth;
  the workflow writes the version into all five `package.json` files.
- Approving the deployment is the user's action. Do not approve on their behalf,
  and do not widen the `npm-publish` environment's protection rules to work
  around it.
- npm versions cannot be republished. Treat every publish as one-shot.

## 1. Preflight

```bash
cd ~/ghq/github.com/ushironoko/hearth
git switch main && git pull

pnpm --filter @hearthdev/napi run build
git diff --exit-code -- crates/hearth-napi/index.js crates/hearth-napi/index.d.ts
```

A diff here means the napi-generated bindings are stale in git — someone changed
a `#[napi]` signature without committing the regenerated files. Commit them
before tagging, or consumers get type declarations that do not match the addon.

Also confirm CI is green on `main`.

## 2. Tag

Pick the version, then (with approval):

```bash
git tag napi-v0.2.0
git push origin napi-v0.2.0
```

## 3. Watch, and hand off the approval

Poll until the run reaches the gate rather than blocking on it:

```bash
gh run list --repo ushironoko/hearth --workflow release.yml --limit 1 \
  --json databaseId,status,conclusion
gh run view <run-id> --repo ushironoko/hearth --json jobs \
  -q '.jobs[] | "\(.name) | \(.status)/\(.conclusion // "-")"'
```

`build` (×4) and `assemble and verify` take 10–15 minutes. When `publish to npm`
reports `waiting`, tell the user to approve it at the run's URL.

Before they approve, it is worth confirming what will ship — the verified tree is
downloadable, so this needs no guessing:

```bash
gh run download <run-id> --repo ushironoko/hearth --dir /tmp/rel-check
```

Check that `release-tree/package.json` carries the tag's version and
`optionalDependencies` pinned to that same version, and that each
`release-tree/npm/*/` holds the `.node` its `main` field names.

## 4. Verify after publishing

```bash
npm view @hearthdev/napi dist-tags          # latest must be the new version
```

Then prove the published bytes actually install and load, outside the workspace:

```bash
d=$(mktemp -d) && cd "$d"
printf '{"name":"c","private":true,"version":"0.0.0","type":"module"}\n' > package.json
npm install --no-audit --no-fund @hearthdev/napi
npm audit signatures                        # expect "verified attestations"
node --input-type=module -e 'import { HearthEngine } from "@hearthdev/napi"; new HearthEngine({cwd:process.cwd()}); console.log("loads")'
```

`npm audit signatures` reporting verified attestations is the proof that
trusted publishing — not some fallback credential — did the publish.

## When it fails

| Failure | Recovery |
| --- | --- |
| `build` or `verify` | Fix, push to `main`, re-point the tag (below). |
| `publish`, nothing published yet | **Re-run failed jobs** on the same run. The verified artifact persists, so no rebuild — but only if the fix needs no workflow change. |
| `publish`, workflow change needed | Fix on `main`, re-point the tag; the run restarts from build. |
| `publish`, some packages already public | Do **not** retry the same version. Bump the patch and release again. |

Re-pointing a tag needs the remote tag deleted first, which is a destructive
remote operation — ask the user to run it:

```bash
git push origin --delete napi-v0.2.0
```

then:

```bash
git fetch origin --prune --prune-tags
git tag -f napi-v0.2.0 <new-sha>
git push origin refs/tags/napi-v0.2.0
```

### Authentication failures

`npm publish` cannot tell you why trusted publishing did not authenticate: npm's
`oidc()` logs the registry's refusal at verbose level, returns undefined, and
lets the publish fall through to a generic error. Read the symptom first.

| Symptom | Meaning |
| --- | --- |
| `404 Not Found - PUT` | npm never attempted the OIDC exchange and published anonymously. Almost always a token configured for the registry — check that `actions/setup-node` is not given `registry-url`, which writes an empty `_authToken` line that npm reads as "already authenticated". |
| `ENEEDAUTH` | npm attempted the exchange and the registry refused it. The trusted-publisher configuration does not match, or is absent for that package. |

For `ENEEDAUTH`, run the probe instead of guessing:

```bash
gh workflow run release.yml --repo ushironoko/hearth \
  --ref napi-v0.2.0 -f diagnose_oidc=true -f dry_run=true
```

It skips the build and reports, per package, whether the registry will exchange
this workflow's token — with the reason when it will not. It takes about 30
seconds.

Two things about it are easy to get wrong:

- **It must run from a `napi-v*` tag ref.** The `npm-publish` environment only
  permits deployments from those tags, so `--ref main` is rejected before the
  job starts.
- **The tag must point at a commit that contains the probe job**, since
  `workflow_dispatch` uses the workflow file from the given ref.

npm does not validate a trusted-publisher configuration when it is saved, so a
package can look configured in the UI and still refuse the exchange. The probe
checks all five separately, which distinguishes "not configured on some
packages" from "configured but mismatched".

## Adding a new platform package

A package that has never been published cannot have a trusted publisher, so a
new target needs the one-time bootstrap for that package only — see
*One-time setup per package* in `references/design.md`. Two things bite there:

- `npm publish ./npm/<dir>` — the `./` is required. npm reads a bare `a/b` as
  the GitHub shorthand `owner/repo` and tries to clone it.
- npm forces `latest` on a package's very first version regardless of `--tag`,
  so the placeholder is briefly what `npm install` resolves to. The next real
  release moves `latest`.

## Dry runs

`workflow_dispatch` with `dry_run: true` builds and verifies everything and
publishes nothing, writing the `npm pack` listing to the run summary. Use it to
see what a release would ship. It does not exercise publish authentication —
only a real publish does.
