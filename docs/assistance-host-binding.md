# Assistance host binding

Revision: r2 (provider accepted; Application, Workspace, and Execution acceptance pending). This
document defines the proposed consumer boundary; the current Rust implementation remains the
smaller actor/call parser until those consumers accept it.

`assistance::host_binding` accepts only metadata that a trusted MCP or hook ingress has already separated from model tool arguments. It parses Codex `_meta.threadId`, `_meta.callId`, and the required `x-codex-turn-metadata` object into a `CandidateInvocation`. A candidate is a transport observation, never a workspace authority grant.

`HostBindingGuard` first buffers the exact native `PreToolUse` event. The MCP handler then validates its candidate against that pre-observation and can return its result without waiting for `PostToolUse`. The sequence is `PreToolUse` → MCP validation/result → `PostToolUse`: post is later settlement evidence, not a condition for invocation validation. Child events use `agent_id`; root events use `session_id`; `tool_use_id` must match `_meta.callId`. Missing, ambiguous, mismatched, repeated, oversized, or post-stop data is `Unavailable`. The guard retains bounded pre-observed, active, and completed identifiers and refuses new input when its fixed storage cap is exhausted rather than discarding replay evidence.

The provider surface is deliberately small: `parse_candidate` returns `CandidateInvocation`, and `parse_hook_event` returns `HookEvent`; their getters expose only actor ID, call ID, and hook phase. `HostBindingGuard::register`, `observe_hook`, and `stop` produce `BindingStatus`. Workspace consumes only `ValidatedInvocation` through its actor/call getters; `PreObserved` and `Settled` are lifecycle observations, while `Unavailable` grants nothing. Application owns crate-root exposure and consumer wiring.

The hook parser retains only phase, actor ID, and call ID. It never returns tool input, tool output, source, cwd, transcript paths, or other raw hook fields. Its failure is local and fail-open: it makes the Assistance claim unavailable but cannot block an ordinary native tool, host completion, or existing host permissions.

## Application transport

Assistance accepts Application r2's single finite `assistance.hook_submit` request. It carries `request_id`, `correlation_id`, `opaque_attachment`, and `sanitized_observation_json`, with one opaque correlated reply. Application owns framing, daemon generation, private endpoint and connection limits, byte limits, and total deadline. `submit_hook_if_running` neither creates runtime files nor starts, retries, or repairs a daemon; it never interprets actor, attachment, or hook identity. Assistance owns sanitization, attachment/identity semantics, and the caller's permissive no-inline-retry behavior.

The current five MCP methods need one additional finite request/reply operation, `assistance.method_dispatch`. It carries the same request/correlation/opaque-attachment envelope plus one registered method name and bounded JSON parameters, and returns one opaque correlated result. Application routes it without interpreting method or host semantics; Assistance owns the fixed method set, input validation, binding checks, rendering, and fail-open result. These two operations are the complete v0.1 transport need: no generic event bus, topic subscription, or health-to-RPC upgrade is required.

## Proposed binding liveness

`BindingRef` is an immutable opaque Assistance value for one actor, trusted channel-session, and binding generation. `ValidatedInvocation` will carry actor ID, call ID, and `BindingRef`. An explicit `ide.start` path, after exact Pre→MCP identity validation, is the sole operation that creates a binding generation; a later explicit start after stop creates a fresh one. Ordinary calls can only attach to an already active matching actor/channel-session and cannot silently establish or reactivate a binding.

Assistance will expose `check_active(BindingRef)` and `consume_active(BindingRef) -> ActiveBindingUse`. `ActiveBindingUse` is opaque, revocable liveness evidence, not Workspace authority or an Execution permit. `stop` and `consume_active` serialize at Assistance's liveness boundary: a consume linearized after stop fails. A consume before stop can complete its already admitted local step, but Workspace must persist the `BindingRef` with every provisional grant and require an active check/consume before grant use. Stop invalidates that generation and Workspace revokes or refuses each tagged grant, so no grant published after revocation is usable.

## Proposed sandbox observation

`ObservedSandboxState` will be a separate invocation-correlated value containing actor ID, call ID, `BindingRef`, `AdvertisedAndReturned` provenance, and one bounded opaque host-state object. Its parser requires the advertised `codex/sandbox-state-meta` capability, a matching active validated invocation, and all four source-proven outer field names: `permissionProfile`, `codexLinuxSandboxExe`, `sandboxCwd`, and `useLegacyLandlock`. It preserves the full nested host object opaquely; missing capability, missing/invalid state, or mismatched binding is `Unavailable`. Field types remain uncommitted until the controlled capture succeeds.

Execution r5 is accepted on these terms: every discovery and post-authority admission receives `ActiveBindingUse` from `consume_active(observed.binding)` for the same opaque `BindingRef` and generation, or invokes that exact consume operation itself. A bare `BindingRef` is insufficient because it can race stop. The admission also requires `ObservedSandboxState` and its own D03/local policy gate; it preserves `sandboxCwd` and limits itself to its fixed read-only Git queries. The observation is neither a physical permit, operator evidence, profile selection, nor sandbox-enforcement proof. Execution owns the separate operator-supplied profile catalog, verification evidence, and effect permits; it may compare the host observation with its own policy but cannot derive configuration, evidence, or enforcement from it.

This validation proves only that the supported host metadata and native hook lifecycle agreed for one invocation. It does not cryptographically attest the host transport, grant authority, or prove sandbox enforcement.
