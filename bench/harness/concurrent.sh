#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REL="$ROOT/target/release"; HEARTH="$REL/hearth"; HEARTHD="$REL/hearthd"; GEN="$REL/gen-corpus"

CORPUS="${CORPUS:-/tmp/hearth-hbench}"
NUM_FILES="${NUM_FILES:-3000}"
DIRS="${DIRS:-48}"
LINES="${LINES:-200}"
LEVELS="${LEVELS:-1 2 4 8}"
QPW="${QPW:-40}"

SOCK="/tmp/hh-conc.sock"
TMP_DIR=""
PID=""

cleanup() {
  if [ -n "${PID:-}" ]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -f -- "${SOCK:-}"
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

worker() {
  local output="$1" i t0 t1
  : >"$output"
  for ((i = 0; i < QPW; i++)); do
    t0=$EPOCHREALTIME
    "$HEARTH" --socket "$SOCK" grep TODO_MATCH "$CORPUS" -l >/dev/null
    t1=$EPOCHREALTIME
    printf '%s %s\n' "$t0" "$t1" >>"$output"
  done
}

ensure_corpus
TMP_DIR="$(mktemp -d /tmp/hh-conc.XXXXXX)"
rm -f -- "$SOCK"

echo "Socket: $SOCK"
"$HEARTHD" --socket "$SOCK" --cwd "$CORPUS" >"$TMP_DIR/daemon.log" 2>&1 &
PID=$!
echo "Daemon PID: $PID"
wait_sock "$SOCK"

echo "Warming daemon caches"
"$HEARTH" --socket "$SOCK" grep TODO_MATCH "$CORPUS" -l >/dev/null
"$HEARTH" --socket "$SOCK" grep function_ "$CORPUS" -g '*.rs' >/dev/null

[[ "$QPW" =~ ^[1-9][0-9]*$ ]] || {
  echo "QPW must be a positive integer: $QPW" >&2
  exit 1
}

printf '\n=== Concurrent benchmark summary ===\n'
printf '%-4s | %-7s | %-10s | %-14s | %-10s | %-10s\n' \
  "N" "queries" "wall_s" "throughput_qps" "p50_ms" "p95_ms"
printf '%s\n' '-----+---------+------------+----------------+------------+-----------'

for n in $LEVELS; do
  [[ "$n" =~ ^[1-9][0-9]*$ ]] || {
    echo "LEVELS entries must be positive integers: $n" >&2
    exit 1
  }

  rm -f -- "$TMP_DIR"/latency-*.txt "$TMP_DIR/latencies-all.txt"
  latency_files=()
  worker_pids=()
  WALL0=$EPOCHREALTIME
  for ((w = 0; w < n; w++)); do
    latency_file="$TMP_DIR/latency-$w.txt"
    latency_files+=("$latency_file")
    worker "$latency_file" &
    worker_pids+=("$!")
  done
  for worker_pid in "${worker_pids[@]}"; do
    wait "$worker_pid"
  done
  WALL1=$EPOCHREALTIME

  total_queries=$((n * QPW))
  cat "${latency_files[@]}" >"$TMP_DIR/latencies-all.txt"
  python3 - "$n" "$WALL0" "$WALL1" "$total_queries" "$TMP_DIR/latencies-all.txt" <<'PY'
import math
import sys

level = int(sys.argv[1])
wall0 = float(sys.argv[2])
wall1 = float(sys.argv[3])
total = int(sys.argv[4])
latencies = []
with open(sys.argv[5], encoding="utf-8") as handle:
    for line in handle:
        t0, t1 = map(float, line.split())
        latencies.append((t1 - t0) * 1000.0)

latencies.sort()


def percentile(fraction):
    position = (len(latencies) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return latencies[lower]
    weight = position - lower
    return latencies[lower] * (1.0 - weight) + latencies[upper] * weight


wall_s = wall1 - wall0
throughput = total / wall_s
print(
    f"{level:<4d} | "
    f"{total:<7d} | {wall_s:<10.3f} | {throughput:<14.2f} | "
    f"{percentile(0.50):<10.3f} | {percentile(0.95):<10.3f}"
)
PY
done
