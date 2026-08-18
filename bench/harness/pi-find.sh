#!/usr/bin/env bash
# Pi tool-operation benchmark: Pi's default fd-backed find vs the same Pi
# wrapper using Hearth's custom glob operation.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CORPUS="${CORPUS:-$ROOT/target/hearth-find-corpus}"
OUT="${OUT:-$ROOT/bench/harness/results/find_pi.md}"
NUM_FILES="${NUM_FILES:-3000}"
DIRS="${DIRS:-48}"
LINES="${LINES:-200}"
PI_EXPECTED_VERSION="${PI_EXPECTED_VERSION:-0.84.1}"
FD_EXPECTED_VERSION="${FD_EXPECTED_VERSION:-10.4.2}"
BENCH_XDG_CONFIG_HOME="${BENCH_XDG_CONFIG_HOME:-$ROOT/target/hearth-find-xdg}"
GENCORPUS="$ROOT/target/release/gen-corpus"
HEARTH_ENTRY="$ROOT/crates/hearth-napi/index.js"

if [ -z "${PI_PACKAGE_ROOT:-}" ] && ! command -v "${PI_BIN:-pi}" >/dev/null 2>&1; then
  echo "error: pi is required; set PI_BIN, PI_PACKAGE_ROOT, or put pi on PATH" >&2
  exit 1
fi
if ! command -v node >/dev/null 2>&1; then
  echo "error: Node.js is required" >&2
  exit 1
fi

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  echo "==> building the release corpus generator"
  cargo build --release -p hearth-bench --bin gen-corpus
  echo "==> building the release Hearth N-API addon"
  pnpm --filter @hearthdev/napi build
fi

if [ ! -x "$GENCORPUS" ]; then
  echo "error: missing $GENCORPUS; rerun without SKIP_BUILD=1" >&2
  exit 1
fi

# Pi's fd honors .gitignore while Hearth deliberately reads bounded root-local
# .ignore/.rgignore files. Give both implementations equivalent rules so this
# benchmark measures execution strategy rather than ignore-policy differences.
echo "==> generating the shared corpus at $CORPUS"
rm -rf "$CORPUS" "$BENCH_XDG_CONFIG_HOME"
"$GENCORPUS" "$CORPUS" "$NUM_FILES" "$DIRS" "$LINES"
cp "$CORPUS/.gitignore" "$CORPUS/.ignore"
printf '.git/\n' >>"$CORPUS/.ignore"
mkdir -p "$BENCH_XDG_CONFIG_HOME"

echo "==> benchmarking Pi default find against Hearth-backed Pi find"
CORPUS="$CORPUS" \
OUT="$OUT" \
NUM_FILES="$NUM_FILES" \
DIRS="$DIRS" \
LINES="$LINES" \
PI_EXPECTED_VERSION="$PI_EXPECTED_VERSION" \
FD_EXPECTED_VERSION="$FD_EXPECTED_VERSION" \
HEARTH_ENTRY="$HEARTH_ENTRY" \
GIT_CONFIG_NOSYSTEM=1 \
GIT_CONFIG_GLOBAL=/dev/null \
XDG_CONFIG_HOME="$BENCH_XDG_CONFIG_HOME" \
node --expose-gc "$ROOT/bench/harness/pi/find-compare.mjs"
