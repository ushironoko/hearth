// The @hearthdev/napi contract suite. Runs unmodified on Node 22+ and on Bun.
//
// `HEARTH_ENTRY` points the suite at whatever build is under test — the local
// `index.js` by default, or an installed tarball in CI.

import assert from "node:assert/strict";
import { readFileSync, realpathSync, writeFileSync, mkdirSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import {
  errorKind,
  rejects,
  run,
  sleep,
  spinUpLoad,
  suite,
  tempDir,
  test,
  throws,
} from "./harness.mjs";

const entry = process.env.HEARTH_ENTRY ?? new URL("../index.js", import.meta.url).href;
const { HearthEngine } = await import(entry);

const seed = (dir, name, content) => {
  const path = join(dir, name);
  writeFileSync(path, content);
  return path;
};

// ---------------------------------------------------------------------------
suite("read");

test("windows a file and reports cache hits", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.txt", "l1\nl2\nl3\n");

  const whole = engine.read({ path });
  assert.equal(whole.content, "l1\nl2\nl3\n");
  assert.equal(whole.totalLines, 3);
  assert.equal(whole.endsWithNewline, true);
  assert.equal(whole.cacheHit, false);
  assert.equal(engine.read({ path }).cacheHit, true);

  const window = engine.read({ path, offset: 2, limit: 1 });
  assert.equal(window.content, "l2\n");
  assert.equal(window.truncated, true);

  const numbered = engine.read({ path, offset: 2, limit: 1, lineNumbers: true });
  assert.match(numbered.content, /^\s+2\tl2\n$/);
});

test("the split-lines mode reproduces a newline-split window", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "b.txt", "a\nb\nc\n");

  const split = engine.read({ path, lineMode: "splitLines" });
  assert.equal(split.totalLines, "a\nb\nc\n".split("\n").length);

  const window = engine.read({ path, offset: 1, limit: 2, lineMode: "splitLines" });
  assert.equal(window.content, "a\nb\nc\n".split("\n").slice(0, 2).join("\n"));
});

test("readBytes is binary-safe", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = join(dir, "blob.bin");
  const bytes = Buffer.from([0x00, 0xff, 0x10, 0x61, 0x00]);
  writeFileSync(path, bytes);

  const read = engine.readBytes({ path });
  assert.ok(Buffer.isBuffer(read));
  assert.deepEqual([...read], [...bytes]);
  assert.equal(engine.read({ path }).binary, true);
});

test("a missing file reports the notFound kind", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const error = throws(() => engine.read({ path: join(dir, "nope.txt") }));
  assert.equal(error.code, "notFound");
  assert.equal(error.path, join(dir, "nope.txt"), "the failing path is a property");
  assert.match(error.message, /^notFound: /, "the message still leads with the kind");
});

// ---------------------------------------------------------------------------
suite("async + AbortSignal");

test("async methods resolve to the same typed results", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.txt", "hello\n");

  // Warm the cache first so the two calls differ only in how they were made.
  engine.read({ path });
  assert.deepEqual(await engine.readAsync({ path }), engine.read({ path }));
  assert.equal((await engine.readBytesAsync({ path })).toString(), "hello\n");

  const written = await engine.writeAsync({ path: join(dir, "w.txt"), content: "x" });
  assert.equal(written.bytesWritten, 1);
});

test("a pre-aborted signal rejects without doing the work", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const target = join(dir, "never.txt");

  const controller = new AbortController();
  controller.abort();

  const error = await rejects(() =>
    engine.writeAsync({ path: target, content: "should not exist" }, controller.signal),
  );
  assert.equal(errorKind(error), "cancelled");
  assert.throws(() => readFileSync(target), "the write must not have happened");
});

test("aborting mid-flight settles bash with its partial output", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const controller = new AbortController();

  const running = engine.bashAsync(
    { command: "printf started; sleep 30", timeoutMs: 30_000 },
    controller.signal,
  );
  await sleep(300);
  controller.abort();

  const result = await running;
  assert.equal(result.aborted, true);
  assert.equal(result.timedOut, false);
  assert.equal(result.stdout, "started");
});

// ---------------------------------------------------------------------------
suite("bash");

test("streams ordered chunks that reconstruct the result", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const chunks = [];

  const result = await engine.bashStream(
    { command: "printf out; printf err 1>&2; exit 3" },
    (chunk) => chunks.push(chunk),
  );

  assert.equal(result.exitCode, 3);
  assert.equal(result.chunks, chunks.length);
  const text = (channel) =>
    chunks.filter((c) => c.channel === channel).map((c) => c.text).join("");
  assert.equal(text("stdout"), result.stdout);
  assert.equal(text("stderr"), result.stderr);
  const seqs = chunks.map((c) => c.seq);
  assert.deepEqual(seqs, [...seqs].sort((a, b) => a - b));
});

test("a settled promise means every chunk was already delivered", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  // `bashStream` hands chunks to JS without blocking, so the promise must not
  // settle until they have actually run — otherwise a streaming caller loses
  // the tail, and a `collectOutput: false` caller loses everything.
  //
  // The window only opens when the worker thread and the JS thread compete for
  // a core: on an idle machine libuv drains the callback queue before resolving
  // the promise every single time, and the bug is invisible. So this test
  // creates the contention itself, and uses the command shape that reproduced
  // it — one that writes a little and exits immediately, leaving no time for
  // the queue to drain on its own.
  //
  // It is a sampling test, not a proof: a single run catches a regression
  // roughly 90% of the time, which across the CI matrix is decisive. It cannot
  // fail when the code is correct.
  const load = await spinUpLoad();
  try {
    for (let attempt = 0; attempt < 60; attempt++) {
      const chunks = [];
      const result = await engine.bashStream({ command: "printf x" }, (c) => chunks.push(c));
      assert.equal(
        chunks.length,
        result.chunks,
        `attempt ${attempt}: result reported ${result.chunks} chunk(s), the callback saw ${chunks.length}`,
      );
      assert.equal(chunks.map((c) => c.text).join(""), result.stdout, `attempt ${attempt}`);
    }
  } finally {
    await load.stop();
  }
});

test("nothing is delivered twice when output is also collected", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const chunks = [];
  const result = await engine.bashStream({ command: "seq 5000" }, (c) => chunks.push(c));

  const streamed = chunks.map((c) => c.text).join("");
  assert.equal(streamed, result.stdout);
  assert.equal(streamed.split("\n").filter(Boolean).length, 5000, "no duplicated lines");
  assert.deepEqual(
    chunks.map((c) => Number(c.seq)),
    chunks.map((_, i) => i + 1),
    "sequence numbers are dense and start at 1",
  );
});

test("chunks arrive while the command is still running", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  let firstChunkAt;

  const started = Date.now();
  await engine.bashStream({ command: "printf early; sleep 1" }, () => {
    firstChunkAt ??= Date.now();
  });
  const settled = Date.now();

  assert.ok(firstChunkAt !== undefined, "at least one chunk must be delivered");
  assert.ok(
    firstChunkAt - started < settled - started - 300,
    `the first chunk must precede completion (chunk +${firstChunkAt - started}ms, done +${settled - started}ms)`,
  );
});

test("timeout returns partial output rather than throwing it away", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const result = await engine.bashAsync({ command: "printf partial; sleep 30", timeoutMs: 300 });
  assert.equal(result.timedOut, true);
  assert.equal(result.stdout, "partial");
});

test("cwd, env and shell are configurable", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  const withEnv = await engine.bashAsync({
    command: 'printf "%s" "$HEARTH_TEST"',
    env: { HEARTH_TEST: "value" },
  });
  assert.equal(withEnv.stdout, "value");

  const viaStdin = await engine.bashAsync({
    command: "printf stdin-shell",
    shell: { program: "/bin/sh", args: ["-s"], transport: "stdin" },
  });
  assert.equal(viaStdin.stdout, "stdin-shell");
});

// ---------------------------------------------------------------------------
suite("edit");

test("applies disjoint edits atomically and describes the change", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const body = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n") + "\n";
  const path = seed(dir, "big.txt", body);

  const result = await engine.editBatchAsync({
    path,
    edits: [
      { oldText: "line 5", newText: "LINE FIVE" },
      { oldText: "line 15", newText: "LINE FIFTEEN" },
    ],
    returnContent: true,
  });

  assert.equal(result.replacements, 2);
  assert.equal(result.firstChangedLine, 5);
  assert.equal(result.usedNormalizedFallback, false);
  assert.equal(result.hunks.length, 2, "two distant changes are two hunks");
  assert.ok(result.hunks[0].rows.some((r) => r.op === "insert" && r.text === "LINE FIVE"));
  assert.equal(readFileSync(path, "utf8"), result.content);
  assert.ok(result.content.includes("LINE FIVE") && result.content.includes("LINE FIFTEEN"));
});

test("rejects duplicate and overlapping targets without touching the file", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "dup.txt", "x\nx\n");

  const duplicate = throws(() => engine.editBatch({ path, edits: [{ oldText: "x", newText: "y" }] }));
  assert.equal(errorKind(duplicate), "multipleMatches");
  assert.equal(duplicate.editIndex, 0, "the ambiguous edit is identified by index");

  const overlap = throws(() =>
    engine.editBatch({
      path: seed(dir, "ov.txt", "abcdef\n"),
      edits: [
        { oldText: "abcd", newText: "1" },
        { oldText: "cdef", newText: "2" },
      ],
    }),
  );
  assert.equal(errorKind(overlap), "overlap");
  assert.equal(overlap.editIndex, 1, "the later of the overlapping pair is reported");
  assert.equal(readFileSync(path, "utf8"), "x\nx\n");
});

test("sync and async failures carry identical structured fields", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "structured.txt", "alpha\nbeta\n");
  const params = {
    path,
    edits: [
      { oldText: "alpha", newText: "ALPHA" },
      { oldText: "missing", newText: "x" },
    ],
  };

  const syncError = throws(() => engine.editBatch(params));
  const asyncError = await rejects(() => engine.editBatchAsync(params));
  for (const error of [syncError, asyncError]) {
    assert.ok(error instanceof Error);
    assert.equal(error.code, "noMatch");
    assert.equal(error.editIndex, 1, "the failing edit is identified by index");
    assert.equal(error.path, path);
    assert.match(error.message, /^noMatch: /);
  }
  assert.equal(readFileSync(path, "utf8"), "alpha\nbeta\n", "all-or-nothing: file untouched");
});

test("returnOriginalContent hands back the raw pre-edit text, sync and async alike", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  // BOM + CRLF: exactly the representations the normalized `content` field
  // strips, and what `originalContent` must preserve.
  const raw = "﻿one\r\ntwo\r\n";
  const params = (path) => ({
    path,
    edits: [{ oldText: "two", newText: "TWO" }],
    returnContent: true,
    returnOriginalContent: true,
  });

  const syncPath = seed(dir, "raw-sync.txt", raw);
  const syncResult = engine.editBatch(params(syncPath));
  const asyncPath = seed(dir, "raw-async.txt", raw);
  const asyncResult = await engine.editBatchAsync(params(asyncPath));
  for (const result of [syncResult, asyncResult]) {
    assert.equal(result.originalContent, raw);
    assert.equal(result.content, "one\nTWO\n", "content stays normalized");
    assert.equal(result.hadBom, true);
    assert.equal(result.crlf, true);
  }
  // The reconstruction identity the adapter relies on: persisted bytes are
  // derivable from content + hadBom + crlf.
  const persisted =
    (syncResult.hadBom ? "﻿" : "") +
    (syncResult.crlf ? syncResult.content.replaceAll("\n", "\r\n") : syncResult.content);
  assert.equal(readFileSync(syncPath, "utf8"), persisted);

  // Off by default.
  const quiet = seed(dir, "raw-quiet.txt", "a\n");
  const result = engine.editBatch({ path: quiet, edits: [{ oldText: "a", newText: "A" }] });
  assert.equal(result.originalContent, undefined);
});

test("whitespaceOnlyTargetPolicy exactFile allows exactly the whole-file case", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  // Default keeps the rejection, with structured fields.
  const rejected = seed(dir, "ws-default.txt", "   ");
  const error = throws(() =>
    engine.editBatch({ path: rejected, edits: [{ oldText: "   ", newText: "x" }] }),
  );
  assert.equal(error.code, "invalidInput");
  assert.equal(error.editIndex, 0);
  assert.equal(readFileSync(rejected, "utf8"), "   ");

  // Opted in: the issue's motivating case — a file of exactly three spaces.
  const allowed = seed(dir, "ws-exact.txt", "   ");
  const result = await engine.editBatchAsync({
    path: allowed,
    edits: [{ oldText: "   ", newText: "x" }],
    whitespaceOnlyTargetPolicy: "exactFile",
    returnOriginalContent: true,
  });
  assert.equal(result.originalContent, "   ");
  assert.equal(readFileSync(allowed, "utf8"), "x");

  // A partial whitespace target stays unmatched — never a positional guess.
  const partial = seed(dir, "ws-partial.txt", "a   b\n");
  const noMatch = await rejects(() =>
    engine.editBatchAsync({
      path: partial,
      edits: [{ oldText: "   ", newText: "x" }],
      whitespaceOnlyTargetPolicy: "exactFile",
    }),
  );
  assert.equal(noMatch.code, "noMatch");
  assert.equal(noMatch.editIndex, 0);

  // Empty oldText stays invalid even with the policy on.
  const empty = seed(dir, "ws-empty.txt", "a\n");
  const invalid = throws(() =>
    engine.editBatch({
      path: empty,
      edits: [{ oldText: "", newText: "x" }],
      whitespaceOnlyTargetPolicy: "exactFile",
    }),
  );
  assert.equal(invalid.code, "invalidInput");
});

test("preserves BOM and CRLF", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  const bom = seed(dir, "bom.txt", "﻿alpha\nbeta\n");
  const bomResult = engine.editBatch({
    path: bom,
    edits: [{ oldText: "alpha", newText: "ALPHA" }],
  });
  assert.equal(bomResult.hadBom, true);
  assert.equal(readFileSync(bom, "utf8"), "﻿ALPHA\nbeta\n");

  const crlf = seed(dir, "crlf.txt", "one\r\ntwo\r\n");
  const crlfResult = engine.editBatch({
    path: crlf,
    edits: [{ oldText: "two", newText: "TWO" }],
  });
  assert.equal(crlfResult.crlf, true);
  assert.equal(readFileSync(crlf, "utf8"), "one\r\nTWO\r\n");
});

// ---------------------------------------------------------------------------
suite("grep");

test("limits matches globally and deterministically", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  for (let file = 0; file < 6; file++) {
    seed(dir, `f${file}.txt`, Array.from({ length: 5 }, (_, i) => `needle ${file}-${i}`).join("\n") + "\n");
  }

  const first = engine.grep({ pattern: "needle", path: dir, mode: "content", maxTotalCount: 7 });
  assert.equal(first.totalMatches, 7);
  assert.equal(first.limitReached, true);

  const again = engine.grep({ pattern: "needle", path: dir, mode: "content", maxTotalCount: 7 });
  assert.deepEqual(
    again.files.map((f) => [f.path, f.matchCount]),
    first.files.map((f) => [f.path, f.matchCount]),
  );

  const unlimited = await engine.grepAsync({ pattern: "needle", path: dir, mode: "content" });
  assert.equal(unlimited.totalMatches, 30);
  assert.equal(unlimited.limitReached, false);
  assert.equal(unlimited.rootIsDir, true);
});

test("a pre-aborted search rejects", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  seed(dir, "a.txt", "needle\n");

  const controller = new AbortController();
  controller.abort();
  const error = await rejects(() =>
    engine.grepAsync({ pattern: "needle", path: dir }, controller.signal),
  );
  assert.equal(errorKind(error), "cancelled");
});

// ---------------------------------------------------------------------------
suite("find");

test("returns pi-compatible relative paths and reuses the walk cache", async () => {
  const dir = tempDir();
  mkdirSync(join(dir, "src", "nested"), { recursive: true });
  mkdirSync(join(dir, "empty"));
  seed(dir, "root.rs", "");
  seed(dir, ".hidden", "");
  seed(dir, "src/a.rs", "");
  seed(dir, "src/nested/b.rs", "");
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  const first = engine.find({ pattern: "*", path: dir, respectGitignore: false });
  assert.deepEqual(first.paths, [
    ".hidden",
    "empty/",
    "root.rs",
    "src/",
    "src/a.rs",
    "src/nested/",
    "src/nested/b.rs",
  ]);
  assert.equal(first.totalMatches, 7);
  assert.equal(first.walkCacheHit, false);

  const warm = await engine.findAsync({ pattern: "src/*.rs", path: dir, respectGitignore: false });
  assert.deepEqual(warm.paths, ["src/a.rs"]);
  assert.equal(warm.walkCacheHit, true);
  assert.equal(warm.limitReached, false);
  assert.equal(warm.outputLimitReached, false);
});

test("applies pi custom-operation exclusions before limits", () => {
  const dir = tempDir();
  mkdirSync(join(dir, "node_modules", "pkg"), { recursive: true });
  mkdirSync(join(dir, "src"));
  seed(dir, "node_modules/pkg/a.js", "");
  seed(dir, "src/a.js", "");
  seed(dir, "src/b.js", "");
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  const result = engine.find({
    pattern: "*.js",
    path: dir,
    limit: 2,
    respectGitignore: false,
    excludeGlobs: ["**/node_modules/**", "**/.git/**"],
  });
  assert.deepEqual(result.paths, ["src/a.js", "src/b.js"]);
  assert.equal(result.totalMatches, 2);
  assert.equal(result.limitReached, false);
});

test("keeps one complete crossing path so Pi can report its 50 KiB truncation", () => {
  const dir = tempDir();
  for (let i = 0; i < 240; i++) {
    seed(dir, `${String(i).padStart(3, "0")}-${"x".repeat(230)}.txt`, "");
  }
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

  const result = engine.find({ pattern: "*.txt", path: dir, limit: 1000 });
  const joinedBytes = Buffer.byteLength(result.paths.join("\n"));
  const prefixBytes = Buffer.byteLength(result.paths.slice(0, -1).join("\n"));
  assert.equal(result.totalMatches, 240);
  assert.equal(result.outputLimitReached, true);
  assert.ok(prefixBytes <= 50 * 1024);
  assert.ok(joinedBytes > 50 * 1024);
});

test("rejects non-integral or out-of-range limits with a structured error", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  for (const limit of [-1, 1.5, 1_000_001]) {
    const error = throws(() => engine.find({ pattern: "*", path: dir, limit }));
    assert.equal(errorKind(error), "invalidInput");
  }
});

test("a pre-aborted find rejects as cancelled", async () => {
  const dir = tempDir();
  seed(dir, "a", "");
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const controller = new AbortController();
  controller.abort();

  const error = await rejects(() =>
    engine.findAsync({ pattern: "*", path: dir }, controller.signal),
  );
  assert.equal(errorKind(error), "cancelled");
});

// ---------------------------------------------------------------------------
suite("write + cache");

test("writing through a symlink keeps the link", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const target = seed(dir, "target.txt", "old\n");
  const link = join(dir, "link.txt");
  symlinkSync(target, link);

  const result = engine.write({ path: link, content: "new\n" });
  assert.equal(result.followedSymlink, true);
  assert.equal(readFileSync(target, "utf8"), "new\n");
});

test("trustCache serves warm bytes until invalidated", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false, trustCache: true });
  const path = seed(dir, "warm.txt", "before\n");

  assert.equal(engine.read({ path }).content, "before\n");
  writeFileSync(path, "after\n"); // out of band, behind Hearth's back
  assert.equal(engine.read({ path }).content, "before\n");

  const dropped = engine.invalidatePath("warm.txt");
  assert.equal(dropped.filesInvalidated, 1, "relative invalidation resolves against engine cwd");
  assert.equal(engine.read({ path }).content, "after\n");
});

test("invalidateRoot covers what a shell command did", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false, trustCache: true });
  mkdirSync(join(dir, "sub"));
  seed(dir, "sub/a.txt", "marker\n");

  const before = engine.grep({ pattern: "marker", path: dir });
  assert.equal(before.files.length, 1);

  await engine.bashAsync({ command: "printf 'marker\\n' > sub/b.txt" });
  engine.invalidateRoot(dir);

  const after = engine.grep({ pattern: "marker", path: dir });
  assert.equal(after.files.length, 2);

  const cleared = engine.clearCaches();
  assert.ok(cleared.walksInvalidated >= 1);
});

// ---------------------------------------------------------------------------
suite("graph");

test("prefetches only direct dependencies and reports cache and graph outcomes separately", () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const seedPath = seed(
    root,
    "seed.ts",
    'import { direct } from "./direct";\nexport const value = direct;\n',
  );
  const directPath = seed(
    root,
    "direct.ts",
    'import { deep } from "./deep";\nexport const direct = deep;\n',
  );
  const deepPath = seed(root, "deep.ts", "export const deep = 42;\n");
  const params = { root, files: ["seed.ts"] };

  const cold = engine.graphPrefetch(params);
  assert.equal(cold.seedsRequested, 1);
  assert.equal(cold.seedsProcessed, 1);
  assert.equal(cold.seedsIndexed, 1);
  assert.equal(cold.importsExamined, 1);
  assert.equal(cold.targetsDiscovered, 1);
  assert.equal(cold.targetsWarmed, 1);
  assert.equal(cold.cacheHits, 0, "the first file-cache loads are cold");
  assert.equal(cold.graphUpdates, 2, "the seed and direct target update graph state");
  assert.equal(cold.truncated, false);

  const warm = engine.graphPrefetch(params);
  assert.equal(warm.cacheHits, 2, "the seed and direct target reuse cached bytes");
  assert.equal(warm.graphUpdates, 0, "cache reuse does not imply a graph update");
  assert.equal(engine.read({ path: seedPath }).cacheHit, true);
  assert.equal(engine.read({ path: directPath }).cacheHit, true);
  assert.equal(engine.read({ path: deepPath }).cacheHit, false, "depth two stays cold");
});

test("prefetches successfully on a worker thread", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  seed(root, "async.ts", 'import "./target";\n');
  seed(root, "target.ts", "export const target = true;\n");

  const result = await engine.graphPrefetchAsync({ root, files: ["async.ts"] });
  assert.equal(result.seedsIndexed, 1);
  assert.equal(result.targetsWarmed, 1);
  assert.equal(result.graphUpdates, 2);
  assert.equal(result.truncated, false);
});

test("rejects a pre-aborted prefetch without warming its seed", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const seedPath = seed(root, "cancelled.ts", "export const untouched = true;\n");
  const controller = new AbortController();
  controller.abort();

  const error = await rejects(() =>
    engine.graphPrefetchAsync({ root, files: [seedPath] }, controller.signal),
  );
  assert.equal(errorKind(error), "cancelled");
  assert.equal(engine.read({ path: seedPath }).cacheHit, false);
});

test("rejects hostile prefetch limits instead of coercing them", () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const fields = [
    "maxSeeds",
    "maxTargetsPerSeed",
    "maxTargets",
    "maxFileBytes",
    "maxTotalBytes",
  ];
  const hostile = [-1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, 2 ** 53, Number.MAX_VALUE];

  for (const field of fields) {
    for (const value of hostile) {
      const error = throws(() => engine.graphPrefetch({ root, files: [], [field]: value }));
      assert.equal(errorKind(error), "invalidInput", `${field} must reject ${String(value)}`);
    }
  }

  const accepted = engine.graphPrefetch({
    root,
    files: [],
    maxSeeds: Number.MAX_SAFE_INTEGER,
  });
  assert.equal(accepted.seedsRequested, 0, "safe integers reach the native cap logic");
});

test("keeps all graph operations in sync and async parity", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const path = seed(
    root,
    "a.ts",
    'import { beta } from "./b";\nexport function alpha() { return beta(); }\n',
  );
  const dependency = seed(root, "b.ts", "export function beta() { return 42; }\n");
  const outputFields = [
    "symbols",
    "outline",
    "search",
    "definitions",
    "deps",
    "rdeps",
    "neighborhood",
    "status",
  ];
  const operations = [
    {
      field: "status",
      syncMethod: "graphStatus",
      asyncMethod: "graphStatusAsync",
      params: { root },
    },
    {
      field: "symbols",
      syncMethod: "graphSymbols",
      asyncMethod: "graphSymbolsAsync",
      params: { root, path },
    },
    {
      field: "outline",
      syncMethod: "graphOutline",
      asyncMethod: "graphOutlineAsync",
      params: { root, path },
    },
    {
      field: "search",
      syncMethod: "graphSearch",
      asyncMethod: "graphSearchAsync",
      params: { root, query: "alpha", limit: 10 },
    },
    {
      field: "definitions",
      syncMethod: "graphDefinitions",
      asyncMethod: "graphDefinitionsAsync",
      params: { root, name: "alpha", limit: 10 },
    },
    {
      field: "deps",
      syncMethod: "graphDeps",
      asyncMethod: "graphDepsAsync",
      params: { root, path },
    },
    {
      field: "rdeps",
      syncMethod: "graphRdeps",
      asyncMethod: "graphRdepsAsync",
      params: { root, path: dependency, verify: true },
    },
    {
      field: "neighborhood",
      syncMethod: "graphNeighborhood",
      asyncMethod: "graphNeighborhoodAsync",
      params: { root, path, depth: 1 },
    },
  ];

  for (const { field, syncMethod, asyncMethod, params } of operations) {
    const sync = engine[syncMethod]({ ...params });
    const asyncResult = await engine[asyncMethod]({ ...params });
    assert.deepStrictEqual(
      sync[field],
      asyncResult[field],
      `${syncMethod} and ${asyncMethod} return the same ${field} payload`,
    );

    for (const result of [sync, asyncResult]) {
      assert.notEqual(result[field], undefined, `${field} output is present`);
      for (const otherField of outputFields) {
        if (otherField !== field) {
          assert.equal(result[otherField], undefined, `${otherField} output is absent`);
        }
      }
    }
  }
});

test("reports the graph build transition and counters", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.ts", "export function alpha() {}\n");

  const before = engine.graphStatus({ root: dir });
  assert.equal(before.status.built, false);

  engine.graphSymbols({ root: dir, path });
  const after = engine.graphStatus({ root: dir });
  assert.equal(after.status.built, true);
  assert.ok(after.status.universeFiles >= 1);
  assert.ok(after.status.indexedFiles >= 1);
  assert.ok(after.status.symbols >= 1);
  assert.equal(typeof after.status.components, "number");
});

test("limits graph search results", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  seed(
    dir,
    "a.ts",
    "export function matchAlpha() {}\nexport function matchBeta() {}\n",
  );

  const result = engine.graphSearch({ root: dir, query: "match", limit: 1 });
  assert.equal(result.search.symbols.length, 1);
  assert.equal(result.search.limitReached, true);
});

test("revalidates an out-of-band edit", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.ts", "export function alpha() {}\n");

  engine.graphSymbols({ root: dir, path });
  writeFileSync(
    path,
    "export function alpha() {}\nexport function newlyAddedFunction() {}\n",
  );

  const result = engine.graphSymbols({ root: dir, path });
  assert.ok(result.symbols.symbols.some((symbol) => symbol.name === "newlyAddedFunction"));
  assert.ok(result.meta.reindexedFiles >= 1);
});

test("reports a missing graph root", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const missing = join(dir, "missing");
  const error = throws(() =>
    engine.graphSymbols({ root: missing, path: join(missing, "a.ts") }),
  );

  assert.equal(errorKind(error), "notFound");
  assert.equal(typeof error.path, "string");
  assert.ok(error.path.includes(missing));
});

test("rejects a pre-aborted graph query", async () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.ts", "export function alpha() {}\n");
  const controller = new AbortController();
  controller.abort();

  const error = await rejects(() =>
    engine.graphSymbolsAsync({ root: dir, path }, controller.signal),
  );
  assert.equal(errorKind(error), "cancelled");
});

test("uses camelCase counters and lowercase hex node ids", () => {
  const dir = tempDir();
  const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
  const path = seed(dir, "a.ts", "export function alpha() {}\n");
  const result = engine.graphSymbols({ root: dir, path });

  assert.equal(typeof result.meta.universeFiles, "number");
  assert.equal(typeof result.meta.sweepAgeMs, "number");
  const lastAt = result.symbols.nodeId.lastIndexOf("@");
  assert.ok(lastAt >= 0);
  // This is the same wire format as GraphBasisEntry.contentHashHex, which
  // coverage-bearing graph operations produce when includeBasis is true.
  assert.match(result.symbols.nodeId.slice(lastAt), /^@[0-9a-f]{16}$/);
});

test("returns dependency edges in sync and async queries", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const a = seed(
    root,
    "a.ts",
    'import { value } from "./b";\nexport const doubled = value * 2;\n',
  );
  const b = seed(root, "b.ts", "export const value = 21;\n");
  const params = { root, path: a, includeBasis: true };

  const sync = engine.graphDeps(params);
  const asyncResult = await engine.graphDepsAsync(params);
  for (const result of [sync, asyncResult]) {
    const edge = result.deps.edges.find((candidate) => candidate.to === b);
    assert.ok(edge, "a.ts has an edge to b.ts");
    assert.deepEqual(
      Object.keys(edge).sort(),
      [
        "from",
        "fromNodeId",
        "guarantee",
        "kind",
        "line",
        "specifier",
        "to",
        "toKind",
        "toNodeId",
      ],
    );
    assert.equal(edge.from, a);
    assert.equal(edge.from, result.deps.node.path);
    assert.equal(edge.fromNodeId, result.deps.node.nodeId);
    assert.equal(typeof edge.toNodeId, "string");
    assert.equal(edge.toKind, "path");
    assert.equal(edge.specifier, "./b");
    assert.equal(edge.kind, "import");
    assert.equal(edge.line, 1);
    assert.equal(edge.guarantee, "exact");

    const basis = result.deps.coverage.basis.find((entry) => entry.path === a);
    assert.ok(basis);
    assert.deepEqual(Object.keys(basis).sort(), ["contentHashHex", "path"]);
    assert.match(basis.contentHashHex, /^[0-9a-f]{16}$/);
  }
});

test("uses bare package names for external dependency targets", () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const packageDir = join(root, "node_modules", "react");
  mkdirSync(packageDir, { recursive: true });
  seed(packageDir, "package.json", '{"name":"react","main":"index.js"}\n');
  seed(packageDir, "index.js", "export default {};\n");
  const importer = seed(
    root,
    "external.ts",
    'import React from "react";\nexport const element = React;\n',
  );

  const result = engine.graphDeps({ root, path: importer });
  const edge = result.deps.edges.find((candidate) => candidate.specifier === "react");
  assert.ok(edge, "external package import produces an edge");
  assert.equal(edge.fromNodeId, result.deps.node.nodeId);
  assert.equal(edge.to, "react");
  assert.equal(edge.toNodeId, undefined);
  assert.equal(edge.toKind, "external");
  assert.equal(edge.to.startsWith("/"), false);
});

test("returns verified reverse dependencies in sync and async queries", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const a = seed(
    root,
    "a.ts",
    'import { value } from "./b";\nexport const doubled = value * 2;\n',
  );
  const b = seed(root, "b.ts", "export const value = 21;\n");
  const params = { root, path: b, verify: true };

  const sync = engine.graphRdeps(params);
  const asyncResult = await engine.graphRdepsAsync(params);
  for (const result of [sync, asyncResult]) {
    assert.equal(typeof result.rdeps.verified, "boolean");
    assert.equal(result.rdeps.verified, true);
    const importer = result.rdeps.importers.find((entry) => entry.node.path === a);
    assert.ok(importer, "b.ts has a.ts as an importer");
    assert.equal(importer.specifier, "./b");
    assert.equal(importer.line, 1);
    assert.equal(importer.guarantee, "exact");
  }
});

test("files views omit out-of-view reverse dependencies", () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const a = seed(
    root,
    "a.ts",
    'import { value } from "./b";\nexport const doubled = value * 2;\n',
  );
  const b = seed(root, "b.ts", "export const value = 21;\n");

  engine.graphRdeps({ root, path: b, verify: true });
  const result = engine.graphRdeps({ root, path: b, files: [b], verify: true });
  const importer = result.rdeps.importers.find((entry) => entry.node.path === a);

  assert.equal(importer, undefined);
  assert.equal(result.meta.guarantee, "approximate");
});

test("returns neighborhood nodes and edges in sync and async queries", async () => {
  const root = realpathSync(tempDir());
  const engine = new HearthEngine({ cwd: root, enableOptimizer: false });
  const a = seed(
    root,
    "a.ts",
    'import { value } from "./b";\nexport const doubled = value * 2;\n',
  );
  const b = seed(root, "b.ts", "export const value = 21;\n");
  const params = { root, path: a };

  const sync = engine.graphNeighborhood(params);
  const asyncResult = await engine.graphNeighborhoodAsync(params);
  for (const result of [sync, asyncResult]) {
    assert.equal(result.neighborhood.center.path, a);
    assert.deepEqual(
      result.neighborhood.nodes.map((node) => node.path).sort(),
      [a, b].sort(),
    );
    const edge = result.neighborhood.edges.find(
      (candidate) => candidate.from === a && candidate.to === b,
    );
    assert.ok(edge, "the neighborhood connects a.ts to b.ts");
    const fromNode = result.neighborhood.nodes.find((node) => node.path === edge.from);
    const toNode = result.neighborhood.nodes.find((node) => node.path === edge.to);
    assert.equal(edge.fromNodeId, fromNode.nodeId);
    assert.equal(edge.toNodeId, toNode.nodeId);
    assert.equal(edge.toKind, "path");
  }
});

await run(`@hearthdev/napi contract (${typeof Bun !== "undefined" ? "bun" : "node"})`);
