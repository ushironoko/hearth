#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REL="$ROOT/target/release"; HEARTH="$REL/hearth"; HEARTHD="$REL/hearthd"; GEN="$REL/gen-corpus"

CORPUS="${CORPUS:-/tmp/hearth-hbench}"
NUM_FILES="${NUM_FILES:-3000}"
DIRS="${DIRS:-48}"
LINES="${LINES:-200}"

SOCK_WARM="/tmp/hh-cold-warm.sock"
COLD_SOCK="/tmp/hh-cold-run.sock"
TMP_DIR=""
PID_WARM=""

cleanup() {
  local pid
  if [ -n "${PID_WARM:-}" ]; then
    kill "$PID_WARM" 2>/dev/null || true
    wait "$PID_WARM" 2>/dev/null || true
  fi
  if [ -n "${COLD_SOCK:-}" ]; then
    while IFS= read -r pid; do
      [ "$pid" = "$$" ] && continue
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    done < <(pgrep -f "$HEARTHD --socket $COLD_SOCK" 2>/dev/null || true)
  fi
  rm -f -- "${SOCK_WARM:-}" "${COLD_SOCK:-}"
  if [[ -n "${TMP_DIR:-}" && "$TMP_DIR" == /tmp/* ]]; then
    rm -rf -- "$TMP_DIR"
  fi
}
trap cleanup EXIT INT TERM

wait_sock() {
  local s="$1" d=0
  while [ ! -S "$s" ]; do
    sleep 0.02
    d=$((d + 1))
    if [ "$d" -gt 250 ]; then
      echo "socket $s never appeared" >&2
      return 1
    fi
  done
}

ensure_corpus() {
  if [ "${FORCE_CORPUS:-0}" = "1" ] || [ ! -f "$CORPUS/d000/f00000.rs" ]; then
    case "$CORPUS" in
      ""|"/")
        echo "refusing to regenerate unsafe corpus path: '$CORPUS'" >&2
        return 1
        ;;
    esac
    echo "Generating corpus: $CORPUS"
    rm -rf -- "$CORPUS"
    "$GEN" "$CORPUS" "$NUM_FILES" "$DIRS" "$LINES"
  else
    echo "Reusing corpus: $CORPUS"
  fi
}

ensure_corpus
TMP_DIR="$(mktemp -d /tmp/hh-cold.XXXXXX)"
rm -f -- "$SOCK_WARM" "$COLD_SOCK"

echo "Warm socket: $SOCK_WARM"
"$HEARTHD" --socket "$SOCK_WARM" --cwd "$CORPUS" >"$TMP_DIR/warm-daemon.log" 2>&1 &
PID_WARM=$!
echo "Warm daemon PID: $PID_WARM"
wait_sock "$SOCK_WARM"

echo "Warming daemon caches"
"$HEARTH" --socket "$SOCK_WARM" grep TODO_MATCH "$CORPUS" -l >/dev/null
"$HEARTH" --socket "$SOCK_WARM" grep function_ "$CORPUS" -g '*.rs' >/dev/null

COLD_HELPER="$TMP_DIR/cold_oneshot.sh"
cat >"$COLD_HELPER" <<'COLD_HELPER_EOF'
#!/usr/bin/env bash
set -euo pipefail

p=""
cleanup() {
  if [ -n "${p:-}" ]; then
    kill "$p" 2>/dev/null || true
    wait "$p" 2>/dev/null || true
  fi
  rm -f -- "$COLD_SOCK"
}
trap cleanup EXIT INT TERM

rm -f -- "$COLD_SOCK"
"$HEARTHD" --socket "$COLD_SOCK" --cwd "$CORPUS" >/dev/null 2>&1 &
p=$!
d=0
while [ ! -S "$COLD_SOCK" ]; do
  sleep 0.02
  d=$((d + 1))
  if [ "$d" -gt 250 ]; then
    echo "socket $COLD_SOCK never appeared" >&2
    exit 1
  fi
done
"$HEARTH" --socket "$COLD_SOCK" grep TODO_MATCH "$CORPUS" -l >/dev/null
COLD_HELPER_EOF
chmod +x "$COLD_HELPER"

export HEARTHD HEARTH CORPUS COLD_SOCK

echo "Cold-run socket: $COLD_SOCK"
echo "Benchmarking cold one-shot including daemon spawn"
hyperfine --warmup 1 --runs 12 \
  --export-json "$TMP_DIR/cold-incl-spawn.json" \
  -n "cold-incl-spawn" "$COLD_HELPER"

echo "Benchmarking warm daemon"
hyperfine --warmup 5 --min-runs 25 -N \
  --export-json "$TMP_DIR/warm-daemon.json" \
  -n "warm-daemon" "$HEARTH --socket $SOCK_WARM grep TODO_MATCH $CORPUS -l"

echo "Benchmarking hearth --no-daemon"
hyperfine --warmup 3 --min-runs 20 -N \
  --export-json "$TMP_DIR/hearth-no-daemon.json" \
  -n "hearth-no-daemon" "$HEARTH --no-daemon grep TODO_MATCH $CORPUS -l"

echo "Benchmarking ripgrep one-shot"
hyperfine --warmup 3 --min-runs 20 -N \
  --export-json "$TMP_DIR/ripgrep-oneshot.json" \
  -n "ripgrep-oneshot" "rg -l TODO_MATCH $CORPUS"

python3 - \
  "$TMP_DIR/cold-incl-spawn.json" \
  "$TMP_DIR/warm-daemon.json" \
  "$TMP_DIR/hearth-no-daemon.json" \
  "$TMP_DIR/ripgrep-oneshot.json" <<'PY'
import json
import math
import sys


def mean(path):
    with open(path, encoding="utf-8") as handle:
        return float(json.load(handle)["results"][0]["mean"])


cold, warm, no_daemon, ripgrep = map(mean, sys.argv[1:])
spawn_overhead = cold - warm


def break_even(other, unavailable):
    if other <= warm:
        return None, unavailable
    calls = math.ceil(spawn_overhead / (other - warm))
    return max(0, calls), None


vs_rg, rg_reason = break_even(ripgrep, "n/a (rg already faster warm)")
vs_no_daemon, no_daemon_reason = break_even(no_daemon, "n/a")

print()
print("=== Cold-start benchmark summary ===")
print(f"cold-incl-spawn mean:  {cold * 1000:.3f} ms")
print(f"warm-daemon mean:      {warm * 1000:.3f} ms")
print(f"hearth-no-daemon mean: {no_daemon * 1000:.3f} ms")
print(f"ripgrep-oneshot mean:  {ripgrep * 1000:.3f} ms")
print(f"spawn overhead:        {spawn_overhead * 1000:.3f} ms")
if vs_rg is None:
    print(f"break-even vs ripgrep: {rg_reason}")
else:
    print(f"break-even vs ripgrep: {vs_rg} warm calls")
    print(f"≈{vs_rg} warm calls amortize one daemon spawn vs ripgrep")
if vs_no_daemon is None:
    print(f"break-even vs no-daemon: {no_daemon_reason}")
else:
    print(f"break-even vs no-daemon: {vs_no_daemon} warm calls")
    print(f"≈{vs_no_daemon} warm calls amortize one daemon spawn vs hearth --no-daemon")
PY
