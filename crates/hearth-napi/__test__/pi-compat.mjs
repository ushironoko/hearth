// Differential test: run the same edit fixtures through pi 0.80.7's own
// `edit-diff.js` and through Hearth's `editBatch`, and require the resulting
// file bytes (and the failure classification) to agree.
//
// pi is not a dependency of this package — it is the *consumer*. When it is not
// installed the suite skips, so CI stays green without it while a developer who
// has pi gets a real compatibility check against the exact version they run.
//
// Point it at a specific install with PI_PACKAGE_ROOT=/path/to/pi-coding-agent.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { errorKind, run, suite, tempDir, test, throws } from "./harness.mjs";

const entry = process.env.HEARTH_ENTRY ?? new URL("../index.js", import.meta.url).href;
const { HearthEngine } = await import(entry);

/** Package roots to look for pi under, most specific first. */
function piRoots() {
  const roots = [];
  if (process.env.PI_PACKAGE_ROOT) {
    roots.push(process.env.PI_PACKAGE_ROOT);
  }
  try {
    const require = createRequire(import.meta.url);
    roots.push(dirname(require.resolve("@earendil-works/pi-coding-agent/package.json")));
  } catch {
    // Not resolvable from here; the explicit root, if any, still stands.
  }
  return roots;
}

/** Load pi's bundled edit machinery, whose deep path its exports map hides. */
async function loadPiEditDiff(roots) {
  for (const root of roots) {
    try {
      const module = await import(
        pathToFileURL(join(root, "dist/core/tools/edit-diff.js")).href
      );
      return { module, root };
    } catch {
      // Try the next root.
    }
  }
  return undefined;
}

const roots = piRoots();
const loaded = await loadPiEditDiff(roots);
if (!loaded) {
  console.log(
    "pi compatibility: SKIPPED (set PI_PACKAGE_ROOT, or install @earendil-works/pi-coding-agent)",
  );
  process.exit(0);
}

const pi = loaded.module;
const version = (() => {
  try {
    return JSON.parse(readFileSync(join(loaded.root, "package.json"), "utf8")).version;
  } catch {
    return "unknown";
  }
})();

/** pi's own edit pipeline, lifted verbatim from its `edit` tool. */
function piApply(raw, edits, path) {
  const { bom, text: content } = pi.stripBom(raw);
  const originalEnding = pi.detectLineEnding(content);
  const normalized = pi.normalizeToLF(content);
  const { baseContent, newContent } = pi.applyEditsToNormalizedContent(normalized, edits, path);
  return {
    finalContent: bom + pi.restoreLineEndings(newContent, originalEnding),
    firstChangedLine: pi.generateDiffString(baseContent, newContent).firstChangedLine,
  };
}

const fixtures = [
  {
    name: "single exact replacement",
    content: "alpha\nbeta\ngamma\n",
    edits: [{ oldText: "beta", newText: "BETA" }],
  },
  {
    name: "multiple disjoint replacements",
    content: "one\ntwo\nthree\nfour\n",
    edits: [
      { oldText: "one", newText: "ONE" },
      { oldText: "four", newText: "FOUR" },
    ],
  },
  {
    name: "edits are matched against the original, not each other",
    content: "alpha\nbeta\n",
    edits: [
      { oldText: "alpha", newText: "beta" },
      { oldText: "beta", newText: "gamma" },
    ],
  },
  {
    name: "multi-line target",
    content: "head\nfirst\nsecond\ntail\n",
    edits: [{ oldText: "first\nsecond", newText: "merged" }],
  },
  {
    name: "BOM is preserved",
    content: "﻿alpha\nbeta\n",
    edits: [{ oldText: "alpha", newText: "ALPHA" }],
  },
  {
    name: "CRLF is preserved",
    content: "one\r\ntwo\r\nthree\r\n",
    edits: [{ oldText: "two", newText: "TWO" }],
  },
  {
    name: "CRLF with a multi-line target written as LF",
    content: "a\r\nb\r\nc\r\n",
    edits: [{ oldText: "a\nb", newText: "A\nB" }],
  },
  {
    name: "fallback: smart quotes",
    content: "say “hello” there\nuntouched\n",
    edits: [{ oldText: 'say "hello" there', newText: 'say "goodbye" there' }],
  },
  {
    name: "fallback: em dash",
    content: "a — b\nuntouched\n",
    edits: [{ oldText: "a - b", newText: "a - c" }],
  },
  {
    name: "fallback: non-breaking space",
    content: "gap here\nuntouched\n",
    edits: [{ oldText: "gap here", newText: "gap THERE" }],
  },
  {
    name: "fallback: trailing whitespace on the matched line",
    content: "target   \nuntouched   \n",
    edits: [{ oldText: "target", newText: "TARGET" }],
  },
  {
    name: "fallback preserves untouched lines byte for byte",
    content: "keep me   \nsay “hi”\ntail\t\n",
    edits: [{ oldText: 'say "hi"', newText: 'say "bye"' }],
  },
  {
    name: "no trailing newline",
    content: "alpha\nbeta",
    edits: [{ oldText: "beta", newText: "BETA" }],
  },
  { name: "target not found", content: "alpha\n", edits: [{ oldText: "absent", newText: "x" }], fails: true },
  { name: "duplicate target", content: "x\nx\n", edits: [{ oldText: "x", newText: "y" }], fails: true },
  {
    name: "overlapping targets",
    content: "abcdef\n",
    edits: [
      { oldText: "abcd", newText: "1" },
      { oldText: "cdef", newText: "2" },
    ],
    fails: true,
  },
  {
    name: "nested targets",
    content: "abcdef\n",
    edits: [
      { oldText: "abcdef", newText: "1" },
      { oldText: "cd", newText: "2" },
    ],
    fails: true,
  },
  { name: "replacement changes nothing", content: "a\n", edits: [{ oldText: "a", newText: "a" }], fails: true },
  { name: "empty target", content: "a\n", edits: [{ oldText: "", newText: "x" }], fails: true },
];

suite(`pi ${version} edit compatibility`);

for (const fixture of fixtures) {
  test(fixture.name, () => {
    const dir = tempDir("hearth-pi-");
    const engine = new HearthEngine({ cwd: dir, enableOptimizer: false });
    const path = join(dir, "fixture.txt");
    writeFileSync(path, fixture.content);

    let expected;
    let piError;
    try {
      expected = piApply(fixture.content, fixture.edits, path);
    } catch (error) {
      piError = error;
    }

    if (piError) {
      assert.ok(fixture.fails, `pi rejected a fixture marked as succeeding: ${piError.message}`);
      const ours = throws(
        () => engine.editBatch({ path, edits: fixture.edits }),
        "Hearth accepted an edit pi rejected",
      );
      assert.ok(errorKind(ours), "the rejection must carry a typed kind");
      assert.equal(
        readFileSync(path, "utf8"),
        fixture.content,
        "a rejected edit must leave the file untouched",
      );
      return;
    }

    assert.ok(!fixture.fails, "pi accepted a fixture marked as failing");
    const ours = engine.editBatch({ path, edits: fixture.edits });
    assert.equal(
      readFileSync(path, "utf8"),
      expected.finalContent,
      "the written bytes must match pi's byte for byte",
    );
    assert.equal(
      ours.firstChangedLine ?? undefined,
      expected.firstChangedLine,
      "firstChangedLine must agree so a shared diff renders identically",
    );
  });
}

await run(`pi ${version} compatibility`);
