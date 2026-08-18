# `@hearthdev/napi`

Node.js bindings for [Hearth](https://github.com/ushironoko/hearth) — one
resident engine serving `read`, `write`, `edit`, `bash`, `grep`, `find`, and
`graph` from shared
warm caches and a warm shell pool.

Prebuilt binaries ship for macOS (arm64, x64) and Linux (x64, arm64 glibc); no
Rust toolchain is needed to install. The package is ESM-only and exposes named
exports for Node.js 18+ and Bun; CommonJS `require()` is not supported.

```bash
npm install @hearthdev/napi
```

## Usage

```ts
import { HearthEngine } from "@hearthdev/napi";

// Construct one per process and keep it: the caches only pay off while it lives.
const engine = new HearthEngine({ cwd: process.cwd(), warmShell: true });

const file = await engine.readAsync({ path: "src/main.rs" });

const paths = await engine.findAsync({
  pattern: "**/*.rs",
  path: ".",
  limit: 1000,
  excludeGlobs: ["**/node_modules/**", "**/.git/**"],
}); // { paths: ["src/lib.rs", ...], totalMatches, walkCacheHit, ... }

const found = await engine.grepAsync({
  pattern: "TODO",
  path: "src",
  mode: "content",
  maxTotalCount: 100,
});

const edited = await engine.editBatchAsync({
  path: "src/main.rs",
  edits: [
    { oldText: "fn old_name", newText: "fn new_name" },
    { oldText: "old_name()", newText: "new_name()" },
  ],
});

const result = await engine.bashStream(
  { command: "cargo build", timeoutMs: 120_000 },
  (chunk) => process.stdout.write(chunk.text),
);
```

## Cancellation

Every async method takes an optional `AbortSignal`:

```ts
const controller = new AbortController();
const search = engine.grepAsync({ pattern: "needle", path: "." }, controller.signal);
controller.abort();
```

An already-aborted signal rejects before any work starts. An abort mid-flight
stops the native work at its next safe point: `grep` joins every worker,
`find` polls every warm-snapshot candidate (a cold bounded walk completes as one
non-preemptive step),
`bash` kills the command's whole process group, and a file mutation keeps its
per-path lock until its bytes are committed — so when the promise settles,
nothing is still running.

`bash` is the exception to rejecting: an abort or a timeout **resolves** with
`aborted`/`timedOut` set and the partial output intact, so a caller keeps what
it already rendered.

## Errors

Every failure — a synchronous throw or a promise rejection — is a JS `Error`
carrying the structured fields to branch on:

- `code`: the stable kind tag, one of `notFound`, `permission`, `noMatch`,
  `multipleMatches`, `overlap`, `noChange`, `invalidInput`, `timeout`,
  `cancelled`, `indeterminate`, `io`, `internal`
- `editIndex`: the 0-based index of the failing replacement, when one
  `editBatch` edit is at fault
- `path`: the file involved, when one is

The `message` still leads with `"<kind>: "`, but that is presentation — the
properties are the contract. Never parse the message.

`indeterminate` is the one that needs care: it means a mutating command reached
a warm shell and the shell then died before reporting a result. Hearth
guarantees **at-most-once** execution, so it never retries such a command —
and neither should the caller without checking what actually happened.

## Pi `find` adapter

`find` returns deterministic search-root-relative POSIX paths and marks
directories with `/`. It accepts the exclusion list Pi supplies to custom glob
operations before applying count/output limits:

```ts
import { access } from "node:fs/promises";

const operations = {
  exists: async (path: string) => {
    try { await access(path); return true; } catch { return false; }
  },
  glob: async (pattern: string, root: string, options: { ignore: string[]; limit: number }) =>
    (await engine.findAsync({
      pattern,
      path: root,
      limit: options.limit,
      excludeGlobs: options.ignore,
    })).paths,
};
```

Patterns without `/` match basenames at every depth. Slash patterns follow Pi's
full-path `**/` transform; matching is fd-compatible smart-case and an empty
pattern matches all entries. The result reports `limitReached` and
`outputLimitReached` separately and always gives exact `totalMatches`. When the
joined text crosses 50 KiB, `paths` includes that first complete crossing path
so Pi's wrapper detects the overflow and emits its standard warning. Hearth's
ignore discovery is deliberately bounded to root-local `.ignore`/`.rgignore`;
it does not inherit ancestor/global Git configuration like Pi's bundled `fd`.
Exclusions post-filter the shared snapshot, so they do not reduce cold-walk work
or its safety budget. Use `findAsync` in the adapter so a cold walk runs off the
JavaScript event loop. Pi's current custom `glob` hook does not pass its
`AbortSignal` through `options`, so that adapter cannot forward cancellation;
direct `findAsync(params, signal)` callers retain the full cancellation contract.

## Cache coherence

`trustCache: true` skips the per-read freshness `stat`, which is where most of
the warm-read speed comes from. It assumes Hearth is the only writer: changes
made outside it stay cached until invalidated.

```ts
engine.invalidatePath("/abs/path/to/file");  // one file
engine.invalidateRoot("/abs/path/to/dir");   // everything beneath a directory
engine.clearCaches();                        // everything
```

After a shell command, `invalidateRoot(cwd)` is the sound choice: an arbitrary
command can create, delete, rename or rewrite anything under its working
directory. This also refreshes `find`'s warm structural snapshot; without
watching or explicit invalidation, out-of-band file and empty-directory changes
remain absent/present in that snapshot.

## Documentation

- [Architecture](https://github.com/ushironoko/hearth/blob/main/docs/ARCHITECTURE.md)
- [Benchmarks](https://github.com/ushironoko/hearth/blob/main/docs/BENCHMARKS.md)
- [Release process](https://github.com/ushironoko/hearth/blob/main/.claude/skills/release-napi/SKILL.md)

MIT licensed.
