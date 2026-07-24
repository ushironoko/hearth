// Node smoke test for the Hearth napi binding.
import { HearthEngine } from "./index.js";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import assert from "node:assert";

const dir = mkdtempSync(join(tmpdir(), "hearth-napi-"));
writeFileSync(join(dir, "a.rs"), "fn main() {}\nlet answer = 42;\n");
writeFileSync(join(dir, "b.txt"), "no code here\n");

const eng = new HearthEngine({ cwd: dir, enableOptimizer: false });

// write
const w = eng.write({ path: join(dir, "c.rs"), content: "fn helper() {}\n" });
assert.equal(w.bytesWritten, 15);

// read (warm on 2nd)
const r1 = eng.read({ path: join(dir, "a.rs") });
assert.equal(r1.totalLines, 2);
const r2 = eng.read({ path: join(dir, "a.rs") });
assert.equal(r2.cacheHit, true);

// edit
const e = eng.edit({ path: join(dir, "a.rs"), oldString: "42", newString: "43", replaceAll: false });
assert.equal(e.replacements, 1);

// writeFast (moves content) + readBytes (binary-safe Buffer)
const wf = eng.writeFast(join(dir, "d.txt"), "fast\ncontent\n");
assert.equal(wf.bytesWritten, 13);
const rb = eng.readBytes({ path: join(dir, "d.txt") });
assert.ok(Buffer.isBuffer(rb) && rb.toString("utf8") === "fast\ncontent\n", "readBytes byte-exact");

// grep sync
const g = eng.grep({ pattern: "fn ", path: dir, mode: "content", globs: ["*.rs"] });
assert.equal(g.totalMatches, 2);

// grep async (returns JSON string)
const gaStr = await eng.grepAsync({ pattern: "fn ", path: dir, mode: "filesWithMatches", globs: ["*.rs"] });
const ga = JSON.parse(gaStr);
assert.equal(ga.files.length, 2);
assert.equal(ga.walkCacheHit, true, "async grep should reuse warm walk cache");

// bash async
const baStr = await eng.bashAsync({ command: "echo hi from node" });
const ba = JSON.parse(baStr);
assert.equal(ba.stdout, "hi from node\n");
assert.equal(ba.exitCode, 0);

console.log("napi smoke test: ALL PASS");
console.log("  read.totalLines =", r1.totalLines, " cacheHit(2nd) =", r2.cacheHit);
console.log("  grep.totalMatches =", g.totalMatches);
console.log("  bash.stdout =", JSON.stringify(ba.stdout));
