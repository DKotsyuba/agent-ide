# v0.3 feasibility probe P2: persistent (warm) language-server session

Time-boxed (60 min) probe on branch `probe/v03-warm-session`, worktree
`/Users/pluto/projects/agent-ide-worktrees/v03-probe-warm-session`. Prototype code lives in
`tests/v03_persistent_session_probe.rs` and is isolated from production: it is a standalone
`mod probe` built directly on `async-lsp`, not a modification of
`src/intelligence/session.rs::Session`. Nothing in `src/` changed.

## 1. Exactly which limits make `Session` unusable as a warm long-lived connection

| Limit | Where | What it does |
| --- | --- | --- |
| Session lifetime fence | `src/intelligence/session.rs:307-308` (`SessionOptions.lifetime` doc: "must be positive and at most five minutes"), enforced at `session.rs:451-452` (`options.lifetime > Duration::from_secs(300)` rejected as invalid), default `120s` at `session.rs:315-316`, computed into an absolute deadline at `session.rs:476` (`Instant::now() + options.lifetime`), enforced by `tokio::time::timeout_at(deadline, …)` at `session.rs:528`, surfaced as `io::ErrorKind::TimedOut` "LSP session lifetime expired" at `session.rs:544-547` | Every `with_session` call — including its transport driver, not just caller operations — is force-terminated at an absolute wall-clock deadline of at most 300s. There is no way to renew or extend it from inside a running session; a new `Session` must be constructed. |
| Cumulative outbound admission budget | `OutboundBudget` struct and `reserve()` at `session.rs:264-289`; caps at `MAX_SESSION_OUTBOUND_BYTES = 8 * 1024 * 1024` (`session.rs:36`) and `MAX_SESSION_OUTBOUND_MESSAGES = 256` (`session.rs:38`); doc comment states explicitly: "The allowance is deliberately never replenished; a new bounded session is needed after exhaustion" (`session.rs:265-266`) | **Cumulative over the whole session's lifetime, not per-in-flight and not a rate.** Every client-to-provider request or notification (`initialize`, `initialized`, `didOpen`/`didChange`/`didClose`, every `textDocument/definition`/`references` call, `shutdown`, `exit`) consumes from the same one-shot counters via `send_notification` (`session.rs:293-300`) and `Session::request` (`session.rs:936-940`, which invalidates the whole generation and stops the driver on budget exhaustion). A warm session issuing more than 256 total messages over its life -- trivial within minutes of real editor traffic -- permanently exhausts the budget; nothing frees a slot. |
| Single-document assumption | `Document` struct (`session.rs:351-358`, comment: "switching files closes the previous document to bound retained state"); `State.document: Option<Document>` -- "Current document; only one exact file is retained" (`session.rs:374-375`); `Session::synchronize` (`session.rs:807-925`) explicitly sends `didClose` for the previously open URI before opening/changing the new one whenever the URI differs (`session.rs:860-873`) | The session can only ever have one open document. Editing/querying a second file forces a close+reopen round trip and discards all prior diagnostic evidence for the first file (`session.rs:918-923`), which is unusable for an agent working across multiple files in one warm connection. |
| One request in flight / cancellation retires the whole session | `Session` doc: "A single exclusive client API; mutable methods bound outgoing requests to one at a time" (`session.rs:397-398`); `Session::request` takes `&mut self` (`session.rs:929-932`); `RequestGuard::drop` invalidates session state and emits `Stop` on **any** cancellation or timeout of a **single** request (`session.rs:1072-1079`), not just that request | A session cannot serve two concurrent semantic requests. Worse, if a caller drops one in-flight request's future (e.g., an agent that cancels a slow `references` query) or it times out, `RequestGuard::drop` retires the *entire* connection (`state.invalidate()` + `server.emit(Stop)`), not just that request -- there is no partial/per-request cancellation today. |
| One `with_session` call per query, not a long-lived handle | `src/assistance/providers.rs:1740-1760` (`session_operation`): each provider query builds a brand-new `with_session(...)` around exactly one `session.context(...)` call, optionally waits for diagnostics, then calls `session.shutdown()` -- the process is (re)admitted and the protocol handshake is redone every time (`providers.rs:1709-1718` also shows `remaining_options` deriving `SessionOptions` from the *job's* remaining deadline, further shortening the already-300s ceiling per call) | Confirms the product never holds a `Session` open across calls today; "warm" would be a new usage pattern entirely, not a configuration change to existing callers. |
| async-lsp's own outbound channel is unbounded | `async-lsp` crate (pinned `0.2.4`) at `~/.cargo/registry/.../async-lsp-0.2.4/src/lib.rs:503` (`let (tx, rx) = mpsc::unbounded();`, the socket's outbound event channel) and `:726,736` (`PeerSocket`'s `mpsc::unbounded()` / `unbounded_send`) | `OutboundBudget` exists *because* async-lsp itself will happily queue an unbounded number of outbound messages without back-pressure (confirmed directly in the pinned dependency's source, not inferred) -- this is the production mitigation the probe's `InFlightBudget` replaces with a different (per-request, self-releasing) shape. |
| async-lsp: one single-threaded run loop per socket, request cancellation is connection-wide by construction | `~/.cargo/registry/.../async-lsp-0.2.4/src/lib.rs:553` (`event = self.rx.next() => self.dispatch_event(event.expect("Sender is alive"))`) | Directly discovered while building the probe's fake peer: dropping every clone of a socket associated with a running `MainLoop` panics the driver loop with `"Sender is alive"` the next time its internal channel is polled, even mid-run (see `tests/v03_persistent_session_probe.rs`'s `spawn_fake_peer`, which had to add an explicit `keepalive` capture to avoid this). A persistent session's socket handles must never be allowed to drop to zero clones while the driver task is alive -- an easy-to-hit correctness trap for any warm design that recycles sockets. |

## 2. Minimal persistent shape -- prototype in `tests/v03_persistent_session_probe.rs`

`mod probe` (in the test file, not production) implements, against a deterministic in-memory
`async-lsp` fake peer (`spawn_fake_peer`, built the same way `src/intelligence/session_tests.rs`
builds its fake peers, but reimplemented locally per the isolation requirement):

- `InFlightBudget`/`InFlightGuard`: an atomic **concurrent** admission counter (not cumulative).
  A permit is returned to the pool the instant its own request completes.
- `Generation`: a monotonic `AtomicU64` fence. A reply that arrives after the generation has moved
  on (simulating a document version bump or an explicit cancel) is discarded rather than published.
- `PersistentProbeSession`: wraps one `async-lsp` `ServerSocket` plus the two primitives above and
  a `HashMap<Url, i32>` of open documents (vs. production's single `Option<Document>`).

Four tests, run non-ignored (`cargo test --locked --test v03_persistent_session_probe -- --test-threads=1`):

| Test | Demonstrates | Result |
| --- | --- | --- |
| `sequential_round_trips_exceed_the_production_message_cap_without_exhausting_the_budget` | 300 sequential round trips (> production's cumulative 256-message cap) complete without the in-flight budget ever being exhausted; ends at 0 in flight | **pass** |
| `a_blocked_peer_hits_the_bounded_admission_cap_instead_of_growing_memory` | With a peer that never replies, exactly `cap` (4) concurrent requests are admitted; the 5th fails closed with a typed `Overload` *before* it ever reaches the network, instead of queuing | **pass** |
| `a_cancelled_request_cannot_publish_after_a_generation_change` | A request already accepted by the peer, whose generation is bumped before its reply arrives, resolves to `Ok(None)` -- its late result is never surfaced | **pass** |
| `one_session_tracks_more_than_one_open_document_at_once` | One session opens 3 documents, changes 2 of them, closes all 3, driven entirely through one connection -- the peer observes exactly 3 opens / 2 changes / 3 closes | **pass** |

`cargo test --locked --test v03_persistent_session_probe -- --list` lists all 5 tests (4 above plus
the real-Pyright one, `#[ignore]`d). All 4 non-ignored tests pass single-threaded in ~0.05s total.

## 3. Real warm check -- pinned Pyright held alive past the production cap

Bypasses `with_session` entirely (per the task's fallback instruction): the ignored test
`real_pyright_process_stays_warm_past_the_production_lifetime_cap` spawns
`$AGENT_IDE_NODE $AGENT_IDE_PYRIGHT --stdio` directly and drives it with its own minimal
`async-lsp` `MainLoop::new_client` client (`diagnostics_router`), because `Session::with_session`
cannot be configured to outlive its hard 300s cap (`session.rs:452`) without a production code
change. Env pins used (from the local product-route memory, verified 2026-09-17):
`AGENT_IDE_NODE=/Users/pluto/.nvm/versions/node/v24.4.0/bin/node`,
`AGENT_IDE_PYRIGHT=/opt/homebrew/bin/pyright-langserver`.

Fixture: one `probe.py` with `def value() -> int:\n    return 1\n`, toggled between a passing and a
type-error return every 60s lap for 6 laps after the cold open (total wall clock ~6 minutes, i.e.
past the 300s production ceiling), measuring first-`publishDiagnostics`-after-change latency and the
process's RSS (`ps -o rss=`) at each step.

| Lap | Wall clock since process start | Trigger | First-diagnostics latency | RSS (KB) |
| --- | --- | --- | --- | --- |
| cold-open | ~0s | didOpen | 208.0 ms | 147616 |
| warm 1 | ~60s | didChange | 316.0 ms | 147696 |
| warm 2 | ~120s | didChange | 310.8 ms | 112464 |
| warm 3 | ~180s | didChange | 318.1 ms | 112976 |
| warm 4 | ~240s | didChange | 315.1 ms | 112816 |
| warm 5 | ~300s (past the 300s production `SessionOptions::lifetime` cap) | didChange | 313.6 ms | 113136 |
| warm 6 | ~360s | didChange | 316.6 ms | 113456 |

Process held open end-to-end for 362.29s (measured by the harness's own `cargo test` wall clock) --
1.2x the production 300s hard ceiling -- with no crash, no leak-shaped RSS growth (it drops after the
first didChange as Pyright finishes its initial workspace scan, then stays flat within +-1000 KB for
the remaining ~5 minutes), and stable ~310-320 ms warm diagnostic latency (vs. 208 ms for the cold
open, which is *faster* here only because the fixture is a single trivial file with no real workspace
to scan; the warm laps' slightly higher latency is consistent with each one flipping a real type
error, i.e. actual analysis work, not idle noise). Full command and raw output:
`AGENT_IDE_PYRIGHT=/opt/homebrew/bin/pyright-langserver AGENT_IDE_NODE=/Users/pluto/.nvm/versions/node/v24.4.0/bin/node cargo test --locked --test v03_persistent_session_probe real_pyright_process_stays_warm_past_the_production_lifetime_cap -- --ignored --nocapture --test-threads=1` -> `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 362.29s`.

## 4. Recommendation: minimum change list for a production persistent session

1. **Split lifetime into per-process vs. per-operation.** Keep a per-operation deadline (today's
   `request_timeout`, `session.rs:305-306`) but replace the single absolute `deadline`
   (`session.rs:419,476,528`) with an idle/renewal model: the *process* connection has no fixed
   expiry, each *operation* still gets a bounded deadline. Risk: today's single `tokio::time::timeout_at`
   around the whole `with_session` body is what currently guarantees the driver task and its resources
   are always reclaimed -- an unbounded-lifetime session needs an equally reliable non-deadline-based
   reclamation path (explicit close, health check, or idle timeout) or it can leak a provider process
   per abandoned session.
2. **Replace the cumulative `OutboundBudget` with a per-request/in-flight budget**, as prototyped here
   (`probe::InFlightBudget`). Risk: async-lsp's own channel is unbounded
   (`async-lsp-0.2.4/src/lib.rs:503,726`), so the in-flight cap is still the *only* thing preventing
   unbounded queuing under a misbehaving or slow peer -- it must be sized and load-tested against real
   provider stall behavior (rust-analyzer indexing, Pyright large-workspace scans), not just the fake
   peer here.
3. **Track multiple open documents** (`HashMap<Uri, DocState>` as prototyped) instead of
   `State.document: Option<Document>` (`session.rs:374-375`). Risk: production's diagnostic snapshot
   (`DiagnosticSnapshot`, `session.rs:331-348`) and the `PublishDiagnostics` handler
   (`session.rs:1106-1149`) are keyed to *the* single document; multi-document requires per-URI
   diagnostic state and revision channels, which is a real (if mechanical) expansion of `State`, and
   every close/reopen-on-switch code path in `synchronize` (`session.rs:860-873`) needs to become
   per-URI instead of "close the other one."
4. **Cancel by generation, not by retiring the whole connection.** Replace
   `RequestGuard::drop`'s unconditional `state.invalidate()` + `server.emit(Stop)`
   (`session.rs:1072-1079`) with a per-request generation check (as prototyped in `probe::Generation`)
   so one slow/cancelled/timed-out request degrades only itself. Risk: production currently relies on
   "any cancellation kills the generation" as its *only* mechanism for discarding stale
   `textDocument/definition`/`references` replies after a document changes underneath them
   (`context()`'s `self.state.lock()...active` checks at `session.rs:694,778`); a warm session needs an
   explicit generation-compare on every async reply path (request results *and* async
   `PublishDiagnostics`/status notifications), not just on drop, or stale semantic results can leak
   into a caller that has already moved to a new document version.
5. **Re-verify the `async-lsp` socket-keepalive trap** found while building this probe
   (`async-lsp-0.2.4/src/lib.rs:553`, "Sender is alive" panic on zero remaining socket clones):
   a persistent session that recycles or hands out socket clones across many callers must guarantee at
   least one clone survives for the full life of its driver task, or the whole provider connection
   panics its driver, not just the caller.

## Verdict

**Partial -- a warm session is feasible, but not as a configuration change to `Session`.**
Sections 1-2 show the two hardest blockers (cumulative one-shot budget, single document) are
straightforward to redesign, and the probe demonstrates a working alternative shape end-to-end
against a deterministic peer. Section 3 confirms with a real process: the pinned Pyright itself
stayed alive and kept answering diagnostics for 362.29s -- past the 300s production ceiling -- with
flat RSS and consistent ~310-320 ms warm latency; nothing about the *real provider process* resists
staying warm. The lifetime cap and per-request-kills-connection cancellation semantics are therefore
purely `Session`-side policy, not a provider-imposed constraint, and are the two production behaviors
that actively assume short-lived, single-caller sessions -- they would need real design work
(recommendation items 1 and 4), not just relaxed constants, before `Session` could safely back a
shared, long-lived multi-agent service.
