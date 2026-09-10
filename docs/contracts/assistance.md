# Agent integration, scenario facade and feedback

> Historical pre-SPEC-v2 design record. It is not the current v0.1 contract: the current MCP
> surface has exactly five tools (`start`, `context`, `diff`, `inspect`, `stop`). Current behavior
> and remaining host-binding gates are defined in `assistance-v0.1.md` and
> `../assistance-host-binding.md`.

Revision: r3. Provider: Assistance for trusted host attachment and facade/render/delivery; consumers: Workspace and Application, with Codex/Claude as external systems whose support must be tested. Domain inputs are Workspace, Intelligence, Changes and indirect Execution facts. Vocabulary: [common](common.md).

## Host binding and explicit activation

`bind_invocation` returns Bound(HostAttachment) / Unavailable(capability) / Rejected(reason). A trusted attachment identifies runtime/host instance, exact root/native actor, actor-bound transport invocation and delivery channel, proof validity and model context generation. A provider-private evidence reference/handle is verified by the host adapter; it is not a model-provided bearer UUID or a parent PID/cwd guess. IPC peer-user verification is necessary infrastructure but not subagent identity.

Workspace calls `validate_attachment(attachment_ref, InvocationRef)` at activation/start-resume admission. InvocationRef identifies the trusted host instance, connection generation and exact invocation/activation-attempt ID; it does not require an already active AuthorityStamp. Assistance returns Validated(actor, channel binding, context generation, validity/revocation generation, bounded applicability) / Rejected / Unavailable. Validation is local/bounded and cannot wait on the same in-flight MCP operation or require workspace activation. It grants no workspace ownership. The validated result applies only to that admission attempt (or its exact idempotent retry); it is not reusable for another actor/channel/invocation. Workspace must reject expired/revoked evidence at claim commit; revocation racing a committed claim triggers Workspace revocation, not continued silent authority. The adapter owns validity/revocation checks and their tested host evidence; no new cryptographic scheme is presumed.

The exact proof mechanism must be established by the first real host task; do not invent signatures or undocumented runtime metadata. Local contracts require fail-closed Unavailable when proof or active feedback cannot be supported. Host attachment is produced through trusted adapter ingress and never reconstructed by parsing model-visible tool output. Root/native child isolation and no inherited activation are mandatory real checks.

`ide.start` first verifies host attachment plus an active feedback path, then asks Workspace.activate. No scans/LSP/checks are started before success. Same actor/worktree repeat is idempotent; new actor/other worktree/restart cannot reuse stale authority. Resume is an explicit mode of start. Attach/detach of one frontend is not logical stop.

## Nine scenario tools

| Tool | Model inputs | Domain route and output |
|---|---|---|
| start | worktree, optional profile/reuse policy, explicit resume intent | Workspace activation plus Intelligence readiness; current capabilities/baseline |
| context | query or scoped target, optional detail/ref | Intelligence context, bounded exact source/relations/issues |
| impact | target/change intent and scope | Intelligence confirmed consumers/tests and unknown coverage |
| edit | expected snapshot, exact patch or symbol/new-name rename, scope | Changes edit with actual effects and scheduled-check state |
| check | scope and known profile/purpose | Changes current receipt or queued/unavailable state |
| diff | scope and baseline selector within the session | Changes current Git-view classification and verification association |
| status | optional owner-scoped issue/job/detail reference | composed current state; not required polling |
| finish | requested scope | Changes Verified/Waiting/Incomplete/NeedsAction, no facade override |
| stop | explicit stop intent for current bound session | Workspace revocation plus domain cleanup status; no rollback/peer shutdown |

Authority/channel fields are host-bound internal context, not editable model arguments. References resolve only within the bound owner namespace. Validate sizes/unknown keys/ambiguous targets before dispatch. rmcp exposes only these scenario tools. Wire schemas are generated from accepted Rust input types during preparation; protocol capability negotiation is tested on supported client versions.

## Hook/event contract

Normalize only events actually supported by an adapter: before/after native action, action failure, next model boundary, turn end, owner lost, context restored. An inactive shim is silent and schedules nothing; its bounded activity check cannot launch the daemon/LSP to discover activity. An active hook queues invalidation and reads available feedback without synchronously waiting for a compiler/LSP. Unknown shell effects mark coverage unknown through Workspace. Do not add a general native-tool denial gate or rewrite native business payloads. Advice never executes an edit/check or grants permission.

Tool replies, post-native-tool feedback and subsequent-context delivery are separate capabilities. A supported activation requires trusted binding, usable tool replies and at least one verified active feedback path. MCP notification acceptance alone is insufficient. Do not promise every event/channel for every host; publish the measured matrix and reject an unsupported activation mode.

## Rendering and durable delivery

Internal typed result/evidence is the source of truth. Output is bounded English TextContent/Markdown, without duplicate giant structured JSON. Preserve exact selected code and mandatory outcome/freshness/scope/overflow; if a mandatory envelope itself cannot fit, return a compact explicit budget error. Expand via owner-scoped references. Token estimates are labelled estimates unless the host exposes the actual tokenizer/count; byte/character bounds remain hard transport limits. Renderer changes cannot affect domain decisions.

### Diff page delivery freshness

A retained Diff page is an immutable captured snapshot, not proof of the repository's state when it is delivered. The short `inspect` service performs no heavyweight Git recapture, so HEAD/index identities, untracked and conflict sets, and durable current-observation tokens are never revalidated before a page is handed over; only tracked working-tree bytes are rechecked, and a silent out-of-band edit found there fails the delivery closed. Delivery therefore renders freshness `Unknown` for every page, including the first ready delivery of a just-captured one, and reports the freshness Changes computed at capture time separately as `captured_freshness` alongside the untouched captured comparison identities and provenance. Known invalidation still rejects a page outright rather than labelling it: a changed native generation or lost/advanced Workspace authority is a permanent invalidation, which also releases the retained evidence.

Pagination fits one whole page against the actual serialized envelope by selecting fewer whole hunks, never by lowering the captured byte ceiling and never by cutting rendered text; a page that cannot fit even one whole hunk, or a required continuation the retention ceiling refuses to hold, is a compact explicit budget error rather than a silently partial delivery.

Select actionable changes and causal advice from facts, without another LLM per edit. Deduplicate by actor/authority, issue identity/revision and model context generation across channels; per-channel delivery records do not cause the same fact to be repeatedly presented. Compaction may re-present current critical issues once; stopped/stale events stay suppressed. Always expose hidden-count/coverage loss. Stop nudge is disabled by default; configured nudges require a new useful fact and a finite per-turn budget.

Outbox entries carry stable event ID, owner/epoch, relevant snapshot, type/severity, bounded payload, channel/context generation, dedupe key and delivery state. Persist before retryable asynchronous send. Fair bounded delivery revalidates current authority/freshness at initiation, and at model injection when supported. Stop/restart suppress pending stale sends; already host-accepted non-retractable data is explicitly outside an unsend guarantee. No global LSP lock is held while delivering.

Distinguish persisted, queued, transport_accepted, sent_unconfirmed, ambiguous, failed and model_seen. The last state requires evidence that a supported host actually included the event in model context, not that the model acted on it. An ambiguous attempt retains event ID and cannot silently switch channels to create duplicates. TTL/backoff/attempt/fairness/size budgets are configured. Source/server text is untrusted observation, not a new user command.

## Existing adapters and acceptance

An optional agent-run alias can identify a managed child; existing completion relay is terminal-only and parent-targeted, not a general native-subagent bus. Claude UDS successful send without peer ACK remains sent_unconfirmed. These observed limitations do not define universal future host support.

Contract suites: assistance_contracts, assistance_facade_contract, assistance_binding_contract, assistance_hooks_contract, assistance_feedback_contract, assistance_delivery_contract. Fake gateway/adapter/clock/store/channel return fixed contracted outcomes. Real suites assistance_codex_live and assistance_claude_live prove root/native-child identity, off/start/stop, ordinary edit feedback and context visibility on Linux/macOS CLI where supported. Desktop-specific routes are tested only on available OS/app surfaces and never substitute for Linux CLI support. No real-host support is declared by fixtures alone.

## Fail-open frontend and handoff behavior

Assistance's compiled nine-tool registration is available without daemon health. Application supplies independent bounded transport calls; Assistance supplies frontend-local fallback rendering. Hook failure exits permissively within its local budget and schedules no repair/retry loop. A native tool or Stop/turn completion is never denied because IDE/MCP is broken. The integration must not install a mandatory exclusive IDE-only workflow; confirmed IDE faults allow ordinary source/search/edit/check tools under existing host permissions.

Degraded output includes component, operation/effects state (none/known/partial/unknown), unavailable verification, freshness/coverage and one practical native fallback. Partial/unknown edits name exact known affected paths and require inspection-first continuation. A missing lookup after daemon loss is unknown, not no effects. Breaker cooldown suppresses duplicate warnings; optional recovery is bounded or explicitly requested, never a prerequisite for continuing the task. Domain result truth is unchanged: no fabricated Verified result.

For sequential handoff, suppress old queued delivery and revoke the departing channel, then require a new bound/validated host attachment for the reviewer. Workspace admits the successor; Intelligence decides cache reuse and supplies the typed reuse report. Assistance does not infer warm state from equal worktree paths. Real fault checks must demonstrate ordinary native actions and turn completion continuing after daemon/LSP/store/hook failures, not just fake-channel success. Owned harnesses: `tests/assistance_fail_open_contract.rs` and `tests/assistance_handoff_contract.rs`, plus the actual host suites.
