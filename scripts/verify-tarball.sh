#!/usr/bin/env bash
# Pack @hearthdev/napi and install it into a scratch directory outside the
# workspace, so the smoke and contract suites can run against the bytes that
# would actually be published rather than against the build tree.
#
# Ordinarily the package contains the locally built addon. A release sets
# HEARTH_PLATFORM_PACKAGE_DIR to install a separately packed platform package
# instead, exercising the optional-dependency loader path consumers receive.
#
# Leaves the install at ../hearth-tarball-check relative to the repo root, and
# prints the entry point to use as HEARTH_ENTRY.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$repo_root/crates/hearth-napi"
check_dir="${1:-$(dirname "$repo_root")/hearth-tarball-check}"
platform_package_dir="${HEARTH_PLATFORM_PACKAGE_DIR:-}"

if [ -n "$platform_package_dir" ]; then
  case "$platform_package_dir" in
    /*) ;;
    *) platform_package_dir="$repo_root/$platform_package_dir" ;;
  esac
  if ls "$package_dir"/hearth.*.node >/dev/null 2>&1; then
    echo "error: root package contains an addon, which would bypass the platform-package check" >&2
    exit 1
  fi
  if ! ls "$platform_package_dir"/hearth.*.node >/dev/null 2>&1; then
    echo "error: no built addon in platform package $platform_package_dir" >&2
    exit 1
  fi
else
  if ! ls "$package_dir"/hearth.*.node >/dev/null 2>&1; then
    echo "error: no built addon in $package_dir — run 'pnpm run build' there first" >&2
    exit 1
  fi
fi

rm -rf "$check_dir"
mkdir -p "$check_dir"
# Keep pack/install state inside the disposable check directory. This avoids
# ambient user configuration and lets --offline prove no registry is needed.
export npm_config_cache="$check_dir/npm-cache"

root_tarball="$(cd "$package_dir" && npm pack --silent --pack-destination "$check_dir")"
echo "packed $root_tarball"
platform_tarball=""
if [ -n "$platform_package_dir" ]; then
  platform_tarball="$(cd "$platform_package_dir" && npm pack --silent --pack-destination "$check_dir")"
  echo "packed $platform_tarball"
fi

cd "$check_dir"
# A bare package.json keeps npm from walking up into the workspace.
cat > package.json <<'JSON'
{
  "name": "hearth-tarball-check",
  "private": true,
  "version": "0.0.0",
  "type": "commonjs"
}
JSON

if [ -n "$platform_tarball" ]; then
  # Both tarballs are explicit install requests. --omit=optional prevents npm
  # from consulting the registry for the root's platform dependency while the
  # explicitly supplied matching package still installs. --offline makes a
  # registry request fail rather than weakening this local-artifact proof.
  npm install --offline --cache "$check_dir/npm-cache" --ignore-scripts \
    --no-audit --no-fund --package-lock=false --install-strategy=nested \
    --omit=optional "./$root_tarball" "./$platform_tarball"
else
  npm install --offline --cache "$check_dir/npm-cache" --ignore-scripts \
    --no-audit --no-fund --package-lock=false --install-strategy=nested \
    "./$root_tarball"
fi

# Exercise package metadata resolution as an ESM consumer, not only the absolute
# index.js path used by the full suites below.
node --input-type=module --eval '
  import { HearthEngine } from "@hearthdev/napi";
  if (typeof HearthEngine !== "function") {
    throw new TypeError("@hearthdev/napi has no named HearthEngine ESM export");
  }
'

entry="$check_dir/node_modules/@hearthdev/napi/index.js"
test -f "$entry" || { echo "error: installed package has no index.js" >&2; exit 1; }
test -f "$check_dir/node_modules/@hearthdev/napi/index.d.ts" ||
  { echo "error: installed package ships no type declarations" >&2; exit 1; }

echo "installed to $check_dir"
echo "HEARTH_ENTRY=$entry"

# Export for the rest of a CI job, so a workflow never has to interpolate a
# path into a `run:` block to point the suites at the installed copy.
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "HEARTH_ENTRY=$entry" >> "$GITHUB_ENV"
fi
