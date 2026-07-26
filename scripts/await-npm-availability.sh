#!/usr/bin/env bash
# Block until every named package@version is actually resolvable from the
# registry.
#
# npm's publish endpoint returns before the new version is visible everywhere.
# The root package's `optionalDependencies` name the platform packages, so
# publishing it into that window produces a release that installs *sometimes* —
# whoever resolves against a replica that has not caught up gets a package whose
# native dependency does not exist yet, and npm skips optional dependencies
# silently, so the failure surfaces later as "cannot find the addon".
#
# Usage: await-npm-availability.sh <version> <package>...
set -euo pipefail

version="${1:?usage: await-npm-availability.sh <version> <package>...}"
shift
packages=("$@")
[ "${#packages[@]}" -gt 0 ] || { echo "error: no packages given" >&2; exit 1; }

max_attempts="${NPM_AVAILABILITY_ATTEMPTS:-30}"
delay="${NPM_AVAILABILITY_DELAY:-10}"

for pkg in "${packages[@]}"; do
  printf 'waiting for %s@%s' "$pkg" "$version"
  attempt=1
  while true; do
    if [ "$(npm view "$pkg@$version" version 2>/dev/null || true)" = "$version" ]; then
      printf ' — available\n'
      break
    fi
    if [ "$attempt" -ge "$max_attempts" ]; then
      printf '\n'
      echo "error: $pkg@$version still not resolvable after $((max_attempts * delay))s" >&2
      exit 1
    fi
    printf '.'
    attempt=$((attempt + 1))
    sleep "$delay"
  done
done

echo "all ${#packages[@]} package(s) resolvable at $version"
