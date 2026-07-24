#!/usr/bin/env bash
# CLI benchmark harness: Hearth (warm resident daemon) vs ripgrep / cat / sed.
#
# Honest framing:
#  * "hearth-warm"  = fresh `hearth` process → long-lived `hearthd` with warm
#                     caches. Valid for the "repeated calls over an unchanged
#                     tree" scenario the daemon exists for.
#  * "hearth-cold"  = `hearth --no-daemon`, an in-process one-shot (NO daemon
#                     spawn). This is the fair one-shot-vs-`rg` row.
#  * The corpus is a real `git init`-ed tree with a `.gitignore`, ignored
#     subtrees, hidden/binary/large files, and a skewed size distribution, so
#     the walk cache's gitignore-parsing avoidance is actually exercised.
#  * read/content-grep are expected to LOSE at the CLI (payload serialization
#     over the socket) — shown, not hidden.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CORPUS="${CORPUS:-/tmp/hearth-corpus}"
SOCK="${SOCK:-/tmp/hearth-bench.sock}"
NUM_FILES="${NUM_FILES:-3000}"
DIRS="${DIRS:-48}"
LINES="${LINES:-200}"
OUT="${OUT:-$ROOT/bench/harness/results}"
mkdir -p "$OUT"

HEARTH="$ROOT/target/release/hearth"
HEARTHD="$ROOT/target/release/hearthd"
GENCORPUS="$ROOT/target/release/gen-corpus"

echo "==> building release binaries"
cargo build --release -p hearth-cli -p hearth-daemon -p hearth-bench >/dev/null 2>&1

echo "==> generating realistic corpus at $CORPUS"
rm -rf "$CORPUS"
"$GENCORPUS" "$CORPUS" "$NUM_FILES" "$DIRS" "$LINES"

echo "==> starting hearthd"
rm -f "$SOCK"
"$HEARTHD" --socket "$SOCK" --cwd "$CORPUS" >/tmp/hearthd-bench.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null || true; rm -f "$SOCK"' EXIT
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done

echo "==> warming caches"
"$HEARTH" --socket "$SOCK" grep TODO_MATCH "$CORPUS" -l >/dev/null || true
"$HEARTH" --socket "$SOCK" grep function_ "$CORPUS" -g '*.rs' >/dev/null || true

echo; echo "############ grep -l (files-with-matches): warm vs cold vs ripgrep ############"
hyperfine --warmup 5 --min-runs 30 -N --export-markdown "$OUT/grep_files.md" \
  -n "hearth-warm (daemon)" "$HEARTH --socket $SOCK grep TODO_MATCH $CORPUS -l" \
  -n "hearth-cold (one-shot)" "$HEARTH --no-daemon grep TODO_MATCH $CORPUS -l" \
  -n "ripgrep (one-shot)" "rg -l TODO_MATCH $CORPUS" || true

echo; echo "############ grep -c (count): warm vs ripgrep ############"
hyperfine --warmup 5 --min-runs 30 -N --export-markdown "$OUT/grep_count.md" \
  -n "hearth-warm" "$HEARTH --socket $SOCK grep TODO_MATCH $CORPUS -c" \
  -n "ripgrep" "rg -c TODO_MATCH $CORPUS" || true

echo; echo "############ grep content (many matches — IPC-bound): warm vs ripgrep ############"
hyperfine --warmup 5 --min-runs 30 -N --export-markdown "$OUT/grep_content.md" \
  -n "hearth-warm" "$HEARTH --socket $SOCK grep function_ $CORPUS -g '*.rs'" \
  -n "ripgrep" "rg function_ $CORPUS -g '*.rs'" || true

echo; echo "############ read small file vs cat (IPC-bound) ############"
SMALL="$CORPUS/d000/f00000.rs"
hyperfine --warmup 10 --min-runs 50 -N --export-markdown "$OUT/read_small.md" \
  -n "hearth-warm read" "$HEARTH --socket $SOCK read $SMALL" \
  -n "cat" "cat $SMALL" || true

echo; echo "############ edit vs sed -i (both cold: file reset each run) ############"
EFILE="$CORPUS/d002/f00002.rs"
cp "$EFILE" "$EFILE.orig"
hyperfine --warmup 5 --min-runs 30 -N --export-markdown "$OUT/edit_sed.md" \
  --prepare "cp $EFILE.orig $EFILE" \
  -n "hearth edit" "$HEARTH --socket $SOCK edit $EFILE --old engine --new ENGINE --all" \
  --prepare "cp $EFILE.orig $EFILE" \
  -n "sed -i" "sed -i '' 's/engine/ENGINE/g' $EFILE" || true
rm -f "$EFILE.orig"

echo; echo "==> stopping hearthd"
"$HEARTH" --socket "$SOCK" stop >/dev/null 2>&1 || true
echo "results written to $OUT/"
