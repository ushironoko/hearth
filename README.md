# Hearth 🔥

**A resident, all-in-one agent-tool orchestrator.** `read`, `write`, `edit`,
`bash`, and `grep` are served by one warm, long-lived engine that bundles a file
cache, a directory-walk cache, a sharded profiler, an adaptive self-optimizer,
and a warm-shell pool — all shared across every tool and reachable from native
Rust, from Node.js (napi-rs), and over a Unix-socket daemon + CLI.

Built in Rust, inspired by the *corsa-bind* orchestration model and the
*vize_carton* performance substrate.

---

## Overview

The five tools an agent leans on most, behind one engine:

| Tool | What it does |
|------|--------------|
| `read` | Windowed file reads served from the warm cache (line offset/limit, line numbers, binary-safe bytes). |
| `write` | Atomic full-file writes (temp + rename) that refresh the cache in place. |
| `edit` | Exact string replace / replace-all on the cached buffer, persisted atomically. |
| `bash` | Shell commands with timeout + process-group kill; opt-in **warm-shell pool**. |
| `grep` | ripgrep-grade search (`grep-searcher` + `grep-regex`) over a **cached walk** and **cached file bytes**. |

Three surfaces, one core:

- **Native Rust** — `hearth_tools::{read,write,edit,bash,grep}(&Engine, &params)`.
- **Node.js** — `@hearth/napi`'s `HearthEngine` class (sync + async methods).
- **Daemon + CLI** — `hearthd` (a resident server) and the thin `hearth` client,
  talking length-prefixed msgpack over a Unix socket.

Full design in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); full, honest
benchmark methodology in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

---

## Why Hearth

**A one-shot tool re-pays its cold costs on every invocation** — it re-walks the
tree, re-parses `.gitignore`, re-opens and re-reads files, re-validates UTF-8,
and spawns a fresh shell. **A resident server pays those once and reuses them**,
across tools and across calls. That reuse is where — and only where — the speed
comes from.

What that buys, **measured** (Apple Silicon, `--release`; see
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for methodology and caveats):

- **`grep` beats ripgrep** end-to-end at the CLI: **4.5× / 6.5×** (`--trust-cache`)
  on files-with-matches, 5.9× on counts, and it wins content mode too. At the
  engine level (native/napi) it is **9–31×** the ripgrep engine.
- **`read` and `grep` beat Node.js and Bun `fs`** under a *fair, sync-vs-sync*
  comparison: `read` 1.1–7.2×, a `readdir`+`readFile`+regex search 1.3–27×.
- **`bash` gets a resident advantage too**: the opt-in warm-shell pool is **3.8×**
  faster than spawning a shell per command.
- **`edit`** is ~2× a naive disk read-replace-write for large files.

### Honest about the limits

Hearth wins where the amortized work it saves exceeds the cost of reaching it,
and says so plainly where it doesn't:

- **CLI `read`/`edit` of a small file lose to `cat`/`sed`.** A daemon-client must
  spawn a process *and* round-trip a socket; for a trivial op that costs ~as much
  as a tiny purpose-built tool's entire runtime. We proved it: fd-passing drove
  the read's marginal cost to ~0 and `cat` still won — it's the client's startup
  floor, not the payload. **The read/edit speed win lives in-process (native/napi).**
- **`write` loses to `fs.writeFile`.** Hearth's write is *atomic* (crash-safe temp
  + rename) and cache-coherent, so it does strictly more work; against an equally
  atomic baseline it is only ~1.1–1.4× behind, and that residual gap is inherent
  to the extra syscalls (temp + rename + stat), not a copy-elision problem.

The wins are real and reproducible; so are the boundaries.

---

## How it works

### One resident engine

```
                 ┌──────────────────────── one resident Engine ─────────────────────────┐
   native Rust ──┤  FileCache   — mtime/size-validated, UTF-8-validity cached, LRU        │
   napi (Node) ──┤  WalkCache   — parallel ignore-walk cached per root, glob post-filter  │
   daemon/CLI  ──┤  Profiler    — sharded timing + histogram + allocation counters        │
                 │  Optimizer   — adaptive byte-budget LRU + hysteresis controller         │
                 │  WarmShells  — opt-in pooled shells for bash                            │
                 │  fs-watch    — best-effort proactive invalidation                       │
                 └──────────────────────────────────────────────────────────────────────┘
   crates:  hearth-proto → hearth-core → hearth-tools → { hearth-daemon, hearth-cli, hearth-napi }
```

`Engine` is a cheap `Arc`-clone handle. The daemon, CLI, and napi addon each
construct exactly one and hold it for their whole lifetime; tools borrow it per
call.

### The mechanisms that make it fast

- **Cached walk + cached-bytes grep.** The directory traversal and `.gitignore`
  parse happen once (`WalkCache`); files ≤ 4 MiB are searched straight from the
  `FileCache` (`search_slice`), so a repeated search does **zero** `open`/`read`
  syscalls — only one `stat` per file for coherence.
- **UTF-8-validity-cached read.** A warm `read` is a `stat`, an `Arc` clone, and
  one copy — validity is validated once and cached, so it skips the re-validation
  a fresh `read_to_string` pays.
- **`--trust-cache` (opt-in).** Skips even the per-file `stat` on warm hits under
  a single-writer assumption (Hearth owns the workspace); this is the dominant
  warm-grep cost, so removing it takes warm grep from 1.97 ms → 0.58 ms.
- **Arena grep sink.** Matched lines are appended to one growing buffer + spans,
  not a `String` per line, keeping the parallel search hot path allocation-lean.
- **Compiled-matcher cache.** Regex + glob sets are compiled once per pattern and
  kept on the engine (via a type-erased extension), so repeated greps don't recompile.
- **`SCM_RIGHTS` fd-passing.** For CLI `read`, the client hands the daemon its
  stdout fd and the daemon writes the cached content straight to it — no payload
  serialization.
- **Pipe-based warm-shell pool.** Persistent shells with a random 128-bit nonce
  marker on both streams and `eval`-wrapped commands (fast-fail on incomplete
  input); subshell-isolated, `/dev/null` stdin, timeout kills the group, and any
  anomaly falls back to a fresh spawn.
- **Adaptive self-optimizer.** A background loop reads the cache's live hit-rate
  and grows/shrinks a byte budget with hysteresis, enforcing it via LRU eviction —
  closing the measurement→tuning loop in-process (where *vize_carton*'s profiler
  stops at human-read recommendations).

### At the boundaries

- **Daemon/CLI**: length-prefixed msgpack over a Unix socket, one thread per
  connection, engine shared by `Arc` clone.
- **napi**: JSON at the boundary; sync methods plus `*Async` twins that offload
  to a libuv worker via `AsyncTask` (no embedded tokio). The engine is an explicit
  object the caller constructs — no hidden global singleton.

---

## Development

### Prerequisites

- Rust **1.95** (pinned in `rust-toolchain.toml`)
- Node ≥ 18 and **pnpm** (for the napi addon)
- Optional for benchmarks: `hyperfine`, `ripgrep`, `bun`

### Build

```bash
# Rust workspace (CLI + daemon + core)
cargo build --release

# Node addon (@hearth/napi): generates index.js/.d.ts + the native .node
pnpm install
pnpm --filter @hearth/napi build          # or: build:debug
```

### Test

```bash
cargo test --workspace                     # unit + integration (incl. warm-shell)
node crates/hearth-napi/smoke.mjs          # napi smoke (after building the addon)
```

### Benchmark

```bash
cargo bench -p hearth-bench                # in-process (criterion) micro-benchmarks
bash bench/harness/compare.sh              # CLI vs ripgrep / cat / sed (hyperfine)
node bench/harness/node/compare.mjs        # fair vs Node fs/promises
bun  bench/harness/bun/compare.js          # fair vs Bun fs
```

### Usage

**CLI + daemon** (repeated calls are warm):

```bash
./target/release/hearthd --socket /tmp/hearth.sock --cwd "$PWD" --trust-cache &
./target/release/hearth  --socket /tmp/hearth.sock grep -l "TODO" .
./target/release/hearth  --socket /tmp/hearth.sock stop
```

The CLI falls back to an in-process (cold) engine when no daemon is reachable.

**Node.js** (where read/grep/edit win in-process):

```js
import { HearthEngine } from "@hearth/napi";
const eng = new HearthEngine({ cwd: process.cwd(), trustCache: true });
const r  = eng.read({ path: "src/main.rs" });               // { content, totalLines, cacheHit }
const b  = eng.readBytes({ path: "assets/logo.png" });      // binary-safe Buffer
const g  = JSON.parse(await eng.grepAsync({ pattern: "fn ", path: "src", globs: ["*.rs"] }));
```

**Native Rust:**

```rust
use hearth_core::Engine;
use hearth_tools::grep;
use hearth_proto::{GrepParams, GrepMode};

let engine = Engine::with_defaults();
let hits = grep(&engine, &GrepParams {
    pattern: "fn ".into(), path: "src".into(),
    mode: GrepMode::FilesWithMatches, ..Default::default()
})?;
```

### Project layout

| Crate | Role |
|-------|------|
| `hearth-proto` | Shared request/response types (the one contract; `serde`, `camelCase`). |
| `hearth-core`  | The resident `Engine`: caches, profiler, optimizer, warm-shells, fs-watch. |
| `hearth-tools` | The five tools + msgpack transport, built on the engine. |
| `hearth-daemon`| `hearthd` — the Unix-socket server. |
| `hearth-cli`   | `hearth` — the thin client (daemon or inline). |
| `hearth-napi`  | `@hearth/napi` — the Node addon. |
| `bench`        | Corpus generator + criterion benches + CLI/Node/Bun harnesses. |

### Notes for contributors

- The default is always correct; the fast paths (`--trust-cache`, `--warm-shell`)
  are opt-in and documented with their trade-offs.
- Benchmarks are held to a fair standard (sync-vs-sync, path-set equality,
  atomicity caveats) — see the "honest" framing in `docs/BENCHMARKS.md` before
  quoting a number.
- Rust edition 2024, functional-leaning style, no hidden globals except the
  process-wide allocator hook the profiler necessarily rides on.
