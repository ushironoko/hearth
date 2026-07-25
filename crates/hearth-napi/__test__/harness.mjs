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

/** The `kind` prefix every Hearth error message leads with. */
export function errorKind(error) {
  const match = /^([a-zA-Z]+):/.exec(error?.message ?? "");
  return match ? match[1] : undefined;
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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
