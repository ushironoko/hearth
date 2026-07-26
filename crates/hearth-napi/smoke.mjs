// Smoke test for a packed/installed @hearth/napi: does the addon load, and is
// every tool reachable through the published entry point?
//
// The contract suite covers behaviour; this covers *packaging*, which is why it
// touches every method once and asserts nothing subtle. `HEARTH_ENTRY` points
// it at an installed copy; it defaults to the local build.

import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const entry = process.env.HEARTH_ENTRY ?? new URL("./index.js", import.meta.url).href;
const { HearthEngine } = await import(entry);

const runtime = typeof Bun !== "undefined" ? `bun ${Bun.version}` : `node ${process.version}`;
const dir = mkdtempSync(join(tmpdir(), "hearth-smoke-"));
writeFileSync(join(dir, "a.rs"), "fn main() {}\nlet answer = 42;\n");
writeFileSync(join(dir, "b.txt"), "no code here\n");

const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });

// write / writeFast / writeAsync
assert.equal(engine.write({ path: join(dir, "c.rs"), content: "fn helper() {}\n" }).bytesWritten, 15);
assert.equal(engine.writeFast(join(dir, "d.txt"), "fast\ncontent\n").bytesWritten, 13);
assert.equal((await engine.writeAsync({ path: join(dir, "e.txt"), content: "async\n" })).bytesWritten, 6);

// read / readBytes, cold then warm
assert.equal(engine.read({ path: join(dir, "a.rs") }).totalLines, 2);
assert.equal(engine.read({ path: join(dir, "a.rs") }).cacheHit, true);
assert.equal((await engine.readAsync({ path: join(dir, "a.rs") })).totalLines, 2);
assert.equal(engine.readBytes({ path: join(dir, "d.txt") }).toString(), "fast\ncontent\n");
assert.equal((await engine.readBytesAsync({ path: join(dir, "d.txt") })).toString(), "fast\ncontent\n");

// edit and editBatch
assert.equal(
  engine.edit({ path: join(dir, "a.rs"), oldString: "42", newString: "43" }).replacements,
  1,
);
const batch = await engine.editBatchAsync({
  path: join(dir, "a.rs"),
  edits: [
    { oldText: "fn main", newText: "fn entry" },
    { oldText: "43", newText: "44" },
  ],
});
assert.equal(batch.replacements, 2);
assert.ok(batch.hunks.length >= 1, "a batch edit reports at least one diff hunk");

// grep, sync and async
assert.equal(
  engine.grep({ pattern: "fn ", path: dir, mode: "content", globs: ["*.rs"] }).totalMatches,
  2,
);
const grepped = await engine.grepAsync({
  pattern: "fn ",
  path: dir,
  mode: "filesWithMatches",
  globs: ["*.rs"],
});
assert.equal(grepped.files.length, 2);

// bash, sync / async / streaming
assert.equal(engine.bash({ command: "printf sync" }).stdout, "sync");
assert.equal((await engine.bashAsync({ command: "printf async" })).stdout, "async");
const chunks = [];
const streamed = await engine.bashStream({ command: "printf streamed" }, (c) => chunks.push(c));
assert.equal(streamed.stdout, "streamed");
assert.equal(chunks.map((c) => c.text).join(""), "streamed");

// cancellation reaches the native side
const controller = new AbortController();
controller.abort();
await assert.rejects(() => engine.readAsync({ path: join(dir, "a.rs") }, controller.signal));

// cache invalidation
assert.ok(engine.invalidatePath(join(dir, "a.rs")).filesInvalidated >= 0);
assert.ok(engine.invalidateRoot(dir).walksInvalidated >= 0);
assert.ok(engine.clearCaches().filesInvalidated >= 0);

// profiler surface
engine.enableProfiler();
assert.ok(engine.stats().includes("cache"));

console.log(`hearth napi smoke: OK (${runtime})`);
