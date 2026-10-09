# Exact changes and current verification

Historical contract for the earlier design. The current v0.1 boundary is [bounded Git diff composition](changes-v0.1.md); the narrow v0.2 one-file edit boundary is [Changes v0.2](changes-v0.2.md). The material below is retained as historical reference.

Revision: r3. Provider: Changes. Consumer: Assistance. Inputs: Workspace authority/snapshots/permits, Intelligence proposals/diagnostics, Execution job evidence, Application config/SQLite. Vocabulary: [common](common.md).

## Operations and results

`edit` accepts an exact-base patch or explicit semantic-rename request for a bounded scope. Each target resolves canonically once, carries expected input version/bytes, and has non-overlapping byte edits. Changes may call Intelligence.prepare_rename but never delegates disk writing to a server. Return Applied/Cancelled/Partial/Rejected plus operation/journal ID and per-file actual before/after effects. Applied never means Verified.

`check` accepts scope and a known profile/purpose; returns queued/running/reused/completed/unavailable with a receipt reference. `diff` returns current GitViewId and entries distinguishing pre-existing, staged, unstaged and untracked changes, with API provenance only where journal evidence proves it. `finish` returns Verified/Waiting/Incomplete/NeedsAction with current scope, accepted evidence and specific missing/stale/failing evidence. It never creates success from a missing diagnostic or empty test selection. `recover` is an internal explicit bound-owner operation, not an additional public scenario tool or implicit rollback on stop.

## Duplicate requests and uncertain outcomes

Every effectful edit, check and explicit recover has a stable operation ID from trusted CallContext. The same ID and canonical request digest returns/waits for the existing operation record; it never reapplies a file effect or submits another check. The same ID with different canonical input is ConflictingDuplicate. Reauthorization/resume must resolve the prior operation under current owner continuity; a stale ID alone grants no authority.

If persisting Prepared returns outcome_unknown, perform no first filesystem write until lookup proves it committed. If a journal transition after a possible effect is uncertain, return OutcomeUnknown/Partial with the existing lookup reference, inspect store plus actual bytes/metadata and reconcile before retry. The record binds effect_index to its one Workspace permit; uncertain reporting never creates a fresh permit for the same completed replacement.

## Edit/recovery contract

Validate all targets, bases, expected bytes, overlap, size and authority before the first write. Persist Prepared with exact pre/post images (or durably referenced blobs and digests) before changing files; record per-file intent/outcome and fsync policy. Adjacent temporary-file replacement can be atomic for a single regular file; multi-file changes are not globally atomic. Unsupported links/aliases/special files are rejected. Serialize IDE edits within a session; native editors remain independent.

Revalidate targets/bytes and acquire a current Workspace effect permit immediately before each bounded file replacement. This is not a filesystem compare-and-swap against arbitrary concurrent writers; the final check/syscall race remains explicit. A permit admitted before stop may settle its atomic replacement. After cancellation is observed no more forward writes occur. Return a partial receipt where applicable; stop itself does not roll back.

Recovery inspects real bytes under fresh authority. If all files match intended post-images, record completed; otherwise only a file still matching the journal's known image may be changed by an explicit recovery action. External byte changes cause RecoveryConflict and are preserved. Do not equate equal bytes with preserved unrelated metadata: compare the contracted file identity/metadata before overwriting. Unresolved journals block finish for affected scope. SQLite transactions do not provide filesystem atomicity; crash points require real tests.

## Profiles, fingerprints and receipts

A CheckProfile declares purpose (build/test/lint/etc), command/argv/cwd, selection and nonempty-target rule, complete reusable input set, environment allowlist, toolchain identity, resource/output policy, evidence parser and coverage domain. Unknown commands from source/LSP text are not profiles. Opaque shell work is non-reusable unless effective inputs and coverage can be established.

Rust inputs include applicable manifests/lock/config/toolchain/build scripts/source and declared environment. Git state is included only for a profile that depends on it. A commit/stage without changed check inputs refreshes diff but need not rerun a matching receipt. Acceptance-policy changes reevaluate acceptance; only semantic command/input changes require rerun.

Success requires actual terminal execution evidence and the profile's coverage evidence. A test filter matching zero tests is not a passing test receipt. A build-only profile may legitimately prove build coverage without claiming tests ran. When no target is applicable, return explicit NotApplicable/Incomplete under policy, never fabricate exercised targets. An unknown/truncated necessary parser result cannot prove coverage. Real stable-tool output formats and supported versions are validated in provider tests, not assumed from a guessed JSON flag.

After actual job completion, obtain a Workspace observation of the declared inputs and starting observation interval. Input mismatch or detected/unknown relevant activity makes the receipt stale/non-reusable even if the command exited successfully. A check that rewrites its own declared input is a required failure-to-verify scenario. Before/after equality alone does not prove a stable intermediate snapshot: the receipt records its profile-declared consistency basis (controlled/isolated inputs or bounded observed working-tree evidence) and coverage. Never claim immutable-input assurance for ordinary uncontrolled working-tree execution; policy requiring that assurance returns Incomplete unless it is provided.

`finish` refreshes authority, relevant input/Git state and policy; checks unresolved edits and the configured required evidence for scope. Current failed evidence produces NeedsAction; active jobs/drain produce Waiting; absent/stale/unsupported/incomplete evidence produces Incomplete. A required capability unavailable on a provider cannot be waived silently. Cancellation yields no reusable success until the actual outcome is known.

## Boundary scenarios

`tests/changes_contract.rs`: exact/stale base, incomplete rename, overlapping edits, wrong authority, partial effects, duplicate/conflicting request, uncertain Prepared/outcome commit, metadata-only external change, input reuse, check-mutates-input, changed/unknown observation interval and missing coverage. Other owned suites: changes_edit_recovery, changes_rename, changes_check, changes_diff, changes_finish. Scripted Workspace/Intelligence/Execution/store responses permit autonomous checks. Real SQLite restart, two-process external modification, Git stage/ref movement, language rename and latest-revision finish live in changes_real_git, changes_real_execution, changes_restart and changes_finish_e2e. These real checks cannot be replaced by an in-memory mock.

## Non-blocking failure and review handoff

An unavailable journal/provider/runner/IDE transport blocks only this module's unsafe effect or unsupported Verified assertion. It must not veto native tools or task completion. Return a bounded degraded outcome with stable operation/effects references; after possible partial or unknown effects the next native action is inspection of the known target paths and current bytes/metadata, not blind retry. Independent native verification can proceed without obtaining an IDE finish receipt; do not label it as an IDE-issued receipt.

After Workspace admits a sequential reviewer, compatible prior code/check evidence may be re-evaluated under the new authority and current policy/input/coverage basis. Never transfer old mutation permits or assume pending old operations finished. Unresolved conflicting effects must be reconciled before handoff permits; known immutable evidence may be reused only after fresh owner-scoped validation. Handoff tests include cached successful check reuse, stale-input rejection, old operation replay rejection and native fallback after unavailable cache/journal.
