# Intelligence v0.1 contract

Revision: v0.1-r2 (production session API; direct consumer acceptance pending). Provider:
Intelligence. Direct providers: Workspace supplies current authority, worktree
identity and source observations; Execution supplies admitted owned protocol
children; Application supplies effective provider settings and owned cache
directories. Direct consumers: Assistance renders returned observations and
Workspace receives a stop drain receipt. Shared vocabulary:
[common](common.md).

This proposal consumes Workspace v0.1-r4, frozen at SHA-256
`7ad84f5c834f2e3295709f9c7a5b65f086060d58a78159e47875b2483590d180`,
and Execution r5, frozen at SHA-256
`bb14ea5e4d9668ddaf412b34700988694b05a18ff1d4602c32129c57a9514a87`.

## Ownership and exclusions

Intelligence owns provider compatibility, logical views, protocol generations,
semantic reads, diagnostic observations, and native-cache validity decisions.
It does not create a child process, interpret host proof, mint a Workspace
authority or Execution profile permit, read protocol stdout outside an owned
protocol lease, apply source bytes, run checks, or compose a diff. Execution
owns the physical process, admission and reaping; Workspace owns worktree
identity, authority and source observations.

v0.1 has no DAP, check, edit, rename, arbitrary JSON-RPC, task-context, or
`Scope` surface. Server-originated content is an observation, never an
instruction or authority.

## Read-only surface

`attach_view` accepts a current Workspace authority bound to one `WorktreeRef`
including incarnation and epoch, current source/configuration/toolchain
observations with coverage, one declared provider profile, and a stable request
ID. It returns one opaque owner-bound `ViewLease` plus backend generation,
readiness and reuse disposition, or `queued`, `unavailable`, `stale`, or
`incompatible`. A queue ticket reserves no process resource. A cold owned start
is requested only through Execution's admission/profile path, with the
freshly-correlated Assistance binding and observed sandbox state required by
that path.

`context` accepts a `ViewLease`, an exact current source observation reference,
and a bounded semantic query. It returns only bounded semantic facts with the
provider document version, negotiated position encoding, source observation
reference and sequence, backend generation, coverage, and freshness. A changed
relevant source sequence, worktree incarnation, epoch, configuration, toolchain
or generation makes older evidence `stale`; missing or partial coverage is
`unknown` or `incomplete`, never a clean result.

`diagnostics` accepts the same lease and exact source observation reference. A
pulled result has the same freshness envelope as `context`. A pushed diagnostic
without a provider-specific validated barrier is `provisional`; no diagnostic
message is `unknown`, never clean. Each view owns document synchronization,
document versions and request/result mapping; a reply is routed by backend
generation, view and request ID, never arrival order or current cwd.

`release_view` releases only one logical lease. On Workspace's finite direct
`AuthorityRevoked { worktree_ref, old_epoch, reason }` call, Intelligence
rejects new attachment, query and diagnostic work for that exact view, cancels
its mappings, releases the view and returns a drain receipt. It requests
Execution reaping only for an owned backend with no surviving view and no
supported quiescent retention. It never kills a borrowed endpoint or a shared
peer backend.

## Protocol safety and callbacks

The implementation uses `async-lsp` and its re-exported compatible LSP types;
it does not add a second JSON-RPC multiplexer. The wire boundary owns explicit
header, body, outstanding-request and retained-output ceilings. It handles
fragmented and coalesced frames, rejects malformed framing/encoding/IDs,
disposes a cancelled request before accepting a late response, and invalidates
the affected generation on protocol failure or EOF. Pipe completion is handed
to Execution for owned-child reap evidence.

Server `workspace/applyEdit` is rejected without writing bytes and no
callback-write capability is advertised. Configuration responses are scoped to
the requested URI; scope-less settings must be globally compatible or fail.
Noninteractive prompts have no affirmative default. Unknown server commands
remain inert observations.

`intelligence_context_contract` runs the production session on an Execution-owned
real gopls child. It proves capability negotiation, exact Workspace-byte open/change,
definition/references, missing-path close/reopen, provisional pushed diagnostics,
shutdown and reap. The focused session tests exercise malformed framing, UTF position
conversion, callback refusal, request timeout/cancellation and EOF.

## Production context API

`intelligence::session::with_session` borrows independently owned protocol stdout
and stdin, one admitted `WorktreeRef` and authority epoch, a `ViewGeneration`, closed `ProviderSettings`, and
finite deadlines. One absolute session deadline covers initialize, every request, the caller exchange, shutdown, and driver drain; nested request budgets cannot extend it. Dropping the session future invalidates readiness and stops its driver so the owning caller can cancel and reap the child. It drives `async-lsp` while the caller's asynchronous operation
owns one `Session`. It never starts a process or consumes the caller's reap lease.
`Session::shutdown` fences further context immediately and performs the bounded shutdown/exit handshake.
Transport errors remain failures even when the operation returns Ok; only EOF after completed shutdown
is normal. A sender stays alive through graceful EOF to meet async-lsp's driver lifetime contract. Every exit drops the protocol driver; the Execution owner must then close
and reap its child. Cancellation of the enclosing future is the revocation boundary.
The caller must check its live authority and cancel on its exact revocation event.

`Session::capabilities` returns the actual initialize report, optional server identity,
and negotiated UTF-8/16/32 positions; an omitted encoding means UTF-16. It does not
assert diagnostic cleanliness. `ProviderSettings::GoplsDefaults` preserves the accepted null/default configuration and cannot identify a Rust server. `ProviderSettings::Rust(profile)` retains the exact immutable Rust identity, checks the initialize analyzer name/version, sends `cache-priming-disabled-v1` initialization/configuration, and waits for `experimental/serverStatus` with both `health=ok` and `quiescent=true`. Missing, malformed, unhealthy, or non-quiescent status cannot satisfy the bounded barrier. `provider_readiness` is an opaque provider-status observation minted only by the correlated session router; consumers can query it but cannot construct healthy evidence. It remains separate from push-diagnostic readiness.

`Session::context(observation, bytes, ContextQuery::File | Symbol { byte_offset })`
checks the exact Workspace worktree, epoch, source sequence, size and digest. It
does not read disk. UTF-8 byte offsets are validated and converted to the negotiated
encoding; raw Unix paths are converted directly to percent-encoded file URIs.
Only one document is open at a time. A changed observation sends full `didChange`;
a file switch sends close/open; an explicit missing-path observation accepts empty
bytes and closes the current document. Versions remain monotonic across reopening.
The source byte ceiling is Workspace's 1 MiB limit.

For symbol queries, advertised definition/reference methods return typed locations.
`None` means unsupported/unavailable; an empty vector means a completed empty reply.
On unavailable synchronization, unsupported methods or failed requests, the result
explicitly reports lexical provenance and no semantic locations. The standalone
`intelligence::context::lexical_context` provides the same fallback even when a
provider cannot initialize. It scans only the exact supplied observation and labels
matches as lexical, never as project-wide references. Results retain at most 64 KiB
of source text and 128 locations per collection, with explicit truncation. Source
binding and generation/version facts accompany results; callers must compare these
snapshots with their latest Workspace/provider observations before reuse.

The driver validates a 4 KiB header and an 8 MiB JSON body before deserialization.
Retained fragmented input is capped at body + header + one 8 KiB read chunk.
The exclusive mutable session permits one outgoing request at a time. Before any client request or notification enters async-lsp's unbounded sender, a cumulative budget admits at most 256 messages and 8 MiB of serialized parameters plus conservative framing overhead. Exhaustion retires the generation; the allowance is never replenished within a session. Protocol callback responses remain immediate and driver-backpressured under the existing bounded input envelope. async-lsp
owns IDs and response correlation. A timed-out or caller-cancelled request retires
the entire driver generation, disposing its pending mappings and late replies;
it does not claim protocol-level per-request cancellation or reuse that connection.
Per-request timeouts are positive and at most 60 seconds; total session lifetime is
positive and at most five minutes. The default is 30 seconds and two minutes.

Callbacks reject source edits, decline interactive choices, acknowledge progress,
and reject unknown requests through async-lsp's default handler. Configuration
replies contain the selected profile's fixed globally compatible value, with exact response
cardinality and a 128-item ceiling. Workspace configuration/folders and progress capabilities are
advertised exactly; dynamic configuration is not advertised.
Unknown notifications remain inert and unretained.

`Session::diagnostics` returns a bounded push observation tied to this generation.
Wrong URIs and stale document versions are discarded. Matching versioned pushes are
provisional; unversioned pushes carry no source binding and remain provisional.
Source changes and close clear diagnostic evidence. Invalidation clears source identity, document
version, truncation, items and readiness. No document diagnostic pull is implemented, so diagnostic
readiness remains `Unknown`, including empty pushes; Rust startup quiescence does not make a document clean.
The pure freshness/cache lifecycle remains available for future verified pull results.

Assistance routes the public `ide.context` facade to this API with live
authority/revocation cancellation. It presents diagnostics only when the Session snapshot's
source binding, provider generation, and positive document version match the returned context,
and labels the resulting feedback delta provisional. This module does not implement public tool
dispatch, checks, source writes, Scope or a context compiler.

## Profiles, isolation and caches

Every profile declares binary and protocol identity, provider revision,
configuration, toolchain, trust boundary, transport, lifecycle and sharing
mode. The compatibility key contains those inputs. An exclusive profile also
contains the measured executable-byte digest and the canonical `WorktreeRef` including incarnation. A shared profile
uses that worktree identity as an isolated view key and never shares mutable
document buffers across views. A `gopls` profile supplies the absolute Go
toolchain path and forwards only its parent as the process `PATH`.

`rust-analyzer` is exclusive in v0.1. The shared `gopls` profile starts one
controlled `gopls -listen=unix;<owned socket> -listen.timeout=0` listener per
compatibility key. Each `WorktreeRef` incarnation receives an independently
piped explicit `gopls -remote=unix;<owned socket>` forwarder, initialize
root/workspace folder, document state, request IDs, source sequence and logical
lease. `-remote=auto` is never used. The worktree is therefore an isolated view
key, not a second heavy daemon. If divergent-worktree isolation or
detach-with-peer-survival is not proved, the profile reports
unsupported/exclusive rather than shared.
For `gopls v0.23.0`, an explicitly shutdown/exit forwarder may report its
documented terminal `remote disconnected` exit after the daemon closes that
session; it is accepted only with exact captured evidence and a still-live peer
semantic check.

The listener consumes a registry-issued, one-time `ProviderSpawnLease` bound to
the full admitted Workspace authority. Each forwarder consumes a distinct
central process slot through a non-cloneable `ProviderForwarderSpawnLease`,
bound once to its registry view and authority. The registry counts one shared
backend and two logical views; two live forwarders add two separately counted
process slots, giving three centrally admitted processes. Repeated capability
issuance, slot reuse, and identity/incarnation/root/authority-epoch substitution
are rejected before spawning. Listener, forwarder, and Rust spawns receive a newly consumed ActiveBindingUse; none caches liveness across delayed admission. Gopls view keys use Workspace's canonical
`WorktreeRef` rather than independently supplied names.

`observe_source` advances an exact logical view lease monotonically without
resetting its request IDs. A reply's worktree, lease and source sequence must
still match `result_is_current`; older sequences and released views are stale.
Each forwarder slot settles only from its own direct-child proof. Last-view release moves the
listener/backend to draining and returns its one-time reap capability without freeing admission.
After owned listener reap, complete_reap consumes matching evidence and frees/promotes once.
Definite no-spawn failures use the separate one-time unstarted settlement; borrowed peers remain untouched.

The Rust profile revision is `1`. Its compatibility identity includes the
absolute `rust-analyzer` binary and observed version, observed Cargo and rustc
versions, the explicit rustup toolchain selector, configuration, trust, stdio
transport, native-cache namespace, and the canonical `WorktreeRef` identity with
incarnation. Each admitted Rust view
owns its document sequence and request generation. A later sequence or a
different generation makes a reply `stale`; EOF, stop, or revocation makes it
`unavailable`. The profile creates only the configured `rust-analyzer` controlled
provider command using its default stdio transport and passes owned pipes only
through Execution's validated request and owned-child boundary. The controlled
environment contains only `RUSTUP_TOOLCHAIN`; the configured executable must
provide tool discovery. The accepted local selector is
`1.98.1-aarch64-apple-darwin`, including rust-analyzer
`1.98.1 (48a229ce 2026-09-01)`. Changing the selector changes compatibility.
The `cache-priming-disabled-v1` configuration sets
`{"cachePriming":{"enable":false}}` in initialization options and configuration
replies, avoiding eager dependency-cache warmup while retaining semantic analysis.

Real acceptance waits for `experimental/serverStatus` with `quiescent=true`
and `health=ok`, services server requests independently of client response IDs,
and requires both a typed hover and a definition in each divergent worktree.
Each semantic session has a 60-second deadline, checks the initialized analyzer
build, and reaps its owned child before reporting failure. Reaping returns
Execution's capped stderr evidence, including truncation and completion flags;
its drain deadline begins after direct-child exit. A second worktree queues
while the first child is owned and is promoted only after reap and release.

Reusable native-cache identity excludes actor, session, binding, context and
authority IDs. It includes compatible provider/profile/configuration/toolchain
and trust inputs, plus the provider-supported worktree state. Release, stop and
handoff do not delete a valid owned cache. Only Workspace's verified deletion
or reset fact can retire its namespace; moving, unmounting or a temporary
missing path cannot. Unsupported, corrupt or incompatible state is cold or
unavailable with a reason, never an empty valid analysis.

## Acceptance queue

1. Build the bounded `async-lsp` wire adapter and prove frame limits,
   malformed/fragmented/coalesced framing, cancellation with late response
   disposal, read-only callbacks, EOF generation invalidation and owned-pipe
   cleanup handoff.
2. Prove a real shared `gopls` profile with two divergent worktrees containing
   duplicate module/symbol names and different configuration; stop one view and
   confirm the peer remains semantically usable and isolated. This acceptance
   also reports one listener and its separately counted per-view forwarders.
3. Prove the exclusive real `rust-analyzer` profile: no cross-worktree reuse;
   a second demand is separately admitted, queued or refused.
4. Prove source-sequence freshness, provisional diagnostics, same-worktree
   coder-to-reviewer warm handoff, and cache retirement only after a verified
   Workspace closure/reset fact.

Consumer acceptance freezes this revision and SHA-256 before code relies on it.
Subsequent contract changes require bilateral revision acceptance.

## Freshness lifecycle

`intelligence::freshness` supplies reusable source bindings and lifecycle fencing alongside
the production session. It binds provider document result facts to the Workspace observation's
worktree incarnation, authority epoch, source sequence, reference, revision, byte digest and
coverage, plus backend, configuration, toolchain and view generations. Any mismatch is stale;
partial or unknown coverage is unknown; pushed diagnostics are provisional. An absent diagnostic
pull is unknown, never clean. Diagnostic references have an explicit bounded delta log.

Reusable native-cache identity contains provider, profile, configuration, toolchain, trust and
provider-supported worktree state. It excludes actor, session, binding and authority identifiers.
It may pass from coder to reviewer only after quiescence and exact identity compatibility. Stop,
handoff and a missing source retain the namespace. Only the existing verified Workspace closure
or reset facts retire it. Authority revocation is represented by logical view quiescence and does
not signal or kill a peer backend.
