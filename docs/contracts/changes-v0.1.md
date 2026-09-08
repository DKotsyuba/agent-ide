# Bounded Git diff composition

Revision: v0.1-r1. Provider: Changes. Direct consumer: Assistance. Input provider: Workspace. Shared vocabulary: [common](common.md); current raw evidence boundary: [Workspace v0.1](workspace-v0.1.md).

## Responsibility

Changes produces the logical v0.1 `diff` result for the closed modes `head`, `staged`, and `unstaged`. It composes a bounded summary and selected exact Git hunks, with untracked paths listed separately. Assistance owns tool schemas, host delivery, rendering, and cross-channel feedback policy.

Workspace owns authority, Git commands and raw parsing, comparison identities, baseline capture, observations, and owner-scoped detail expansion. Execution physically admits and runs Workspace's commands. Changes never spawns Git, reads a different worktree directly, changes files, or treats a model-supplied identity as authority.

## Required evidence

For the requested current authority, Workspace supplies its opaque worktree reference and incarnation, authority epoch, selected mode, and the exact left/right Git identities applicable to that mode. Each comparison has its own raw bounded stdout/stderr, fixed command kind, exit status, truncation/timing, coverage, and operation reference.

Workspace derives those identities only from complete fixed `HEAD`, index, and selected patch evidence. Its status command remains NUL-delimited porcelain v2; diff commands request complete patches, disable external diff and textconv helpers, and terminate path parsing with `--`. An unborn `HEAD` is explicit rather than an empty identity.

Raw NUL-safe path/status/conflict data and a separate bounded untracked list identify the affected entries. Baseline completeness and provenance are context only: a session snapshot is never substituted for HEAD, index, or working-state comparison. An absent HEAD or unsupported comparison is explicit rather than a fabricated empty comparison.

Workspace supplies currentness and ownership checks before composition and detail expansion. A revoked, stale, unsupported, failed, or incomplete input remains visibly so in the result; Changes never reconstructs a missing patch or declares partial evidence clean.

## Offered result

The result uses the existing bounded envelope: `ready`, `unavailable`, `incomplete`, or `failed`, with scope, freshness, coverage, and provenance. Its payload contains comparison identities, status counts, selected exact hunks, separate untracked/conflict information, overflow limits, and owner-scoped operation/detail references.

Hunk selection respects both byte and hunk-count budgets. Omitted data is counted or marked as unknown when the source itself is truncated. Changes does not silently cut a hunk into a different patch. Binary changes and raw Unix paths remain explicit; safe display escaping must not replace the underlying raw identity.

Further detail is requested through Workspace by operation reference and bounded hunk cursor. A reference grants no new authority, and invalidation is rechecked before expansion. Changes stores no persistent ChangeSet and creates no second reference store.

## Current acceptance

- The three modes keep HEAD/index/working-state identities distinct and never use the session baseline as a replacement.
- Budget overflow preserves selected exact hunks, a bounded summary, and honest omissions; untracked and conflicted entries remain visible.
- Space, newline, leading-dash, and non-UTF-8 paths retain their identities; an unsupported encoding is reported explicitly.
- Stale, revoked, truncated, binary, empty, and failed Git evidence cannot become a clean complete result.
- A real macOS Git fixture proves staged/unstaged separation and the supported raw-path cases through the Workspace boundary.

ChangeSet persistence, semantic enrichment, history, checks, verification, edits, and Scope integration belong to later versions. The v0.1 composer has no effects and no direct Execution dependency.
