// Fair Bun fs comparison: Hearth napi engine vs Bun's fs (node:fs sync + Bun.file/Bun.write).
// Same fairness rules as the Node harness: sync-vs-sync headline, async as a
// separate reference, naive-but-correct search (no redundant stat/access),
// PATH-SET equality asserted, atomic-write caveat always shown, flat corpus.

import { HearthEngine } from "../../../crates/hearth-napi/index.js";
import { readFileSync, writeFileSync, renameSync, mkdirSync, rmSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import assert from "node:assert";

const ROOT = join(tmpdir(), "hearth-bun-fair");
const CORPUS = join(ROOT, "corpus");
const WORDS = ["engine", "cache", "token", "buffer", "index", "walk", "shard", "vector"];
function fileText(seed, lines) {
  let s = "";
  for (let l = 0; l < lines; l++) {
    const r = (seed * 2654435761 + l * 40503) >>> 0;
    if (l % 37 === 0) s += "// TODO_MATCH revisit\n";
    else s += `    ${WORDS[r % WORDS.length]}_${r % 1000} = ${WORDS[(r >> 3) % WORDS.length]};\n`;
  }
  return s;
}
function genCorpus(n, dirs, lines) {
  rmSync(CORPUS, { recursive: true, force: true });
  for (let d = 0; d < dirs; d++) mkdirSync(join(CORPUS, `d${d}`), { recursive: true });
  const paths = [];
  for (let i = 0; i < n; i++) {
    const p = join(CORPUS, `d${i % dirs}`, `f${i}.rs`);
    writeFileSync(p, fileText(i, lines));
    paths.push(p);
  }
  return paths;
}
const now = () => Number(Bun.nanoseconds()) / 1e6; // ms
function benchSync(fn, { minMs = 800, minIters = 100 } = {}) {
  for (let i = 0; i < 5; i++) fn();
  let iters = 0;
  const t0 = now();
  do { fn(); iters++; } while (now() - t0 < minMs || iters < minIters);
  const el = now() - t0;
  return { opsPerSec: iters / (el / 1000), nsPerOp: (el * 1e6) / iters };
}
async function benchAsync(fn, { minMs = 800, minIters = 50 } = {}) {
  for (let i = 0; i < 5; i++) await fn();
  let iters = 0;
  const t0 = now();
  do { await fn(); iters++; } while (now() - t0 < minMs || iters < minIters);
  const el = now() - t0;
  return { opsPerSec: iters / (el / 1000), nsPerOp: (el * 1e6) / iters };
}
const fmt = (r) => `${r.opsPerSec.toFixed(0).padStart(8)} ops/s`;
function verdict(h, o, label) {
  const ratio = o.nsPerOp / h.nsPerOp;
  const tag = ratio >= 1 ? `WIN ${ratio.toFixed(2)}x` : `LOSS ${(1 / ratio).toFixed(2)}x`;
  console.log(`  ${label.padEnd(34)} hearth ${fmt(h)}  vs  ${fmt(o)}  -> ${tag}`);
  return ratio >= 1;
}
function bunSearch(root, re) {
  const hits = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      if (ent.isDirectory()) stack.push(p);
      else if (ent.isFile() && ent.name.endsWith(".rs") && re.test(readFileSync(p, "utf8"))) hits.push(p);
    }
  }
  return hits.sort();
}

async function main() {
  mkdirSync(ROOT, { recursive: true });
  console.log("Bun", Bun.version, "— generating flat corpus (2000 x 200, no .gitignore)...");
  const paths = genCorpus(2000, 48, 200);
  const MID = paths[0];
  const BIG = join(ROOT, "big.rs");
  writeFileSync(BIG, fileText(99, 40000));
  const eng = new HearthEngine({ cwd: CORPUS, trustCache: true, enableOptimizer: false });
  eng.read({ path: MID });
  eng.read({ path: BIG });
  let wins = 0, total = 0;
  const w = (b) => { total++; if (b) wins++; };

  console.log("\n== READ (sync vs sync: eng.read vs node:fs.readFileSync under Bun) ==");
  assert.strictEqual(eng.read({ path: MID }).content, readFileSync(MID, "utf8"));
  w(verdict(benchSync(() => eng.read({ path: MID }).content.length), benchSync(() => readFileSync(MID, "utf8").length), "readFileSync mid"));
  w(verdict(benchSync(() => eng.read({ path: BIG }).content.length), benchSync(() => readFileSync(BIG, "utf8").length), "readFileSync big"));

  console.log("\n== READ (Bun-native async reference: Bun.file().text()) ==");
  {
    const h = benchSync(() => eng.read({ path: MID }).content.length);
    const b = await benchAsync(async () => (await Bun.file(MID).text()).length);
    console.log(`  Bun.file().text() mid   hearth(sync) ${fmt(h)}  vs  Bun.file ${fmt(b)}`);
  }

  console.log("\n== WRITE (sync) — Hearth is ATOMIC ==");
  {
    const CONTENT = fileText(7, 2000);
    const hp = join(ROOT, "wh.rs"), np = join(ROOT, "wp.rs"), ap = join(ROOT, "wa.rs");
    const h = benchSync(() => eng.write({ path: hp, content: CONTENT }));
    const plain = benchSync(() => writeFileSync(np, CONTENT));
    const atomic = benchSync(() => { writeFileSync(ap + ".tmp", CONTENT); renameSync(ap + ".tmp", ap); });
    assert.strictEqual(readFileSync(hp, "utf8"), CONTENT);
    w(verdict(h, plain, "writeFileSync PLAIN (not atomic)"));
    w(verdict(h, atomic, "writeFileSync ATOMIC (temp+rename)"));
    console.log("  NOTE: Hearth write is atomic + refreshes the warm cache; the ATOMIC row is apples-to-apples.");
  }

  console.log("\n== SEARCH (readdir+readFile+regex; PATH-SET equality asserted) ==");
  {
    const re = /TODO_MATCH/;
    const hpaths = eng.grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.map((f) => f.path).sort();
    const bpaths = bunSearch(CORPUS, re);
    assert.deepStrictEqual(hpaths, bpaths, `path sets must match (${hpaths.length} vs ${bpaths.length})`);
    console.log(`  correctness: ${hpaths.length} matching files, path sets identical`);
    const warm = benchSync(() => eng.grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.length, { minMs: 1500, minIters: 20 });
    const cold = benchSync(() => new HearthEngine({ cwd: CORPUS, enableOptimizer: false }).grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.length, { minMs: 1500, minIters: 8 });
    const bun = benchSync(() => bunSearch(CORPUS, re).length, { minMs: 1500, minIters: 8 });
    w(verdict(warm, bun, "grep WARM vs bun walk"));
    w(verdict(cold, bun, "grep COLD vs bun walk"));
  }
  console.log(`\nSUMMARY: Hearth wins ${wins}/${total} fair cases.`);
  rmSync(ROOT, { recursive: true, force: true });
}
main().catch((e) => { console.error(e); process.exit(1); });
