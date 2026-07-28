# `@hearthdev/napi`

Node.js bindings for [Hearth](https://github.com/ushironoko/hearth) — one
resident engine serving `read`, `write`, `edit`, `bash`, and `grep` from shared
warm caches and a warm shell pool.

Prebuilt binaries ship for macOS (arm64, x64) and Linux (x64, arm64 glibc); no
Rust toolchain is needed to install.

```bash
npm install @hearthdev/napi
```

## Usage

```ts
import { HearthEngine } from "@hearthdev/napi";

// Construct one per process and keep it: the caches only pay off while it lives.
const engine = new HearthEngine({ cwd: process.cwd(), warmShell: true });

const file = await engine.readAsync({ path: "src/main.rs" });

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
directory.

## Documentation

- [Architecture](https://github.com/ushironoko/hearth/blob/main/docs/ARCHITECTURE.md)
- [Benchmarks](https://github.com/ushironoko/hearth/blob/main/docs/BENCHMARKS.md)
- [Release process](https://github.com/ushironoko/hearth/blob/main/.claude/skills/release-napi/SKILL.md)

MIT licensed.
