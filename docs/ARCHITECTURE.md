# Architecture

Hearth is a Cargo workspace plus a Node package. The design separates a
transport-agnostic **core + tools** from the three **surfaces** that expose them
(daemon, CLI, napi) — exactly the corsa-bind split of "orchestration core" from
"bindings".

## Crates

| crate | role |
|-------|------|
| `hearth-proto` | The single contract: `ReadParams`/`ReadResult`/… and the `Request`/`Response` envelope. `serde`, `camelCase`, dependency-light. Shared by every surface so they never drift. |
| `hearth-core` | The resident `Engine`: shared caches, profiler, self-optimizer, fs-watch. No tool logic. |
| `hearth-tools` | The five tools as plain `fn(&Engine, &Params) -> Result<Result>`. Plus `dispatch()` and the msgpack `transport`. |
| `hearth-daemon` | `hearthd`: one `Engine`, a Unix-socket server, thread-per-connection. |
| `hearth-cli` | `hearth`: thin client; connects to the daemon or runs inline (cold) as a fallback. |
| `hearth-napi` | `@hearth/napi`: a `#[napi]` `HearthEngine` object; sync methods + `*Async` (libuv worker) twins. |
| `hearth-bench` | Deterministic corpus generator + criterion benches. |

## The Engine

`Engine` is a cheap `Arc`-clone handle. It owns:

* **`FileCache`** (`cache/file.rs`) — `DashMap<PathBuf, Arc<FileEntry>>`.
  A `get` is served warm when the on-disk `(mtime_ns, size)` matches the cached
  entry. `FileEntry` lazily memoizes its `LineIndex`, xxh3 content hash, and —
  critically for read speed — its **UTF-8 validity**, so warm reads skip
  re-validation. Content is stored as owned `Arc<[u8]>` (never a held `mmap`, so
  an external truncation cannot `SIGBUS`). Concurrent identical loads collapse
  through a `SingleFlight`. `get_bounded` refuses to cache oversize files so one
  grep over a huge tree can't flood memory.
* **`WalkCache`** (`cache/walk.rs`) — a parallel `ignore`-walk (ripgrep's
  walker) cached per `(root, ignore-config)`. Globs post-filter the cached list,
  so one walk serves every glob over the same tree.
* **`Profiler`** (`profiler/`) — a sharded (32-way, name-hashed) timing store
  with per-op log2-µs histograms for p50/p99, plus process-global allocation
  counters via a `ProfilingAllocator` that decorates mimalloc. Macro-gated
  (`profile!`): one relaxed atomic load when disabled. Poison-tolerant locks and
  `&'static str` keys keep the hot path allocation-free.
* **Self-optimizer** — a background thread running an **adaptive control loop**.
  Each tick it reads the file cache's always-on windowed hit-rate (independent of
  the timing profiler) and its live byte size, then retunes the cache **byte
  budget** with hysteresis: grow (×1.5, up to `max_cache_bytes`) when reuse is
  high and the cache is nearly full — a warm workload earns more warm memory —
  and shrink (×0.75, down to `min_cache_bytes`) when reuse is low. It enforces
  the budget with **LRU eviction** (entries carry a monotonic access stamp;
  `total_bytes` is maintained O(1) across insert/replace/evict) plus a hard
  entry-count safety cap. Decisions are emitted as counters and `cache_report()`
  surfaces the live state. Unlike vize's profiler (measurement + human-read
  recommendations), this **closes the loop in-process** — the measurement
  substrate feeds an actual runtime controller.
* **fs-watch** (`watch.rs`) — best-effort `notify` watcher that invalidates the
  file cache on content changes and the walk cache on structural changes.
  Correctness never depends on it (the `stat` validation is always there); it's
  a latency optimization.

## Tool → cache mapping

* `read` → `FileCache`. Warm = stat + `Arc` clone + one copy (validity cached).
* `write`/`edit` → atomic temp-file + rename, then `put_written` refreshes the
  cache in place so the next `read`/`edit` is warm without touching disk.
* `grep` → `WalkCache` for the file set + `FileCache` (`get_bounded`) for the
  bytes, searched with `grep-searcher`/`grep-regex` in a work-stealing thread
  pool. Warm = no walk, no file opens.
* `bash` → fresh `/bin/sh -c` per command in its own process group, pipes
  drained on reader threads, timeout enforced by killing the group. (A pooled
  warm-shell fast path is the next optimization.)

## Surfaces

* **Daemon/CLI**: length-prefixed msgpack (`transport.rs`) over a Unix socket.
  Synchronous request→response, one thread per connection, engine shared by
  `Arc` clone. The daemon reads each request with `recvmsg` so a client can
  attach its stdout fd via `SCM_RIGHTS`; for a `read` the daemon then writes the
  cached content **straight to that fd**, skipping payload serialization
  entirely. (This makes CLI `read` as fast as the client's own startup floor —
  which, for a daemon-client, is still ~0.7 ms above a tiny tool like `cat`; the
  read speed win therefore lives in-process, not at the CLI.)
* **napi**: JSON at the boundary (`serde_json::Value` in/out). Sync methods run
  on the JS thread; `grepAsync`/`bashAsync` offload to a libuv worker via
  `AsyncTask` (no embedded tokio), resolving a JSON string. The engine is an
  explicit object the caller constructs — no hidden global singleton.

## Deliberate non-goals / trade-offs

* **Owned bytes over `mmap`** in the persistent cache — safety over a marginal
  large-file read speedup; `mmap` stays available as a transient grep fast path.
* **`stat`-per-warm-read** — correctness over shaving one syscall; fs-watch can
  make it skippable when the tree is known-quiet (a future `trust_watch` mode).
* **Externally-tagged transport enums** — internally-tagged enums don't
  round-trip through `rmp-serde`.
