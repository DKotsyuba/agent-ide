# Core IPC boundary

The Application layer owns the local daemon, transport, owned runtime directory and lock. It has no Workspace authority, Git/provider admission, language semantics or host-delivery policy. The first production boundary implements only a side-effect-free health request; product operations are added as their owning domains become available.

## Executable modes

`agent-ide daemon --runtime-dir PATH` serves until interrupted or killed. `agent-ide doctor --runtime-dir PATH` reports whether that daemon can answer a health request; doctor never starts a daemon, creates a directory or opens a database. The runtime directory is explicit in this initial slice. The command parser must reject unknown or missing arguments.

## Endpoint ownership

The runtime directory belongs to the effective local user, is a real directory rather than a symlink, and has no group/other access. The socket and lock are inside it with private permissions. Retain an exclusive nonblocking file lock for the daemon lifetime. A lock pathname or PID is not proof of liveness. A second daemon must not unlink an active socket or replace another owner's files. An owned, refused socket may be retired only after taking the lock and establishing that it has no live listener; permission errors and timeouts are not proof of staleness. Cleanup must not remove a replacement socket.

Accepted peer credentials must match the daemon's effective UID; this proves only a local-user boundary, never actor authority. Actor authentication remains Assistance's responsibility.

## Wire version 1

One request and response per Unix connection. Each frame is a four-byte unsigned big-endian byte length followed by that many UTF-8 JSON bytes. A frame is at most 64 KiB. Use bounded asynchronous reads and writes, with an independently enforced total connection deadline and a finite concurrent-connection limit. No allocation proportional to an unchecked declared length.

The health request is `{"version":1,"request_id":"opaque-client-id","method":"health"}`. Request IDs are nonempty and at most 128 bytes. Unknown versions, fields and methods, malformed JSON, oversized frames and truncated input are rejected before handler dispatch. No future payload, actor, authority or persistence fields are reserved in this request.

A successful reply is `{"version":1,"request_id":"opaque-client-id","status":"ok","daemon_generation":"fresh-instance-id"}`. The request ID is echoed exactly after validation. Generate a new collision-resistant daemon generation at every daemon start; generation is not authentication. Invalid transport input may be closed without a reply. Doctor must report unavailable/failed rather than interpreting a closed or stale endpoint as healthy.

## Wire version 2: finite Assistance ingress

Version 1 health remains unchanged. Version 2 has the same one-request/one-reply Unix connection lifecycle, peer-UID boundary, daemon generation, and total connection deadline. Its frame limit is 160 KiB; every opaque identifier (`request_id`, `correlation_id`, and `opaque_attachment`) is required, UTF-8, nonempty, and at most 128 bytes. `sanitized_observation_json`, `params_json`, and `opaque_result_json` are valid JSON values independently capped at 144 KiB. A value at its field limit may still be rejected when its containing frame exceeds 160 KiB.

Only two version-2 methods exist. `assistance.hook_submit` is `{version:2,request_id,correlation_id,opaque_attachment,sanitized_observation_json}`. Its observation is supplied already sanitized by Assistance and is an opaque JSON object to Application; it carries no tool input, tool output, or source content. The bounded reply keeps `request_id` and `correlation_id` and contains only an opaque Assistance reply or the bounded transport state `unavailable` or `overflow`.

`assistance.method_dispatch` is `{version:2,request_id,correlation_id,opaque_attachment,method,dispatch_method,params_json}`. `method` is the literal tag `assistance.method_dispatch`; `dispatch_method` is the closed enum `start | context | diff | inspect | stop`. Unknown method tags, fields, versions, malformed JSON, or over-limit values are rejected before forwarding. Its reply is `{version:2,request_id,opaque_result_json}` or the bounded transport state `unavailable` or `overflow`. Application correlates and bounds transport only. Assistance alone interprets attachment, identity, params, results, rendering, and tool failures.

Connection lanes: the daemon serves at most `ipc.max_connections` (16 by default) concurrent tool-call connections and, in a lane of its own, at most four concurrent hook submissions, so a burst of slow tool calls cannot starve the hooks that authenticate them. A request that finds its lane full is read and answered `{version,request_id,status:"busy"}` (a hook reply also echoes `correlation_id`) instead of being dropped; the refusal ran nothing, so the front reports `busy` ("nothing was applied; repeat this call") and a hook still fails open. Each refusal is counted in the error journal (`daemon`, `refused`, reason `capacity`, detail `connection_busy:call` or `connection_busy:hook`), one line per lane per minute carrying the suppressed count. An older front reads the reply without a result as an unknown outcome, an older daemon still drops.

`submit_hook_if_running` is connect-only: it never prepares a runtime directory, starts or retries a daemon, or retries inline. Every connect, timeout, framing, or dispatch failure is `unavailable` to its caller, which must exit the host hook permissively. There is no subscription, queue fan-out, retained event stream, or other generic bus.

## Wire version 3: closed v0.2 method dispatch

Version 3 preserves the version-2 framing, identity limits, one-request/one-reply lifecycle, and opaque Application treatment. Its only method is `assistance.method_dispatch` with the same envelope and a closed method enum that adds `edit`, `outline`, `read`, and `symbol` to the version-2 tags. `edit` parameters and result semantics are owned exclusively by [Assistance v0.2](assistance-v0.2.md) and [Changes v0.2](changes-v0.2.md); the symbol tools are specified by [tools v0.4](tools-v0.4.md). Application validates only framing, JSON, field sizes, version, and the closed method tag. It neither logs nor retains the parameters/result as telemetry.

The version-3 reply remains `{version:3,request_id,opaque_result_json}` or `unavailable`/`overflow`. Unknown fields, versions, and method tags are rejected before dispatch. Version 2 remains the v0.1 five-method contract; version 3 is required for the sixth method and is not a generic protocol extension.

## Wire versions 4 and 5: `ide.test` and `ide.graph`

Version 4 adds exactly one `dispatch_method` tag, `test` ([tools v0.4 §2.6](tools-v0.4.md)); version 5 adds exactly `graph`. Each keeps the version-2/3 framing, identity limits, envelope, and opaque Application treatment, and each tag is accepted only on its own wire version.

Wire version 4 also carries the long-lived `assistance.client_lease` handshake (EYES-r2 §2), distinguished from a versioned dispatch by its fixed `method` tag: one `{version:4,request_id,method:"assistance.client_lease",candidate?}` request on its own connection, acknowledged once with `{version:4,request_id,status:"ok",attachment?}`, after which the daemon holds the connection open as one lease until the peer's EOF. The handshake is bounded by a three-second connect/ack deadline, never creates runtime state on its own, and is not actor proof.

## Verification

Use actual Unix sockets and separate daemon processes. Cover correct reply/correlation, private directory/socket permissions and current peer UID, invalid/oversized/truncated frames, unknown fields/methods, bounded partial-input wait, lock contention, stale-socket recovery, restart generation change and doctor on an absent runtime directory. Version 2 additionally covers inactive-hook non-autostart, one opaque hook reply, a permitted current-method dispatch, and rejection of a closed-enum violation before dispatch. Test wrong-UID rejection as far as the local test privileges permit and state that limit. These checks establish Application mechanics only; they do not establish host binding, sandbox propagation or a working IDE.
