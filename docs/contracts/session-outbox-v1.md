# Session outbox and delivery v1 contract (OUTBOX-r4)

Revision 4 (2026-10-15). Canonical definition of the completion facts a session's deferred work
produces, the producer seam that hands them to Bridge, and the session ledger that fences them.
Revision 4 removes every delivery path that IDE once ran itself (reply piggyback of queued facts,
the native `PostToolUse` carrier, the `Stop`/`SubagentStop` hold, the idle wake and the
`ide.inspect {}` fact list). Bridge is the only delivery owner: it owns transport, queueing, retry,
carrier selection and native wake (`docs/architecture.md`, "Staged Bridge-only delivery").

All rules are fail-open: a missing ledger entry, Bridge route or hook never blocks a native tool or
an `ide.*` call. Nothing here proves that a model *read* a fact; "handed over" always means "IDE
asked Bridge to deliver it once".

## 1. Terms

| Term | Meaning |
|---|---|
| Session | One `BindingRef` (host + actor + trusted channel + generation). Created by `ide.start`, ended by `ide.stop`, displacement (stale-lease takeover), a daemon restart or MCP EOF. |
| Ledger | Per-session, in-memory change counters and in-flight work (`session_outbox.rs`). Lives in the daemon worker beside the job ledger; queues no fact. |
| Fact | One state change the agent did not receive in the reply of the call that caused it (§2). |
| Ambient line | The status plate (check counts), git and environment notices (EYES-r2 §6). Not a fact; it rides native hook feedback and replies under its own rules. |
| Hand-off | The single point at which IDE gives a fact to Bridge: a reserved route, a re-proof of source access and one publish. |

## 2. Fact kinds

The kind set is closed. Adding a kind is a contract revision.

| Kind | Producer | Subject form | Source-bearing | Body |
|---|---|---|---|---|
| `test-finished` | test run settlement; shared `terminal_fact` | `tests #N` | no | `summary` (<= 256 bytes) + `detail`, together <= 3584 bytes: counts, <= 8 `FAIL` entries, target exits, `rerun:` (<= 300 bytes), explicit `<ref>/output/1` route |
| `edit-checked` | edit project check; single- and multi-file | `edit <operation_id>` | no | §3.3 |
| `job-ready` | a job registered with `defer` (§3.2): a reply that went `pending`, a pending batch job, a precise answer after an approximate one, an accepted graph job | `<tool> <target>` or `batch <detail_ref>: items i,j` | yes for context/outline/read/symbol/graph/diff answers; no for edit, start, test | the job's final reply or final failure |
| `lost-after-restart` | session re-activation after a daemon replacement | `restart` | no | §10 |

## 3. Interface, envelope and size budget

### 3.1 The narrow in-daemon interface

Producers use one worker-internal trait (`outbox.rs`). The worker implements it by transferring each
fact to Bridge; a recorder with the same interface serves boundary tests of every producer.

```rust
pub trait SessionOutbox: Send + Sync {
    /// Hands one fact to Bridge for each recipient; false when the draft is invalid.
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool;
    /// Makes the final reply (or failure) of the job behind `reference` one job-ready fact for
    /// the job's owning binding. Idempotent per reference: a second call changes nothing.
    fn defer(&self, reference: &str, spec: DeferSpec) -> bool;
    /// Drops every deferral and in-flight registration of `binding` silently.
    fn forget(&self, binding: &BindingRef);
    /// Per-session compaction counter (§9.1); None when this channel has no native hook stream.
    fn context_epoch(&self, binding: &BindingRef) -> Option<u64>;
}
```

`FactDraft` carries `kind`, `subject` (<= 128 bytes, one line), `summary` (<= 256 bytes, one
line), `detail` (summary + detail <= 2048 bytes; 3584 for `test-finished`; 32 KiB for a
source-bearing `job-ready` handed to Bridge), `detail_ref`, `failed`, `source_bearing` and up to 16
`fence_paths`. `DeferSpec` carries `tool`, `target` (<= 120 bytes; subject = `"<tool> <target>"`)
and `fence_paths`. `JobEpoch` is the fence the worker captured when the job **started executing**
(native epoch plus the session's `ide.edit` counter).

### 3.2 `defer` semantics

- The inline-wait timeout of `WorkerHandle::submit` calls `defer`; so does the early approximate
  answer of a loading server (the precise answer stays retained under its reference), a pending
  batch and an accepted graph job.
- The settled result stays retained under its reference. With an eligible Bridge route, exactly
  one `job-ready` fact per deferred job is handed to the owning binding. Its body is the final
  reply; a final failure becomes a `failed = true` fact whose summary names the failure code.
- Reading the job through its own `detail_ref` before it settles ends the deferral: nothing is
  handed to Bridge afterwards.
- Without a reserved Bridge route (no `native_stores` mapping, an unavailable client, an
  unassociated recipient) nothing delivers the fact: the result stays retained under its
  `detail_ref` and is read explicitly with `ide.inspect`. There is no fallback carrier.

### 3.3 `edit-checked` body (single- and multi-file)

```text
edit-checked edit <operation_id>: <K> files, <N> new errors, <M> new warnings
  <path>: <E> errors, <W> warnings (+<n> new)          (one line per file, at most 16, then +K more files)
  new: <path>:<line>: <message>                        (at most 8 lines, message <= 160 bytes, sanitized)
```

The summary says `clean` only for completed analysed checks with no new problems. A known
unavailable environment is reported inline before check acceptance and creates no check wake.
Every accepted check settles through Bridge, including later unavailable, cancelled, superseded
or failed checks; those terminal reasons never imply a clean check. Transient failed attempts
remain visible to the accepted waiter even if the ordinary problem feed retains an older usable
report. Fence: the checked operation's generation and written files with post-write digests. The whole
fact stays <= 2048 bytes. Labelled data only: no imperative sentences, no `poll`, no elapsed
seconds of unfinished work. Untrusted runner/checker text is sanitized (control characters become
spaces; `<` and `>` become `?`).

## 7. Source fencing

- A source-bearing fact is re-proved immediately before its publication: the binding must still be
  active and the retained source observation and provenance of its `detail_ref` must still be
  current. Otherwise the body is replaced by `superseded: source access or freshness changed`
  with no obsolete source text.
- A `job-ready` fact whose captured `JobEpoch` differs from the session's epoch at settlement
  becomes `superseded: source changed during computation` and carries only its `result_ref`.
- Non-source facts (`test-finished`, `edit-checked`) are never suppressed.
- A retired binding (stop, displacement) hands nothing over; Bridge scope for it ends.

## 8. `ide.inspect` and replies

### 8.5 `ide.inspect`

`ide.inspect` reads results explicitly: `detail_ref` (one reference: a pending job, a page
continuation or a test handle `tests #N`) or `detail_refs` (1-8 references answered in order within
the reply budget), each with an optional `wait_ms` (0-240000, clamped) that returns as soon as a
named reference settles and cancels nothing at its deadline. A call naming neither is refused;
there is no session fact list to read.

`ide.test {"status": N}` and `ide.inspect "tests #N"` stay: an on-demand read of one run. A running
run answers `tests #N: running` (no elapsed seconds, no poll hint); a settled run answers the
`test-finished` body. Neither is taught as a way to wait. The original result reference always
returns that compact completion. Only explicit `<ref>/output/N` references deliver full-log
pages; continuations name the next numbered page. Repeating a delivered page returns a single
pointer line to the compact result. Successful reply handoff advances the cursor, and the last
page keeps the bounded capture until the owning detail expires. Explicit
`ide.test {"output":"<ref>/output/N"}` restores any retained page after context loss without
moving the ordinary cursor. It uses the same ownership/access proof and refuses malformed,
foreign, expired, wrong-tool and out-of-range references.

### 8.6 `ide.edit` and its project check

`ide.edit` can answer after its write with `diagnostics: pending` when its completion is tracked.
A known unavailable environment answers inline with its reason, without a redundant check wake.
Checks continue up to `EDIT_CHECK_WAIT` (90 s); every outcome stays under the original reference.
Every accepted check produces a terminal fact (§3.3). Later unanalysed/unavailable outcomes
carry their cause, cancellation and deadline closure stay explicit, and a newer checked source
generation supersedes the old waiter rather than verifying its edit. An owed report that ends
unknown produces a failed `job-ready` notice stating the edit settled but checks did not finish;
terminal job errors also remain completion facts.

### 8.7 Reply wording

Reply texts name no polling call. Pending, edit-check and test-start replies say the result is
retained under its reference and that Bridge delivers it automatically. Without
Bridge delivery (`docs/assistance-launcher.md`) the retained result is read explicitly with its
reference.

## 9. Session ledger

### 9.1 Compaction signal

`context_epoch(binding)` is a per-session counter that rises when the host compacts or clears the
agent's context, so a consumer ("unchanged since your read") never claims text the model no longer
holds.

- Claude: the plugin `hooks/hooks.json` carries `PreCompact` (matcher empty: manual and auto) and
  `SessionStart` with matcher `compact|clear`.
- Codex: the managed fragment carries `PreCompact`.
- An event raises the counter of every live session of that host channel (main actor and its
  subagents), because the payload may not name the subagent; this errs towards re-sending.
- `None` means the channel never delivered a native hook (Codex with absent or untrusted hooks, or a
  harness without the hook entries): no compaction can be observed there, and the consumer must not
  assume the model still holds an earlier read.

## 10. Restart

- The ledger is in memory and dies with the daemon. Bridge retains its own immutable handoff intents
  (`docs/assistance-launcher.md`).
- The MCP facade (one per host session, survives daemon replacement) remembers, per remembered
  actor, up to 16 subjects of requested work it relayed without seeing them resolved: a `pending`
  reply, a `tests #N: started` reply, an edit with `diagnostics: pending`. Reading a reference back
  or relaying the settled run resolves a subject.
- When that actor re-activates after a replacement (`docs/assistance-host-binding.md`, "Restart
  recovery"), the new daemon attempts one `lost-after-restart` handoff: `restart: the IDE
  restarted; results not delivered before it may be lost: tests #3 (before restart), edit op-7`.
  Subjects are labelled "before restart" because new numbering starts again. The daemon awaits
  the bounded Bridge publication and reports true only for accepted or uncertain custody. The
  front forgets the subjects after that result; no route or a definite no-effect refusal retains
  them for a later eligible re-activation. Model observation remains invisible, hence "may be"
  and no retry after an accepted or uncertain publication.
- Stale `detail_ref`s keep today's answer ("issued before the IDE restarted; re-read").

## 11. `ide.stop` and displacement

- `ide.stop` revokes the binding's authority and ends its Bridge scope. Its reply lists the test
  runs the binding started and never read; it carries no fact list.
- A displaced session (stale-lease takeover or any committed revoke) is dropped silently through
  `forget`, called from `release_binding_state`.
- After stop or displacement nothing is handed over for that session. A test run that settles after
  stop stays readable through `ide.inspect "tests #N"` as before.

## 12. Several actors

- Facts are addressed to sessions and never delivered to another actor or generation.
- One-run-per-worktree: a session that requests a test while another session's run on the same
  worktree is active becomes a subscriber; it receives its own `test-finished` fact for that run.
  Producers hand the fact to the owner and every subscriber.
- Claude subagents are separate actors with separate ledgers.

## 14. Excluded

An IDE-run reply piggyback, `PostToolUse` carrier, `Stop`/`SubagentStop` hold, idle wake or
`ide.inspect {}` fact list; agent-run finish and pool waiting; `asyncRewake`; background `wait`
commands; MCP notifications as a model carrier; persistence of facts across daemon restarts.

## Revision history

- r1: initial contract.
- r2: M-024 answers; `SessionOutbox` trait with `defer`, `forget`, `context_epoch` and
  `RecordingOutbox`; M-025 definition-file fence and discard text; multi-file `edit-checked` body;
  compaction signal; displacement; deployment list.
- r3: M-024 boundary only: consumes `M024-M026-TEST-FACT` r3 (`summary` + `detail` <= 1536 bytes).
- r4: both live Bridge passive-wake proofs (Codex and Claude) passed, so the superseded IDE-run
  delivery paths were removed (§ intro, §2, §3, §7, §8.5, §11, §14); the carrier, hold, wake, agent
  state, probe and deployment sections of r3 were deleted with them.
