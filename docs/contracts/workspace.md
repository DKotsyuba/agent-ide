# Historical Workspace authority and snapshots

> This r3 draft is historical and superseded for v0.1 by
> [Workspace v0.1 contract](workspace-v0.1.md), with the narrow v0.2 edit
> boundary in [Workspace v0.2](workspace-v0.2.md). It remains here only as prior
> design material and is not an implementation contract.

Revision: r3. Provider: Workspace. Consumers: Assistance, Intelligence, Changes, Execution, Application. Shared vocabulary: [common](common.md).

## Operations

| Operation | Inputs | Success | Failure/effects |
|---|---|---|---|
| activate | verified HostAttachment, requested canonical worktree, activation intent/request ID | ActiveSession with authority stamp, baseline, effective scope and readiness | proof/integration unavailable, ownership conflict, identity/Git/storage failure; no analysis before successful binding |
| start resume mode | fresh HostAttachment and recorded session reference | revalidated continuity and new epoch when required | stale/moved/recreated identity never silently restored; no separate tenth public resume tool |
| observe | current context, Scope, purpose and required inputs | SnapshotRef, exact selected bytes, GitViewId and coverage | unavailable/inconsistent observation; bounded retries, never infer clean |
| acquire_effect | current context, expected snapshot, operation kind and exact targets | short-lived operation permit bound to targets/epoch | stale/escaping/ambiguous target rejected before an effect |
| report_effect | operation permit plus actual per-path effects | independently reread/invalidate affected inputs and typed change event | a stale receipt cannot manufacture current source truth |
| revoke | current bound session and stop request ID | authority revoked; domain drain receipts collected separately | idempotent repeat for same stop; no peer stop or file rollback |
| reconcile | watcher/native-activity hints or restart records | dirty/unknown coverage, refreshed required observations and revocation when identity changed | event loss creates explicit coverage gap, not silent success |

Repeated activate for the same live actor/worktree returns the existing logical session; a second frontend is not another actor. Another worktree for an already active actor is a conflict until explicit stop/rebinding. Atomic persistent uniqueness covers actor and worktree incarnation. Assistance alone discovers host identity; Workspace independently checks that the attestation is current, authorized and applicable to this invocation.

## Canonical identity and observation

Workspace owns Git CLI probing, path resolution and file watcher/reconciliation policy. Application only supplies configured runtime infrastructure. Canonical worktree identity includes resolved root and Git administrative/common-directory identity with an incarnation marker; path spelling alone is insufficient. Detect same-path recreation, root changes and aliases. Content-only changes advance input snapshots, not authority epoch. Stage/commit changes GitViewId; a content-only check may still match.

Git snapshots include regular files and treat unchanged gitlinks as opaque index entries. A changed gitlink remains unsupported because the raw-file comparison cannot represent a submodule commit or its worktree contents. A clean attribute query permits skipping byte capture only when every conversion value is exactly `unspecified` and `core.autocrlf` is false.

Read actual selected bytes and relevant HEAD/ref/config/toolchain inputs at edit/diff/finish boundaries. Bound read size and retry unstable before/after observations; report inconsistent/unknown when stability cannot be obtained. Watchers and native tool reports are hints. Shared Git refs are re-resolved even when no local HEAD-file event occurred. Initial pre-existing staged/unstaged/untracked state is retained as the session baseline without attributing it to IDE edits.

The write scope accepts only supported regular-file targets and explicit creation/deletion semantics. Workspace returns canonical target identity and parent identity; Changes enforces operation-specific byte/edit rules. Symlink/hardlink-sensitive or ambiguous aliases are rejected for mutation until a documented safe policy exists. Reads have an independent scope and never silently expand to a peer worktree.

## Mutation versions and observation windows

A mutation observation returns `MutationTargetVersion { canonical_file_identity, canonical_parent_identity, content_version, file_kind, relevant_metadata_version, metadata_policy_id }`. The supported metadata policy compares mode, file replacement identity, ownership and link count/policy in addition to exact content. Unsupported ACL/xattr/flags or other metadata that cannot be preserved/compared safely cause an explicit unsupported mutation before the first write. Do not silently strip metadata. Domain tests cover same bytes with chmod, changed inode/parent and new link aliases.

An effect permit is bound to `(operation_id, effect_index, authority_epoch, target, target_version, expiry)` and authorizes one bounded replacement only. Repeated report/lookup for that same effect index returns the recorded effect; it does not grant another replacement. Workspace does not own Changes' durable operation journal.

Observation includes an interval token identifying relevant observed-change sequence and coverage generation. A check can request a post-execution observation against its starting token. Detected relevant activity or lost/unknown observation coverage marks the interval changed/unknown even when final bytes equal initial bytes. This is observation evidence, not a claim that filesystem watchers prove the absence of every possible unmanaged transient write. Profiles requiring immutable-input assurance must supply an isolated/controlled input basis rather than infer it from two equal hashes.

## Effect permits and stop

Effect admission and revocation serialize within Workspace authority. Changes owns per-session edit serialization and actual filesystem operations. It acquires/revalidates permits before a bounded atomic replacement; it cannot hold an authority lock while waiting for an LSP or compiler. A replacement admitted before revocation may complete. Once cancellation is observed, further forward file writes stop and the receipt becomes partial/cancelled as appropriate. `stop` acknowledges revocation separately from completed drain. Snapshot comparison is not an OS-wide compare-and-swap against unmanaged editors.

On daemon restart active records are recovery-required, not automatically active. Fresh host binding and explicit start/resume are required before domain recovery writes. Read-only inspection of pending effects is allowed through the bound owner flow; no other actor can inspect another view by guessing an ID.

## Scenarios and preparation

Shared scenarios: same actor repeated start; two actors racing one worktree; one actor requesting two roots; stale proof; effect permit then stop; late snapshot after revoke; commit-only change; shared-ref movement; root recreation; duplicate/lost watcher hints; unknown native-shell effects; state-store failure during claim. Minimal inputs are a fixed valid/rejected HostAttachment, scripted byte/Git observations and a small claim-store substitute. Real tests use temporary Git worktrees, SQLite restart and actual platform watcher behavior. Contract harness: `tests/workspace_sessions_contract.rs`; real Git scenarios: `tests/workspace_git.rs`.

## Sequential coder-to-reviewer handoff

On explicit release, persist revocation and a bounded handoff descriptor for canonical worktree incarnation, last observed input/Git references and cleanup state. The descriptor is a lookup hint, not authority or a bearer permission. The successor's start requests the same worktree with fresh host proof/invocation validation; Workspace admits a new actor/epoch only after exclusivity and conflicting-effect drain/reconciliation are satisfied. An actor already owning another worktree must release it first. An orchestrator is a normal new actor for these rules, never an ownership bypass.

Semantic/provider-cache references may survive release; old CallContext, permits, pending messages and old-owner result routing do not. Workspace returns fresh observations to Intelligence for cache validation. Compatible source metadata is reused only as a hint until required inputs are revalidated. A failed IDE activation/handoff reports unavailable/conflict promptly; it does not block ordinary native agent tools or final response. Fault-driven detachment stops optional workspace watching within its configured budget and never requires the agent to repair the IDE.

Workspace publishes an ownership-checked WorktreeLifecycle observation (present, moved/reconcile_required, unavailable, verified_deleted) to Application/Intelligence. Only verified_deleted or explicit owner-authorized reset permits retirement of the worktree's persistent cache namespace. A path miss alone is unavailable, not proof of deletion.
