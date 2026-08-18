# Benchmarks & methodology

Three layers are measured because Hearth has distinct engine, CLI, and adapter
surfaces:

1. **Engine level** — `cargo bench -p hearth-bench` (criterion, in-process).
   A function call against a warm engine: no socket, no serialization, no
   process spawn. This is the **native/napi** surface agents call directly.
   These are **component microbenchmarks**, not end-to-end product numbers.
2. **CLI level** — `bench/harness/compare.sh` (hyperfine). A fresh `hearth`
   process connects to a long-lived `hearthd` over a Unix socket each run, vs
   the real installed tool (`rg`/`cat`/`sed`). This is the product-level,
   end-to-end comparison; it pays an IPC tax ∝ payload size.
3. **Pi tool-operation level** — `bench/harness/pi-find.sh` (Node timing). It
   executes Pi's installed default find implementation and the same Pi wrapper
   with Hearth's custom glob adapter. This measures the replacement boundary,
   including Pi's result processing but excluding package import and UI render.

Hardware: Apple Silicon, 10 cores, 32 GB, macOS 25.3, Rust 1.95, `--release`
(`lto="fat"`, `codegen-units=1`). Corpus (`gen-corpus`): a **`git init`-ed**
3000-file tree with a real `.gitignore`, ~900 ignored files under
`target/`/`node_modules/`/`dist/`, 200 ignored `*.log`, a hidden file, a binary
file, a >4 MiB file, and a heavy-tailed size distribution.

## CLI level — end-to-end vs the real tools

| command | hearth-warm vs tool | verdict |
|---------|---------------------|---------|
| `grep -l TODO` (default) | ripgrep | **hearth 4.53× faster** |
| `grep -l TODO` (`--trust-cache`) | ripgrep | **hearth 6.45× faster** |
| `grep -c TODO` (default) | ripgrep | **hearth 4.31× faster** |
| `grep -c TODO` (`--trust-cache`) | ripgrep | **hearth 5.86× faster** |
| `grep` content (`function_`, many matches) | ripgrep | **hearth 1.34× faster** |
| `grep -l` **one-shot** (`--no-daemon`) | ripgrep | ≈ parity (~1.05×) |
| `read` small file | `cat` | `cat` 1.57× faster |
| `edit` (reset each run) | `sed -i` | `sed` 1.29× faster |

**The headline:** *repeated `grep` (all three modes) over an unchanged
git-tracked tree, via a warm daemon, is 1.34–4.53× faster than one-shot
ripgrep.* The advantage is resident amortization — the cold one-shot path is
only ≈ ripgrep. `read` and `edit` **lose** at the CLI: their payload (file
content / the whole edited file) crosses the socket, and `edit` also does an
atomic temp+rename; `cat`/`sed` pay neither. Those wins exist only in-process.

### Why the grep win is real *and* what confounds it

The warm-grep speedup blends four effects and the harness does **not** yet
ablate them: (1) the walk cache (skips traversal + `.gitignore` parse), (2) the
userspace file-content cache (`search_slice` on cached bytes — no `open`/`read`),
(3) the OS page cache, (4) implementation/process differences. All four favor
the resident daemon; a one-shot tool can only get (3)–(4). The number to trust
as "engine amortization" is the warm-vs-cold-`hearth` gap (~4.3×), which
isolates daemon reuse from `rg`-vs-`hearth` implementation differences.

### Why CLI `read` loses to `cat` — and why it is unfixable (startup floor)

`read` uses `SCM_RIGHTS` fd-passing: the client sends its stdout fd, the daemon
writes the cached content straight to it (no msgpack, no socket payload, no
re-print). We then measured the floor (100+ runs, `--time-unit millisecond`):

| command | mean |
|---------|-----:|
| `true` (process-spawn floor) | 1.6 ms |
| `cat` small file | 1.6 ms |
| `hearth ping` (connect + tiny round-trip) | 2.3 ms |
| `hearth read` small (fd-passed) | 2.3 ms |

`hearth read` costs **exactly** `hearth ping` — fd-passing drove the read's
marginal cost to ~0. The whole ~0.7 ms deficit vs `cat` is the CLI client's
**process-startup + socket-connect floor**: a daemon-client must spawn a process
*and* round-trip a socket, which for a trivial op costs ~as much as `cat`'s
entire runtime. Dropping mimalloc from the client shaved 2.6 → 2.3 ms; the rest
is binary page-in + the connect round-trip + Rust runtime init, none of which a
payload optimization can remove. Conclusion: **CLI small-`read` cannot beat a
tiny one-shot tool**; the read win is in-process (native/napi), where there is
no per-call spawn. This is exactly why `grep` *does* win at the CLI — the work
it amortizes (walking + reading + searching thousands of files) dwarfs the
~0.7 ms client floor, whereas a small read's saved work does not.

## Pi tool-operation level — default find vs Hearth adapter

`pnpm bench:pi-find` imports Pi 0.84.1's actual private
`dist/core/tools/find.js` and calls `createFindToolDefinition().execute()`. The
baseline follows Pi's default branch through `ensureTool`, an `fd` child
process, readline parsing, path relativization, the 1000-result limit, and the
50 KiB output limit. The comparison uses the same Pi wrapper but supplies its
custom `exists`/`glob` hooks, with `glob` backed by a release
`HearthEngine.findAsync` N-API call so cold walks do not block the JavaScript
event loop.

The generated 3000-file corpus gives both implementations equivalent ignore
rules and asserts the complete uncapped `*.rs` path set (3001 paths) before any
timing. Results on Apple M4, Node 24.6.0, Pi 0.84.1, and fd 10.4.2:

| Pi find scenario | Pi default | Hearth fresh engine | Hearth resident | resident speedup |
|---|---:|---:|---:|---:|
| selective `d000/*.rs` | 8.59 ms | 11.30 ms | **1.01 ms** | **8.54×** |
| no-match `missing-*.rs` | 8.23 ms | 11.30 ms | **0.775 ms** | **10.62×** |
| broad `*.rs`, limit 1000 | 9.54 ms | 11.72 ms | **1.11 ms** | **8.59×** |

The result is specifically a **resident-cache win**: a fresh Hearth engine is
1.23–1.37× slower than Pi's fd-backed default, while reuse of one walk snapshot
makes the replacement 8.54–10.62× faster. The broad Hearth row also computes
exact `totalMatches`, even though Pi consumes only `paths`; fd can stop at
`--max-results 1000`, so Hearth does more semantic work in that row.

“Fresh” means a new Hearth engine on every operation, not cold storage. Pi tool
discovery/download, module imports, corpus generation, and UI rendering are
outside the timed region, and every row benefits from the OS page cache. Pi's
custom `glob` hook does not expose its operation `AbortSignal`, so the adapter
cannot forward cancellation; direct `findAsync(params, signal)` callers can.
The async adapter still keeps cold traversal off the JavaScript event loop. The
harness rejects Pi/fd version drift, records the managed fd binary hash, and
isolates global/system Git ignore configuration. See
[`bench/harness/results/find_pi.md`](../bench/harness/results/find_pi.md) for
p50/p95, sample counts, correctness gates, and all reproduction caveats.

## Engine level — component microbenchmarks (criterion)

### `read` — warm vs `std::fs::read_to_string`

| file size | hearth warm | std::fs | ratio |
|-----------|------------:|--------:|------:|
| 100 lines | 1.23 µs | 9.37 µs | 7.6× |
| 2 000 lines | 2.49 µs | 13.1 µs | 5.3× |
| 20 000 lines | 10.4 µs | 34.1 µs | 3.3× |

**Caveat:** `std::fs::read_to_string` is **not** `cat`. It runs in-process (no spawn, no stdout write) but validates UTF-8;
`cat` spawns, writes stdout, and skips validation. This measures the read
*primitive* (cache serve + one alloc/copy, validity cached), not "faster than
cat" — at the CLI, `read` loses to `cat` (above).

### `grep` — 2000-file tree, files-with-matches

| case | time | note |
|------|-----:|------|
| hearth warm (`trust_cache`) | **0.58 ms** | cached bytes + cached walk + **no stat** |
| hearth warm (default) | 1.97 ms | cached bytes + cached walk, one stat per file |
| hearth cold | 19.1 ms | first run: walks + reads + caches every file |
| ignore-walk baseline | 18.4 ms | hand-rolled `ignore`+`grep-searcher` = rg's engine |

Default warm is 9.3× the ripgrep-engine baseline; `trust_cache` warm is **31×**.
Cold ≈ baseline (a first search over a fresh tree is not faster than ripgrep;
the win is on reuse).

**The `stat` is the warm bottleneck:** the
per-file `std::fs::metadata` freshness check is ~70% of default warm grep
(1.97 → 0.58 ms when removed). Skipping it needs a coherence story:

* **fs-watcher-backed stat-free — tried and rejected.** On macOS FSEvents
  reports every change (including atime bumps from Hearth's *own* reads) as
  `Modify(Any)`, indistinguishable from a real write, so the watcher invalidated
  the cache on every read → a read→atime→invalidate→re-read loop that made warm
  grep *slower* (~6 ms, high variance). The watcher cost more than the stat.
* **single-writer `trust_cache` — kept.** No watcher; warm hits skip the stat
  and rely on Hearth's own `write`/`edit` (`put_written`) to keep the cache
  coherent. External edits are served stale until eviction — hence opt-in and
  off by default. This is the 0.58 ms number above.

### `edit` — there-and-back vs disk read+replace+write

| file size | hearth warm | disk baseline | ratio |
|-----------|------------:|--------------:|------:|
| 2 000 lines | 311 µs | 301 µs | ~parity |
| 20 000 lines | 1.11 ms | 2.29 ms | 2.1× |

## Reproducing

```bash
cargo bench -p hearth-bench --bench read_bench
cargo bench -p hearth-bench --bench grep_bench
cargo bench -p hearth-bench --bench edit_bench
cargo bench -p hearth-bench --bench bash_bench
CORPUS=/tmp/hearth-corpus NUM_FILES=3000 bash bench/harness/compare.sh
pnpm bench:pi-find
```

## Orchestrator-level & cross-runtime benchmarks

These substantiate the *orchestrator* claim — sustained, multi-op, resident
workloads — beyond the single-op micro-benchmarks above.

### Ablation — what the warm-grep win is actually made of

`cargo bench -p hearth-bench --bench ablation_bench` (2000 files, files-with-matches):

| cache state | time | isolates |
|-------------|-----:|----------|
| all cold (fresh engine) | 19.2 ms | nothing warm |
| walk only (content cleared each iter) | 14.8 ms | walk cache alone |
| content only (walk cleared each iter) | 6.25 ms | file-content cache alone |
| both warm | **1.98 ms** | full resident engine |

So the ~9.7× warm win is **content-cache-dominated** (14.8 → 2.0 ms once content is
warm) with a modest walk-cache contribution (19.2 → 14.8 ms); they compound. This
is the decomposition the earlier "grep is 9× faster" number hid.

### Multi-op amortization (the actual orchestrator workload)

`multiop_bench` — a read→grep→edit sequence over one tree:

| | time |
|---|---|
| warm resident engine | **2.32 ms** |
| fresh engine each iteration | 19.6 ms |

**8.5× cross-op amortization** — the daemon's whole reason to exist, now measured.

### Cold-start, break-even, concurrency, RSS

From `bench/harness/{cold_start,concurrent,rss}.sh`:

* **Cold-start incl. daemon spawn**: warm grep 6.1 ms; a full cold one-shot
  *including* `hearthd` spawn is 68.8 ms (spawn ≈ 62.6 ms); `hearth --no-daemon`
  28.1 ms ≈ ripgrep 29.1 ms. **Break-even ≈ 3 warm calls** amortize the spawn.
* **Concurrency** (N parallel clients, one warm daemon): 154 → 218 → 273 → 304
  qps at N = 1/2/4/8 (p95 6.7 → 28.5 ms). Throughput scales sublinearly (thread
  -per-connection + shared caches).
* **RSS**: steady ≈ 74 MiB after warming a 3000-file corpus; flat across an
  optimizer tick when idle.

### Cross-runtime: Hearth (napi) vs Node.js / Bun `fs` — **fair** comparison

`bench/harness/{node/compare.mjs, bun/compare.js}` — release napi, sync-vs-sync
headline (async listed separately), naive-but-correct search (`readdir` +
`readFile` + regex, no redundant `stat`/`access`), **path-set equality asserted**,
flat corpus (no ignore-rule asymmetry). These fairness rules matter — a looser
methodology overstates the wins.

| operation | vs Node 24 | vs Bun 1.3 | verdict |
|-----------|-----------:|-----------:|---------|
| `read` small (sync) | **5.2× faster** | **7.2× faster** | Hearth wins (serves cached bytes; fs re-reads) |
| `read` big ~1.5 MB (sync) | **1.5× faster** | **1.1× faster** | Hearth wins |
| `grep`/search WARM | **21.7× faster** | **26.7× faster** | Hearth wins (cached walk + bytes) |
| `grep`/search COLD | **1.3× faster** | **1.3× faster** | Hearth wins (fresh engine) |
| `write` vs plain `writeFileSync` | 3.9× **slower** | 5.7× **slower** | Hearth loses |
| `write` vs **atomic** (temp+rename) | 1.3× **slower** | 1.1× **slower** | Hearth ~parity, slightly slower |

**Verdict:** Hearth's `read` and `grep` beat Node and Bun `fs` decisively
even under a scrupulously fair, sync-vs-sync, release comparison. **`write` is the
exception** — it is *atomic* (temp + rename, crash-safe replace) and also refreshes
the warm cache, so it does strictly more work than a plain `writeFile`; against an
equally-atomic baseline it is only ~1.1–1.3× behind. That residual gap is the napi
content marshalling (a `serde_json` copy) plus the cache-update copy.

**napi `readBytes` / `writeFast` (80 KB, ns/op ratios):** `readBytes` returns a
binary-safe `Buffer` and wins **1.58×** vs `fs.readFile` (Buffer-to-Buffer) —
matching the string `read` win, and correct for non-UTF-8 files. `writeFast`
(direct `path,content` args, content moved into the cache, no `serde_json`
intermediate) came out only **1.04×** faster than `write` (211 → 202 µs): the
two saved content copies are tiny next to the atomic temp+rename+stat syscalls,
so `writeFast` still loses **1.42×** to an atomic `writeFile` and **3.37×** to a
plain one. This **confirms the write loss is inherent to atomicity**, not a
marshalling artifact — a real, measured negative result. True zero-copy `read`
via an external `Buffer` was **rejected**: a JS `Buffer` is mutable and a view
into the shared, mtime-validated cache entry could be silently corrupted by the
caller, serving wrong bytes on later reads. Safety won; `readBytes` copies once. `fs.stat`/`access`/`readdir` have no
standalone Hearth tool; they appear only as components of the search comparison,
where the composite decisively favours Hearth.

## What the correctness guarantees cost

Cancellation, streaming, global limiting and atomic batch editing are not free.
These are the measured prices, so the trade can be judged rather than assumed.

Measured on the same hardware with a release napi build, against a 1000-file
tree (120 lines each, ~2100 matches) and a 2000-line file for the edit rows.
Each figure is the mean of 30 iterations after warm-up; anything inside ±2 % is
noise on this machine.

| guarantee | measurement | cost |
|---|---|---|
| `grep` global limit, never reached | `maxTotalCount: 100000` vs no limit | **~1 %** (one mutex op per *file*, not per match) |
| `grep` global limit, reached early | `maxTotalCount: 100` vs no limit | **−97 %** — the early stop is the point |
| `grep` cancellation polling | live `AbortSignal` vs none | **~0 %** (one relaxed atomic load per file and per match) |
| `bash` streaming | `bashStream` + callback vs `bashAsync` | **~0 %** — the pipes were already drained on reader threads; the callback rides along |
| `bash` without collection | `collectOutput: false` | **~5 % faster**, and it drops the second copy of the output |
| `editBatch` vs single `edit`, no diff | `skipDiff: true` | **+52 %** |
| `editBatch` vs single `edit`, with diff | default (4 lines of context) | **+193 %** |

The `editBatch` overhead is what pi-compatible matching costs: LF normalization,
one normalized view of the file for ambiguity checking, and the line-span scan
that keeps untouched lines byte-exact. It is bounded to *one* normalization per
call regardless of how many edits it carries, and the common case — an ASCII
file with no trailing whitespace — borrows rather than copies. The remaining
+141 % between the two rows is the diff itself, which `skipDiff` turns off for a
caller that does not render one.

In absolute terms all of these are sub-millisecond on a 2000-line file
(`edit` 0.17 ms, `editBatch` 0.26 ms without a diff, 0.49 ms with one), against
a `bash` call that costs milliseconds and a `grep` over a real tree that costs
more.

### Warm shell, after the at-most-once rework

`cargo bench -p hearth-bench --bench bash_bench`, trivial command (`true`):

| | time |
|---|---|
| warm pool | 563 µs |
| spawn per command | 2.04 ms |

**3.62× faster than spawning**, down from 3.8× before the rework. The ~5 % of
the advantage that was given up buys the control pipe, `set -m` job isolation,
the post-command process-group kill, and the pre-dispatch drain — i.e. the
at-most-once guarantee and the output isolation. Correctness was the better
trade here: the previous pool would silently re-run a command after an ambiguous
protocol failure.

## Known gaps

Read every number above as scoped to *this* synthetic corpus and *this* access
pattern. Still not measured:

* **Post-change re-warm** — invalidation plus re-read cost after an edit, and
  what `invalidateRoot` costs on a large warm cache.
* **Multiple real repositories**, rather than one generated corpus.
* **Sustained streaming throughput** — the `bash` streaming rows use a command
  that finishes in milliseconds; a long-running build's chunk rate is not
  characterised.
* **Cancellation latency** — the contract suite proves cancellation *happens*
  and that nothing outlives it, but not how quickly it lands under load.
* **Windows** — not a supported target, so not measured.
