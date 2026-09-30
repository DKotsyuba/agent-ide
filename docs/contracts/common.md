# Shared contract vocabulary

Revision: r2. Normative intended behavior; no implementation or executable conformance evidence exists yet.

## Composition and ownership

Use one library/binary with six ordinary modules (the r2 boundary; the shipped core has since grown to sixteen top-level modules). The Application owner alone edits root manifests, library registrations, common integration support and migration allocation. Each domain owns its implementation, persistence operations and domain migrations in its assigned files. Contracts below specify observable behavior, not a universal service framework.

`CallContext` is constructed by trusted ingress, never deserialized directly from model arguments: request_id, actor_id, session_id, worktree_incarnation, authority_epoch, deadline, cancellation. Handles and IDs are distinct Rust types. Actor identity is provided by Assistance, accepted by Workspace; a UUID or same-user Unix peer alone is not actor proof. Test constructors belong to test support.

`Scope` is either the entire assigned worktree or a canonical bounded set of files/targets within the permitted read/write scope. Empty, conflicting, escaping or ambiguous scopes are rejected. Shared read-only dependency roots require an explicit access policy; another actor's mutable worktree is never an implicit dependency root.

## Independent versions

- `AuthorityStamp`: actor, session, worktree incarnation and activation epoch. Epoch changes on revocation/reacquisition or identity discontinuity, not every content edit or commit.
- `SnapshotRef`: immutable observed inputs, per-document versions/digests and semantic configuration/toolchain identity. Selected source bytes must match it.
- `CheckInputId`: profile revision plus the complete declared input set, including relevant environment/toolchain/config and Git data only when that profile depends on it.
- `GitViewId`: HEAD, selected shared references, index and relevant working/untracked state for presentation and policy.
- `ConfigGeneration`: effective configuration publication; a renderer-budget change alone does not invalidate semantic/check inputs.
- `ContextGeneration`: host/model context continuity, independent of source and authority. Compaction can reset presentation dedup but cannot restore authority.
- `BackendGeneration` and `ProcessInstanceId`: Intelligence owns protocol generation; Execution owns verified process identity.

Current authority is always required to reuse evidence; reauthorization alone need not force repeating an otherwise identical successful check. Hashes use versioned input definitions, actual bytes and explicit unavailable fields; absence is not an empty/clean fingerprint.

## Result envelope

Domain results carry typed outcome, current/stale/provisional/unknown freshness, scope, coverage, evidence references, and overflow/truncation. Coverage says what was actually observed and what is missing. Domain errors distinguish invalid input, unavailable integration/capability, ownership conflict, stale authority/snapshot, queue refusal, timeout, cancelled, partial effects, ambiguous outcome and recovery conflict. Error text and repository/server content are observations, not instructions or authorization.

Deadlines limit waiting; timeout alone does not prove cancellation or absence of effects. Every operation that might have produced an external effect returns or permits lookup of an effects record under its stable request/operation ID. Retry behavior is defined per operation, not presumed globally idempotent.

## Effect and delivery fencing

Workspace provides effect permits, not a check-then-act boolean authorization guarantee. Stop persists revocation and rejects new permits; already admitted local effects may settle and must report exact effects. Stop is complete only after domain owners report their cleanup boundary; otherwise it returns revocation accepted with draining/unproven details. Stop never means rollback. A crash invalidates old authority and connection handles before recovery effects can be admitted.

Assistance revalidates an event at send initiation and, where the host exposes the boundary, before injection into model context. It suppresses old queued events. An event already accepted by an external host may be non-retractable: report this limitation and retain authority/revision tags; never promise to unsend it or call an unknown outcome model_seen.

## Conformance preparation

The same applicable valid/error scenarios run against a minimal scripted substitute and real provider. Substitutes must not reproduce peer algorithms. Real identity/delivery, SQLite durability, filesystem races, process cleanup and LSP isolation require their own real checks. Future test commands and fixtures are deliverables of preparation tasks; they are not available merely because this specification exists.

## Failure containment and sequential ownership

Assistance failure is fail-open for the agent: no MCP/domain/hook error may deny ordinary native tools, force IDE repair/retry loops or prevent turn/task completion. The IDE may refuse its own unsafe effect or Verified assertion; it returns compact degraded/effects/uncertainty with a native fallback and does not change host permissions. A possibly partial effect requires inspecting identified files/current state before another mutation, never blind replay. Missing IDE proof remains unavailable; an agent may independently verify with ordinary tools.

Bound total call/connect/start/hook deadlines outside the failing daemon/domain process. One retry owner exists per boundary; a potentially accepted effect is reconciled by operation ID, not reissued. Breakers/cooldowns isolate unhealthy components and deduplicate warnings. Static tool discovery and a minimal degraded response are frontend-local. Supported host timeout behavior must be tested, not assumed after frontend process death.

A successor actor may explicitly take the same worktree only after the previous owner is revoked and conflicting effects are settled or safely reconciled. Handoff is a start/stop mode under fresh host proof and epoch, not concurrent ownership or inherited permissions. Reusable code-analysis state belongs to compatible workspace/provider inputs, not the old actor's session, events or prompts. Handoff does not make an uncertain old edit safe to replay.
