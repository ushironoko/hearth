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
| `hearth-graph` | The code-index and module-graph layer: language registry, symbol/import extraction, `SymbolIndex`, `ModuleGraph`, and resolver abstractions. |
| `hearth-tools` | The seven tools as plain `fn(&Engine, &Params) -> Result<Result>`, each with a `*_cancellable` twin. Plus `dispatch()` and the msgpack `transport`. |
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
  walker) cached per `(root, ignore-config)`. Files, directories, and unresolved/
  unfollowed symlinks are retained in separate **sorted** slices; followed
  symlink-to-file entries stay in the original file slice consumed by grep and
  graph. All slices share one bounded completeness snapshot and resident path-byte
  accounting. Globs post-filter the snapshot, so one walk serves every find/grep
  pattern over the same tree.
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
| `find` | Rejects before/after the walk or while filtering. A cold bounded walk is one non-preemptive safe step, so cancellation may not settle until that step returns; a warm filter polls every candidate. |
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
* `find` → `WalkCache` only. It allocation-free merges the three sorted entry
  slices, applies path exclusions and a compiled include glob, counts every
  match, and retains a bounded deterministic prefix.
* `graph` → `WalkCache` for the universe + `FileCache` for bytes and hashes +
  the `GraphState` engine extension + `InvalidationLog` for derived-state
  coherence.
* `graph_prefetch` / `graphPrefetch` (native/N-API integration only) →
  `FileCache` plus additive `GraphState` publication for explicit seeds and
  their direct resolved targets. It never populates `WalkCache` or
  discovers/parses ignore files.
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

## Find

`find` reproduces Pi 0.84.1's `fd --glob --hidden` path contract without
spawning `fd`. Patterns without `/` match each entry basename. Slash-containing
relative patterns gain Pi's `**/` prefix and match the lexical absolute
candidate with literal separator semantics (`*` never crosses `/`, legal `**`
does); absolute and already-`**/` patterns are unchanged. Output is sorted by
raw path, rendered root-relative with POSIX separators, and directories carry a
trailing `/`. Unfollowed and dangling symlinks are plain entries; followed
symlink directories carry `/` and are traversed.

The result separately reports count truncation and Pi's 50 KiB path-text
truncation while `totalMatches` remains exact. It retains the first complete
path that crosses 50 KiB as bounded headroom, allowing Pi's custom-operation
wrapper to detect the oversized joined text and emit its standard warning.
Exclusion globs are applied
before counting and limiting, so a Pi adapter can pass
`["**/node_modules/**", "**/.git/**"]` without discarding valid later paths.
Matching uses fd-compatible smart-case (all-lowercase patterns ignore case;
uppercase makes them sensitive), and an empty pattern matches all entries. The
include pattern is capped at 4 KiB; exclusions at 128 patterns / 16 KiB; and
the caller limit at 1,000,000. Exclusions are post-filters over the shared
snapshot and therefore do not reduce cold-walk work or its safety budget.

Ignore behavior is intentionally narrower than `fd`: Hearth discovers only
bounded root-local `.ignore`/`.rgignore`, not ancestor/global Git configuration.
A warm walk is a structural snapshot. After a mutation performed outside Hearth,
an adapter must use fs-watch or call `invalidateRoot`; otherwise newly created,
removed, or renamed entries (including empty directories) remain stale.
Non-UTF-8 Unix names use lossy stdout-compatible rendering.

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

## Graph

The graph tool has two layers. `hearth-graph` owns the index and module-graph
semantics, while the cache-aware adapter in `hearth-tools` supplies the source
universe, file bytes, freshness, and publication boundary. Stage A covers
symbols, outlines, symbol search, definitions, and status; Stage B layers deps,
rdeps, and neighborhood traversal on the same analyzed files. Cross-language
import resolution is outside that stage's scope.

The native/N-API dependency-prefetch primitive (`graph_prefetch` in Rust,
`graphPrefetch` in JavaScript) is deliberately outside those query operations.
An integration adapter supplies explicit observed seeds; Hearth
loads and analyzes those seeds, resolves their imports, and may additionally
warm only direct in-root targets. It performs no directory walking and no
ignore-file discovery or parsing. Explicit/resolved paths are still subject to
canonical root containment, regular-file, supported-language, UTF-8, byte, and
symlink checks. `hidden` and `respectGitignore` are retained only for graph-call
option parity and do not cause filtering or discovery.

Prefetch publication is additive, but retention is opportunistic: `FileCache`
entries and `GraphState` roots remain bounded and can be evicted or invalidated.
The structured result therefore preserves cache and graph outcomes separately:
`cacheHits` counts reused file entries while `graphUpdates` counts publications
that changed graph state. Candidate-specific `skips` and `truncated` expose
partial work without turning warming into a durability guarantee. The native
hard caps are 32 seeds, 64 imports examined per seed, 256 unique direct targets,
2 MiB per file, and 16 MiB total source per request; caller-supplied limits can
only reduce them.

`GraphState` is an `Engine` extension with one `RootGraph` per resolved
absolute root. Roots are not canonicalized by the shared adapter, so two
symlink spellings of the same directory build two independent indexes; the
CLI canonicalizes existing roots before dispatch, native and napi callers get
the spelling they pass.
Each root moves through `Uninitialized`, `Building`, `Ready { generation }`, and
`Failed`. Only `Ready` may answer from stale state after losing the sweep lock.
During a cold build, competing queries wait at a cancellable barrier until the
first complete generation is published; they never treat the initial empty
index as an empty repository.

Each query selects a `SweepKey`: either the `WalkKey` that defines a cached walk
or the sorted caller-supplied file view. A revalidation sweep first consumes the
root's `InvalidationLog` delta, then uses the `(mtime_ns, size)` stat record and
the cached content hash as its reindex gate. Parsing and deletion detection
happen outside the state write lock; the complete delta, counters, sweep time,
and next generation are published together. `max_stale_ms` may reuse only a
matching sweep key inside the caller's explicit window.

`Exact` means exact for the verified snapshot identified by the result's basis
and `sweep_age_ms`, scoped to supported languages; reuse explicitly allowed by
`max_stale_ms` stays exact, with `sweep_age_ms` taken from that matching sweep
key's stamp rather than a newer sweep of another view. A non-voluntary stale
answer caused by sweep-lock contention is `Approximate`, and so is any answer
from a root with known unindexable files (`failedFiles` or `oversizeFiles` above
zero) — those files may hold symbols the answer cannot see. The stat gate shares
grep's trust contract: an out-of-band edit that preserves both size and mtime
stays invisible until an invalidation arrives. The result meta reports the
weakest guarantee for the complete answer.

The dependency layer extracts imports during the same parse that feeds the
symbol index, then resolves JavaScript and TypeScript specifiers through
`oxc_resolver`. Workspace paths become graph nodes (initially stubs when the
target has not been analyzed), installed packages remain external targets, and
failed lookups remain unresolved edges. `ModuleGraph` updates both outgoing
edges and reverse memberships incrementally. A forward result is `Exact` only
when its source was analyzed without opaque imports, had an import extractor
and a live resolver, every outgoing resolution was complete, and it was
resolved under the current resolver generation; reverse traversal additionally
requires a complete universe in which every node satisfies those conditions.
Edges and operation results carry their own `Exact` or `Approximate` labels.
Multi-hop traversal composes them by taking the weakest guarantee, and
`meta.guarantee` takes the weakest again across graph structure, sweep
freshness, and unindexable files.

Resolver configuration is part of that generation boundary. A root begins with
tracked optional stat records for both `tsconfig.json` and `jsconfig.json`,
including `None` when either file is missing. `tsconfig.json` takes precedence
when both exist; otherwise `jsconfig.json` is used as the root tsconfig-format
configuration. Resolution adds the selected config and every tracked `extends`
dependency, including missing relative targets, and every sweep compares fresh
optional `(mtime_ns, size)` records with the previous set. A change, including a
missing-to-present transition, replaces the resolver, advances its generation,
and re-resolves all analyzed imports before publishing the sweep, so callers do
not observe edges from the old configuration.

Verified rdeps has a source-text backstop for cases where structural exactness
cannot be established. It greps the whole root for needles derived from the
target stem, then skips a hit only when the file's current hash is present in
both indexes and its module node is analyzed, opaque-free, import-extraction
capable, backed by a live resolver, and resolved in the current generation.
Hits that are already analyzed but structurally incomplete are reported as
`Approximate` importer entries without re-analysis; stale or unknown candidates
are re-analyzed so comment-only hits can be rejected and newly discovered
imports can repair the graph. At most `MAX_RDEPS_REPAIR` such candidates are re-analyzed
per query; stopping at that cap sets `repair_truncated`. The grep candidate set
is also bounded, and overflow reports `repair_truncated` rather than claiming
complete verification. Files that cannot be analyzed, including oversized
files, and analyzed files with opaque imports remain `Approximate` importer
entries rather than being silently discarded. Concurrent identical repairs
collapse into a single flight keyed by the target, the graph and resolver
generations, and the hidden/ignore/symlink parameters that affect the grep.

Rust resolution v1 keeps best-effort filesystem edges for the standard `src/`
layout and direct `src/bin/*.rs`, `examples/*.rs`, and `tests/*.rs` crate roots,
but its resolver baseline and every outcome are `Partial`: Rust nodes remain
`Approximate` even when import extraction finds no imports. `Exact` Rust
resolution would require Cargo target metadata plus a declaration-tree model
for inline modules, `#[path]`, `cfg`, macros, and `mod` ambiguity; that remains
future work.

Node IDs are `<path-prefix>@<xxh3-hex>`. For indexed nodes the prefix is the
root-relative path and the hash is the xxh3 of the file content. A stub node
(`indexed == false`) whose content hash is unknown uses the
`0000000000000000` sentinel and has path-based rather than content-based
identity. Nodes outside the root, reachable through resolver aliases or
relative escapes, use their absolute path as the prefix. Because a path may
contain `@`, clients must split on the last delimiter (`rsplit`), not the first.
The legacy dependency edge endpoints remain paths: `from` is the importing
file's absolute path, while `to` is the resolved target's absolute path or an
external package's bare name. `fromNodeId` is always the byte-for-byte
`GraphNode.nodeId` of that importing file. `toKind` is the frozen vocabulary
`path | external`; path targets carry a joinable `toNodeId`, while external
targets omit it.

With no `files` list the cached walk is the universe and may drive deletion from
the index. A supplied `files` list is only a query view and revalidation set; it
never removes other files from the shared per-root index. Dependency, reverse
dependency, and neighborhood starts must be inside the view. Their traversals
still use the shared graph, but returned file nodes, importers, and edge
endpoints are confined to the view. External-package edges from in-view owners
remain visible because their targets are not files. Encountering an out-of-view
endpoint or verified-rdeps grep candidate downgrades the result to
`Approximate`; repair skips such candidates before file loading, parsing,
budget accounting, or graph publication.

All eight operations share one `Request::Graph(GraphParams)` wire variant.
`GraphOp` is externally tagged and the transport carries no protocol version,
so an unknown operation tag makes an older daemon fail the request decode and
drop the connection; adding an operation therefore requires upgrading and
restarting the daemon before new clients use it. Compatibility is asymmetric: an old CLI continues to send its known
requests to a new daemon, while a newer CLI whose daemon exchange fails falls
back to the existing cold inline path.

Dependency prefetch intentionally adds neither a ninth `GraphOp` nor any
`Request`/`Response` variant. It is an in-process integration API only, so the
daemon protocol and CLI surface do not change.

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
* `invalidateRoot(root)` — everything beneath a directory when the caller has
  an independently enforced write set.
* `invalidate(path, recursive, scope)` — the scoped form.
* `clearCaches()` — every file/walk entry plus resident tool extensions,
  watcher state, graph roots, compiled matchers, and warm shells.

A dispatched unrestricted `bash` call automatically clears all
filesystem-derived state, including graph roots. Cwd-only invalidation is not
sound because a command can leave cwd, use absolute paths, and follow symlinks.

Cost is proportional to the number of *cached* entries, not to the size of the
tree on disk, so it stays bounded by the cache's own entry cap.

With `trustCache` off (the default), every warm hit still stats, and none of this
is needed for correctness — only for latency.

`InvalidationLog` is the bounded, revisioned journal for derived graph state.
Watcher events, `invalidatePath`, non-recursive `invalidate`, and Hearth
mutations record exact paths; root/recursive invalidation, `clearCaches`, and
an unexpandable watcher event record a conservative wipe. Ring overflow does
not record anything itself — it advances the eviction boundary so lagging
consumers behind it receive a conservative full-discard signal. A
graph sweep consumes entries since its saved revision and drops the matching
stat records. A wipe makes it discard every stat record and revalidate the
whole selected universe, so missing history cannot leave a stale graph entry
trusted.

## Security and authority model

The daemon is a same-UID performance component, not a privilege, tenant, or
workspace boundary. Its single predictable endpoint per UID is deliberate: one
warm engine can serve that user's repositories. Consequently any accepted
client can request operations against every path and command available to that
UID. The daemon must not run as root, a more privileged service identity, or a
multi-tenant/shared service. Endpoint DAC and peer credentials exclude other
UIDs; they do not defend against a compromised same-UID process.

An LLM or LLM-controlled adapter is an untrusted protocol-input source even
when the model is benign: extreme sizes, invented paths, long Bash commands,
parallel bursts, and non-idempotent retry are expected failure modes. The
adapter, not this warm-cache engine, owns least-authority policy: allowed
lexical roots and resolved symlink targets, operation grants, environment
allowlists, budgets, and human approval. Direct unrestricted daemon access is
equivalent to the OS user's read/write/execute authority.

The CLI's delivery contract is at-most-once. It may fall back inline only before
any request byte could have reached the daemon. A later transport failure is
indeterminate and is never replayed. FD-streamed Read can have emitted a valid
partial prefix before such a failure; the contract is non-duplication, not
atomic stdout.

## Surfaces

* **Daemon/CLI**: length-prefixed msgpack (`transport.rs`) over a Unix socket.
  Synchronous request→response, one thread per connection (hard default ceiling
  64), engine shared by `Arc` clone. A frame is at most 256 MiB. The default
  endpoint lives in an euid-owned mode-0700 runtime directory; both sides verify
  the peer UID. A lifetime lock and socket dev+ino protect stale cleanup. The
  daemon reads each request with `recvmsg` so a client can
  attach its stdout fd via `SCM_RIGHTS`; for a `read` the daemon then writes the
  cached content **straight to that fd**, skipping payload serialization
  entirely. (This makes CLI `read` as fast as the client's own startup floor —
  which, for a daemon-client, is still ~0.7 ms above a tiny tool like `cat`; the
  read speed win therefore lives in-process, not at the CLI.)
* **napi**: concrete `#[napi(object)]` types at the boundary — the generated
  `index.d.ts` describes the real shapes, with no `any` on any tool method.
  Sync methods run on the JS thread; every `*Async` twin offloads to a libuv
  worker via `AsyncTask` (no embedded tokio) and takes an optional `AbortSignal`.
  This includes the integration-only `graphPrefetch`/`graphPrefetchAsync` pair;
  its numeric reduction limits are validated as non-negative JavaScript safe
  integers before the native hard caps are applied.
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
* **Global filesystem-derived invalidation after unrestricted `bash`** — a
  command can leave cwd, use absolute paths, or follow symlinks, so cwd-only
  invalidation is unsound. A narrower write set requires a future enforced
  sandbox/declared-write protocol.
* **Tracked process groups, not a portable descendant sandbox** — timeout and
  shutdown terminate/reap groups Hearth owns. A descendant that deliberately
  double-forks and creates a new session can escape POSIX process-group
  tracking; preventing that requires an external sandbox/service manager.
* **Atomic received-FD CLOEXEC where the OS supports it** — Linux and supported
  BSDs use `MSG_CMSG_CLOEXEC`. macOS requires a post-`recvmsg`
  `fcntl(F_SETFD)`, leaving a short race with concurrent fork/exec. This is a
  residual same-UID limitation, not a strict descriptor-inheritance boundary.
* **One engine per process, not shared across processes** — the caches are
  in-process memory; sharing them would mean re-introducing the daemon's IPC cost
  on the path where Hearth is fastest.
* **Externally-tagged transport enums** — internally-tagged enums don't
  round-trip through `rmp-serde`.

## Default safety limits

| Resource | Default/hard maximum |
|---|---:|
| Daemon connections | 64 default; 1,024 maximum (`--max-connections`) |
| Request/response frame | 256 MiB; 30 s receive deadline; 1,000,000 MessagePack values / depth 64 |
| Aggregate admitted frame reservation | 512 MiB default; 4 GiB maximum |
| Shutdown drain | 5 s default, 60 s maximum |
| Bash timeout | 120 s default, 24 h maximum |
| Bash collected/streamed output | 16 MiB total; excess drained and discarded |
| File cache | 65,536 entries, 1 GiB hard max including reserved line-index heap |
| Tool read/edit file and rewritten result | 64 MiB |
| Walk cache | 64 snapshots; 1,000,000 visited/files and 256 MiB path bytes; 16 MiB root-local ignore files |
| Grep matcher/glob cache | 256 entries per cache |
| Grep content/result bytes | 16 MiB per file; 4 MiB aggregate result |
| Grep pattern/globs/context/matches | 1 MiB / 256×16 KiB / 10,000 / 1,000,000 |
| Graph files/path/query/depth/results | 100,000 / 64 KiB / 1 MiB / 64 / 100,000 |
| Graph build/resident roots | 2 concurrent builds; 256 MiB build estimate; 16 roots / 512 MiB resident estimate |
| Graph prefetch | 32 seeds; 64 imports/seed; 256 unique direct targets; 2 MiB/file; 16 MiB total source |

Walks intentionally honor only regular root-local `.ignore` and `.rgignore` files. Ancestor/global Git ignore discovery and `.git/info/exclude` are disabled because they escape the bounded root. JavaScript resolver config reads are same-FD, regular-file-only, and capped at 1 MiB; automatic tsconfig project-reference fan-out is disabled.
