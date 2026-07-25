// The @hearth/napi contract suite. Runs unmodified on Node 22+ and on Bun.
//
// `HEARTH_ENTRY` points the suite at whatever build is under test — the local
// `index.js` by default, or an installed tarball in CI.

import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdirSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { errorKind, rejects, run, sleep, suite, tempDir, test, throws } from "./harness.mjs";

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
  assert.equal(errorKind(error), "notFound");
  assert.equal(error.code, "notFound", "sync errors also carry the kind as Error.code");
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
  assert.equal(readFileSync(path, "utf8"), "x\nx\n");
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

  const dropped = engine.invalidatePath(path);
  assert.equal(dropped.filesInvalidated, 1);
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

await run(`@hearth/napi contract (${typeof Bun !== "undefined" ? "bun" : "node"})`);
