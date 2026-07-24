#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REL="$ROOT/target/release"; HEARTH="$REL/hearth"; HEARTHD="$REL/hearthd"; GEN="$REL/gen-corpus"

CORPUS="${CORPUS:-/tmp/hearth-hbench}"
NUM_FILES="${NUM_FILES:-3000}"
DIRS="${DIRS:-48}"
LINES="${LINES:-200}"

SOCK="/tmp/hh-rss.sock"
SOCK_NOOPT="/tmp/hh-rss-noopt.sock"
TMP_DIR=""
PID=""
PID_NOOPT=""

cleanup() {
  local pid
  for pid in "${PID:-}" "${PID_NOOPT:-}"; do
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -f -- "${SOCK:-}" "${SOCK_NOOPT:-}"
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

warm_daemon() {
  local socket="$1" i file read_count
  "$HEARTH" --socket "$socket" grep function_ "$CORPUS" -g '*.rs' >/dev/null
  "$HEARTH" --socket "$socket" grep TODO_MATCH "$CORPUS" -l >/dev/null
  "$HEARTH" --socket "$socket" grep TODO_MATCH "$CORPUS" -c >/dev/null

  read_count=300
  if [ "$NUM_FILES" -lt "$read_count" ]; then
    read_count=$NUM_FILES
  fi
  for ((i = 0; i < read_count; i++)); do
    printf -v file '%s/d%03d/f%05d.rs' "$CORPUS" "$((i % DIRS))" "$i"
    "$HEARTH" --socket "$socket" read "$file" >/dev/null
  done
}

rss_for_pid() {
  local pid="$1" rss
  rss="$(ps -o rss= -p "$pid" | awk '{print $1}')"
  [[ "$rss" =~ ^[0-9]+$ ]] || {
    echo "could not read RSS for daemon PID $pid" >&2
    return 1
  }
  printf '%s\n' "$rss"
}

ensure_corpus
TMP_DIR="$(mktemp -d /tmp/hh-rss.XXXXXX)"
rm -f -- "$SOCK" "$SOCK_NOOPT"

echo "Socket: $SOCK"
"$HEARTHD" --socket "$SOCK" --cwd "$CORPUS" --profile >"$TMP_DIR/daemon.log" 2>&1 &
PID=$!
echo "Daemon PID: $PID"
wait_sock "$SOCK"

echo "Warming daemon and file cache"
warm_daemon "$SOCK"
sleep 0.5
steady_rss_kib="$(rss_for_pid "$PID")"

echo "Waiting 7 seconds for optimizer ticks"
sleep 7
post_rss_kib="$(rss_for_pid "$PID")"

"$HEARTH" --socket "$SOCK" --json stats >"$TMP_DIR/stats.json"
jq -r '.stats // ""' "$TMP_DIR/stats.json" >"$TMP_DIR/stats.txt"
optimizer_lines="$(
  grep -E 'optimizer\.(byte_budget|cached_bytes|evictions|bytes_freed)' \
    "$TMP_DIR/stats.txt" || true
)"

contrast_rss_kib=""
if [ "${RSS_CONTRAST:-0}" = "1" ]; then
  echo "No-optimizer socket: $SOCK_NOOPT"
  "$HEARTHD" --socket "$SOCK_NOOPT" --cwd "$CORPUS" --no-optimizer \
    >"$TMP_DIR/noopt-daemon.log" 2>&1 &
  PID_NOOPT=$!
  echo "No-optimizer daemon PID: $PID_NOOPT"
  wait_sock "$SOCK_NOOPT"
  echo "Warming no-optimizer contrast daemon"
  warm_daemon "$SOCK_NOOPT"
  sleep 7
  contrast_rss_kib="$(rss_for_pid "$PID_NOOPT")"
fi

python3 - "$steady_rss_kib" "$post_rss_kib" "${contrast_rss_kib:-}" <<'PY'
import sys

steady = int(sys.argv[1])
post = int(sys.argv[2])
contrast = int(sys.argv[3]) if sys.argv[3] else None
delta = post - steady

if delta < 0:
    note = "Optimizer interval shrank the observed daemon RSS."
elif delta > 0:
    note = "Optimizer interval grew the observed daemon RSS."
else:
    note = "Optimizer interval held the observed daemon RSS steady."

print()
print("=== RSS benchmark summary ===")
print(f"steady RSS:         {steady / 1024:.3f} MiB ({steady} KiB)")
print(f"post-optimizer RSS: {post / 1024:.3f} MiB ({post} KiB)")
print(f"delta:              {delta / 1024:+.3f} MiB ({delta:+d} KiB)")
print(note)
if contrast is not None:
    print(f"no-optimizer post RSS: {contrast / 1024:.3f} MiB ({contrast} KiB)")
PY

echo "Optimizer counters:"
if [ -n "$optimizer_lines" ]; then
  printf '%s\n' "$optimizer_lines"
else
  echo "(optimizer counters not reported)"
fi
