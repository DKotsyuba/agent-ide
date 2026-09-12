# Host metadata probe

This test instrument is not the Agent IDE product API or a supported release. It establishes which non-secret metadata a real host supplies to MCP requests and hook events before the application relies on actor identity or execution authority.

The `host_probe` example has two modes: `host_probe mcp LOG KEY [SANDBOX_STATE_OUTPUT]` serves the single diagnostic tool `probe_observe` over local stdio using the official `rmcp` SDK; `host_probe hook LOG KEY` reads one bounded JSON input from stdin and exits successfully without model-facing output. The diagnostic tool accepts no identity/authority arguments and returns `binding_unproven`. It does not expose placeholder `ide.*` tools, create activation, touch repository source, start a daemon/provider, or schedule other processes.

A caller-selected private observation file receives bounded records of field paths/types and keyed BLAKE3 fingerprints of scalar values. The separate local key file must contain exactly 32 bytes; an absent or invalid key makes recording inert. Dynamic field names are themselves keyed fingerprints, and `tool_input`, `arguments`, `content`, and `environment` subtrees are not traversed. Raw source, prompts, credentials, authorization headers, arguments, environment dumps, and raw scalar values never enter the observation log. The only startup environment values considered are the optional `CODEX_THREAD_ID`, `CODEX_TURN_ID`, and `CODEX_SESSION_ID`, and their names and values are fingerprinted.

The MCP handler advertises only `tools` plus the opt-in experimental `codex/sandbox-state-meta` capability. It records the rmcp request ID, request `_meta`, and available client initialize information from `RequestContext<RoleServer>` before routing its one tool. When the optional `SANDBOX_STATE_OUTPUT` path is supplied, the handler creates a new mode-`0600` private file containing only the bounded `codex/sandbox-state-meta` fields `permissionProfile`, `codexLinuxSandboxExe`, `sandboxCwd`, and `useLegacyLandlock`; it never replaces an existing fixture. The handler has no `ide.*` API, domain, authority, or provider surface. Hook mode reads at most 64 KiB of stdin JSON and exits successfully without stdout model output. Malformed or oversized hook input records only a fixed fingerprinted status marker. Projection stops at depth 8 or 256 fields; each line is capped at 32 KiB and the log at 1 MiB, with an explicit truncation marker. A nonblocking file lock holds the metadata-size check and append together for cooperating writers. Observation failures never block the host. Ordinary stdio protocol EOF ends MCP mode cleanly.

The control test launches a real host parent and two native subagents with identical diagnostic tool arguments. Compare host-origin metadata between the MCP and hook paths. Parent session IDs, CWD, PID/UID, model inputs, timestamps and proximity alone do not establish actor authority. A positive binding claim requires a documented host-origin relation for the actor, specific invocation and delivery channel, validated with adversarial cross-actor cases. Missing evidence remains `binding_unproven`; the recorder must never automatically infer or grant authority.

Separately, any later daemon/provider launch requires real proof that the chosen execution profile enforces the actual host sandbox contract, including an allowed fixture operation and a genuinely denied operation. This recorder does not perform that launch or claim sandbox propagation.

Verification covers a real stdio MCP handshake, exact one-tool discovery, a call returning unproven binding, clean EOF, and hook valid/malformed/oversized-input cases. Log tests check that secret marker strings are absent, exact key sizing is enforced, dynamic key fingerprints differ, and limits hold. Real Codex/Claude host observation is separate from those protocol tests and is recorded with exact host/OS versions.

## Observed Codex field contract

Codex CLI 0.154.0 on macOS 26.6.2 arm64 was exercised with a real parent and two native subagents. Each actor completed exactly one `probe_observe({})` call. Host traces confirmed the empty arguments; all three calls had distinct actor and invocation identities, with exactly one matching PreToolUse and PostToolUse event each.

| MCP field | Matching hook field | Meaning |
|---|---|---|
| `_meta.threadId`, parent call | `session_id` | Parent actor |
| `_meta.threadId`, subagent call | `agent_id` | Individual subagent actor |
| `_meta.callId` | `tool_use_id` | Exact invocation |
| `_meta["x-codex-turn-metadata"].session_id` | `session_id` | Common root session, shared by both subagents |

SubagentStart and SubagentStop carried the same individual `agent_id` values. The common root session therefore cannot distinguish sibling actors. The validator must use the exact actor/invocation relation within its configured channel; the diagnostic result remains `binding_unproven` because the recorder grants no authority.

The associated captured managed workspace profile also passed the three real `execution_d03` tests, including profile-boundary enforcement and fixed Git discovery. Correlation, execution-profile enforcement, and product feedback delivery remain separate acceptance checks.

This run used workspace-write sandboxing with restricted command network access and on-request approvals. The four diagnostic hook definitions were individually reviewed through the normal hook trust UI. New or changed hook definitions must be trusted before this probe can produce hook evidence.
