# Workspace v0.1 contract

Revision: v0.1-r4 (proposed; direct consumer acceptance pending). Provider: Workspace. Direct consumers: Assistance, Execution, Application, and Changes; Intelligence consumes only the stated observations when its v0.1 slice arrives. Shared vocabulary: [common](common.md).

## Ownership and boundary

Workspace owns Git/worktree identity and incarnation, authority grants and revocation after host verification, source observations and session baselines, worktree ownership/lifecycle, and raw Git intent/parsing. Its Git adapter belongs under `workspace/git`.

Workspace does not prove host identity, spawn a process, own SQLite mechanics or migrations, provide LSP semantics, semantically enrich a diff, run checks, write source, keep an edit journal, interpret a WorkBundle, or compile task context. Assistance owns host proof and the five MCP schemas; Execution owns physical Git execution; Application owns transaction and migration mechanics; Intelligence owns semantic lookup; Changes owns v0.1 bounded diff composition, with ChangeSet persistence and semantic enrichment beginning in v0.2.

The only v0.1 logical tools are `start`, `context`, `diff`, `inspect`, and `stop`. No future tool is registered as a placeholder.

## Inputs and activation

Assistance supplies a fresh opaque `ActiveBindingUse` for a `BindingRef`, plus the separately correlated `ObservedSandboxState`. A bare actor ID, call ID, cwd, HEAD, model argument, PID, or time correlation is not authority. Workspace consumes/rechecks binding liveness before every authority-scoped use; a revoked binding yields no data or new authority.

Before authority exists, Workspace may ask Execution for the accepted read-only discovery edge. The request carries the active binding use, observed sandbox state, raw candidate Unix path, and a stable operation ID. Execution applies its D03/local `git_discovery` gate, preserves `sandboxCwd`, and runs only the fixed `git -C <candidate>` discovery queries. Discovery returns raw bounded evidence or `unavailable`; it cannot create an authority or profile permit.

Workspace parses that evidence as raw bytes and derives identity from the discovered worktree path, repository root, and Git common directory. It then claims the worktree exclusively and grants authority. The durable claim uses an Application stable activation operation ID; retrying it resolves the same activation. `OutcomeUnknown` is reconciled by lookup and never blindly replayed.

## Offered values and behavior

`WorktreeRef` contains an opaque worktree ID and incarnation plus the raw Unix worktree path, repository root, and Git common directory. A changed path, common directory, or identity evidence creates/requires reconciliation; HEAD alone never identifies a worktree.

`WorkspaceAuthority` is opaque and bound to one `WorktreeRef`, active `BindingRef`, and authority epoch. It is granted only after the preceding sequence succeeds. Concurrent claims for one worktree admit one actor; an actor with another active worktree is refused. A retried `stop` carries its expected authority: an old stop never revokes a newly activated generation.

After Assistance validates `stop` while the binding is live, it calls `stop_binding(authority.binding_ref)` before or atomically with Workspace final revocation. Workspace finalization does not consume or reactivate that now-revoked binding. If the host-binding stop handoff is absent, Workspace records `revocation-incomplete` and refuses all later authority use. A later explicit `start` creates a fresh binding generation.

Workspace sends one finite typed `AuthorityRevoked { worktree_ref, old_epoch, reason }` directly to Intelligence. `worktree_ref` includes its incarnation. Intelligence fences new view/query/diagnostic work, releases only that logical view, and returns a drain receipt; it asks Execution to reap only an owned unshared backend, never a surviving shared peer. This is a direct call/result, not an event bus. `stop` never rolls back files, closes a worktree, or stops peers.

`SourceObservation` contains an opaque reference, `WorktreeRef`, monotonic source sequence, raw path, byte digest and length, source revision, and explicit coverage. A newer relevant sequence makes an older observation stale. A source revision is distinct from HEAD, index, session baseline, task BASE, and submitted COMMIT. Unknown or partial coverage is never reported as clean.

For a `head`, `staged`, or `unstaged` request, Workspace validates authority, worktree incarnation and epoch, and supplies Changes exact left/right Git identities, bounded patch comparison evidence, NUL-safe paths/status/conflicts, separately listed untracked files, and baseline completeness/provenance only as context. A baseline never substitutes for any of those three comparison modes. Workspace derives identities only from complete fixed read-only HEAD, index, and working-state evidence; an unborn HEAD or truncated/failed input is explicit rather than an identity. Workspace parses NUL-delimited Git data and terminal-LF discovery paths without UTF-8 assumptions and keeps Git HEAD/index/baseline facts separate from source bytes. Changes composes the bounded logical diff result; Assistance renders it.

`RawGitEvidence` contains an operation reference, `WorktreeRef`, fixed command kind, raw bounded stdout/stderr, exit status, truncation/timing, and observation coverage. An owner-scoped bounded expansion takes its `OperationRef` and hunk cursor; invalidated evidence is `unavailable` or `stale`, and neither Workspace nor Changes reconstructs omitted patches.

Workspace contributes current, ownership-scoped bounded observations/evidence and detail expansion to `context`, `diff`, and `inspect`; Changes produces the logical `diff` result and Assistance renders every public tool response. Invalid evidence returns `unavailable`, `incomplete`, or `failed` with scope, freshness, coverage, provenance, and a bounded detail reference. `ready` means requested evidence is available, never that a product, check, or criterion is verified.

## Persistence, lifecycle, and cache gate

Workspace installs its first domain table through Application's accepted migration admission. It supplies stable domain/key and trusted SQL/digest, treats `AlreadyApplied`, `Incompatible`, `OutcomeUnknown`, and `BackupUnavailable` exactly as Application reports them, and never assigns migration versions or runs a second attempt after an unknown result. After `OutcomeUnknown`, it calls only `Store::migration_admission(domain, key)` to reconcile; that lookup never replays SQL.

A temporary missing or moved path is not closure. Stop, reconnect, handoff, missing paths, and moves never request cache retirement. Workspace may publish a verified closure/reset fact to a later retention boundary, but does not retire any cache itself. Strict lifetime of opaque/native provider indexes at closure remains an owner decision; no weaker cache-retention claim is made here.

## v0.1 contract scenarios

- Two actors race `start` for one discovered worktree: exactly one receives authority; neither receives a second active worktree.
- A binding is stopped between validation and grant: no authority is issued; an already admitted local transaction may settle but no later authority use succeeds.
- Discovery returns a newline, leading-dash, space, or non-UTF-8 path: Workspace preserves raw bytes and either parses the exact supported encoding or reports an explicit unsupported/incomplete result.
- An observation arrives after a newer sequence, or baseline capture is partial: it is stale/incomplete rather than current/clean.

Consumer acceptance freezes this revision and its SHA-256 before code relies on it. Subsequent contract changes require bilateral revision acceptance.
