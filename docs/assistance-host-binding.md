# Assistance host binding

Revision: r3 (accepted by Assistance; Application still transports the hook reply opaquely). The Rust module
implements this bounded binding/state boundary; Application separately owns its IPC framing.

`assistance::host_binding` accepts only metadata that a trusted MCP or hook ingress has already separated from model tool arguments. Host kind is selected explicitly and is never inferred from CWD, PID, timing, permission mode, or arbitrary arguments. It parses Codex `_meta.threadId`, `_meta.callId`, and the required `x-codex-turn-metadata` object into a `CandidateInvocation`. A candidate is a transport observation, never a workspace authority grant.

`HostBindingGuard` first buffers the exact native `PreToolUse` event. The MCP handler then validates its candidate against that pre-observation and can return its result without waiting for `PostToolUse`. The sequence is `PreToolUse` → MCP validation/result → `PostToolUse`: post is later settlement evidence, not a condition for invocation validation. Child events use `agent_id`; root events use `session_id`; `tool_use_id` must match `_meta.callId`. Missing, ambiguous, mismatched, repeated, oversized, or post-stop data is `Unavailable`. The guard retains bounded pre-observed, active, and completed identifiers and refuses new input when its fixed storage cap is exhausted rather than discarding replay evidence.

The provider surface is deliberately small: `parse_candidate`, `parse_channel_session`, and `parse_hook_event` create bounded ingress values; `HostBindingGuard::observe_hook`, `establish_start`, `validate_active`, `take_native_change_hint`, `check_active`, `consume_active`, `stop_binding`, and `stop` manage lifecycle. `ValidatedInvocation` exposes only actor ID, call ID, and opaque `BindingRef`; `ActiveBindingUse` exposes only its same opaque ref. Workspace consumes only `ValidatedInvocation` plus a fresh `ActiveBindingUse`; `PreObserved` and `Settled` are lifecycle observations; `NativeObserved` is only an active registered-path recheck hint, while `Unavailable` grants nothing. Application owns crate-root exposure and consumer wiring.

The Codex parser preserves the established root `session_id` versus child `agent_id` mapping and exact `tool_use_id`. The Claude parser requires exact `session_id`, selects optional `agent_id` as the subagent actor, and retains optional `agent_type` only as descriptive data. It never treats `permission_mode` as OS sandbox authority. `PostToolBatch` without a tool ID remains uncorrelated rather than being synthesized as `session_id`. Both parsers discard tool input/output, source, paths, permission data, and unknown fields. Failure is local and fail-open.

## Application transport

Assistance accepts Application r2's single finite `assistance.hook_submit` request. It carries `request_id`, `correlation_id`, `opaque_attachment`, and `sanitized_observation_json`, with one opaque correlated reply. Application owns framing, daemon generation, private endpoint and connection limits, byte limits, and total deadline. `submit_hook_if_running` neither creates runtime files nor starts, retries, or repairs a daemon; it never interprets actor, attachment, or hook identity. Assistance owns sanitization, attachment/identity semantics, and the caller's permissive no-inline-retry behavior.

The current five MCP methods need one additional finite request/reply operation, `assistance.method_dispatch`. It carries the same request/correlation/opaque-attachment envelope plus one registered method name and bounded JSON parameters, and returns one opaque correlated result. Application routes it without interpreting method or host semantics; Assistance owns the fixed method set, input validation, binding checks, rendering, and fail-open result. These two operations are the complete v0.1 transport need: no generic event bus, topic subscription, or health-to-RPC upgrade is required.

## Proposed binding liveness

`BindingRef` is an immutable opaque Assistance value for one actor, trusted channel-session, and binding generation. `ValidatedInvocation` carries actor ID, call ID, and `BindingRef`. An explicit `ide.start` path, after exact Pre→MCP identity validation, is the sole operation that creates a binding generation; a later explicit start after stop creates a fresh one. Ordinary calls can only attach to an already active matching actor/channel-session and cannot silently establish or reactivate a binding.

Assistance exposes `check_active(BindingRef)` and `consume_active(BindingRef) -> ActiveBindingUse`. `ActiveBindingUse` is opaque, revocable liveness evidence, not Workspace authority or an Execution permit. `stop` and `consume_active` serialize at Assistance's liveness boundary: a consume linearized after stop fails. A consume before stop can complete its already admitted local step, but Workspace must persist the `BindingRef` with every provisional grant and require an active check/consume before grant use. Stop invalidates that generation and Workspace revokes or refuses each tagged grant, so no grant published after revocation is usable.

## Proposed sandbox observation

`ObservedSandboxState` is a separate invocation-correlated value containing actor ID, call ID, `BindingRef`, `AdvertisedAndReturned` provenance, and one bounded opaque host-state object. Its parser requires the advertised `codex/sandbox-state-meta` capability, a matching active validated invocation plus its `ActiveBindingUse`, and all four source-proven outer field names: `permissionProfile`, `codexLinuxSandboxExe`, `sandboxCwd`, and `useLegacyLandlock`. It preserves the full nested host object opaquely; missing capability, missing/invalid state, or mismatched binding is `Unavailable`. Field types remain uncommitted until the controlled capture succeeds.

Execution r5 is accepted on these terms: every discovery and post-authority admission receives `ActiveBindingUse` from `consume_active(observed.binding)` for the same opaque `BindingRef` and generation, or invokes that exact consume operation itself. A bare `BindingRef` is insufficient because it can race stop. The admission also requires `ObservedSandboxState` and its own D03/local policy gate; it preserves `sandboxCwd` and limits itself to its fixed read-only Git queries. The observation is neither a physical permit, operator evidence, profile selection, nor sandbox-enforcement proof. Execution owns the separate operator-supplied profile catalog, verification evidence, and effect permits; it may compare the host observation with its own policy but cannot derive configuration, evidence, or enforcement from it.

This validation proves only that the supported host metadata and native hook lifecycle agreed for one invocation. It does not cryptographically attest the host transport, grant authority, or prove sandbox enforcement.

## Product MCP boundary

`agent-ide mcp --runtime-dir PATH` serves exactly `ide.start`, `ide.context`,
`ide.diff`, `ide.inspect`, and `ide.stop` over stdio. It validates closed model-argument
schemas, writes only MCP protocol messages to stdout, and never creates runtime state,
starts a daemon, or repairs transport. Start `agent-ide daemon --runtime-dir PATH`
separately. Its `ProductDispatcher` retains one bounded `HostBindingGuard` for the daemon
lifetime, serialized across all finite Unix IPC connections.

For Codex, configure native `PreToolUse`, `PostToolUse`, and available `PostToolBatch` commands to invoke
`agent-ide codex-hook --runtime-dir PATH` for supported native tools as well as this MCP
server's five `ide.*` tools. The placeholder-only example is
[`docs/examples/codex-hooks.toml`](examples/codex-hooks.toml). Supply the same `AGENT_IDE_HOST_ATTACHMENT` launch
environment value to those commands and the MCP process. It must be a fresh opaque
handle for that host channel/session, nonempty UTF-8 and at most 128 bytes. Distinct host
sessions must use distinct handles; root and native children in the same channel may
share one. Do not put the handle in model arguments or tool output. Missing hook
configuration or mismatched handles leaves calls unavailable. Ordinary edits, deletes,
renames and failed commands must remain observable after activation; their hook payloads
are not a source-effect authority.

Claude Code uses the separate explicit `claude-hook` mode and its documented `session_id`,
optional `agent_id`, and optional `agent_type` fields. The placeholder-only example is
[`docs/examples/claude-settings.json`](examples/claude-settings.json). Claude hook ingress is
implemented, but Claude MCP calls do not claim Codex sandbox evidence. Without a separately proven
execution profile, provider children remain fail closed; `permission_mode` is never mapped to OS
authority. Already-authorized product state may still return safe current source feedback.

The hook command reads at most 64 KiB plus one overflow byte and uses a separate **250 ms
total deadline** for stdin, parsing, connect, dispatch and reply. An open stdin pipe cannot
hold up process exit. Missing/invalid attachment, absent daemon, invalid or oversized
JSON, malformed or ambiguous identity, transport loss and timeout all exit successfully
without stdout or stderr. It performs one connect-only submission, with no retry,
autostart, workspace scan or LSP work. Configure the command as written; malformed CLI
syntax is a command configuration error, not a hook payload result.

Hook parsing rejects duplicate known JSON keys and retains only explicit host, phase, bounded
identity, and optional call ID. Codex root events require `session_id`, Codex child events require
`agent_id`, and supplying both is ambiguous. Claude always retains `session_id` and selects its
optional `agent_id` as the child actor. `tool_use_id` identifies an exact per-tool call; a batch
does not fabricate one. Tool input/output, cwd, transcript paths and all other raw fields are discarded before IPC. The raw hook payload is never
logged or retained in daemon state. `Debug` formatting of the trusted transport, opaque
JSON, hook/method requests and nested dispatch/reply wrappers redacts private fields,
including echoed hook correlations; it cannot be used to print attachment or payload data.

The MCP ingress obtains `threadId`, `callId` and the presence of the supported
`x-codex-turn-metadata` object from rmcp `RequestContext.meta`, separately from model
arguments. It advertises `codex/sandbox-state-meta` and preserves that complete measured
object alongside selected actor/call fields and an empty turn-support marker; arbitrary
turn object contents are discarded. After exact binding, Assistance validates the
correlated sandbox observation and Execution's supported profile shape. Assistance owns the opaque method parameter envelope
`{"parameters":...,"host_meta":...}`; Application only frames it. The MCP request ID
and exact call ID remain finite transport request/correlation values. All matching is by
**attachment + actor + call**, never argument equality, timing, CWD, PID or parent identity.

Only an exact pre-hook followed by `ide.start` creates an actor/channel binding. Later
ordinary methods require their own matching pre-hook and that binding's current liveness.
Post-hooks settle previously validated invocations after MCP result delivery. A complete
native Pre/Post lifecycle without an MCP invocation coalesces a registered-path recheck
hint only for an already active binding. This applies equally to successful and failed
commands: actor/call/phase are sufficient triggers; command text, paths and tool results
are never trusted as effects. `take_native_change_hint` consumes that bounded hint after
a fresh liveness check. The worker invalidates old detail immediately, then reconciles only
registered paths when the next MCP invocation supplies a current sandbox observation.
Duplicate pre-hooks, premature post-hooks and MCP-before-pre ordering reject that
invocation for subsequent MCP validation in the remaining daemon lifetime; late hooks cannot repair it. Explicit stop revokes the
exact binding and rejects its pending pre-hooks before any Workspace handoff could occur.
Post settlement for already validated calls can still complete. A fresh explicit start
is required after stop; another actor's binding is unaffected.

State is bounded to 128 pending/settling invocations, 64 active bindings, 1024 completed
identities, 1024 rejected identities and at most one coalesced native hint per active
binding. Native lifecycle identities consume the same finite replay budget. Replay
evidence is never evicted to make room.
Exhaustion makes further binding operations unavailable. Daemon restart discards all
bindings and requires fresh exact pre/start input; it does not recover authority.

Closed daemon outcomes are:

| Outcome | Meaning |
| --- | --- |
| `{"state":"unavailable","reason":"host_binding"}` | No validated exact invocation. |
| `{"state":"unavailable","reason":"workspace_activation"}` | Host invocation and current binding are proven; Workspace activation is not connected. |
| `{"state":"hook_observed"}` | One pre-hook was retained; no authority or delivery claim. |
| `{"state":"hook_settled"}` | One exact post-hook settled a validated invocation. |
| `{"state":"native_hook_observed"}` | Active native lifecycle requested a registered-path recheck; no source effect is claimed. |
| `{"state":"feedback","text":"..."}` | One positive-version delta survived the current binding and exact-source recheck. |
| `{"state":"host_stopped"}` | The exact host binding was revoked; no Workspace authority was created. |

The MCP facade renders only the corresponding closed method outcomes. Without trusted
configuration, stop reports host binding revocation and other methods direct the model to native tools. Unknown
states or extra fields cannot become successful peer results. Hook transport submission
is not proof of binding or model-context delivery.

This adapter relies on the trusted launcher and the existing private local daemon endpoint;
it does not cryptographically authenticate local processes or attest sandbox enforcement.
Physical execution requires trusted configured profile evidence and a fresh spawn use.
Configured product fixtures exercise `start → context`, `start → diff` and `stop` through
the shipping CLI/MCP boundary. Host-shaped fixtures also exercise parent/subagent parsing,
closed output JSON, stale suppression, malformed input and daemon loss. They do not establish
`model_seen`, live-host support or end-user IDE readiness.

Verify with `cargo test --offline --test product_mcp_contract --test assistance_binding_contract
--test assistance_facade_contract --test app_ipc_contract` (one command). The real binary
MCP/Unix IPC tests cover parallel root/child actors with identical inputs and correlation
IDs, cross-actor rejection, stop isolation, replay/order failures, daemon loss, inactive
and malformed hooks, open stdin and hung-daemon deadlines, and discarded payload fields.
Native edit/delete/rename and failed-command-shaped lifecycle fixtures verify coalesced
active hints and suppression after stop, without claiming those fixtures changed source.
These are controlled host-shaped process tests, not live-host proof. Still unverified are fresh
Codex and Claude CLI acceptance on macOS and Linux, Claude MCP invocation correlation/activation,
real `PostToolBatch` availability in supported host versions, and model-visible delivery of a
real-provider delta after an actual edit.

The executable deadline regressions enforce a 450 ms wall-clock ceiling: the 250 ms
product deadline plus 200 ms for child startup and scheduling. Child-process Tokio clocks
are independent of the test runtime, so paused test time cannot control these paths.
The allowance remains below doubled (500 ms) and sixfold (1500 ms) timeout regressions;
both open-stdin and hung-daemon scenarios keep their real subprocess/Unix IPC boundary.

`ide.context` now requires a relative `path` (at most 1024 UTF-8 bytes), with optional
`byte_offset` (0..1048576) and `detail_ref`; absolute/traversal/NUL paths are rejected.
`ide.diff` accepts only `head`, `staged`, or `unstaged` mode, defaulting to `head`. Both
MCP discovery and runtime validation use the same schema definitions. Identity, profiles
and candidate worktrees remain outside model arguments.

Daemon launch may supply `AGENT_IDE_LAUNCHER_CONFIG` naming the bounded restart-only
[trusted configuration](assistance-launcher.md). The closed result protocol additionally
supports `pending` with a same-binding `detail_ref`, fixed error codes, and owner-produced
`complete` results. The serialized envelope is capped at 64 KiB including escaping;
UTF-8-safe owner-text truncation is explicit. MCP exposes structured content, the standard
text fallback and a fixed short summary. Their combined serialized size has its own bound,
including reserve for JSON-RPC framing.

Cold executable startup is bounded separately by an inactive fixture invocation before
measuring the hung-daemon case. The observed test-harness cold start can exceed 800 ms
before the first hook instruction; the subsequent real ingress still uses the unchanged
450 ms test ceiling and 250 ms product deadline.

Configured daemon startup now creates one bounded job/detail worker only after Application
holds its exclusive runtime lock. That worker opens one DurableWorkspace owner and the
registered-source schema for the boot. A rejected second daemon cannot advance the
Workspace boot fence. Hook binding locks are released before every queue or Store await.
Each daemon contributes a fresh opaque nonce to the effective hook/MCP channel; both
paths still match the same exact attachment/actor/call. Detail and SQLite operation
references are boot-unique without using PID, timing or cwd as identity.

Long operations are retained in a bounded queue and return pending details. The same
worker services short inspections during pending work; results require same-binding
ownership, fresh durable authority and source/native-revision rechecks. Native lifecycle
hints trigger only registered-path reconciliation under the next current invocation.
Activation consumes three controlled Git discovery results and commits durable authority
before successful results are visible. It then attempts a durable baseline capture from three
fixed, filter-free Git metadata reads. A stored v0.1 baseline is reported as partial with an
unverified joint window; a capture failure leaves activation usable but reports baseline coverage
as unknown. Exact activation retries reuse committed facts.

Source context reads one registered relative path under current sandbox and durable authority.
Optional accepted Go/Rust profiles supply semantic results over those exact bytes; absent or
unavailable providers return explicit lexical context. Compatible Go worktrees share one
accounted listener with separate protocol forwarders; Rust uses an exclusive session.
Semantic replies include only bounded diagnostics from the same Session when source binding,
provider generation, and positive document version all match the returned context. Push feedback
is labelled provisional. One nonempty current delta per binding is retained. A later native post
increments that binding's epoch, rechecks the exact registered bytes, consumes the delta once, and
emits only `hookSpecificOutput.{hookEventName,additionalContext}`. Changed, empty, stale,
unversioned, stopped, missing-daemon and already-consumed feedback emits no model context. Hook
output never includes source bodies, raw diagnostics, native tool input/output, attachments or
launcher data.
Requests have at most a 60-second protocol deadline within the configured operation budget;
warmup remains pending while short inspections and stop remain available.

An initial operation reserves its independent inspection-channel slot before publishing its
job, pending detail, or stable start reference. A full live inspection channel returns `capacity`
without consuming any of those ledgers; a closed inspection service returns `internal`. Graceful
daemon SIGINT/SIGTERM fences new work, cancels queued and active work, reaps owned providers, and
removes owned provider sockets before exit. SIGKILL cannot provide those cleanup guarantees.

Git comparisons compose typed HEAD/index/worktree snapshots. Discovery and capture use
controlled filter-free commands and direct-child reap evidence. Results retain their
non-atomic snapshot coverage and explicit unknown/not-captured baseline facts. A current stored
activation baseline accompanies same-scope HEAD comparisons; staged and unstaged comparisons keep
their distinct scope and therefore report the baseline as not captured rather than crossing modes.

`ide.stop` revokes only the exact binding, cancels pending work and reaps its owned provider
processes before success. A shared listener survives another active Go view. Stop is not a
worktree closure or cache-retirement fact: configured provider cache namespaces are quiesced and
retained under canonical worktree/incarnation identity for a compatible successor. Only the
existing verified Workspace closure or explicit reset fact may retire the retained namespace.
Restart discards bindings and detail references;
new activation uses a boot-specific channel identity and the durable native-identity fence.
Stop also reclaims that binding's queued jobs and retained start/detail references, without
evicting a live peer's results. Active retained details fail with `capacity` at their configured
limit; the worker never silently evicts live retry evidence to admit another operation.
