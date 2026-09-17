# v0.3 feasibility probe P4: Python project-wide diagnostics with Pyright

Evidence-only probe, no production code. Run 2026-09-17 on branch `probe/v03-python`
(worktree `agent-ide-worktrees/v03-probe-python`, base `90dda98`,
`claude/v0-2-release-prep-69807e`). Harness: `probes/v03/python/run.sh`, raw artifacts in
`probes/v03/python/out/` (gitignored).

- Machine: Apple M1 Max, macOS 27.0, 32 GB RAM.
- Toolchain (accepted product route, `docs/macos-acceptance.md`): `pyright` CLI 1.1.413 next to
  `pyright-langserver` (`/opt/homebrew/bin`), pinned node v24.4.0 (nvm). Both pinned via
  `AGENT_IDE_NODE` / `AGENT_IDE_PYRIGHT` in `run.sh` with the same defaults the acceptance route uses.
- Subjects: `probes/v03/python/fixture` (5 analyzed files) and `/Users/pluto/projects/traider-lab`
  (READ-ONLY, branch `codex/t11b-parallel-backfill`, strict mode, 380 files analyzed; output went to
  this worktree, `git status --porcelain` there stayed empty).
- Wall/RSS: `/usr/bin/time -l`; on macOS the max-RSS figure is bytes, converted to MB here.

## Measurements

| Run | Subject | Wall | Max RSS | Exit | generalDiagnostics = summary | Files |
|---|---|---|---|---|---|---|
| clean | fixture/clean | 0.35 s | 144 MB | 0 | 0 = 0/0 | 1 |
| fixture_cold | fixture | 0.34 s | 151 MB | 1 | e=2 w=2 = errors=2 warnings=2 | 5 |
| fixture_repeat | fixture | 0.33 s | 152 MB | 1 | e=2 w=2 = 2/2 | 5 |
| fixture_edit | fixture copy +1 error line in a.py | 0.33 s | 150 MB | 1 | e=3 w=2 = 3/2 | 5 |
| traider_cold | traider-lab (strict) | 8.55 s | 1135 MB | 1 | e=44602 w=1 = 44602/1 | 380 |
| traider_repeat | traider-lab (strict) | 7.98 s | 1140 MB | 1 | e=44602 w=1 = 44602/1 | 380 |
| badconfig (invalid JSON) | fixture/badconfig | — | — | **3** | fatal | — |
| badenum (`typeCheckingMode: "bogus"`) | fixture/badenum | — | — | **0** | soft: stderr message, run continues | — |
| LSP warm (lsp_warm.mjs) | fixture | — | — | — | handshake 107 ms; first publishDiagnostics 309 ms; stale c.py error 318 ms | — |

## Answers

### Q1 — JSON counts, never-opened files, exit codes — works

- `generalDiagnostics` severity counts match `summary` exactly in every run (e.g. fixture
  `error=2 warning=2` = `errorCount=2 warningCount=2`). Note the fixture's
  `definitely_not_installed_thirdparty` import legitimately produces two diagnostics: an
  error (`reportMissingImports`) plus the config-forced warning (`reportUnusedImport`).
- The error in the never-opened file is included: `c.py` (never opened in any editor)
  reports `reportCallIssue: Expected 2 positional arguments` in the CLI run.
- Exit codes: clean `fixture/clean` → `0`; diagnostics → `1`; unparseable `pyrightconfig.json`
  → `3` with `Config file ... could not be parsed` on stderr. Fatal is distinguishable.
- Caveat: an *invalid enum value* (`typeCheckingMode: "bogus"`) is soft — message on stderr,
  run continues with defaults, exit code unaffected (stays `0` with zero files). Exit code
  alone does not surface this class of config mistake; stderr must be inspected.

### Q2 — Timings, memory, cache — works, with cost numbers

- Fixture (5 files): ~0.33–0.35 s wall, ~150 MB RSS, identical cold vs repeat.
- traider-lab (380 files, strict): 8.55 s cold / 7.98 s repeat, 1135/1140 MB RSS. The delta is
  noise, not cache.
- No on-disk cache: after marker-timestamped runs, `find` over the project, `~/Library/Caches`
  and `TMPDIR` shows zero created/modified files (`out/cache_check.txt`), and cold vs repeat
  times are flat. As expected, every run — including the post-edit run — pays the full cost.
- Post-edit rerun (`+1` type error in a.py): 0.33 s, error count `2 → 3`, i.e. refresh cost
  equals a full rerun on any project size.

### Q3 — Side-effect sentinel — works

`sentinel.py` (module-level `write_text` into `fixture/sentinel_written.txt`) is parsed and
analyzed (it is inside `include`), but the sentinel file is absent after all runs: Pyright
never executes project code.

### Q4 — Missing third-party import without network — works, with an environment caveat

The `reportMissingImports` diagnostic for `definitely_not_installed_thirdparty` appears in the
same 0.34 s run, no hang. A `--verbose` run contains zero mentions of install/download/fetch/http.
Caveat recorded honestly: this sandbox turned out to have network access (`curl -m 3
https://pypi.org` → exit 0), so the strictly closed-network condition was not exercised — but
Pyright CLI has no installer integration at all; import resolution is purely local
(`.venv`/`site-packages`/stubs), so a closed network cannot change this outcome.

### Q5 — Comparison with the warm LSP route — done by direct probe, not via ignored tests

The existing ignored product tests
(`tests/product_mcp_contract.rs::configured_product_returns_real_pyright_semantic_context_and_reaps`
and friends) require the accepted env plus a fresh `cargo` build of this worktree's base — that
alone exceeds this probe's budget, so they were skipped. Instead `probes/v03/python/lsp_warm.mjs`
spawns the pinned `pyright-langserver --stdio`, handshakes, and opens fixture files:

- initialize handshake: 107 ms; first `publishDiagnostics` for a.py: 309 ms;
- the stale `c.py` error is published only after its `didOpen` (at +318 ms) — an unopened file is
  invisible to the LSP route, which is exactly the gap a project-problem counter must close.

On a 5-file fixture the CLI full-project run (0.34 s) costs about the same as LSP
time-to-first-diagnostics; the LSP amortization only pays off on larger projects (traider-lab:
8 s per full CLI rerun vs a warm server already holding the project).

## Verdicts

| Question | Verdict |
|---|---|
| 1. JSON counts / never-opened / exit codes | works (fatal = exit 3; soft enum misconfig exits 0 — check stderr) |
| 2. Timings, RSS, no cache | works; no cache, full cost per rerun |
| 3. Sentinel (no code execution) | works |
| 4. Missing import, no network | works (network closure itself not exercised — sandbox had network; resolution is local-only) |
| 5. Warm LSP comparison | partial — measured via direct LSP probe instead of ignored tests (build cost out of budget) |

## Recommendation for the v0.3 Python project-problem counter

- Use `pyright --outputjson --project <cfg>` under the pinned node as the project-wide counter:
  counts and exit codes are machine-readable and reliable, never-opened files are covered, the
  target tree is neither written to nor executed.
- Refresh strategy — hybrid, **not** incremental (incremental does not exist for the CLI):
  - warm LSP session for opened-file diagnostics (first publish ~0.3 s, then instant);
  - full CLI rerun for the project counter, debounced (on save/save-all or a few-second
    quiet period). Small projects: ~0.3 s, negligible. Mid-size strict repos like traider-lab:
    ~8 s and ~1.1–1.2 GB RSS per rerun — acceptable on demand, not per keystroke.
- Treat exit `3` (fatal config) as a distinct "counter unavailable" state, and surface stderr:
  a bad enum value keeps exit `0`. Never merge exit `0` with "zero problems" without checking
  `filesAnalyzed > 0`.
- Memory-guard large repos: budget ~3 MB RSS per analyzed file under strict mode (380 files →
  1.1 GB), or run the counter with a lighter `typeCheckingMode` than the editor LSP uses.
