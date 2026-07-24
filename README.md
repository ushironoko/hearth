# Hearth 🔥

**A resident, all-in-one agent-tool orchestrator.** `read`, `write`, `edit`,
`bash`, and `grep` are not one-shot CLIs here — they are served by one warm,
long-lived engine that bundles a file cache, a directory-walk cache, a sharded
profiler, and a background self-optimizer, all shared across every tool. The
same engine is reachable from native Rust, from Node.js (napi-rs), and over a
Unix-socket daemon.

The thesis (from the *corsa-bind* orchestration model and the *vize_carton*
performance substrate): a one-shot tool re-pays its cold costs on every
invocation — re-walking the tree, re-parsing `.gitignore`, re-opening and
re-reading files, re-validating UTF-8. **A resident server pays those once and
reuses them.** That reuse is where — and *only* where — the speed comes from.

## What is actually faster (measured, not aspirational)

Apple Silicon (10 cores, 32 GB), Rust 1.95, `--release`. Corpus: a **`git
init`-ed** 3000-file tree with a real `.gitignore`, ignored subtrees, hidden +
binary + >4 MiB files, skewed sizes. Full methodology and caveats in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### ✅ `grep`: the warm daemon beats real ripgrep end-to-end (CLI)

Repeated searches over an **unchanged** tree — the scenario a resident daemon
exists for. `hyperfine`, fresh `hearth` process → warm `hearthd` vs one-shot `rg`:

| mode | default (stat-validated) | `--trust-cache` (stat-free) |
|------|--------------------------|-----------------------------|
| `grep -l` (files-with-matches) | **4.53× faster** | **6.45× faster** |
| `grep -c` (count) | **4.31× faster** | **5.86× faster** |
| `grep` content (many matches) | **1.34× faster** | — |

`--trust-cache` is an opt-in **single-writer** mode: warm hits skip the
per-file freshness `stat` (the dominant warm-path syscall — removing it takes
in-process warm grep from 1.97 ms → **0.58 ms**, ~31× the ripgrep engine). It
assumes files change only *through* Hearth (whose `write`/`edit` refresh the
cache); an external edit is served stale until evicted. Default is
`stat`-validated and always correct. (An fs-watcher-backed variant was tried and
**rejected**: on macOS FSEvents can't tell an atime bump from a write, so the
watcher invalidated the cache on Hearth's own reads — the watcher cost more than
the stat it saved. See BENCHMARKS.)

A **one-shot** `hearth --no-daemon` grep is only ≈ ripgrep (~1.0–1.1×): the win
is resident amortization (cached walk + cached file bytes), not a faster inner
loop. State it precisely — *"repeated grep over an unchanged tree via a warm
daemon is 1.3–6.5× faster than one-shot ripgrep."*

### ⚠️ `read` / `edit` at the CLI: **slower** than `cat` / `sed` — and why that is unfixable

| CLI comparison | result |
|----------------|--------|
| `hearth read` vs `cat` (small file) | `cat` **~1.5× faster** |
| `hearth edit` vs `sed -i` | `sed` **~1.3× faster** |

`read` uses `SCM_RIGHTS` **fd-passing**: the client hands the daemon its stdout
fd and the daemon writes the cached content straight to it — no payload
serialization, no re-print. That worked: `hearth read` now costs the *same as
`hearth ping`* (~2.3 ms) — the read itself adds ≈0. Yet `cat` is ~1.6 ms, so
`read` still loses. The entire gap is the **CLI client's process-startup +
socket-connect floor** (`true` no-op ≈ 1.6 ms, `cat` ≈ 1.6 ms, `hearth ping` ≈
2.3 ms). A daemon-client must spawn a process *and* round-trip a socket; for a
trivial one-shot op that irreducibly costs ~as much as a purpose-built tiny
tool's entire runtime. **No payload optimization can close it** — we measured
it to zero and the gap remained. The read win is real only where there is no
per-call spawn: **in-process (native/napi)**, below.

### ✅ Engine-level primitives (the native / napi surface agents use)

These are **component microbenchmarks** (criterion, in-process — no socket, no
serialization, no process spawn). They are what a napi/native caller sees, and
they are *not* end-to-end CLI numbers:

| primitive | Hearth warm | baseline | ratio |
|-----------|------------:|---------:|------:|
| `read()` small | 1.23 µs | `std::fs::read_to_string` 9.37 µs | 7.6× |
| `read()` large | 10.4 µs | `std::fs::read_to_string` 34.1 µs | 3.3× |
| `grep()` 2000 files | 1.97 ms | ripgrep engine (`ignore`+`grep-searcher`) 18.4 ms | 9.3× |
| `edit()` large | 1.11 ms | disk read+replace+write 2.29 ms | 2.1× |

The gap between "engine 9× / 3–8×" and "CLI wins only on grep, loses on read"
**is** the IPC tax; closing it for `read`/content-grep (via `SCM_RIGHTS`
fd-passing so the daemon writes straight to the client's stdout) is the top
open item.

## Architecture

```
                 ┌───────────────────────── one resident Engine ──────────────────────────┐
   native Rust ──┤  FileCache (mtime/size-validated, UTF-8-validity cached, single-flight) │
   napi (Node) ──┤  WalkCache (parallel ignore-walk cached per root, glob post-filter)     │
   daemon/CLI  ──┤  Profiler (sharded timing + histogram + allocation counters)            │
                 │  Optimizer (background: bounds cache memory, retunes thresholds)         │
                 │  fs-watch (best-effort proactive invalidation)                          │
                 └────────────────────────────────────────────────────────────────────────┘
        crates: hearth-proto → hearth-core → hearth-tools → {hearth-daemon, hearth-cli, hearth-napi}
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Usage

### CLI + daemon

```bash
cargo build --release
# --trust-cache: opt-in single-writer mode, ~6.5× ripgrep (skips freshness stat)
./target/release/hearthd --socket /tmp/hearth.sock --cwd "$PWD" --trust-cache &
./target/release/hearth --socket /tmp/hearth.sock grep -l "TODO" .   # warm
./target/release/hearth --socket /tmp/hearth.sock stop
```

The CLI falls back to an in-process (cold) engine when no daemon is reachable.

### Node.js (napi-rs) — where read/grep/edit win in-process

```bash
pnpm install && pnpm --filter @hearth/napi build   # or build:debug
```

```js
import { HearthEngine } from "@hearth/napi";
const eng = new HearthEngine({ cwd: process.cwd() });
const r = eng.read({ path: "src/main.rs" });        // { content, totalLines, cacheHit }
const g = JSON.parse(await eng.grepAsync({ pattern: "fn ", path: "src", globs: ["*.rs"] }));
```

### Native Rust

```rust
use hearth_core::Engine;
use hearth_tools::grep;
use hearth_proto::{GrepParams, GrepMode};
let engine = Engine::with_defaults();
let hits = grep(&engine, &GrepParams { pattern: "fn ".into(), path: "src".into(),
    mode: GrepMode::FilesWithMatches, ..Default::default() })?;
```

## Benchmarks

```bash
cargo bench -p hearth-bench                    # in-process component microbenchmarks
bash bench/harness/compare.sh                  # end-to-end CLI vs ripgrep/cat/sed
```

## Honest status & open work

* **Substantiated:** repeated `grep` (all modes) over an unchanged git tree via
  a warm daemon beats one-shot `ripgrep` 1.34–4.53× end-to-end. Engine-level
  read/grep/edit primitives beat their baselines by the ratios above.
* **Not yet won:** CLI `read`/`edit` are IPC-bound and lose to `cat`/`sed`; the
  wins there live only at the in-process (native/napi) surface.
* **Cross-model reviewed:** a codex (OpenAI) review pass refuted an internal
  perf assumption, corrected the benchmark framing (this README reflects those
  corrections), and produced the optimization backlog below.
* **Done since the review:** grep searches cached bytes (not re-opened files);
  opt-in stat-free `--trust-cache` mode (warm grep 1.97 ms → 0.58 ms, CLI
  6.45× ripgrep); grep orchestration cleanup (no double `PathBuf` clone, no
  shared result mutex); `SCM_RIGHTS` stdout fd-passing for `read` (payload tax
  → 0; proved CLI read is startup-bound, not payload-bound); dropped mimalloc
  from the CLI client (startup 2.6 → 2.3 ms); macOS socket-path-length guard;
  **adaptive self-optimizer** (byte-budget LRU eviction + hysteresis controller
  driven by live cache hit-rate — closes the measurement→tuning loop in-process).
* **Benchmark rigor (E), added:** ablation (the warm-grep win is
  content-cache-dominated), multi-op amortization (**8.5×** for read→grep→edit),
  cold-start incl. daemon spawn (**break-even ≈ 3 warm calls**), concurrency
  (154→304 qps at N=1→8), RSS (~74 MiB), and a **fair** cross-runtime comparison
  vs Node/Bun `fs` — see below.
* **vs Node.js / Bun `fs` (fair, release, sync-vs-sync):** Hearth `read` wins
  1.1–7.2×, `grep`/search wins 1.3–26.7×; **`write` loses** (it is atomic +
  cache-coherent — only ~1.1–1.3× behind an *equally atomic* baseline, the rest
  is napi marshalling targeted by phase B). Full table in `docs/BENCHMARKS.md`.
* **A — done:** arena grep sink (per-line `String` allocs removed from the
  parallel search hot path — text is appended to one growing buffer + spans,
  materialized once); compiled-matcher cache (regex + glob sets cached on the
  engine via a type-erased extension, so a repeated pattern is compiled once);
  **opt-in warm-shell pool for `bash`** (`--warm-shell` / `warmShell` /
  `EngineConfig::warm_shell`) — a **pipe-based** protocol (persistent stdout/stderr
  pipes, a random 128-bit per-command nonce marker, `eval`-wrapped commands),
  **3.8× faster than spawn-per-command** (546 µs vs 2.08 ms), all 8 correctness
  cases pass (cwd isolation, large output, stdin, timeout recovery) **plus
  fast-fail on syntactically-incomplete commands**, falling back to a fresh spawn
  on any anomaly. This gives `bash` the resident advantage the other tools had.
  (The design came from an *ultracode* competition: a cross-model codex PoC beat
  the initial temp-file implementation by 1.3× and fixed its unbalanced-quote
  limitation; it was reviewed and integrated here.)
* **B (napi) — done, with an honest outcome:** `readBytes` returns a binary-safe
  Node `Buffer` and **wins 1.58× vs `fs.readFile`** (Buffer-to-Buffer). True
  zero-copy into the shared cache was **rejected** — a JS `Buffer` is mutable and
  would corrupt the shared, mtime-validated cache entry — so `readBytes` copies
  once, like `read`. `writeFast` (direct `path,content` args, content **moved**
  into the cache) shaved only ~1.04× off `write`: the `write` loss to `fs` is
  **confirmed inherent** to atomicity (temp+rename+stat syscalls dominate, not
  the content copies), so it stays ~1.4× behind an atomic `writeFile` and loses
  to a plain one. Documented, not papered over.
* **Backlog** (low-value): content-grep fd-passing; persistent worker pool
  (thread spawn is already cheap); `>4 MiB` mmap policy (niche). (CLI small-`read`
  beating `cat`, and `write` beating a non-atomic `writeFile`, are **not** on the
  list — both are structurally impossible; those wins live in-process / are the
  cost of crash-safety.)
