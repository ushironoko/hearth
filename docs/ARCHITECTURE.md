# Architecture

Hearth is a Cargo workspace plus a Node package. The design separates a
transport-agnostic **core + tools** from the three **surfaces** that expose them
(daemon, CLI, napi) — exactly the corsa-bind split of "orchestration core" from
"bindings".

## Crates

| crate | role |
|-------|------|
| `hearth-proto` | The single contract: `ReadParams`/`ReadResult`/… and the `Request`/`Response` envelope. `serde`, `camelCase`, dependency-light. Shared by every surface so they never drift. |
| `hearth-core` | The resident `Engine`: shared caches, cancellation, per-path mutation locks, profiler, self-optimizer, fs-watch. No tool logic. |
| `hearth-tools` | The five tools as plain `fn(&Engine, &Params) -> Result<Result>`, each with a `*_cancellable` twin. Plus `dispatch()` and the msgpack `transport`. |
| `hearth-daemon` | `hearthd`: one `Engine`, a Unix-socket server, thread-per-connection. |
| `hearth-cli` | `hearth`: thin client; connects to the daemon or runs inline (cold) as a fallback. |
| `hearth-napi` | `@hearthdev/napi`: a `#[napi]` `HearthEngine` object; typed sync methods + cancellable `*Async` (libuv worker) twins + `bashStream`. |
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
  walker) cached per `(root, ignore-config)`. The file list is **sorted** once at
  build time, so every consumer is deterministic and `grep` can treat index order
  as path order. Globs post-filter the cached list, so one walk serves every glob
  over the same tree.
* **`PathLocks`** (`pathlock.rs`) — one mutex per **canonical** path, taken for
  the whole read-modify-write of any mutation. Two concurrent `edit`s of the same
  file each compute a whole-file image from the original, so without this the
  second write would silently discard the first. Canonicalization means
  `dir/file`, `./dir/file` and `link-to-dir/file` all serialize against each
  other; a path that does not exist yet keys on its canonical parent plus its
  name, covering the create-then-edit race. Entries are reclaimed once nobody
  holds them.
* **`CancelToken`** (`cancel.rs`) — a one-way latch every tool polls at its own
  safe points. The "no token" case is a `None` inside the struct, so the
  synchronous paths pay nothing for the feature.
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
  surfaces the live state.
* **fs-watch** (`watch.rs`) — best-effort `notify` watcher that invalidates the
  file cache on content changes and the walk cache on structural changes.
  Correctness never depends on it (the `stat` validation is always there); it's
  a latency optimization.

## Cancellation

Cancellation is **cooperative and non-preemptive**. A JS `AbortSignal` latches a
`CancelToken` from the JS thread; the tool, running on a libuv worker, polls it.
Nothing is interrupted mid-step. What each tool guarantees when a cancelled call
settles:

| tool | on cancel |
|------|-----------|
| `read` / `readBytes` | Rejects. A pre-aborted call never touches the cache. |
| `write` / `edit` / `editBatch` | Rejects **before** the write is issued, or completes it. The path's mutation lock is held across the write *and* the cache refresh, so the token is never observed while native work could still commit — releasing early would let a queued mutation of the same file interleave. |
| `grep` | Stops scheduling files, abandons the file being searched at its next match, **joins every worker**, then rejects. No search thread outlives the call. |
| `bash` | SIGKILLs the command's whole process group, reaps it, and **resolves** with `aborted: true` and the partial output — a streaming caller keeps what it already rendered rather than losing it to an exception. |

A pre-aborted signal is detected by reading `signal.aborted` while converting it,
not by waiting for an `abort` event that will never fire for a signal that is
already latched.

The cost is one relaxed atomic load per poll site, plus a 10 ms wakeup cap in
`bash`'s wait loop **only when a live token is present** — a call without a
signal blocks on its real deadline as before.

## Tool → cache mapping

* `read` → `FileCache`. Warm = stat + `Arc` clone + one copy (validity cached).
  `lineMode` picks between a `cat`-style slice and `split('\n')` semantics, which
  differ in whether a window keeps its trailing newline and whether a file's
  trailing newline counts as a line.
* `write` → the requested `WriteMode` (see below), then `put_written` refreshes
  the cache in place so the next `read`/`edit` is warm without touching disk.
* `edit` / `editBatch` → `FileCache` for the original bytes, matched under the
  path's mutation lock, then one commit through the same write path.
* `grep` → `WalkCache` for the file set + `FileCache` (`get_bounded`) for the
  bytes, searched with `grep-searcher`/`grep-regex` across worker threads. Warm =
  no walk, no file opens.
* `bash` → a fresh shell per command in its own process group, or the opt-in
  warm pool. Both stream ordered chunks.

## Batch edit

`editBatch` applies several disjoint replacements in one atomic commit, matching
pi 0.80.7's rules exactly (`edit_text.rs` is a direct port, and
`__test__/pi-compat.mjs` runs the same fixtures through pi's own module and
requires byte-identical output). The rules, in order:

1. A UTF-8 BOM is stripped before matching and restored on write.
2. Content is normalized to LF for matching, then restored to the file's own
   convention — CRLF when the *first* newline in the file was part of a CRLF
   pair, otherwise LF.
3. Every `oldText` is matched against the **same original content**, never
   against the result of an earlier edit in the same call.
4. Exact matching first. If *any* edit needs the normalized fallback, the whole
   call switches to normalized space so all offsets share one coordinate system.
   Normalization is NFKC, then per-line trailing-whitespace removal, then folding
   of smart quotes, Unicode dashes, and special spaces.
5. Each target must be unique and must not overlap another. Uniqueness is always
   judged in normalized space, so two regions differing only in trailing
   whitespace still count as ambiguous.
6. When the fallback was used, only the lines a replacement actually touches are
   rewritten from normalized text; every other line keeps its original bytes. So
   matching through a smart quote never rewrites an unrelated line's whitespace.

Any failure — not found, ambiguous, overlapping, or "the result is identical" —
leaves the file untouched, because nothing is written until every edit resolves.

The result carries **diff hunks** rather than a second copy of the file: each
changed region plus its context, with 1-based old/new line numbers, and
`firstChangedLine`. The gap between one hunk's end and the next hunk's start is
exactly the number of unchanged lines a renderer elides, which is enough to
produce both a line-numbered display diff and a unified patch without a second
read that could observe a *different* file. `returnContent` adds the full
post-edit text for a caller that wants it.

Two opt-ins serve adapters that render with their frontend's own diff
generators instead of the hunks:

* `returnOriginalContent` adds the exact pre-edit text — BOM and line endings
  intact, unlike the normalized `content` — snapshotted while the same
  mutation lock as the write is held, so the snapshot and the commit are one
  transaction. That guarantee spans writers going through the same engine; the
  lock is in-process, so external writers are not serialized by it. The
  persisted bytes need no third field: they are always
  `(hadBom ? BOM : "") + (crlf ? content with CRLF restored : content)`.
* `whitespaceOnlyTargetPolicy: "exactFile"` permits an `oldText` whose fuzzy
  normalization is empty — spaces and tabs with no newline — in exactly one
  situation: its LF-normalized text equals the entire file. Such a target has
  no coordinates in normalized matching space (occurrence counting cannot see
  it, and the fallback would resolve it as a zero-width match at offset 0), so
  whole-file equality is the one case with nothing left to guess. Empty
  `oldText` stays invalid under every policy, and the default (`"reject"`)
  keeps Hearth 0.1.0 behavior.

## Write semantics

Hearth's default write is **not** `fs.writeFile`, and the difference is visible:

| | `atomic` (default) | `inPlace` |
|---|---|---|
| mechanism | temp file + `rename(2)` | `open(O_TRUNC)` + write |
| partial reads | impossible | possible |
| crash mid-write | old file intact | truncated file |
| inode | **replaced** | preserved |
| mode | copied from the previous file | preserved |
| owner, xattrs | not carried over | preserved |
| other hardlinks | keep the **old** content | see the new content |

`inPlace` exists so an adapter that must match `fs.writeFile` can. `atomic` is
the default because a crash-safe write is the better default for a tool that
rewrites source files.

Both modes **follow symlinks by default**: writing to a link rewrites its target
and leaves the link alone. Replacing a link with a regular file needs an explicit
`followSymlinks: false` — an atomic write would otherwise `rename(2)` over the
link and silently destroy it, which is never what "edit this file" means. The
resolved target is what the mutation lock and the cache key use, so a write
through a link and a write to the target serialize against each other.

## Bash

Both paths stream ordered stdout/stderr chunks through the caller's callback as
they arrive. `seq` is a single monotonic counter shared by both channels, so
replaying chunks in `seq` order reproduces the observed interleaving. Multi-byte
UTF-8 sequences are never split across chunks: an incomplete trailing sequence is
held back until its continuation arrives. The same bytes are also accumulated
into the result unless `collectOutput` is off.

The shell is configurable (`program`, `args`, and whether the command arrives as
an argv entry or on stdin), so an adapter can keep its own shell semantics rather
than inheriting `/bin/sh -c`.

**Detached descendants.** A command's child can outlive the shell while still
holding its stdout pipe. Waiting for the pipes to close would hang; destroying
them at a fixed deadline would truncate output still being written. Hearth waits
for the pipes to fall *idle* instead: a 100 ms grace re-armed on every chunk, so
an actively writing descendant keeps the call reading while a silent one releases
it. On timeout, cancellation, or shutdown, the whole process group is SIGKILLed
and reaped.

### Warm shell pool (opt-in, `warmShell`)

Skips the per-command process spawn. Each pooled shell owns four pipes: stdin
(the script), stdout and stderr (the command's output), and a private **control**
pipe on fd 3 carrying only Hearth's protocol — so an exit code can never be
confused with command output, and the delimiter search on stdout/stderr has one
job. Each command runs in a `( … )` subshell with stdin from `/dev/null`, which
keeps cwd, environment and variable changes from leaking between commands.

**At-most-once.** A command is "dispatched" the moment any byte of its script
reaches the shell. If the very first write fails having written nothing, the
command provably never ran and Hearth falls back to a fresh spawn. Any later
failure returns `ErrorKind::Indeterminate` and the command is **never re-run** —
a mutating command must not execute twice because a pipe broke. The caller gets a
distinct error kind precisely so it can decide what to do rather than having
Hearth guess.

**Output isolation.** A command that backgrounds a process leaves a descendant
holding the shell's pipes, which could otherwise write into the *next* command's
output. Two measures close that: `set -m` puts each command's subshell in its own
process group, which Hearth kills as soon as the command settles; and before
dispatching anything the pool drains the shell's channel and retires the shell if
a single stray byte shows up. The second check happens *before* dispatch, so
retiring there costs nothing. A shell whose command timed out, was cancelled, or
broke its protocol is never handed to another command.

Correctness relies on a fresh random 128-bit nonce not occurring in command
output — roughly 2^-128 per command.

## Grep

**Global limiting.** `maxTotalCount` caps matches across the whole search,
distinct from the per-file `maxCount`. The kept matches are the first N *in path
order*, which does not depend on how the parallel search interleaved: truncation
happens after the merge and after sorting. A partially kept file retains the
context lines following its last kept match, but not the leading context of the
first dropped one.

Bounding the work without losing that determinism uses a **completed-prefix**
rule: the limiter tracks the contiguous prefix of finished files in path order,
and workers stop pulling new files once that prefix alone holds enough matches.
At that point no unstarted file can contribute to the first N, so stopping is
sound. Files that finish out of order are still accounted; they just cannot
trigger the stop until the gap ahead of them fills in.

## Cache coherence

`trustCache` skips the per-hit freshness `stat` — where most of the warm-read
speed comes from — by assuming **Hearth is the only writer**. Hearth keeps its
own mutations coherent:

* `write`/`edit` refresh the file-cache entry in place, so a following read sees
  the new bytes without touching disk.
* Creating a path, or rewriting one that steers traversal (`.gitignore`,
  `.ignore`, `.rgignore`, `.git/info/exclude`), invalidates the cached walks that
  could have enumerated it. Overwriting an ordinary existing file does not,
  because it cannot change what a walk recorded — that case costs one boolean
  test.
* A write through a symlink also drops the link path's own cache entry.

**The remaining limitation is real and not papered over**: a change made outside
Hearth — by an editor, by `git checkout`, by a subprocess — stays cached until
something invalidates it. That is the whole bargain of `trustCache`, and the
reason the invalidation API is explicit:

* `invalidatePath(path)` — one file, plus any walk that could have listed it.
* `invalidateRoot(root)` — everything beneath a directory. **This is the sound
  choice after a `bash` call**: an arbitrary command can create, delete, rename
  or rewrite anything under its working directory, and no cheaper invalidation
  covers that. Hearth does not do it automatically, because the adapter knows
  which root the command could have touched and Hearth does not.
* `invalidate(path, recursive, scope)` — the scoped form.
* `clearCaches()` — everything.

Cost is proportional to the number of *cached* entries, not to the size of the
tree on disk, so it stays bounded by the cache's own entry cap.

With `trustCache` off (the default), every warm hit still stats, and none of this
is needed for correctness — only for latency.

## Surfaces

* **Daemon/CLI**: length-prefixed msgpack (`transport.rs`) over a Unix socket.
  Synchronous request→response, one thread per connection, engine shared by
  `Arc` clone. The daemon reads each request with `recvmsg` so a client can
  attach its stdout fd via `SCM_RIGHTS`; for a `read` the daemon then writes the
  cached content **straight to that fd**, skipping payload serialization
  entirely. (This makes CLI `read` as fast as the client's own startup floor —
  which, for a daemon-client, is still ~0.7 ms above a tiny tool like `cat`; the
  read speed win therefore lives in-process, not at the CLI.)
* **napi**: concrete `#[napi(object)]` types at the boundary — the generated
  `index.d.ts` describes the real shapes, with no `any` on any tool method.
  Sync methods run on the JS thread; every `*Async` twin offloads to a libuv
  worker via `AsyncTask` (no embedded tokio) and takes an optional `AbortSignal`.
  `bashStream` additionally takes a chunk callback, invoked from the worker
  through a `ThreadsafeFunction` in non-blocking mode so a slow JS consumer
  cannot stall the pipe readers into a deadlock. The engine is an explicit object
  the caller constructs — no hidden global singleton.

  Errors lead their message with `"<kind>: "`; synchronous methods also set the
  kind as `Error.code`. The async ones cannot, because napi fixes a worker task's
  error type — hence the message prefix is the one format to branch on.

  The addon sets `SIGPIPE` to `SIG_IGN` on construction: a cdylib gets no Rust
  runtime init, so a pooled shell dying mid-write would otherwise take the host
  process down.

## Deliberate non-goals / trade-offs

* **Owned bytes over `mmap`** in the persistent cache — safety over a marginal
  large-file read speedup; `mmap` stays available as a transient grep fast path.
* **`stat`-per-warm-read by default** — correctness first; `trustCache` is the
  opt-in for callers that own their workspace.
* **Cooperative, not preemptive, cancellation** — a mutation always finishes or
  never starts, rather than being interrupted somewhere in between.
* **No automatic invalidation after `bash`** — the adapter knows the blast
  radius; guessing it in the engine would either be unsound or drop the whole
  cache on every command.
* **One engine per process, not shared across processes** — the caches are
  in-process memory; sharing them would mean re-introducing the daemon's IPC cost
  on the path where Hearth is fastest.
* **Externally-tagged transport enums** — internally-tagged enums don't
  round-trip through `rmp-serde`.
