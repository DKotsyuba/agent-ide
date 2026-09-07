# Workspace v0.1 contract

Revision: v0.1-r2 (proposed; direct consumer acceptance pending). Provider: Workspace. Direct consumers: Assistance, Execution, Application; Intelligence and Changes consume only the stated observations when their v0.1 slices arrive. Shared vocabulary: [common](common.md).

## Ownership and boundary

Workspace owns Git/worktree identity and incarnation, authority grants and revocation after host verification, source observations and session baselines, worktree ownership/lifecycle, and raw Git intent/parsing. Its Git adapter belongs under `workspace/git`.

Workspace does not prove host identity, spawn a process, own SQLite mechanics or migrations, provide LSP semantics, enrich a diff, run checks, write source, keep an edit journal, interpret a WorkBundle, or compile task context. Assistance owns host proof and the five MCP schemas; Execution owns physical Git execution; Application owns transaction and migration mechanics; Intelligence owns semantic lookup; Changes owns later composition and enrichment.

The only v0.1 logical tools are `start`, `context`, `diff`, `inspect`, and `stop`. No future tool is registered as a placeholder.

## Inputs and activation

Assistance supplies a fresh opaque `ActiveBindingUse` for a `BindingRef`, plus the separately correlated `ObservedSandboxState`. A bare actor ID, call ID, cwd, HEAD, model argument, PID, or time correlation is not authority. Workspace consumes/rechecks binding liveness before every authority-scoped use; a revoked binding yields no data or new authority.

Before authority exists, Workspace may ask Execution for the accepted read-only discovery edge. The request carries the active binding use, observed sandbox state, raw candidate Unix path, and a stable operation ID. Execution applies its D03/local `git_discovery` gate, preserves `sandboxCwd`, and runs only the fixed `git -C <candidate>` discovery queries. Discovery returns raw bounded evidence or `unavailable`; it cannot create an authority or profile permit.

Workspace parses that evidence as raw bytes and derives identity from the discovered worktree path, repository root, and Git common directory. It then claims the worktree exclusively and grants authority. The durable claim uses an Application stable operation ID; `OutcomeUnknown` is reconciled by lookup and never blindly replayed.

## Offered values and behavior

`WorktreeRef` contains an opaque worktree ID and incarnation plus the raw Unix worktree path, repository root, and Git common directory. A changed path, common directory, or identity evidence creates/requires reconciliation; HEAD alone never identifies a worktree.

`WorkspaceAuthority` is opaque and bound to one `WorktreeRef`, active `BindingRef`, and authority epoch. It is granted only after the preceding sequence succeeds. Concurrent claims for one worktree admit one actor; an actor with another active worktree is refused. `stop` revokes only this authority and its leases. It never rolls back files, closes a worktree, or stops peers.

`SourceObservation` contains an opaque reference, `WorktreeRef`, monotonic source sequence, raw path, byte digest and length, source revision, and explicit coverage. A newer relevant sequence makes an older observation stale. A source revision is distinct from HEAD, index, session baseline, task BASE, and submitted COMMIT. Unknown or partial coverage is never reported as clean.

`RawGitEvidence` contains an operation reference, `WorktreeRef`, fixed command kind, bounded raw stdout/stderr, exit status, truncation/timing, and observation coverage. Workspace parses NUL-delimited Git data and terminal-LF discovery paths without UTF-8 assumptions. It keeps Git HEAD/index/baseline facts separate from source bytes. Changes may later consume this evidence; it cannot create semantic meaning or bypass Workspace ownership in v0.1.

`context`, `diff`, and `inspect` return the applicable bounded exact observation/evidence only when it is current and ownership-scoped; otherwise they return `unavailable`, `incomplete`, or `failed` with scope, freshness, coverage, provenance, and a bounded detail reference. `ready` means the requested evidence is available, never that a product, check, or criterion is verified.

## Persistence, lifecycle, and cache gate

Workspace installs its first domain table through Application's accepted migration admission. It supplies stable domain/key and trusted SQL/digest, treats `AlreadyApplied`, `Incompatible`, `OutcomeUnknown`, and `BackupUnavailable` exactly as Application reports them, and never assigns migration versions or runs a second attempt after an unknown result. After `OutcomeUnknown`, it calls only `Store::migration_admission(domain, key)` to reconcile; that lookup never replays SQL.

A temporary missing or moved path is not closure. Stop, reconnect, handoff, missing paths, and moves never request cache retirement. Workspace may publish a verified closure/reset fact to a later retention boundary, but does not retire any cache itself. Strict lifetime of opaque/native provider indexes at closure remains an owner decision; no weaker cache-retention claim is made here.

## v0.1 contract scenarios

- Two actors race `start` for one discovered worktree: exactly one receives authority; neither receives a second active worktree.
- A binding is stopped between validation and grant: no authority is issued; an already admitted local transaction may settle but no later authority use succeeds.
- Discovery returns a newline, leading-dash, space, or non-UTF-8 path: Workspace preserves raw bytes and either parses the exact supported encoding or reports an explicit unsupported/incomplete result.
- An observation arrives after a newer sequence, or baseline capture is partial: it is stale/incomplete rather than current/clean.

Consumer acceptance freezes this revision and its SHA-256 before code relies on it. Subsequent contract changes require bilateral revision acceptance.
