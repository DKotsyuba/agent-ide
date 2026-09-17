# v0.3 probe: Rust project-wide diagnostics + build-cache inheritance in a new worktree

Evidence probe (no production code). Run: 2026-09-17, all stages reproducible via
`probes/v03/rust/run.sh <stage>`; raw cargo JSON / timing logs in `probes/v03/rust/artifacts/`
(gitignored). Fixture: `probes/v03/rust/fixture/` — workspace `probe-a` (lib, carries one
unused-variable warning), `probe-b` (depends on a; `src/broken.rs` has a deliberate E0308 in a
file no editor has open), `probe-c` (bin, depends on a+b). Second subject: this repo
(agent-ide, 165 check units incl. 26 integration-test targets), worktree base 90dda98.

## Environment

| item | value |
|---|---|
| machine | Apple M1 Max, 10 cores, 32 GiB, macOS 27.0, APFS |
| rustc / cargo | 1.98.1 (48a229cea / 797e8a9bc) |
| rust-analyzer | 1.98.1 pinned via `AGENT_IDE_RUST_ANALYZER` (see docs/macos-acceptance.md) |
| network | `CARGO_NET_OFFLINE=true` on every cargo run; deps served from local registry cache |
| RSS caveat | `/usr/bin/time -l` max-RSS covers the direct child; for cargo runs rustc grandchildren are under-reported (cargo itself ~0.6–0.9 GB). rust-analyzer ran directly under `time`, so its RSS is accurate. |

## Q1 — project-wide counts

Command: `cargo check --workspace --all-targets --message-format=json --keep-going --offline [--locked]`,
stream parsed for top-level `compiler-message` (`message.level`), `compiler-artifact.fresh`,
terminal `build-finished`.

| run | wall | errors | warnings | fresh | rebuilt | build-finished |
|---|---|---|---|---|---|---|
| fixture cold (no target, no lock yet) | 0.69 s | 2 | 2 | 0 | 2 | success=false |
| fixture warm `--locked` (no changes) | — | 2 | 2 | 2 | 0 | success=false |
| fixture after one-line edit in a | 0.61 s | 2 | 2 | 0 | 2 | success=false |
| agent-ide cold (empty target) | 18.81 s | 0 | 0 | 0 | 165 | success=true |
| agent-ide warm | 0.10 s | 0 | 0 | 165 | 0 | success=true |
| agent-ide after one-line edit in src/lib.rs | 1.45 s | 0 | 0 | 135 | 30 | success=true |

Findings:
- **Diagnostics are duplicated per unit.** The single E0308 (`crates/b/src/broken.rs:3`) appears
  twice (`probe-b (lib)` and `probe-b (lib test)`), the warning likewise — `--all-targets` checks
  the lib twice. A counter must dedupe by `(code, primary span path:line:col, message)`.
- **Coverage is partial on failure, and dependents are silent.** With `probe-b` failing,
  `probe-c` (bin depending on b) produces zero `compiler-message` and zero `compiler-artifact`
  events — it is neither clean nor reported. `--keep-going` let unrelated units finish (a's
  units still built). Coverage must be computed as: crates with artifacts/messages ⊊ workspace,
  remainder = `not-checked` (transitive dependents of failures).
- **`build-finished` terminal message is present** with `success` true/false — reliable end marker.
- **Warm runs replay cached diagnostics without invoking rustc**: the unchanged failing fixture
  re-emits the same 2 errors + 2 warnings with `fresh=2/rebuild=0`. A whole-workspace diagnostic
  sweep on agent-ide costs 0.10 s warm — cheap enough to run on demand, independent of editors
  (files no editor has open are covered).
Verdict: **works**, with mandatory dedupe + explicit `not-checked` bucket.

## Q2 — inheriting the build cache in a new worktree

Second worktree of the same commit; `target` transferred with APFS clonefile `cp -c -R`.

| run | wall | fresh | rebuilt | notes |
|---|---|---|---|---|
| fixture wt2, empty target | 0.23 s | 0 | 2 | cold reference (lockfile committed → faster than wt1 cold) |
| fixture wt2, cloned target | 0.07 s | 2 | 0 | member crate `probe-a` fully **fresh** in the new path |
| fixture wt2, cloned target, wt1 renamed away | 0.07 s | 2 | 0 | freshness is self-contained; stale absolute paths don't poison the clone |
| agent-ide wt2, empty target | 24.91 s | 0 | 165 | cold reference |
| agent-ide wt2, cloned target | 7.29 s | 135 | 30 | all registry deps + their build scripts fresh; only agent-ide's own units rebuild (lib, bin, 26 integration tests) |

Cache-clone cost (CoW confirmed):
- fixture target: `du` 764 KiB, `df` free-space delta after `cp -c -R` = 68 KiB (metadata only).
- agent-ide target: `du` 610 MB, copy wall 0.22 s, `df` delta 872 KiB (≈0.14 %).

Findings:
- Dependency crates inherit perfectly through a CoW-cloned `target`; the path change does not
  invalidate them (fixture proves it even with the source worktree removed).
- The workspace's **own** units recompiled in the new worktree (30 units, 7.29 s). Not a pure
  path effect — in the virtual-manifest fixture the member stayed fresh — but the root package's
  units (lib/bin/tests) rebuilt despite identical content. Mechanism not isolated within the
  time-box; treat "own units recompile in a fresh worktree" as the expected behavior
  (checkout refreshes source mtimes; hash-fallback evidently did not rescue the root package).
- Net effect: 24.9 s → 7.3 s (−71 %) for a new worktree on this repo, at ~0 disk cost.
- Caveat: measured same-commit clone. Cloning a `target` built from a different branch
  (divergent Cargo.lock) is unmeasured; dep-version mismatches will rebuild that subset.
Verdict: **works** (deps free; own units recompile; never hardlink-share one `target` between
worktrees — clone it).

## Q3 — rust-analyzer priming cost

Pinned binary: `~/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rust-analyzer`
(`AGENT_IDE_RUST_ANALYZER`; product configuration string
`cache-priming-and-proc-macro-disabled-v1` in tests/product_mcp_contract.rs).

| metric | value |
|---|---|
| `rust-analyzer analysis-stats .` on agent-ide worktree | 16.68 s wall |
| peak RSS (direct child, accurate) | 2 279 587 840 B ≈ 2.17 GiB |
| crates loaded | 162 |
| new cache dirs after run (~/Library/Caches, ~/.cache) | none |

rust-analyzer keeps its whole index in memory (salsa DB); nothing is persisted to disk — the
before/after cache-dir sweep showed no writes. Every new LSP session re-pays the full
~17 s + 2.2 GiB load; a prior `cargo check` priming does not warm RA's own representation.
Verdict: **works** as a cost model; one-shot per session, no disk cache to inherit.

## Q4 — network closed

`CARGO_NET_OFFLINE=true cargo check` in a fixture depending on an unfetched crate
(`zzz-v03-probe-unfetched = "1.0"`): fails in **0.23 s** with
`error: no matching package named 'zzz-v03-probe-unfetched' found` — immediate, clear, no hang.
Verdict: **works**.

## Recommendations

1. **Rust project-problem counter**: run
   `cargo check --workspace --all-targets --message-format=json --keep-going --offline`
   (`--locked` when Cargo.lock exists) and parse the JSON stream. Dedupe diagnostics by
   `(code, primary-span path:line:col)`; bucket workspace crates with no `compiler-artifact`/no
   message as `not-checked` when `build-finished.success=false`; use `build-finished` as the
   stream terminator. Warm replay makes on-demand sweeps ~0.1–0.7 s — the counter does not need
   to live inside an editor session to see the whole workspace.
2. **Worktree cache inheritance**: when creating a worktree, CoW-clone the sibling checkout's
   `target` (`cp -c -R`): instant, ~zero disk, deps stay fresh, only workspace-own units
   recompile (7.3 s here vs 24.9 s cold). Do not share one `target` directory via hardlinks or
   `CARGO_TARGET_DIR` across concurrently-used worktrees — clone per worktree. Re-measure
   cross-branch clones (Cargo.lock divergence) before relying on them.
