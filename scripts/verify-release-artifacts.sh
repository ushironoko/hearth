#!/usr/bin/env bash
# Every target declared in package.json must have its compiled addon sitting in
# the matching npm/ platform package before anything is published — and must
# still be there after `npm pack`, since a wrong `files` entry produces a package
# that installs fine and then cannot load.
#
# Packing each platform package is the only way to check the second part: the
# binary being on disk says nothing about whether it is in the tarball.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$repo_root/crates/hearth-napi"

# rust target triple → the npm platform directory napi-rs generates for it
platform_dir_for() {
  case "$1" in
    aarch64-apple-darwin) echo darwin-arm64 ;;
    x86_64-apple-darwin) echo darwin-x64 ;;
    x86_64-unknown-linux-gnu) echo linux-x64-gnu ;;
    aarch64-unknown-linux-gnu) echo linux-arm64-gnu ;;
    *) echo "" ;;
  esac
}

targets="$(node -e '
  const pkg = require(process.argv[1]);
  process.stdout.write(pkg.napi.targets.join("\n"));
' "$package_dir/package.json")"

missing=0
while IFS= read -r target; do
  [ -n "$target" ] || continue
  dir="$(platform_dir_for "$target")"
  if [ -z "$dir" ]; then
    echo "error: no npm directory mapping for target $target" >&2
    missing=1
    continue
  fi
  addon="hearth.$dir.node"
  binary="$package_dir/npm/$dir/$addon"
  if [ ! -f "$binary" ]; then
    printf 'MISS %-28s %s\n' "$target" "npm/$dir"
    missing=1
    continue
  fi

  # Pack into a scratch directory and confirm the addon survived into the
  # tarball. `npm pack` writes relative to --pack-destination, so nothing is
  # left behind in the package directory.
  scratch="$(mktemp -d)"
  if ! tarball="$(cd "$package_dir/npm/$dir" && npm pack --silent --pack-destination "$scratch" 2>/dev/null)"; then
    printf 'FAIL %-28s %s (npm pack failed)\n' "$target" "npm/$dir"
    missing=1
    rm -rf "$scratch"
    continue
  fi
  if tar -tzf "$scratch/$tarball" | grep -qx "package/$addon"; then
    printf 'ok   %-28s %s (%s bytes, in tarball)\n' \
      "$target" "npm/$dir" "$(wc -c < "$binary" | tr -d ' ')"
  else
    printf 'FAIL %-28s %s (addon missing from the packed tarball)\n' "$target" "npm/$dir"
    missing=1
  fi
  rm -rf "$scratch"
done <<< "$targets"

if [ "$missing" -ne 0 ]; then
  echo "error: one or more declared targets have no binary" >&2
  exit 1
fi

echo "all declared targets present"
