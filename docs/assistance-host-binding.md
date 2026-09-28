# Assistance host binding

Revision: r3 (accepted by Assistance; Application still transports the hook reply opaquely). The Rust module
implements this bounded binding/state boundary; Application separately owns its IPC framing.

`assistance::host_binding` accepts only metadata that a trusted MCP or hook ingress has already separated from model tool arguments. Host kind is selected explicitly and is never inferred from CWD, PID, timing, permission mode, or arbitrary arguments. It parses Codex `_meta.threadId`, `_meta.callId`, and the required `x-codex-turn-metadata` object into a `CandidateInvocation`. A candidate is a transport observation, never a workspace authority grant.

In legacy Codex and Claude mode, `HostBindingGuard` first buffers the exact native `PreToolUse` event. The MCP handler then validates its candidate against that pre-observation and can return its result without waiting for `PostToolUse`. The sequence is `PreToolUse` → MCP validation/result → `PostToolUse`: post is later settlement evidence, not a condition for invocation validation. `PostToolUseFailure` settles the same exact call and may request a later registered-path recheck because partial effects remain possible. `PermissionDenied` rejects its exact pending lifecycle without a recheck hint; `PermissionRequest` and manual denial are uncorrelated and unsupported. Child events select `agent_id` while validating the accompanying root `session_id`; root events use `session_id`. `tool_use_id` must match `_meta.callId`. Missing, invalid, mismatched, repeated, oversized, or post-stop data is `Unavailable`. The guard retains bounded pre-observed, active, and completed identifiers and refuses new input when its fixed storage cap is exhausted rather than discarding replay evidence.

The provider surface is deliberately small: `parse_candidate`, `parse_channel_session`, and `parse_hook_event` create bounded ingress values; `HostBindingGuard::observe_hook`, `establish_start`, `validate_active`, `take_native_change_hint`, `check_active`, `consume_active`, `stop_binding`, and `stop` manage lifecycle. `ValidatedInvocation` exposes only actor ID, call ID, and opaque `BindingRef`; `ActiveBindingUse` exposes only its same opaque ref. Workspace consumes only `ValidatedInvocation` plus a fresh `ActiveBindingUse`; `PreObserved` and `Settled` are lifecycle observations; `NativeObserved` is only an active registered-path recheck hint, while `Unavailable` grants nothing. Application owns crate-root exposure and consumer wiring.

The Codex parser selects root `session_id` for a parent. A native child supplies both its own `agent_id` and the root `session_id`; the parser validates both identifiers, selects `agent_id` as the actor, and discards the root session before binding. It preserves exact `tool_use_id`. The Claude parser requires exact `session_id`, selects optional `agent_id` as the subagent actor, and retains optional `agent_type` only as descriptive data. It never treats `permission_mode` as OS sandbox authority. `PostToolBatch` without a tool ID remains uncorrelated rather than being synthesized as `session_id`. Both parsers discard tool input/output, source, paths, permission data, and unknown fields. Failure is local and fail-open.

## Application transport

Assistance accepts Application r2's single finite `assistance.hook_submit` request. It carries `request_id`, `correlation_id`, `opaque_attachment`, and `sanitized_observation_json`, with one opaque correlated reply. Application owns framing, daemon generation, private endpoint and connection limits, byte limits, and total deadline. `submit_hook_if_running` neither creates runtime files nor starts, retries, or repairs a daemon; it never interprets actor, attachment, or hook identity. Assistance owns sanitization, attachment/identity semantics, and the caller's permissive no-inline-retry behavior.

The v0.1 five MCP methods use one additional finite request/reply operation, `assistance.method_dispatch`. It carries the same request/correlation/opaque-attachment envelope plus one registered method name and bounded JSON parameters, and returns one opaque correlated result. Application routes it without interpreting method or host semantics; Assistance owns the fixed method set, input validation, binding checks, rendering, and fail-open result. Version 3 extends that closed set only with `edit` as defined by [EDIT-r1](contracts/assistance-v0.2.md); it remains no generic event bus, topic subscription, or health-to-RPC upgrade.

## Proposed binding liveness

`BindingRef` is an immutable opaque Assistance value for one actor, trusted channel-session, and binding generation. `ValidatedInvocation` carries actor ID, call ID, and `BindingRef`. An explicit `ide.start` path, after exact Pre→MCP identity validation, is the sole operation that creates a binding generation; a later explicit start after stop creates a fresh one. Ordinary calls can only attach to an already active matching actor/channel-session and cannot silently establish or reactivate a binding.

Assistance exposes `check_active(BindingRef)` and `consume_active(BindingRef) -> ActiveBindingUse`. `ActiveBindingUse` is opaque, revocable liveness evidence, not Workspace authority or an Execution permit. `stop` and `consume_active` serialize at Assistance's liveness boundary: a consume linearized after stop fails. A consume before stop can complete its already admitted local step, but Workspace must persist the `BindingRef` with every provisional grant and require an active check/consume before grant use. Stop invalidates that generation and Workspace revokes or refuses each tagged grant, so no grant published after revocation is usable.

## No sandbox observation

The daemon reads no host sandbox state. Earlier revisions correlated a `codex/sandbox-state-meta`
object to each invocation and replayed it for owned children; that layer is removed. Execution
receives only the consumed `ActiveBindingUse` for the same opaque `BindingRef` and generation,
and the only path policy is the launcher `allowed_roots` list applied by Assistance at activation.
Host binding proves only that the supported host metadata and native hook lifecycle agreed for
one invocation; it does not cryptographically attest the host transport, grant authority, or
prove sandbox enforcement.

## Product MCP boundary

### Claude route

Claude uses the same daemon route as Codex: MCP calls reach the daemon and the worker executes
the operation. Claude identity is correlated by `claudecode/toolUseId` against the native hook
pre-observation. `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, and `PermissionDenied` hooks
advance the native epoch and invalidate stale source references; inert tools do not. The managed
Claude shared daemon remains one daemon per repository, with hooks rendezvousing to it.

`agent-ide mcp --launcher-template ABSOLUTE_PATH` is the standard self-contained Codex entrypoint.
It serves exactly `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop` over stdio.
At startup it captures the subprocess current directory once, creates one fresh private runtime,
rebinds only the attachment and candidate of an otherwise unchanged single-target launcher
template, validates the existing launcher/execution evidence, and starts and health-checks one owned
daemon. Stdio EOF, cancellation, SIGINT, SIGTERM, and startup failure terminate and reap that exact
daemon before identity-checked removal of its runtime tree. A failed startup serves the same static
tool list through a deliberately disconnected facade whose calls return bounded native fallback.

Legacy `agent-ide mcp --runtime-dir PATH` remains connect-only: it never creates runtime state,
starts a daemon, or repairs transport, and its separately started daemon retains the existing
hook-correlated behavior. Both forms validate the same closed model-argument schemas and write only
MCP protocol messages to stdout.

`agent-ide mcp --claude-launcher-template ABSOLUTE_PATH` is the explicit self-contained Claude
entrypoint. It captures only an absolute, normalized `CLAUDE_PROJECT_DIR`, canonicalizes that
directory once, and derives `/private/tmp/ai-r-<16 lowercase hex>` directly from the leading 64 bits
of the BLAKE3 digest of the repository's canonical git common directory (or the canonical project
root itself outside a git repository), so every worktree of one repository shares one rendezvous.
Per EYES-r1 §2/EYES-r2, the MCP creates that directory only when no live lock holder answers it; an
existing directory whose lock is held and whose health endpoint answers is instead explicitly
validated (owner, mode `0700`, non-symlink, device/inode re-checked from an open file descriptor)
and adopted, never repaired or removed. The owner binds the template's sole target to the captured
project and a fresh random attachment, writes the bound launcher and project-bound attachment record with mode `0600`, then
starts and health-checks the daemon through the existing legacy Claude route. It serves the v0.1
five tools and the v0.2 `edit` tool. Per
EYES-r1 §2, this MCP never owns that daemon's lifetime: stdio EOF, cancellation, SIGINT, or SIGTERM
end this one MCP process only, leaving an adopted or spawned daemon running for the next MCP of the
same repository to find.

A host that moves a project never restarts this MCP process, so a session can stay bound to a
directory its hooks no longer run in (T15B). The first `ide.start {root}` naming a different
directory re-roots the session before dispatching: the MCP canonicalizes and admits the root against
the template's `allowed_roots` — the same single rule activation applies, with no additional
permission layer — then attaches through the exact fresh-session path of that directory (key cache,
shared daemon, client lease, candidate attachment) and binds the session there. The re-rooted call's
own pre-hook necessarily ran before the new rendezvous existed, so its first reply is the
cause-tagged refusal plus a stable `retry` hint rather than a hard failure, and the next call pairs
normally. A root below no allowed root is never re-rooted to; the daemon's own
`outside_allowed_roots` error answers. A re-root that cannot attach answers
`unavailable: host_binding (project_moved: bound to <path>, asked <path>)`.

The plugin's `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, and `PermissionDenied` handlers run
the argument-free `agent-ide claude-hook`. That command resolves the hook payload's canonical `cwd`
to the nearest worktree with a private candidate cache, then validates the shared runtime and bounded
attachment file's owner, exact modes, shape, and full repository digest. It reads that worktree's
daemon-minted lease attachment from the private cache and reuses the existing Claude parser and
connect-only transport. Missing
or corrupt state is silent fail-open. Root and child lifecycle identity, permission denial, failed-tool settlement and feedback output are unchanged.
The installed binary and launcher template remain machine-specific values in normal Claude MCP
configuration; they are not embedded in the plugin manifest.

For legacy Codex mode, configure native `PreToolUse`, `PostToolUse`, and available `PostToolBatch` commands to invoke
`agent-ide codex-hook --runtime-dir PATH` for supported native tools as well as this MCP
server's five `ide.*` tools. The placeholder-only example is
[`docs/examples/codex-hooks.toml`](examples/codex-hooks.toml) (legacy, operator-run daemon).
Supply the same `AGENT_IDE_HOST_ATTACHMENT` launch
environment value to those commands and the MCP process. It must be a fresh opaque
handle for that host channel/session, nonempty UTF-8 and at most 128 bytes. Distinct host
sessions must use distinct handles; root and native children in the same channel may
share one. Do not put the handle in model arguments or tool output. Missing hook
configuration or mismatched handles leaves calls unavailable. Ordinary edits, deletes,
renames and failed commands must remain observable after activation; their hook payloads
are not a source-effect authority.

## Managed Codex native hooks (T29B)

Managed Codex MCP servers additionally publish private, actor-addressed rendezvous records so
native hooks can find the same daemon without any operator-configured runtime or credential.

- **Discovery.** The daemon destination is derived solely from bounded hook identity — root
  session `session_id` and actor `agent_id.unwrap_or(session_id)` — matched against private
  records under `canonical("/private/tmp")/ai-c-<euid>/<route digest>/<nonce>.json`. The route
  digest is a versioned, domain-separated, length-framed BLAKE3 over effective UID, root session,
  and actor; it is never derived from CWD, repository, PID, tool arguments, or timing. A record is
  live only while its publisher's advisory lock is held; exactly one live record is eligible, and
  zero or several live records (or any unsafe state) are a silent no-match. Discovery never
  repairs, deletes, or picks the newest record, and never starts a daemon. The hook submits the
  record's published attachment — the same credential the MCP bound at startup — through the
  ordinary `assistance.hook_submit` route, so a served hook submission is an ordinary served call
  that also restarts the daemon's idle countdown (T26B).
- **Filesystem and security checks.** Directories must be owned by the effective UID, be exactly
  mode `0700`, and be real directories; records must be owned, exactly `0600`, regular, bounded,
  and not hard-linked. Components are opened descriptor-relative with `O_NOFOLLOW` and validated
  with `fstat`; records are published by atomic rename; runtimes and sockets are re-verified
  through pinned descriptors before use. Unsafe state is rejected without chmod, repair, or
  following symlinks — including when the location was set through the environment.
- **`AGENT_IDE_CODEX_RENDEZVOUS_ROOT`.** This environment override redirects publication and
  discovery away from the fixed root. It exists for tests; the product never sets it. Every safety
  check above (owner, exact `0700`/`0600`, no symlinks) still applies to an overridden root — an
  overridden root with loose permissions is refused by the managed hook with a silent exit 0,
  which a product contract test verifies.
- **Threat model.** Trusted: the installed binary, owner-managed configuration, the host metadata
  ingress, and cooperating same-UID processes. Publishing records deliberately removes obscurity
  against processes of the same owner. There is **no protection against a malicious same-UID
  process**: it could discover credentials more easily than before, forge observations, consume
  feed delivery, or attempt existing authenticated RPCs. The protection goal is other users,
  accidental cross-session/cross-actor routing, stale generations, and unsafe filesystem objects —
  the same documented trust assumption as the Claude worker, not OS attestation; Unix-socket
  pathname connection still relies on the trusted same-user boundary.
- **Managed pairing exception.** Managed MCP admission accepts an otherwise-valid, exact buffered
  pre-observation and consumes it instead of rejecting it as a replay (a catch-all hook observes
  the pre phase of the MCP call itself). Completion is recorded exactly as before, duplicated
  pre-events and completed/rejected call IDs stay rejected, and the call's own later native post is
  then an ordinary completed replay: it stays silent, triggers no check, and cannot invalidate the
  result the MCP call just produced.
- **Supported phases.** Managed mode accepts only `PreToolUse` and `PostToolUse`; Codex has no
  `PostToolUseFailure`, `PostToolBatch`, or `PermissionDenied` handler shipped for it, and none is
  synthesized. The legacy parser keeps accepting the other phases for compatibility.
- **Capacity.** More native events consume the existing bounded replay storage faster: 128
  pending per exact channel/host/actor scope and 1,024 retained rejected/completed IDs per scope,
  never evicted (see the bounds paragraph below). A long-lived session that exhausts its scope
  stays unavailable for that scope until daemon restart.

`agent-ide codex-hook --managed` is the managed hook command: it ignores every credential and
runtime environment override, reads stdin once (at most 64 KiB plus an overflow byte) inside one
250 ms total deadline, discovers exactly one live route, submits once, and renders only a
successful `PostToolUse` feedback result. Missing, ambiguous, stale, or contended routes,
malformed or oversized payloads, unsupported phases, and deadline expiry all exit 0 with empty
stdout. `agent-ide codex-hooks print` prints the managed `hooks.json` fragment (JSON only, the
installed path shell-quoted) — the placeholder-only example is
[`docs/examples/codex-hooks.json`](examples/codex-hooks.json). The owner merges the two handlers
into the existing `PreToolUse`/`PostToolUse` arrays of `~/.codex/hooks.json` preserving existing
entries, then reviews and trusts both definitions through Codex's normal UI; the product never
writes `hooks.json` or any host configuration, and duplicate agent-ide handlers must not be
registered (duplicate pre-events are rejected as replays).

Claude Code 2.1.267 is the tested target. Its example config includes exact `PreToolUse`,
`PostToolUse`, `PostToolUseFailure`, and `PermissionDenied` commands; older Claude versions
are not certified. `PermissionRequest` has no exact tool-use ID and manual denial has no
correlated terminal hook, so neither settles a pending call.

Claude Code uses the separate explicit `claude-hook` mode and its documented `session_id`,
optional `agent_id`, and optional `agent_type` fields. The placeholder-only example is
[`docs/examples/claude-settings.json`](examples/claude-settings.json). Claude hook ingress is
implemented; `permission_mode` is never mapped to OS authority, and the same `allowed_roots`
policy applies as on Codex.

The hook command reads at most 64 KiB plus one overflow byte and uses a separate **250 ms
total deadline** for stdin, parsing, connect, dispatch and reply. An open stdin pipe cannot
hold up process exit. Missing/invalid attachment, absent daemon, invalid or oversized
JSON, malformed or ambiguous identity, transport loss and timeout all exit successfully
without stdout or stderr. It performs one connect-only submission, with no retry,
autostart, workspace scan or LSP work. Configure the command as written; malformed CLI
syntax is a command configuration error, not a hook payload result.
The private error log separates input-thread startup, read, deadline, size, and cwd
failures with closed `hook_input_*` or `hook_no_cwd` details and an elapsed millisecond
count; it never stores the hook payload.

Hook parsing rejects duplicate known JSON keys and retains only explicit host, phase, bounded
identity, and optional call ID. Codex root events require `session_id`; native child events carry
both root `session_id` and child `agent_id`, with the child selected as actor. Claude always retains `session_id` and selects its
optional `agent_id` as the child actor. `tool_use_id` identifies an exact per-tool call; a batch
does not fabricate one. Tool input/output, cwd, transcript paths and all other raw fields are discarded before IPC. The raw hook payload is never
logged or retained in daemon state. `Debug` formatting of the trusted transport, opaque
JSON, hook/method requests and nested dispatch/reply wrappers redacts private fields,
including echoed hook correlations; it cannot be used to print attachment or payload data.

The MCP ingress obtains `threadId`, `callId` and the presence of the supported
`x-codex-turn-metadata` object from rmcp `RequestContext.meta`, separately from model
arguments. It forwards only the selected actor/call fields and an empty turn-support marker;
arbitrary turn object contents are discarded, and no sandbox metadata is requested or
retained. Assistance owns the opaque method parameter envelope
`{"parameters":...,"host_meta":...}`; Application only frames it. The MCP request ID
and exact call ID remain finite transport request/correlation values. All matching is by
**attachment + actor + call**, never argument equality, timing, CWD, PID or parent identity.

In managed Codex mode, `ide.start` creates its binding directly from trusted MCP
`_meta.threadId` and `_meta.callId` on the fresh process-private attachment. Tool arguments
cannot supply the actor; the optional `root` argument is admitted against `allowed_roots`.
New Context and Diff captures request the existing registered-path reconciliation before capture;
the same methods carrying `detail_ref` are retrieval and do not invalidate that retained detail.
Inspect applies the same current-byte/stale-detail fencing and queues reconciliation for the next
capture, so native edits need no Codex hook or watcher. Actors remain isolated by attachment,
actor, and binding generation. Claude and legacy Codex do not use this direct path.

In legacy mode, only an exact pre-hook followed by `ide.start` creates an actor/channel binding. Later
ordinary methods require their own matching pre-hook and that binding's current liveness.
Post-hooks settle previously validated invocations after MCP result delivery. A complete
native Pre/Post lifecycle without an MCP invocation coalesces a registered-path recheck
hint only for an already active binding. This applies equally to successful and failed
commands: actor/call/phase are sufficient triggers; command text, paths and tool results
are never trusted as effects. `take_native_change_hint` consumes that bounded hint after
a fresh liveness check. The worker invalidates old detail immediately, then reconciles only
registered paths on the next MCP invocation.
Duplicate pre-hooks, premature post-hooks and MCP-before-pre ordering reject that
invocation for subsequent MCP validation in the remaining daemon lifetime; late hooks cannot repair it. Explicit stop revokes the
exact binding and rejects its pending pre-hooks before any Workspace handoff could occur.
Post settlement for already validated calls can still complete. A fresh explicit start
is required after stop; another actor's binding is unaffected.

State is bounded to 128 pending/settling invocations for each exact channel, host, and actor scope, 64 active bindings, and at most one coalesced native hint per active binding. Rejected and completed identities share a permanent daemon-lifetime capacity of 64 channels × 64 host/actor scopes × 1024 IDs. Replay evidence is never evicted to make room. A full replay scope leaves only that actor unavailable until daemon restart; a missing terminal hook can exhaust only that actor's pending scope. Daemon restart discards all bindings and replay evidence and requires fresh exact pre/start input; it does not recover authority.

Closed daemon outcomes are:

| Outcome | Meaning |
| --- | --- |
| `{"state":"unavailable","reason":"host_binding"}` | No validated exact invocation. The compact text names one closed cause in parentheses (T15B): `(outside_allowed_roots)` when the attachment's bound project resolves below no allowed root and its channel never delivered a hook, `(hooks_not_delivered)` when a channel that never delivered a hook lacks this observation, `(missing_pre)`, `(replay)`, `(inactive_binding)`, or `(project_moved: bound to <path>, asked <path>)` when a managed re-root failed; the same tag is the journal `detail`. The structured reply keeps its historical fields. |
| `{"state":"unavailable","reason":"workspace_activation"}` | Host invocation and current binding are proven; Workspace activation is not connected. |
| `{"state":"hook_observed"}` | One pre-hook was retained; no authority or delivery claim. |
| `{"state":"hook_settled"}` | One exact post-hook settled a validated invocation. |
| `{"state":"native_hook_observed"}` | Active native lifecycle requested a registered-path recheck; no source effect is claimed. |
| `{"state":"feedback","text":"..."}` | One positive-version delta survived the current binding and exact-source recheck. |
| `{"state":"host_stopped"}` | The exact host binding was revoked; no Workspace authority was created. |

The MCP facade renders only the corresponding closed method outcomes. Without trusted
configuration, stop reports host binding revocation and other methods direct the model to native tools. Unknown
states or extra fields cannot become successful peer results. The one compact model-facing
text block is projected from the complete serialized reply by the static build-embedded
MiniJinja template `assets/mcp/reply.jinja` (`crates/agent-ide-core/src/assistance/content.rs`), with strict
undefined behavior so a missing fact fails the render closed, auto-escaping off because the
carrier is plain text, and untrusted reply text bound only as template data — never as
template source. Hook transport submission
is not proof of binding or model-context delivery.

This adapter relies on the trusted launcher and the existing private local daemon endpoint;
it does not cryptographically authenticate local processes or attest sandbox enforcement.
Physical execution requires an admitted worktree and a fresh spawn use.
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
The controlled process tests are complemented by live macOS checks. Claude Code 2.1.267 completed
Go and Rust Context, native-edit diagnostics, Diff, Stop, parallel isolation and sequential handoff.
Codex CLI 0.154.0 completed parent plus parallel native-child Go isolation: A observed only its
int/string diagnostic, B stayed semantic after A stopped, and a fresh child independently acquired
A's worktree and read its final bytes. Linux, real `PostToolBatch` availability, and formal
host-confirmed `model_seen` delivery remain unverified.

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

Source context reads one registered relative path under durable authority.
Optional accepted Go/Rust profiles supply semantic results over those exact bytes; absent or
unavailable providers return explicit lexical context. Go worktrees whose canonical
effective-rights identity matches share one accounted listener, one shared native cache namespace
and separate protocol forwarders, while each keeps its own private per-view Go build/module/temp
namespace; Rust uses an exclusive session.
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
retained under canonical nonce-bound worktree identity and incarnation for a compatible successor.
A shared native namespace is reference-counted and quiesces only after the last sharing worktree
stops; gopls may evict its contents independently, which costs recomputation, not IDE-owned state. Only the
existing verified Workspace closure or explicit reset fact may retire the retained namespace.
Restart discards bindings and detail references;
new activation uses a boot-specific channel identity and the durable native-identity fence.
Stop also reclaims that binding's queued jobs and retained start/detail references, without
evicting a live peer's results. Active retained details fail with `capacity` at their configured
limit; the worker never silently evicts live retry evidence to admit another operation.
