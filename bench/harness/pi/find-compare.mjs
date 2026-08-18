// Benchmark Pi's actual default find tool operation against the same Pi wrapper
// backed by Hearth's documented custom glob adapter.
//
// This intentionally imports Pi's private dist/core/tools/find.js: the baseline
// is the implementation agents execute, including fd spawn, readline parsing,
// relativization, result-limit detection, and 50 KiB truncation. Import/setup,
// UI rendering, and corpus generation are outside the timed region.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  accessSync,
  constants,
  mkdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { access } from "node:fs/promises";
import { arch, cpus, platform, release } from "node:os";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { pathToFileURL } from "node:url";

const EXPECTED_PI_VERSION = process.env.PI_EXPECTED_VERSION ?? "0.84.1";
const EXPECTED_FD_VERSION = process.env.FD_EXPECTED_VERSION ?? "10.4.2";
const CORPUS = resolve(process.env.CORPUS ?? "/tmp/hearth-find-corpus");
const OUT = process.env.OUT ? resolve(process.env.OUT) : undefined;
const SMOKE = process.env.FIND_BENCH_SMOKE === "1";
const MIN_MS = numberEnv("FIND_BENCH_MIN_MS", SMOKE ? 0 : 2_000, 0);
const MIN_ITERS = numberEnv("FIND_BENCH_MIN_ITERS", SMOKE ? 1 : 30, 1);
const WARMUPS = numberEnv("FIND_BENCH_WARMUPS", SMOKE ? 0 : 5, 0);
const NUM_FILES = numberEnv("NUM_FILES", 3_000, 1);
const DIRS = numberEnv("DIRS", 48, 1);
const LINES = numberEnv("LINES", 200, 1);

function numberEnv(name, fallback, minimum) {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  const value = Number(raw);
  if (!Number.isFinite(value) || value < minimum || !Number.isInteger(value)) {
    throw new Error(`${name} must be an integer >= ${minimum}, got ${raw}`);
  }
  return value;
}

function executablePath(name) {
  if (name.includes("/") || isAbsolute(name)) return realpathSync(name);
  for (const dir of (process.env.PATH ?? "").split(delimiter)) {
    if (!dir) continue;
    const candidate = join(dir, name);
    try {
      accessSync(candidate, constants.X_OK);
      return realpathSync(candidate);
    } catch {
      // Keep looking.
    }
  }
  throw new Error(`${name} was not found on PATH`);
}

function resolvePiPackageRoot() {
  if (process.env.PI_PACKAGE_ROOT) return realpathSync(process.env.PI_PACKAGE_ROOT);
  const cli = executablePath(process.env.PI_BIN ?? "pi");
  if (cli.endsWith(join("dist", "cli.js"))) return resolve(dirname(cli), "..");
  throw new Error(
    `cannot derive the Pi package root from ${cli}; set PI_PACKAGE_ROOT to @earendil-works/pi-coding-agent`,
  );
}

const piRoot = resolvePiPackageRoot();
const piPackage = JSON.parse(readFileSync(join(piRoot, "package.json"), "utf8"));
assert.equal(piPackage.name, "@earendil-works/pi-coding-agent");
assert.equal(
  piPackage.version,
  EXPECTED_PI_VERSION,
  `benchmark baseline changed; set PI_EXPECTED_VERSION explicitly after reviewing Pi's find implementation`,
);

const piFindEntry = join(piRoot, "dist", "core", "tools", "find.js");
const piToolsEntry = join(piRoot, "dist", "utils", "tools-manager.js");
const { createFindToolDefinition } = await import(pathToFileURL(piFindEntry).href);
const { ensureTool } = await import(pathToFileURL(piToolsEntry).href);
const fdPath = await ensureTool("fd", true);
assert.ok(fdPath, "Pi could not resolve its managed fd executable");
const fdVersionRun = spawnSync(fdPath, ["--version"], { encoding: "utf8" });
assert.equal(fdVersionRun.status, 0, fdVersionRun.stderr || "fd --version failed");
const fdVersion = fdVersionRun.stdout.trim();
assert.equal(
  fdVersion,
  `fd ${EXPECTED_FD_VERSION}`,
  "benchmark baseline changed; set FD_EXPECTED_VERSION explicitly after reviewing fd behavior",
);
const fdSha256 = createHash("sha256").update(readFileSync(fdPath)).digest("hex");

const hearthEntry = process.env.HEARTH_ENTRY
  ? pathToFileURL(resolve(process.env.HEARTH_ENTRY)).href
  : new URL("../../../crates/hearth-napi/index.js", import.meta.url).href;
const { HearthEngine } = await import(hearthEntry);

const engineOptions = {
  cwd: CORPUS,
  enableWatch: false,
  enableOptimizer: false,
};

async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

function hearthOperations(engine) {
  return {
    exists,
    glob: async (pattern, root, options) =>
      engine.find({
        pattern,
        path: root,
        limit: options.limit,
        excludeGlobs: options.ignore,
      }).paths,
  };
}

function hearthTool(engine) {
  return createFindToolDefinition(CORPUS, { operations: hearthOperations(engine) });
}

const piDefaultTool = createFindToolDefinition(CORPUS);
const residentEngine = new HearthEngine(engineOptions);
const hearthWarmTool = hearthTool(residentEngine);

async function execute(tool, args) {
  return tool.execute("find-benchmark", args, undefined, undefined, undefined);
}

function textOf(result) {
  const text = result.content.find((item) => item.type === "text")?.text;
  assert.equal(typeof text, "string", "find must return one text result");
  return text;
}

function pathsOf(result) {
  const text = textOf(result);
  if (text === "No files found matching pattern") return [];
  const body = text.split("\n\n[", 1)[0];
  return body ? body.split("\n") : [];
}

function sortedPaths(result) {
  return pathsOf(result).sort((a, b) => a.localeCompare(b, "en"));
}

const scenarios = [
  {
    id: "selective-full-path",
    label: "selective `d000/*.rs`",
    args: { pattern: "d000/*.rs", path: CORPUS, limit: 1_000 },
    exactSet: true,
  },
  {
    id: "basename-miss",
    label: "no-match `missing-*.rs`",
    args: { pattern: "missing-*.rs", path: CORPUS, limit: 1_000 },
    exactSet: true,
  },
  {
    id: "broad-default-limit",
    label: "broad `*.rs`, limit 1000",
    args: { pattern: "*.rs", path: CORPUS, limit: 1_000 },
    exactSet: false,
  },
];

async function verifyCorrectness() {
  // First prove both implementations see the same complete universe. The timed
  // broad case keeps Pi's default 1000-result behavior, whose prefix may differ
  // because fd does not promise Hearth's deterministic raw-path ordering.
  const fullArgs = { pattern: "*.rs", path: CORPUS, limit: 10_000 };
  const piFull = await execute(piDefaultTool, fullArgs);
  const hearthFull = await execute(hearthWarmTool, fullArgs);
  assert.equal(
    residentEngine.find({
      ...fullArgs,
      excludeGlobs: ["**/node_modules/**", "**/.git/**"],
    }).walkCacheHit,
    true,
    "resident benchmark must reuse a warm walk snapshot",
  );
  assert.deepEqual(
    sortedPaths(hearthFull),
    sortedPaths(piFull),
    "uncapped *.rs path sets must be identical before timing",
  );
  assert.ok(pathsOf(piFull).length > 1_000, "the broad scenario must actually reach the default limit");

  for (const scenario of scenarios) {
    const pi = await execute(piDefaultTool, scenario.args);
    const hearth = await execute(hearthWarmTool, scenario.args);
    const piPaths = pathsOf(pi);
    const hearthPaths = pathsOf(hearth);

    if (scenario.exactSet) {
      assert.deepEqual(sortedPaths(hearth), sortedPaths(pi), `${scenario.id}: path sets differ`);
      assert.deepEqual(hearth.details, pi.details, `${scenario.id}: result details differ`);
    } else {
      assert.equal(piPaths.length, 1_000, `${scenario.id}: Pi must hit its default limit`);
      assert.equal(hearthPaths.length, 1_000, `${scenario.id}: Hearth must hit Pi's default limit`);
      assert.equal(pi.details?.resultLimitReached, 1_000);
      assert.equal(hearth.details?.resultLimitReached, 1_000);
      assert.ok(piPaths.every((path) => path.endsWith(".rs")));
      assert.ok(hearthPaths.every((path) => path.endsWith(".rs")));
    }
  }

  return pathsOf(piFull).length;
}

let blackhole = 0;
async function measure(fn) {
  for (let i = 0; i < WARMUPS; i++) {
    const result = await fn();
    blackhole ^= textOf(result).length;
  }

  const samples = [];
  let elapsed = 0;
  do {
    const started = performance.now();
    const result = await fn();
    const duration = performance.now() - started;
    blackhole ^= textOf(result).length;
    samples.push(duration);
    elapsed += duration;
  } while (samples.length < MIN_ITERS || elapsed < MIN_MS);

  const ordered = [...samples].sort((a, b) => a - b);
  const meanMs = samples.reduce((sum, value) => sum + value, 0) / samples.length;
  return {
    meanMs,
    p50Ms: percentile(ordered, 0.5),
    p95Ms: percentile(ordered, 0.95),
    iterations: samples.length,
  };
}

function percentile(ordered, quantile) {
  return ordered[Math.min(ordered.length - 1, Math.floor(ordered.length * quantile))];
}

async function benchmarkScenario(scenario, scenarioIndex) {
  const implementations = {
    pi: () => execute(piDefaultTool, scenario.args),
    fresh: () => {
      const engine = new HearthEngine(engineOptions);
      return execute(hearthTool(engine), scenario.args);
    },
    warm: () => execute(hearthWarmTool, scenario.args),
  };
  // Rotate deterministic order between scenarios to avoid giving one
  // implementation the same thermal/order position in every row.
  const rotations = [
    ["pi", "fresh", "warm"],
    ["warm", "pi", "fresh"],
    ["fresh", "warm", "pi"],
  ];
  const stats = {};
  for (const name of rotations[scenarioIndex % rotations.length]) {
    stats[name] = await measure(implementations[name]);
    globalThis.gc?.();
  }
  return { scenario, stats };
}

const matchingPaths = await verifyCorrectness();
const results = [];
for (let i = 0; i < scenarios.length; i++) {
  results.push(await benchmarkScenario(scenarios[i], i));
}

const generatedAt = new Date().toISOString();
const cpu = cpus()[0]?.model ?? "unknown";
const markdown = renderMarkdown({ matchingPaths, results, fdVersion, fdSha256, generatedAt, cpu });
console.log(markdown);
if (OUT) {
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${markdown}\n`);
  console.error(`wrote ${OUT}`);
}
// Keep the sink observable without polluting the report.
if (blackhole === Number.MIN_SAFE_INTEGER) console.error("unreachable", blackhole);

function formatMs(value) {
  if (value < 1) return `${(value * 1_000).toFixed(1)} µs`;
  return `${value.toFixed(2)} ms`;
}

function ratio(pi, hearth) {
  return (pi.meanMs / hearth.meanMs).toFixed(2);
}

function renderMarkdown({ matchingPaths, results, fdVersion, fdSha256, generatedAt, cpu }) {
  const lines = [
    "# Pi default find vs Hearth-backed Pi find",
    "",
    `Generated: ${generatedAt}`,
    "",
    `- Pi package: \`${piPackage.name}@${piPackage.version}\``,
    `- Pi implementation: \`${piPackage.name}/dist/core/tools/find.js\` (private \`createFindToolDefinition().execute\`)`,
    `- fd: \`${fdVersion}\`, SHA-256 \`${fdSha256}\``,
    `- Node: \`${process.version}\``,
    `- Platform: \`${platform()} ${release()} ${arch()}\`, CPU: ${cpu}`,
    `- Corpus: ${NUM_FILES} tracked files / ${DIRS} directories / ${LINES} baseline lines, plus the standard ignored, hidden, binary, and large-file fixtures`,
    `- Correctness gate: both implementations returned the same complete \`*.rs\` path set (${matchingPaths} paths), selective result details matched, and the resident walk reported a cache hit before timing`,
    `- Sampling: ${WARMUPS} warmups, at least ${MIN_ITERS} iterations and ${MIN_MS} ms per implementation/scenario`,
    "",
    "| Scenario | Pi default mean | Hearth fresh mean | Hearth resident mean | Pi / fresh | Pi / resident |",
    "|---|---:|---:|---:|---:|---:|",
  ];
  for (const { scenario, stats } of results) {
    lines.push(
      `| ${scenario.label} | ${formatMs(stats.pi.meanMs)} | ${formatMs(stats.fresh.meanMs)} | ${formatMs(stats.warm.meanMs)} | ${ratio(stats.pi, stats.fresh)}× | ${ratio(stats.pi, stats.warm)}× |`,
    );
  }
  lines.push(
    "",
    "Ratios are Pi latency divided by Hearth latency; values above 1 mean Hearth is faster.",
    "",
    "## Distribution and sample counts",
    "",
    "| Scenario | Implementation | p50 | p95 | iterations |",
    "|---|---|---:|---:|---:|",
  );
  for (const { scenario, stats } of results) {
    for (const [name, label] of [
      ["pi", "Pi default"],
      ["fresh", "Hearth fresh engine"],
      ["warm", "Hearth resident"],
    ]) {
      const stat = stats[name];
      lines.push(
        `| ${scenario.label} | ${label} | ${formatMs(stat.p50Ms)} | ${formatMs(stat.p95Ms)} | ${stat.iterations} |`,
      );
    }
  }
  lines.push(
    "",
    "## Scope and fairness",
    "",
    "- This measures Pi's real tool-operation path, not raw `fd`: default execution includes child spawn, line parsing, path relativization, limit detection, and output truncation. The Hearth row goes through the same Pi wrapper and its custom `exists`/`glob` hooks.",
    "- Pi package import, Pi tool discovery/download, Hearth addon import, corpus generation, and UI rendering are outside the timed region.",
    "- `Hearth resident` reuses one engine and its walk snapshot. `Hearth fresh engine` constructs a new engine for every operation. All rows still benefit from the operating-system page cache; this is not a disk-cold benchmark.",
    "- The broad row intentionally uses Pi's default 1000-result limit. Uncapped path-set equality is checked first because fd does not promise the same capped prefix ordering as Hearth's deterministic raw-path order.",
    "- Hearth computes exact `totalMatches` even though Pi's custom operation consumes only `paths`; the broad Hearth rows therefore do more semantic work than fd's `--max-results` early stop.",
    "- The generated corpus gives both sides identical root-local ignore rules (`.gitignore` plus an equivalent `.ignore`) and Pi custom exclusions for `node_modules` and `.git`. The launcher disables global/system Git ignore configuration and supplies an empty XDG config root.",
    "- Pi and managed-fd version drift fail before timing; the report records the exact fd binary SHA-256 without assuming one cross-platform hash.",
    "",
    "## Reproduce",
    "",
    "```bash",
    "pnpm bench:pi-find",
    "```",
    "",
    "Use `FIND_BENCH_SMOKE=1 pnpm bench:pi-find` for a one-iteration correctness/build smoke. Corpus/sample controls and explicit `PI_PACKAGE_ROOT`, `PI_EXPECTED_VERSION`, or `FD_EXPECTED_VERSION` overrides are available for reviewed baseline changes.",
  );
  return lines.join("\n");
}
