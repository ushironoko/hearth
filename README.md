# Hearth 🔥

**A resident, all-in-one agent-tool orchestrator.** `read`, `write`, `edit`,
`bash`, and `grep` are served by one warm, long-lived engine that bundles a file
cache, a directory-walk cache, and a warm-shell pool — all shared across every
tool and reachable from native Rust, from Node.js (napi-rs), and over a
Unix-socket daemon + CLI.

Built in Rust, inspired by the *corsa-bind* orchestration model and the
*vize_carton* performance substrate.

---

## Overview

The six tools an agent leans on most, behind one engine:

| Tool | What it does |
|------|--------------|
| `read` | Windowed file reads served from the warm cache (line offset/limit, line numbers, binary-safe bytes, two line-window conventions). |
| `write` | Crash-safe atomic writes, or `fs.writeFile`-compatible in-place ones; symlinks are written *through*, not replaced. |
| `edit` | One exact replacement, or a batch of disjoint ones applied atomically — matched against the original file, preserving BOM and CRLF, with diff hunks in the result. |
| `bash` | Shell commands with ordered output streaming, configurable shell, timeout + process-group kill, and an opt-in **warm-shell pool** with at-most-once semantics. |
| `grep` | ripgrep-grade search (`grep-searcher` + `grep-regex`) over a **cached walk** and **cached file bytes**, with a deterministic global match limit. |
| `graph` | Cached symbols, outlines, search, definitions, dependency/reverse-dependency traversal (`deps`/`rdeps`), bidirectional neighborhoods, and index status. |

Every operation is cancellable: pass an `AbortSignal` and the native work stops
at its next safe point, with nothing left running once the promise settles.

Three surfaces, one core:

- **Native Rust** — `hearth_tools::{read,write,edit,bash,grep,graph}(&Engine, &params)`.
- **Node.js** — `@hearthdev/napi`'s `HearthEngine` class (typed sync + cancellable
  async methods, streaming `bash`).
- **Daemon + CLI** — `hearthd` (a resident server) and the thin `hearth` client,
  talking length-prefixed msgpack over a Unix socket.

Full design in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); full benchmark
methodology in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Security model for agents and LLM clients

`hearthd` is a same-user performance service, **not** a privilege, tenant, or
workspace boundary. A client that can reach it can request every operation the
daemon's OS user can perform, including arbitrary paths and shell commands. Do
not run it as root, under a more privileged UID, or as a shared service.

An LLM is an untrusted input producer even when it is not malicious: it can
invent paths, emit extreme numeric/vector values, retry ambiguous mutations,
launch long-running commands, and create parallel load. An adapter that exposes
Hearth to an LLM must enforce allowed roots and operations, environment
allowlists, request/output/deadline budgets, and approval for Bash or mutations
where appropriate. Without those controls, the LLM deliberately has the OS
user's read/write/execute authority and can reach daemon-inherited secrets.

See [`SECURITY.md`](SECURITY.md) for deployment requirements and
[`docs/SECURITY_AUDIT_HEARTHD.md`](docs/SECURITY_AUDIT_HEARTHD.md) for the
source-level threat model and remediation status.

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
- **`bash` gets a resident advantage too**: the opt-in warm-shell pool is **3.6×**
  faster than spawning a shell per command, while guaranteeing at-most-once
  execution.
- **`edit`** is ~2× a naive disk read-replace-write for large files.

### Limits

Safety ceilings are enforced even with the optimizer disabled: 256 MiB wire frames with bounded MessagePack structure, 30 s frame receipt, 64 MiB read/edit files, 16 MiB per searched file, 4 MiB aggregate grep results, bounded walk/build/result state, and a 24 h maximum Bash timeout. Directory walks honor only bounded root-local `.ignore`/`.rgignore`; ancestor/global Git ignore files and project-reference tsconfig fan-out are intentionally not expanded.

Hearth wins where the amortized work it saves exceeds the cost of reaching it.
Where it doesn't:

- **CLI `read`/`edit` of a small file lose to `cat`/`sed`.** A daemon-client must
  spawn a process *and* round-trip a socket; for a trivial op that costs ~as much
  as a tiny purpose-built tool's entire runtime. We proved it: fd-passing drove
  the read's marginal cost to ~0 and `cat` still won — it's the client's startup
  floor, not the payload. **The read/edit speed win lives in-process (native/napi).**
- **`write` loses to `fs.writeFile`.** Hearth's write is *atomic* (crash-safe temp
  + rename) and cache-coherent, so it does strictly more work; against an equally
  atomic baseline it is only ~1.1–1.4× behind, and that residual gap is inherent
  to the extra syscalls (temp + rename + stat), not a copy-elision problem.

---

## How it works

### One resident engine

```
                 ┌──────────────────────── one resident Engine ─────────────────────────┐
   native Rust ──┤  FileCache   — file contents cached, validated by mtime/size           │
   napi (Node) ──┤  WalkCache   — bounded directory walk (+ root-local ignore) per root    │
   daemon/CLI  ──┤  WarmShells  — opt-in pooled shells for bash                            │
                 │  fs-watch    — best-effort proactive invalidation                       │
                 │  (caches are bounded by an LRU byte budget, so the daemon stays small)  │
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

### At the boundaries

- **Daemon/CLI**: capped length-prefixed msgpack over a Unix socket, one thread
  per connection behind a default ceiling of 64, engine shared by `Arc` clone.
  The endpoint lives in an owner-only runtime directory; client and server both
  verify peer UID. FD-passing validates kind/count/CLOEXEC and preserves frame
  boundaries.
- **napi**: concrete generated TypeScript types at the boundary — no `any` on any
  tool method. Sync methods plus `*Async` twins that offload to a libuv worker via
  `AsyncTask` (no embedded tokio) and take an optional `AbortSignal`, and a
  `bashStream` that delivers ordered output chunks while a command runs. The
  engine is an explicit object the caller constructs — no hidden global singleton.

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

# Node addon (@hearthdev/napi): generates index.js/.d.ts + the native .node
pnpm install
pnpm --filter @hearthdev/napi build          # or: build:debug
```

### Test

```bash
cargo test --workspace --all-targets       # unit + contract suites
cargo clippy --workspace --all-targets -- -D warnings

# After building the addon (these all run on Bun too):
pnpm --filter @hearthdev/napi test            # the JS contract suite
pnpm --filter @hearthdev/napi run smoke       # packaging smoke test
pnpm --filter @hearthdev/napi run test:pi     # differential test vs pi's own edit
                                           # implementation; skips if pi is absent
bash scripts/verify-tarball.sh             # pack + install + run against that copy
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
runtime_dir="$(mktemp -d)"
chmod 700 "$runtime_dir"
./target/release/hearthd --socket "$runtime_dir/hearth.sock" --cwd "$PWD" --trust-cache &
./target/release/hearth  --socket "$runtime_dir/hearth.sock" grep -l "TODO" .
./target/release/hearth  --socket "$runtime_dir/hearth.sock" stop
```

The CLI falls back to an in-process (cold) engine only when it cannot connect
or can prove no request byte was sent. Once delivery may have begun it returns
an indeterminate error instead of replaying the operation. A streamed read may
already have emitted a partial, non-duplicated prefix in that case.

**Node.js** (where read/grep/edit win in-process):

```js
import { HearthEngine } from "@hearthdev/napi";
const eng = new HearthEngine({ cwd: process.cwd(), trustCache: true });
const r  = eng.read({ path: "src/main.rs" });               // { content, totalLines, cacheHit }
const b  = eng.readBytes({ path: "assets/logo.png" });      // binary-safe Buffer

const controller = new AbortController();
const g = await eng.grepAsync(
  { pattern: "fn ", path: "src", globs: ["*.rs"], maxTotalCount: 100 },
  controller.signal,
);

// Several disjoint edits, applied atomically against the original file.
await eng.editBatchAsync({
  path: "src/main.rs",
  edits: [
    { oldText: "fn old_name", newText: "fn new_name" },
    { oldText: "old_name()", newText: "new_name()" },
  ],
});

// Output streams while the command runs; a timeout or abort still resolves,
// with the partial output intact.
await eng.bashStream({ command: "cargo build" }, (chunk) =>
  process.stdout.write(chunk.text),
);
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
| `hearth-core`  | The resident `Engine`: the shared caches, warm-shells, and fs-watch. |
| `hearth-graph` | The I/O-free language registry, symbol extraction, and code-index layer. |
| `hearth-tools` | The six tools + msgpack transport, built on the engine. |
| `hearth-daemon`| `hearthd` — the Unix-socket server. |
| `hearth-cli`   | `hearth` — the thin client (daemon or inline). |
| `hearth-napi`  | `@hearthdev/napi` — the Node addon. |
| `bench`        | Corpus generator + criterion benches + CLI/Node/Bun harnesses. |

### Notes for contributors

- The default is always correct; the fast paths (`--trust-cache`, `--warm-shell`)
  are opt-in and documented with their trade-offs.
- Benchmarks are held to a fair standard (sync-vs-sync, path-set equality,
  atomicity caveats) — see `docs/BENCHMARKS.md` before quoting a number, and
  `docs/BENCHMARKS.md#what-the-correctness-guarantees-cost` for what cancellation,
  streaming and atomic batch editing actually cost.
- `crates/hearth-napi/index.js` and `index.d.ts` are generated but **committed**:
  they are the package's public API surface, so a change to them belongs in a
  diff. CI fails if they drift from the Rust source.
- Publishing `@hearthdev/napi` is tag-driven; the procedure and the reasoning
  behind it live in [`.claude/skills/release-napi/`](.claude/skills/release-napi/SKILL.md).
- Rust edition 2024, functional-leaning style, no hidden global state.
