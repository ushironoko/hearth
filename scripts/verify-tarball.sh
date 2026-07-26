#!/usr/bin/env bash
# Pack @hearth/napi and install it into a scratch directory outside the
# workspace, so the smoke and contract suites can run against the bytes that
# would actually be published rather than against the build tree.
#
# Leaves the install at ../hearth-tarball-check relative to the repo root, and
# prints the entry point to use as HEARTH_ENTRY.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$repo_root/crates/hearth-napi"
check_dir="${1:-$(dirname "$repo_root")/hearth-tarball-check}"

if ! ls "$package_dir"/hearth.*.node >/dev/null 2>&1; then
  echo "error: no built addon in $package_dir — run 'pnpm run build' there first" >&2
  exit 1
fi

rm -rf "$check_dir"
mkdir -p "$check_dir"

tarball="$(cd "$package_dir" && npm pack --silent --pack-destination "$check_dir")"
echo "packed $tarball"

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

npm install --no-audit --no-fund --install-strategy=nested "./$tarball"

entry="$check_dir/node_modules/@hearth/napi/index.js"
test -f "$entry" || { echo "error: installed package has no index.js" >&2; exit 1; }
test -f "$check_dir/node_modules/@hearth/napi/index.d.ts" ||
  { echo "error: installed package ships no type declarations" >&2; exit 1; }

echo "installed to $check_dir"
echo "HEARTH_ENTRY=$entry"

# Export for the rest of a CI job, so a workflow never has to interpolate a
# path into a `run:` block to point the suites at the installed copy.
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "HEARTH_ENTRY=$entry" >> "$GITHUB_ENV"
fi
