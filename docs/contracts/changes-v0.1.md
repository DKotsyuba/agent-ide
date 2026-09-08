# Bounded Git diff composition

Revision: v0.1-r4. Provider: Changes. Direct consumer: Assistance. Input provider: Workspace. Shared vocabulary: [common](common.md); current raw evidence boundary: [Workspace v0.1](workspace-v0.1.md).

## Responsibility

Changes produces the logical v0.1 `diff` result for the closed modes `head`, `staged`, and `unstaged`. It composes a bounded summary and selected exact Git hunks, with untracked paths listed separately. Assistance owns tool schemas, host delivery, rendering, and cross-channel feedback policy.

Workspace owns authority, Git commands and raw parsing, comparison identities, baseline capture, observations, and owner-scoped detail expansion. Execution physically admits and runs Workspace's commands. Changes never spawns Git, reads a different worktree directly, changes files, or treats a model-supplied identity as authority.

## Required evidence

For the requested current authority, Workspace supplies one typed `GitSnapshot`: full worktree/incarnation/epoch/mode scope, one nonzero capture generation, exact comparison, separately classified raw status and per-path patch evidence. Each `PathSnapshot` carries the exact path directly; it never relies on Git's temporary headers. Capture, no-filter Git commands, finite limits, process-result validation and retry rules are owned by the [Workspace contract](workspace-v0.1.md).

Composition requires the expected scope, snapshot comparison, every path scope/generation, status scope, and baseline scope to match. A mismatch yields unavailable without identities, hunks, paths or expansion references. Failed, truncated, undrained, unsupported or unstable captures cannot mint a complete `GitSnapshot` and therefore never reach successful composition. Workspace must translate their explicit errors into the public incomplete/failed/stale response.

Raw NUL-safe path/status/conflict data and a separate bounded untracked list identify the affected entries. Baseline completeness, capture-window status, and provenance are context only: a session snapshot is never substituted for HEAD, index, or working-state comparison. An absent HEAD or unsupported comparison is explicit rather than a fabricated empty comparison.

Workspace supplies currentness and ownership checks before composition and detail expansion. A revoked, stale, unsupported, failed, or incomplete input remains visibly so in the result; Changes never reconstructs a missing patch or declares partial evidence clean.

## Offered result

The result uses the existing bounded envelope: `ready`, `unavailable`, `incomplete`, or `failed`, with scope, freshness, coverage, and provenance. Its payload contains comparison identities, status counts, selected exact hunks, separate untracked/conflict information, overflow limits, baseline coverage/window status, and owner-scoped operation/detail references. `NotCaptured` distinguishes descriptive context from stored `Unverified` captures; neither can claim a complete baseline.

Hunk selection respects both byte and hunk-count budgets. Omitted data is counted or marked as unknown when the source itself is truncated. Changes does not silently cut a hunk into a different patch. Binary changes and raw Unix paths remain explicit; safe display escaping must not replace the underlying raw identity.

Every selected hunk has a non-optional exact raw path bound directly to its per-path Workspace evidence. Temporary `diff`, `---` and `+++` headers are discarded; no reverse mapping or path decoding is performed. Binary changes retain the binary flag and exact path with an empty text payload, so temporary names cannot leak into summaries. Tracked metadata remains available for mode-only, empty-file, addition/deletion and other changes without textual hunks. Raw rename paths are represented as delete/add because the collector does not infer rename identity. Conflicts and untracked paths remain separate and never receive fabricated hunks. Budgeted hunks retain their direct paths across selection and expansion cursors.

Further detail is requested through Workspace by operation reference and bounded hunk cursor. A reference grants no new authority, and invalidation is rechecked before expansion. Changes stores no persistent ChangeSet and creates no second reference store.

## Current acceptance

- The three modes keep HEAD/index/working-state identities distinct and never use the session baseline as a replacement.
- Budget overflow preserves selected exact hunks, a bounded summary, and honest omissions; untracked and conflicted entries remain visible.
- Space, newline, leading-dash, and non-UTF-8 paths retain their identities; an unsupported encoding is reported explicitly.
- Stale, revoked, truncated, binary, empty, and failed Git evidence cannot become a clean complete result.
- A real Apple Git fixture through Execution proves all three modes, raw-path attribution, helpers not executing, bounded retries, result acceptance and scratch cleanup. Non-UTF-8 filesystem support is reported separately from raw-byte parser coverage.

ChangeSet persistence, semantic enrichment, history, checks, verification, edits, and Scope integration belong to later versions. The v0.1 composer has no effects and no direct Execution dependency.
