# Workspace v0.1 contract

Revision: v0.1-r6 (proposed; direct consumer acceptance pending). Provider: Workspace. Direct consumers: Assistance, Execution, Application, and Changes; Intelligence consumes only the stated observations when its v0.1 slice arrives. Shared vocabulary: [common](common.md).

## Ownership and boundary

Workspace owns Git/worktree identity and incarnation, authority grants and revocation after host verification, source observations and session baselines, worktree ownership/lifecycle, and raw Git intent/parsing. Its Git adapter belongs under `workspace/git`.

Workspace does not prove host identity, spawn a process, own SQLite mechanics or migrations, provide LSP semantics, semantically enrich a diff, run checks, write source, keep an edit journal, interpret a WorkBundle, or compile task context. Assistance owns host proof and the five MCP schemas; Execution owns physical Git execution; Application owns transaction and migration mechanics; Intelligence owns semantic lookup; Changes owns v0.1 bounded diff composition, with ChangeSet persistence and semantic enrichment beginning in v0.2.

The only v0.1 logical tools are `start`, `context`, `diff`, `inspect`, and `stop`. No future tool is registered as a placeholder.

## Inputs and activation

Assistance supplies a fresh opaque `ActiveBindingUse` for a `BindingRef`, plus the separately correlated `ObservedSandboxState`. A bare actor ID, call ID, cwd, HEAD, model argument, PID, or time correlation is not authority. Workspace consumes/rechecks binding liveness before every authority-scoped use; a revoked binding yields no data or new authority.

Before authority exists, Workspace may ask Execution for the accepted read-only discovery edge. The request carries the active binding use, observed sandbox state, raw candidate Unix path, and a stable operation ID. Execution applies its D03/local `git_discovery` gate, preserves `sandboxCwd`, and runs only the fixed `git -C <candidate>` discovery queries. Discovery returns raw bounded evidence or `unavailable`; it cannot create an authority or profile permit.

Workspace parses discovery as raw bytes. `DurableWorkspace::resolve_worktree` inspects real root, repository, and common directories, rejects symlink components, and records canonical paths plus native device/inode identity. SQLite mints the incarnation; callers cannot select it. The same physical root has one identity record, while a changed native identity at the same path receives a new incarnation. Moves are unavailable until explicit reconciliation. The durable claim uses an Application stable activation operation ID; an exact retry resolves the same immutable start receipt. `OutcomeUnknown` is reconciled by lookup and never blindly replayed.

## Offered values and behavior

`WorktreeRef` contains an opaque worktree ID and incarnation plus the raw Unix worktree path, repository root, Git common directory, and private native identity evidence. `from_discovery` remains an unverified reference/fixture constructor: its caller-chosen incarnation cannot pass durable activation. A changed path, common directory, or identity evidence creates/requires reconciliation; HEAD alone never identifies a worktree.

`WorkspaceAuthority` is opaque and bound to one canonical `WorktreeRef`, active `BindingRef`, durable owner boot, and authority epoch. It is granted only after the preceding sequence succeeds. Concurrent claims for one worktree admit one actor; an actor with another active worktree is refused. A retried `stop` carries its expected authority: an old stop never revokes a newly activated generation.

`DurableWorkspace::open` admits the identity/authority/baseline schema through Application, advances the persistent boot and epoch clocks, and fences prior active grants. The product holds one owner per boot; another open is an explicit new boot. `activate` returns an immutable `StartReceipt`; `authority` and `authorize` additionally require a fresh matching Assistance binding, unchanged native identity, an active durable claim, and the current boot. Old receipts remain inspectable after restart but cannot revive authority. Binding fingerprints used by a prior boot or stopped grant cannot authorize a new start ID. Fingerprints are domain-separated and length-framed; raw channel binding data is not persisted.

Start outcomes, actor/worktree exclusivity, epochs, and exact stop outcomes live in Workspace tables within Application transactions. Durable revoke accepts the immutable start receipt, so a stop outcome can be recovered after process restart without reconstructing live authority. SQL uniqueness constraints enforce one active actor per worktree and one active worktree per actor. Exact retries return committed outcomes; a fresh validated MCP call may retry the same activation ID because per-call IDs are not durable operation identity. Changed actor, channel/binding generation, or worktree intent under that ID conflicts. `AuthorityRegistry` remains a non-authoritative in-process fixture/cache helper. Its stamps cannot satisfy durable product admission, and the remaining product wiring must use the durable owner at every consumption boundary.

After Assistance validates `stop` while the binding is live, it calls `stop_binding(authority.binding_ref)` before or atomically with Workspace final revocation. Workspace finalization does not consume or reactivate that now-revoked binding. If the host-binding stop handoff is absent, Workspace records `revocation-incomplete` and refuses all later authority use. A later explicit `start` creates a fresh binding generation.

Workspace sends one finite typed `AuthorityRevoked { worktree_ref, old_epoch, reason }` directly to Intelligence. `worktree_ref` includes its incarnation. Intelligence fences new view/query/diagnostic work, releases only that logical view, and returns a drain receipt; it asks Execution to reap only an owned unshared backend, never a surviving shared peer. This is a direct call/result, not an event bus. `stop` never rolls back files, closes a worktree, or stops peers.

`SourceObservation` contains an opaque reference, `WorktreeRef`, monotonic source sequence, raw path, byte digest and length, source revision, and explicit coverage. A newer relevant sequence makes an older observation stale. A source revision is distinct from HEAD, index, session baseline, task BASE, and submitted COMMIT. Unknown or partial coverage is never reported as clean.

For a `head`, `staged`, or `unstaged` request, Workspace validates authority, worktree incarnation and epoch, and supplies Changes exact left/right Git identities, bounded patch comparison evidence, NUL-safe paths/status/conflicts, separately listed untracked files, and baseline completeness/provenance only as context. A baseline never substitutes for any of those three comparison modes. Workspace derives identities only from complete fixed read-only HEAD, index, and working-state evidence; an unborn HEAD or truncated/failed input is explicit rather than an identity. Workspace parses NUL-delimited Git data and terminal-LF discovery paths without UTF-8 assumptions and keeps Git HEAD/index/baseline facts separate from source bytes. Changes composes the bounded logical diff result; Assistance renders it.

`RawGitEvidence` contains an operation reference, `WorktreeRef`, fixed command kind, raw bounded stdout/stderr, exit status, truncation/timing, and observation coverage. An owner-scoped bounded expansion takes its `OperationRef` and hunk cursor; invalidated evidence is `unavailable` or `stale`, and neither Workspace nor Changes reconstructs omitted patches.

`GitComparison` retains the full worktree/incarnation/epoch/mode scope. `RawGitEvidence` retains its fixed `GitReadQuery`; constructors reject query/mode mismatch, stdout above 1 MiB, or stderr above 64 KiB even when truncation flags are false. Comparison construction requires HEAD-identity, index-state, and the selected mode's patch query. `GitStatus::from_evidence` accepts only complete successful status-query evidence and preserves its worktree/epoch. Standalone porcelain parsing has no collection authority and cannot be composed as current status.

`BaselineContext::new` supplies descriptive Partial/Unknown context only; it rejects Complete and reports `NotCaptured`. `DurableWorkspace::capture_baseline` requires current durable authority, at most six scoped Git records and 128 registered raw source paths, and stores at most 4 MiB of framed Git/source bytes. Exact retries return the stored capture even if files subsequently change. Stored captures retain a payload digest and exact worktree/incarnation/epoch, but remain `Partial` with an `Unverified` joint capture window. No atomic Git/source window is yet established, so no v0.1 caller can mint Complete. Changes preserves this window status and rejects a captured baseline from another scope.

Content-reading Git commands still have an unresolved process-isolation limitation: `--no-ext-diff`, `--no-textconv`, and disabled fsmonitor do not prevent repository clean/process filters. These commands must not be described as safe against arbitrary repository helper configuration. A raw snapshot collector that reads Git blobs without filters and compares private out-of-repository snapshots is required to close that gap; required comparison modes remain present meanwhile.

Workspace contributes current, ownership-scoped bounded observations/evidence and detail expansion to `context`, `diff`, and `inspect`; Changes produces the logical `diff` result and Assistance renders every public tool response. Invalid evidence returns `unavailable`, `incomplete`, or `failed` with scope, freshness, coverage, provenance, and a bounded detail reference. `ready` means requested evidence is available, never that a product, check, or criterion is verified.

## Persistence, lifecycle, and cache gate

Workspace installs source observations and the immutable identity/authority/baseline schema through Application's accepted migration admission. Nonfresh upgrades require Application's configured private backup root. It supplies stable domain/key and trusted SQL/digest, treats `AlreadyApplied`, `Incompatible`, `OutcomeUnknown`, and `BackupUnavailable` exactly as Application reports them, and never assigns migration versions or runs a second attempt after an unknown result. After `OutcomeUnknown`, it calls only `Store::migration_admission(domain, key)` to reconcile; that lookup never replays SQL.

Registered-path reconciliation loads the exact latest same-worktree/incarnation/path observation in the insertion transaction; only the matching current authority epoch can supply the prior state. Caller-supplied previous observations are ignored. A bare rename hint does not establish native identity continuity, so v0.1 returns independent delete/create facts. A missing worktree root returns `RootUnavailable` without recording a missing descendant or emitting Close; a missing descendant beneath an opened root retains explicit missing-path semantics.

A temporary missing or moved path is not closure. Stop, reconnect, handoff, missing paths, and moves never request cache retirement. Workspace may publish a verified closure/reset fact to a later retention boundary, but does not retire any cache itself. Strict lifetime of opaque/native provider indexes at closure remains an owner decision; no weaker cache-retention claim is made here.

## v0.1 contract scenarios

- Two actors race `start` for one discovered worktree: exactly one receives authority; neither receives a second active worktree.
- A binding is stopped between validation and grant: no authority is issued; an already admitted local transaction may settle but no later authority use succeeds.
- Discovery returns a newline, leading-dash, space, or non-UTF-8 path: Workspace preserves raw bytes and either parses the exact supported encoding or reports an explicit unsupported/incomplete result.
- An observation arrives after a newer sequence, or baseline capture is partial: it is stale/incomplete rather than current/clean.

Consumer acceptance freezes this revision and its SHA-256 before code relies on it. Subsequent contract changes require bilateral revision acceptance.
