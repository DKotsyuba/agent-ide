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

## Verification

Use actual Unix sockets and separate daemon processes. Cover correct reply/correlation, private directory/socket permissions and current peer UID, invalid/oversized/truncated frames, unknown fields/methods, bounded partial-input wait, lock contention, stale-socket recovery, restart generation change and doctor on an absent runtime directory. Test wrong-UID rejection as far as the local test privileges permit and state that limit. These checks establish Application mechanics only; they do not establish host binding, sandbox propagation or a working IDE.
