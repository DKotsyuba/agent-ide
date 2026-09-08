# Assistance host binding

Revision: r2 (accepted by Assistance, Application, Workspace, and Execution). The Rust module
implements this bounded binding/state boundary; Application separately owns its IPC framing.

`assistance::host_binding` accepts only metadata that a trusted MCP or hook ingress has already separated from model tool arguments. It parses Codex `_meta.threadId`, `_meta.callId`, and the required `x-codex-turn-metadata` object into a `CandidateInvocation`. A candidate is a transport observation, never a workspace authority grant.

`HostBindingGuard` first buffers the exact native `PreToolUse` event. The MCP handler then validates its candidate against that pre-observation and can return its result without waiting for `PostToolUse`. The sequence is `PreToolUse` → MCP validation/result → `PostToolUse`: post is later settlement evidence, not a condition for invocation validation. Child events use `agent_id`; root events use `session_id`; `tool_use_id` must match `_meta.callId`. Missing, ambiguous, mismatched, repeated, oversized, or post-stop data is `Unavailable`. The guard retains bounded pre-observed, active, and completed identifiers and refuses new input when its fixed storage cap is exhausted rather than discarding replay evidence.

The provider surface is deliberately small: `parse_candidate`, `parse_channel_session`, and `parse_hook_event` create bounded ingress values; `HostBindingGuard::observe_hook`, `establish_start`, `validate_active`, `check_active`, `consume_active`, `stop_binding`, and `stop` manage lifecycle. `ValidatedInvocation` exposes only actor ID, call ID, and opaque `BindingRef`; `ActiveBindingUse` exposes only its same opaque ref. Workspace consumes only `ValidatedInvocation` plus a fresh `ActiveBindingUse`; `PreObserved` and `Settled` are lifecycle observations, while `Unavailable` grants nothing. Application owns crate-root exposure and consumer wiring.

The hook parser retains only phase, actor ID, and call ID. It never returns tool input, tool output, source, cwd, transcript paths, or other raw hook fields. Its failure is local and fail-open: it makes the Assistance claim unavailable but cannot block an ordinary native tool, host completion, or existing host permissions.

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

For Codex, configure **both** native `PreToolUse` and `PostToolUse` commands to invoke
`agent-ide codex-hook --runtime-dir PATH`, restricted by the host's hook matcher to this
MCP server's five `ide.*` tools. Supply the same `AGENT_IDE_HOST_ATTACHMENT` launch
environment value to those commands and the MCP process. It must be a fresh opaque
handle for that host channel/session, nonempty UTF-8 and at most 128 bytes. Distinct host
sessions must use distinct handles; root and native children in the same channel may
share one. Do not put the handle in model arguments or tool output. Missing hook
configuration or mismatched handles leaves calls unavailable. Hooks for unrelated native
tools are not needed and would consume the finite pending/replay budget.

The hook command reads at most 64 KiB plus one overflow byte and uses a separate **250 ms
total deadline** for stdin, parsing, connect, dispatch and reply. An open stdin pipe cannot
hold up process exit. Missing/invalid attachment, absent daemon, invalid or oversized
JSON, malformed or ambiguous identity, transport loss and timeout all exit successfully
without stdout or stderr. It performs one connect-only submission, with no retry,
autostart, workspace scan or LSP work. Configure the command as written; malformed CLI
syntax is a command configuration error, not a hook payload result.

Hook parsing rejects duplicate known JSON keys and retains only phase, actor and call
ID. Root events require `session_id`; child events require `agent_id`; supplying both is
ambiguous. `tool_use_id` identifies the exact call. Tool input/output, cwd, transcript
paths and all other raw fields are discarded before IPC. The raw hook payload is never
logged or retained in daemon state.

The MCP ingress obtains `threadId`, `callId` and the presence of the supported
`x-codex-turn-metadata` object from rmcp `RequestContext.meta`, separately from model
arguments. It forwards selected actor/call fields and an empty support marker, never the
turn object contents. Assistance owns the opaque method parameter envelope
`{"parameters":...,"host_meta":...}`; Application only frames it. The MCP request ID
and exact call ID remain finite transport request/correlation values. All matching is by
**attachment + actor + call**, never argument equality, timing, CWD, PID or parent identity.

Only an exact pre-hook followed by `ide.start` creates an actor/channel binding. Later
ordinary methods require their own matching pre-hook and that binding's current liveness.
Post-hooks settle previously validated invocations after MCP result delivery. Duplicate
pre-hooks, premature post-hooks and MCP-before-pre ordering reject that invocation for
the remaining daemon lifetime; late hooks cannot repair it. Explicit stop revokes the
exact binding and rejects its pending pre-hooks before any Workspace handoff could occur.
Post settlement for already validated calls can still complete. A fresh explicit start
is required after stop; another actor's binding is unaffected.

State is bounded to 128 pending/settling invocations, 64 active bindings, 1024 completed
identities and 1024 rejected identities. Replay evidence is never evicted to make room.
Exhaustion makes further binding operations unavailable. Daemon restart discards all
bindings and requires fresh exact pre/start input; it does not recover authority.

Closed daemon outcomes are:

| Outcome | Meaning |
| --- | --- |
| `{"state":"unavailable","reason":"host_binding"}` | No validated exact invocation. |
| `{"state":"unavailable","reason":"workspace_activation"}` | Host invocation and current binding are proven; Workspace activation is not connected. |
| `{"state":"hook_observed"}` | One pre-hook was retained; no authority or delivery claim. |
| `{"state":"hook_settled"}` | One exact post-hook settled a validated invocation. |
| `{"state":"host_stopped"}` | The exact host binding was revoked; no Workspace authority was created. |

The MCP facade renders only the corresponding closed method outcomes. Stop reports host
binding revocation, while other methods still direct the model to native tools. Unknown
states or extra fields cannot become successful peer results. Hook transport submission
is not proof of binding or model-context delivery.

This adapter relies on the trusted launcher and the existing private local daemon endpoint;
it does not cryptographically authenticate local processes or attest sandbox enforcement.
The next gate is Workspace activation/revocation with controlled Execution admission and
Git discovery. Full invocation-correlated sandbox observation is not assembled in the
product dispatcher. Intelligence context, Changes diff/detail and delivery visibility
remain separate missing gates. No successful `start → context`, `start → diff`,
`model_seen`, Claude support or end-user IDE readiness is claimed.

Verify with `cargo test --offline --test product_mcp_contract --test assistance_binding_contract
--test assistance_facade_contract --test app_ipc_contract` (one command). The real binary
MCP/Unix IPC tests cover parallel root/child actors with identical inputs and correlation
IDs, cross-actor rejection, stop isolation, replay/order failures, daemon loss, inactive
and malformed hooks, open stdin and hung-daemon deadlines, and discarded payload fields.
These are controlled host-shaped process tests using the established Codex field contract;
they do not substitute for a fresh live Codex host acceptance scenario. Live Claude
root/subagent behavior and feedback delivery are unverified.
