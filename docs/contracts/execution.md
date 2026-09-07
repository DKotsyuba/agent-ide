# Governed execution and process evidence

Revision: r5 (proposed; post-authority rules from r4 unchanged). Provider: Execution. Direct providers: Assistance supplies validated host identity and observed host state; Workspace supplies controlled Git intent and worktree authority; Application supplies config/store mechanics only. Direct consumers: Workspace consumes raw Git execution results; Intelligence consumes owned protocol/process leases after profile admission; Application persists Execution-owned records. Changes composes Workspace raw Git results and does not directly spawn. Vocabulary: [common](common.md).

## Host-state and spawn gate

Execution accepts a physical-effect request only with a peer-validated host binding, Workspace authority epoch, controlled command, immutable local policy, and an Execution-minted profile permit. Assistance creates the host-binding token only after its real D01 proof; an absent or heuristic binding is `unavailable`, never a candidate for admission. Workspace constructs and validates raw Git argv. Intelligence chooses provider topology and a declared profile, then consumes an owned protocol lease. Neither model text nor an MCP field supplies an executable, argv, cwd, environment, or profile to Execution directly.

The only accepted managed macOS/Linux replay input is the complete opaque JSON value from `_meta["codex/sandbox-state-meta"]`, and only when server initialization advertised the same experimental capability. Execution validates its supported managed shape and invokes `codex sandbox --sandbox-state-json <one JSON argv value> -- <controlled child argv>` without shell interpolation. It preserves `sandboxCwd` and special-path semantics as supplied; it does not expand project roots or add `--sandbox-state-readable-root`. `external`, proxy, multi-root, missing, malformed, or unknown states are unavailable. Execution owns the catalog of tested profile template/version pairs and mints a permit only for a supported class and normalized state shape; its per-invocation full-state digest is correlation evidence, not a fixture-root authorization boundary. Application may persist profile evidence but does not verify or issue permits. `disabled` is executable only after the host explicitly supplied that type, Execution accepted its host-case template, and local policy explicitly accepts the no-outer-sandbox case.

The API and its substitute checks are contract-ready. They are not D03 acceptance: a managed provider, check, or write remains unavailable until Assistance supplies the captured state from a D01-validated invocation and Execution records a real macOS Seatbelt result for the applicable template. v0.1 does not expose check-profile execution; Changes does not gain a spawn path by composing a raw Git result.

## Requests and responses

The current API validates `ValidatedHostInvocation`, `WorkspaceAuthority`, `ControlledCommand`, local policy, and an Execution-profile catalog before admission. For v0.1, Workspace is the only producer of a Git command and receives its raw bounded stdout/stderr/exit evidence. Intelligence owns compatibility and logical leases; Execution never infers LSP compatibility or reads protocol stdout. `CommandKind::Job` is an internal controlled-child category, not a public arbitrary-command facility or a v0.1 check API.

Admission returns an admitted lease, bounded queue ticket, or typed refusal. Reservations are acquired before an owned process launch; queue tickets reserve nothing. Cancellation request, TERM/KILL delivery, direct-child reap, and descendant certainty are separate facts. Completed execution returns bounded stdout/stderr plus truncation, exit and timing evidence; it never declares Git-derived diff meaning, test coverage, or task completion.

Owned and borrowed handles have different capabilities. Borrowed endpoints may be observed/detached, never killed or reconfigured. A physical process is counted once where verified process identity permits dedup; marginal views and warmups reserve their own cost. Unknown borrowed process identity/resource usage is reported and conservatively reserved or refused under a required enforcement policy, never counted as zero.

## Bootstrap boundary

Current physical execution requires `WorkspaceAuthority`, but Workspace needs bounded Git discovery before it can issue final worktree authority. Discovery is a separate, Workspace-owned read-only effect and is not generic unauthorised execution.

Its required inputs are an active Assistance `BindingRef` correlated to the observed host state, that complete opaque state, the applicable Execution D03 gate, and local `git_discovery` policy. Missing/unsupported state or a closed D03 gate returns `unavailable`; lack of final Workspace authority never creates an unconfined child. Discovery needs no Intelligence LSP/provider compatibility permit and cannot mint Workspace authority, an Execution profile permit, or any other capability.

Execution preserves `state.sandboxCwd` exactly as the child working directory. It may invoke only a configured Git executable with one argv pattern: `git -C <exact raw candidate_cwd> <fixed query>`. `<candidate_cwd>` is the sole path operand and is carried without normalization, rewriting, or substitution; it must not replace `sandboxCwd`. The fixed queries are separate invocations of `rev-parse --show-toplevel` and `rev-parse --git-common-dir`, followed only by `worktree list --porcelain -z`. Each `rev-parse` result remains raw and separate for Workspace to parse as one terminal LF; path-valued results are never concatenated or newline-split. `worktree list` remains NUL-delimited. No caller-selected flags, environment, shell, additional path operands, provider/check/write class, or generic command is permitted.

Discovery returns bounded raw stdout/stderr, exit, and operation evidence only. Until Workspace confirms this r5 edge and Assistance exposes the correlated state/active binding input, the current post-authority API remains the only Execution spawn path.

## Admission and fairness

Use bounded queues with separate interactive/heavy classes, per-owner bounds, configurable burst and aging. Prevent starvation of eligible work; no preemption/latency guarantee is claimed inside an LSP implementation. Admission cost includes existing reservations and host headroom, not just frontend count. Cancellation/removal releases queued demand once. A lower config ceiling stops new excess admission and drains applicable work without killing unrelated peers.

Authority revocation cancels that actor's queued/owned jobs. A shared physical LSP remains while Intelligence presents surviving leases. Execution accepts a backend shutdown only from the backend owner after the relevant lease decision; revoking one view cannot kill a peer backend. Persistent quiescent retention has its own reservation and lease policy; inactive-workspace analysis is not allowed by retention.

## Process and platform boundary

Execution owns Unix process/resource adapters, strong process identity, pipe draining, signal escalation and cleanup. Retained bytes are bounded while pipes continue draining. A Child handle drop/timeout is not cleanup proof. Wait/reap direct children and legally adopted children; other descendants are tracked and their observed exit/remaining uncertainty is reported. Do not promise waitpid rights over arbitrary grandchildren. Numeric PID alone never authorizes a signal after restart.

TERM/grace/KILL and recovery have configurable budgets. Release physical reservation only after exit evidence or an explicitly conservative recovery handoff. Identity/cleanup uncertainty is quarantined and exposed, not converted into success. Process groups do not contain intentionally escaped processes; unmanaged native shell remains outside this guarantee.

Linux hard_required uses available delegated cgroups/controllers or returns unavailable. macOS resource sampling is monitoring, not a hard RSS guarantee. Soft-budget overage stops further admission and applies configured owned-process policy; OS-enforced memory behavior cannot be invented by Tokio.

## Evidence and acceptance boundary

The current contract harness, `tests/execution_contract.rs`, proves profile rejection, bounded admission, captured output drain, cancellation versus direct-child reap, protocol-stdout exclusivity, and borrowed-endpoint refusal. It does not prove host binding, sandbox replay, durable process identity, or provider isolation.

The D03 acceptance experiment must use an Assistance-captured state from a real D01-bound invocation, invoke the managed child through the exact Codex sandbox argv, and report allowed/denied operations that match the host contract. It must additionally observe bounded output, direct-child reap, and uncertainty about descendants. Until that experiment is recorded for the Execution template/version, managed provider/check/write effects remain unavailable. SQLite persistence, crash recovery, resource ceilings beyond the in-memory admission controller, retained backends, and real Linux/macOS provider scenarios are later integration work, not present v0.1 acceptance evidence.

## Retention and failure isolation

Intelligence may request a budgeted quiescent retained-backend lease for warm handoff; the lease is tied to the compatible backend/resource identity, not an old actor's active authority. Retention never exempts physical memory/process accounting or permits inactive-workspace analysis. Eviction/drain honors surviving active views and borrowed ownership; it never kills a peer or clears borrowed caches.

Admission/process/provider failure completes affected tickets within their declared control deadline or reports effect/cleanup uncertainty. It does not hold unrelated queues, native tools or host turn completion. Provider restart has one bounded owner and consumes the shared call/retry budget; nested layers cannot multiply retries. A breaker-open backend yields fast unavailable/queued-under-policy rather than repeated spawn storms. Execution returns evidence; it does not install an agent-wide completion gate.
