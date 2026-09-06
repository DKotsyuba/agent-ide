# Governed execution and process evidence

Revision: r2. Provider: Execution. Consumers: Intelligence, Changes, Application; Assistance receives job facts through domain results. Workspace supplies authority; Application supplies validated config/store. Vocabulary: [common](common.md).

## Requests and responses

`ExecutionIntent` contains operation/request ID, current caller scope or backend lease authorization, class (interactive/heavy/backend-start/view-warmup), typed resource demand, executable/argv/cwd/environment from a validated profile, output policy and deadline. No shell string is synthesized from model/server text. Intelligence owns compatibility and logical leases; Execution never infers LSP compatibility.

`submit` returns admitted handle, queued ticket or typed refusal. Reservations are acquired before owned process launch or admitted work begins. Tickets support inspect/cancel; request cancellation is distinct from termination. Completed execution returns exit code/signal, spawn/timeout/cancel reason, bounded stdout/stderr references plus truncation, actual timing and process/resource evidence. Changes interprets check coverage; Execution does not declare tests passed.

Owned and borrowed handles have different capabilities. Borrowed endpoints may be observed/detached, never killed or reconfigured. A physical process is counted once where verified process identity permits dedup; marginal views and warmups reserve their own cost. Unknown borrowed process identity/resource usage is reported and conservatively reserved or refused under a required enforcement policy, never counted as zero.

## Admission and fairness

Use bounded queues with separate interactive/heavy classes, per-owner bounds, configurable burst and aging. Prevent starvation of eligible work; no preemption/latency guarantee is claimed inside an LSP implementation. Admission cost includes existing reservations and host headroom, not just frontend count. Cancellation/removal releases queued demand once. A lower config ceiling stops new excess admission and drains applicable work without killing unrelated peers.

Authority revocation cancels that actor's queued/owned jobs. A shared physical LSP remains while Intelligence presents surviving leases. Execution accepts a backend shutdown only from the backend owner after the relevant lease decision; revoking one view cannot kill a peer backend. Persistent quiescent retention has its own reservation and lease policy; inactive-workspace analysis is not allowed by retention.

## Process and platform boundary

Execution owns Unix process/resource adapters, strong process identity, pipe draining, signal escalation and cleanup. Retained bytes are bounded while pipes continue draining. A Child handle drop/timeout is not cleanup proof. Wait/reap direct children and legally adopted children; other descendants are tracked and their observed exit/remaining uncertainty is reported. Do not promise waitpid rights over arbitrary grandchildren. Numeric PID alone never authorizes a signal after restart.

TERM/grace/KILL and recovery have configurable budgets. Release physical reservation only after exit evidence or an explicitly conservative recovery handoff. Identity/cleanup uncertainty is quarantined and exposed, not converted into success. Process groups do not contain intentionally escaped processes; unmanaged native shell remains outside this guarantee.

Linux hard_required uses available delegated cgroups/controllers or returns unavailable. macOS resource sampling is monitoring, not a hard RSS guarantee. Soft-budget overage stops further admission and applies configured owned-process policy; OS-enforced memory behavior cannot be invented by Tokio.

## Durable evidence and tests

Launch intent and obtained identity are durably associated with operation ID; crash gaps become recovery-required. SQLite alone cannot transact process spawn. Running job history must permit distinguishing not_started, started, terminal, and effect_unknown. Old epoch callbacks cannot release a new reservation or update a replacement process.

Shared scenarios cover invalid demand, full queue, fairness, one physical process/multiple views, stale scope, borrowed kill refusal, output flood, cancellation acknowledged before exit, daemon crash between intent/spawn/record, PID reuse and unavailable cgroup policy. Scripted clock/launcher/journal substitutes verify mapping and scheduling; real Linux/macOS subprocess tests prove signals, output, direct-child reap and descendant observations. Harness: `tests/execution_contract.rs`; actual OS scenarios: `tests/execution_processes.rs`, `tests/execution_resources.rs`.

## Retention and failure isolation

Intelligence may request a budgeted quiescent retained-backend lease for warm handoff; the lease is tied to the compatible backend/resource identity, not an old actor's active authority. Retention never exempts physical memory/process accounting or permits inactive-workspace analysis. Eviction/drain honors surviving active views and borrowed ownership; it never kills a peer or clears borrowed caches.

Admission/process/provider failure completes affected tickets within their declared control deadline or reports effect/cleanup uncertainty. It does not hold unrelated queues, native tools or host turn completion. Provider restart has one bounded owner and consumes the shared call/retry budget; nested layers cannot multiply retries. A breaker-open backend yields fast unavailable/queued-under-policy rather than repeated spawn storms. Execution returns evidence; it does not install an agent-wide completion gate.
