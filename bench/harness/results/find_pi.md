# Pi default find vs Hearth-backed Pi find

Generated: 2026-08-18T06:14:33.200Z

- Pi package: `@earendil-works/pi-coding-agent@0.84.1`
- Pi implementation: `@earendil-works/pi-coding-agent/dist/core/tools/find.js` (private `createFindToolDefinition().execute`)
- fd: `fd 10.4.2`, SHA-256 `bbd98b652be41796406f9d793a2909a717fd871d0e0b824f72fb85c645ad5366`
- Node: `v24.6.0`
- Platform: `darwin 25.3.0 arm64`, CPU: Apple M4
- Corpus: 3000 tracked files / 48 directories / 200 baseline lines, plus the standard ignored, hidden, binary, and large-file fixtures
- Correctness gate: both implementations returned the same complete `*.rs` path set (3001 paths), selective result details matched, and the resident walk reported a cache hit before timing
- Sampling: 5 warmups, at least 30 iterations and 2000 ms per implementation/scenario

| Scenario | Pi default mean | Hearth fresh mean | Hearth resident mean | Pi / fresh | Pi / resident |
|---|---:|---:|---:|---:|---:|
| selective `d000/*.rs` | 10.36 ms | 14.83 ms | 1.73 ms | 0.70× | 5.98× |
| no-match `missing-*.rs` | 12.87 ms | 17.16 ms | 1.34 ms | 0.75× | 9.61× |
| broad `*.rs`, limit 1000 | 13.17 ms | 15.08 ms | 2.47 ms | 0.87× | 5.33× |

Ratios are Pi latency divided by Hearth latency; values above 1 mean Hearth is faster.

## Distribution and sample counts

| Scenario | Implementation | p50 | p95 | iterations |
|---|---|---:|---:|---:|
| selective `d000/*.rs` | Pi default | 10.19 ms | 11.51 ms | 194 |
| selective `d000/*.rs` | Hearth fresh engine | 14.69 ms | 16.76 ms | 135 |
| selective `d000/*.rs` | Hearth resident | 1.61 ms | 3.02 ms | 1154 |
| no-match `missing-*.rs` | Pi default | 11.66 ms | 19.59 ms | 156 |
| no-match `missing-*.rs` | Hearth fresh engine | 16.99 ms | 23.16 ms | 117 |
| no-match `missing-*.rs` | Hearth resident | 1.31 ms | 1.63 ms | 1494 |
| broad `*.rs`, limit 1000 | Pi default | 12.70 ms | 17.58 ms | 152 |
| broad `*.rs`, limit 1000 | Hearth fresh engine | 14.83 ms | 19.00 ms | 133 |
| broad `*.rs`, limit 1000 | Hearth resident | 2.40 ms | 3.32 ms | 810 |

## Scope and fairness

- This measures Pi's real tool-operation path, not raw `fd`: default execution includes child spawn, line parsing, path relativization, limit detection, and output truncation. The Hearth row goes through the same Pi wrapper and its custom `exists`/`glob` hooks, backed by `findAsync` so cold walks do not block the JavaScript event loop.
- Pi package import, Pi tool discovery/download, Hearth addon import, corpus generation, Pi wrapper construction, and UI rendering are outside the timed region for every row. The fresh row includes only new Hearth engine construction plus execution.
- `Hearth resident` reuses one engine and its walk snapshot. `Hearth fresh engine` constructs a new engine for every operation. All rows still benefit from the operating-system page cache; this is not a disk-cold benchmark.
- Pi's custom `glob` hook does not expose the operation AbortSignal, so the adapter cannot forward cancellation even though direct `findAsync(params, signal)` callers can; async still preserves event-loop responsiveness.
- The broad row intentionally uses Pi's default 1000-result limit. Uncapped path-set equality is checked first because fd does not promise the same capped prefix ordering as Hearth's deterministic raw-path order.
- Hearth computes exact `totalMatches` even though Pi's custom operation consumes only `paths`; the broad Hearth rows therefore do more semantic work than fd's `--max-results` early stop.
- The generated corpus gives both sides identical root-local ignore rules (`.gitignore` plus an equivalent `.ignore`) and Pi custom exclusions for `node_modules` and `.git`. The launcher disables global/system Git ignore configuration and supplies an empty XDG config root.
- Pi and managed-fd version drift fail before timing; the report records the exact fd binary SHA-256 without assuming one cross-platform hash.

## Reproduce

```bash
pnpm bench:pi-find
```

Use `FIND_BENCH_SMOKE=1 pnpm bench:pi-find` for a one-iteration correctness/build smoke. Corpus/sample controls and explicit `PI_PACKAGE_ROOT`, `PI_EXPECTED_VERSION`, or `FD_EXPECTED_VERSION` overrides are available for reviewed baseline changes.
