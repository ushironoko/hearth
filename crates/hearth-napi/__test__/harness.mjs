// A tiny test runner so the same suite runs unmodified on Node and on Bun,
// without depending on either one's built-in test framework.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const tests = [];
let currentSuite = "";

// One cleanup listener per temp directory, which is more than the default cap.
process.setMaxListeners(0);

export function suite(name) {
  currentSuite = name;
}

export function test(name, fn) {
  tests.push({ name: currentSuite ? `${currentSuite} › ${name}` : name, fn });
}

/** A temp directory that is removed when the process exits. */
export function tempDir(prefix = "hearth-test-") {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  process.on("exit", () => {
    try {
      rmSync(dir, { recursive: true, force: true });
    } catch {
      // Best effort: a leftover temp directory must not fail the run.
    }
  });
  return dir;
}

/** Await `fn()` and return the error it rejected with. */
export async function rejects(fn, message = "expected a rejection") {
  try {
    await fn();
  } catch (error) {
    return error;
  }
  throw new Error(message);
}

/** Call `fn()` and return the error it threw. */
export function throws(fn, message = "expected a throw") {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new Error(message);
}

/**
 * The stable kind tag every Hearth error carries as `Error.code`, on the
 * synchronous and the async path alike. The message also leads with
 * `"<kind>: "`, but that is presentation — the property is the contract.
 */
export function errorKind(error) {
  return error?.code;
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Saturate the CPU with busy worker threads, so a test can observe races that
 * only appear when threads compete for a core.
 *
 * Each worker self-terminates after `budgetMs` even if `stop()` is never
 * reached, so a failing test can never leave the machine pinned.
 */
export async function spinUpLoad({ budgetMs = 30_000 } = {}) {
  const { Worker } = await import("node:worker_threads");
  const os = await import("node:os");
  const count = Math.max(2, os.availableParallelism?.() ?? os.cpus().length);
  const workers = Array.from(
    { length: count },
    () => new Worker(`const t=Date.now(); while(Date.now()-t<${budgetMs});`, { eval: true }),
  );
  // Let them actually start competing before the measurement begins.
  await sleep(100);
  return {
    count,
    stop: async () => {
      await Promise.all(workers.map((w) => w.terminate()));
    },
  };
}

export async function run(label) {
  let failed = 0;
  for (const { name, fn } of tests) {
    try {
      await fn();
      console.log(`  ok   ${name}`);
    } catch (error) {
      failed++;
      console.error(`  FAIL ${name}`);
      console.error(`       ${error?.stack ?? error}`);
    }
  }
  const total = tests.length;
  console.log(`\n${label}: ${total - failed}/${total} passed`);
  if (failed > 0) {
    process.exitCode = 1;
  }
  return failed === 0;
}
