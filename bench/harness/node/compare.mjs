// Fair Node.js fs comparison: Hearth napi engine vs node:fs / node:fs/promises.
//
// Fairness rules (from the cross-model fairness review):
//  * sync is compared to sync; async is reported in a SEPARATE reference table.
//  * the naive search baseline does readdir(withFileTypes) -> ext filter ->
//    readFile -> regex, with NO redundant stat()/access() per file.
//  * search correctness is checked by full PATH-SET equality, not just counts.
//  * write is atomic on the Hearth side, so we show BOTH a plain-writeFile and a
//    both-atomic (temp+rename) Node baseline, and always print the caveat.
//  * both sides are warmed; each case runs to a minimum wall time for stability.
//  * a flat corpus (no .gitignore) is used so both sides see the identical file
//    set — removing any ignore-rule asymmetry.

import { HearthEngine } from "../../../crates/hearth-napi/index.js";
import { readFileSync, writeFileSync, renameSync, mkdirSync, rmSync, readdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { performance } from "node:perf_hooks";
import { tmpdir } from "node:os";
import { join } from "node:path";
import assert from "node:assert";

const ROOT = join(tmpdir(), "hearth-node-fair");
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
function genCorpus(nFiles, dirs, lines) {
  rmSync(CORPUS, { recursive: true, force: true });
  for (let d = 0; d < dirs; d++) mkdirSync(join(CORPUS, `d${d}`), { recursive: true });
  const paths = [];
  for (let i = 0; i < nFiles; i++) {
    const p = join(CORPUS, `d${i % dirs}`, `f${i}.rs`);
    writeFileSync(p, fileText(i, lines));
    paths.push(p);
  }
  return paths;
}

function benchSync(fn, { minMs = 800, minIters = 100 } = {}) {
  for (let i = 0; i < 5; i++) fn();
  let iters = 0;
  const t0 = performance.now();
  do {
    fn();
    iters++;
  } while (performance.now() - t0 < minMs || iters < minIters);
  const el = performance.now() - t0;
  return { opsPerSec: iters / (el / 1000), nsPerOp: (el * 1e6) / iters, iters };
}
async function benchAsync(fn, { minMs = 800, minIters = 50 } = {}) {
  for (let i = 0; i < 5; i++) await fn();
  let iters = 0;
  const t0 = performance.now();
  do {
    await fn();
    iters++;
  } while (performance.now() - t0 < minMs || iters < minIters);
  const el = performance.now() - t0;
  return { opsPerSec: iters / (el / 1000), nsPerOp: (el * 1e6) / iters, iters };
}

const fmt = (r) => `${r.opsPerSec.toFixed(0).padStart(8)} ops/s`;
function verdict(hearth, other, label) {
  const ratio = other.nsPerOp / hearth.nsPerOp;
  const tag = ratio >= 1 ? `WIN ${ratio.toFixed(2)}x` : `LOSS ${(1 / ratio).toFixed(2)}x`;
  console.log(`  ${label.padEnd(34)} hearth ${fmt(hearth)}  vs  ${fmt(other)}  -> ${tag}`);
  return { label, ratio, win: ratio >= 1 };
}

function nodeSearch(root, re) {
  const hits = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      if (ent.isDirectory()) stack.push(p);
      else if (ent.isFile() && ent.name.endsWith(".rs")) {
        if (re.test(readFileSync(p, "utf8"))) hits.push(p);
      }
    }
  }
  return hits.sort();
}

async function main() {
  mkdirSync(ROOT, { recursive: true });
  console.log("Node", process.version, "— generating flat corpus (2000 files x 200 lines, no .gitignore)...");
  const paths = genCorpus(2000, 48, 200);
  const MID = paths[0];
  const BIG = join(ROOT, "big.rs");
  writeFileSync(BIG, fileText(99, 40000));

  const eng = new HearthEngine({ cwd: CORPUS, trustCache: true, enableOptimizer: false });
  eng.read({ path: MID });
  eng.read({ path: BIG });

  const results = [];

  console.log("\n== READ (sync vs sync) — Hearth serves cached bytes; fs re-reads each call ==");
  assert.strictEqual(eng.read({ path: MID }).content, readFileSync(MID, "utf8"));
  results.push(verdict(
    benchSync(() => eng.read({ path: MID }).content.length),
    benchSync(() => readFileSync(MID, "utf8").length),
    "readFileSync mid (~few KB)"
  ));
  results.push(verdict(
    benchSync(() => eng.read({ path: BIG }).content.length),
    benchSync(() => readFileSync(BIG, "utf8").length),
    "readFileSync big (~1.5MB)"
  ));

  console.log("\n== READ (async reference — fs pays a libuv threadpool hop, so listed separately) ==");
  {
    const h = benchSync(() => eng.read({ path: MID }).content.length);
    const n = await benchAsync(async () => (await readFile(MID, "utf8")).length);
    console.log(`  readFile mid            hearth(sync) ${fmt(h)}  vs  fs.readFile(async) ${fmt(n)}`);
  }

  console.log("\n== WRITE (sync) — Hearth is ATOMIC (temp+rename+cache update) ==");
  {
    const CONTENT = fileText(7, 2000);
    const hp = join(ROOT, "w_h.rs"), np = join(ROOT, "w_p.rs"), ap = join(ROOT, "w_a.rs");
    const h = benchSync(() => eng.write({ path: hp, content: CONTENT }));
    const plain = benchSync(() => writeFileSync(np, CONTENT));
    const atomic = benchSync(() => { writeFileSync(ap + ".tmp", CONTENT); renameSync(ap + ".tmp", ap); });
    assert.strictEqual(readFileSync(hp, "utf8"), CONTENT);
    results.push(verdict(h, plain, "writeFileSync PLAIN (not atomic)"));
    results.push(verdict(h, atomic, "writeFileSync ATOMIC (temp+rename)"));
    console.log("  NOTE: Hearth write is atomic + refreshes the warm cache; the ATOMIC row is apples-to-apples.");
  }

  console.log("\n== SEARCH (readdir+readFile+regex; PATH-SET equality asserted) ==");
  {
    const re = /TODO_MATCH/;
    const hpaths = eng.grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.map((f) => f.path).sort();
    const npaths = nodeSearch(CORPUS, re);
    assert.deepStrictEqual(hpaths, npaths, `path sets must match (${hpaths.length} vs ${npaths.length})`);
    console.log(`  correctness: ${hpaths.length} matching files, path sets identical`);
    const warm = benchSync(() => eng.grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.length, { minMs: 1500, minIters: 20 });
    const cold = benchSync(() => new HearthEngine({ cwd: CORPUS, enableOptimizer: false }).grep({ pattern: "TODO_MATCH", path: CORPUS, mode: "filesWithMatches" }).files.length, { minMs: 1500, minIters: 8 });
    const node = benchSync(() => nodeSearch(CORPUS, re).length, { minMs: 1500, minIters: 8 });
    results.push(verdict(warm, node, "grep WARM vs node walk"));
    results.push(verdict(cold, node, "grep COLD vs node walk"));
    console.log("  (this composite is exactly what fs.readdir + fs.stat/access + fs.readFile are used to build;");
    console.log("   Hearth exposes no standalone stat/access/readdir tool, so they appear only as components.)");
  }

  const wins = results.filter((r) => r.win).length;
  console.log(`\nSUMMARY: Hearth wins ${wins}/${results.length} fair cases (see per-row WIN/LOSS above).`);
  rmSync(ROOT, { recursive: true, force: true });
}

main().catch((e) => { console.error(e); process.exit(1); });
