# Session outbox and delivery v1 contract (OUTBOX-r2)

Revision 2 (2026-10-10). Canonical definition of the deferred-fact outbox that every IDE session
owns, the carriers that deliver its facts, and the obligations of the modules that produce facts.
Provider: M-026 "Session outbox and delivery". Consumers: M-024 (terminal test fact and plates),
M-025 (precise answer after an approximate one), M-027 (pending batch items, multi-file edit
checks, compaction signal). M-026 consumes M-024's `M024-M026-TEST-FACT` r2
(`docs/contracts/reply-hygiene-v1.md` §1: the test-finished text).

Design basis, read for the evidence behind each rule:

- `agent-run/experiments/agent-ide-async-delivery-20261010/report.md` §2, §5, §6, §7, §9;
- `agent-run/experiments/agent-ide-hook-probes-20261010/report.md` (live probes P1-P8);
- owner decisions of 10.10.2026: agent state comes from hooks; an idle agent is woken by an
  explicit message following agent-run's delivery; only results the agent requested wake it;
  the notification layer stays behind one narrow interface.

All rules are fail-open: a missing outbox, carrier, hook or wake route never blocks a native tool,
an `ide.*` call or the end of a turn. Nothing here proves that a model *read* a fact; "delivered"
always means "handed to a host carrier once" (the caveat at `worker.rs` `NativeFeedback`).

## 1. Terms

| Term | Meaning |
|---|---|
| Session | One `BindingRef` (host + actor + trusted channel + generation, `host_binding.rs:96`). Created by `ide.start`, ended by `ide.stop`, displacement (stale-lease takeover), a daemon restart or MCP EOF. |
| Actor | Claude: `agent_id`, else `session_id` (main thread). Codex: `threadId` / `agent_id`. |
| Outbox | Per-session, in-memory, bounded queue of facts plus a short delivered history. Lives in the daemon worker beside the `Ledger`. |
| Fact | One state change the agent did not receive in the reply of the call that caused it (§2). |
| Ambient line | The status plate (check counts), git and environment notices (EYES-r2 §6). Not an outbox fact; it shares the block (§4). |
| Carrier | A path that hands text to the model: reply piggyback, PostToolUse hook, Stop/SubagentStop hold, wake message, `ide.inspect`. |
| Hand-off | The single point at which a carrier counts a fact as delivered (§6). |
| Holding fact | A fact with `holds_turn = true` (§2). Only holding facts may hold a Stop or wake an idle agent. |

## 2. Fact kinds

The kind set is closed. Adding a kind is a contract revision.

| Kind | Producer (module) | Subject form | `holds_turn` | Source-bearing | Body |
|---|---|---|---|---|---|
| `test-finished` | test run settlement; text M-024 (`M024-M026-TEST-FACT` r2, `terminal_fact`), push M-026 | `tests #N` | true | no | M-024 `summary` + `detail`, <= 2000 bytes: counts, <= 8 `FAIL` entries, `rerun:`, `full output: ide.inspect <ref>` |
| `edit-checked` | edit project check; single-file M-026, multi-file M-027 | `edit <operation_id>` | true | no | §3.3 |
| `job-ready` | a job registered with `defer` (§3.2): an initial reply that went `pending` (M-026), a pending batch job (M-027), a precise answer after an approximate one (M-025) | `<tool> <target>` or `batch <detail_ref>: items i,j` | true, except **false** when the caller already got an approximate answer | yes for context/outline/read/symbol/graph/diff answers; no for edit, start, test | the job's final reply or final failure |
| `lost-after-restart` | session re-activation after a daemon replacement (M-026) | `restart` | false | no | §10 |

`holds_turn` is true exactly when the call that requested the work returned **no** answer for this
subject: a `pending` reply, a `tests #N: started` reply, `diagnostics: pending` on an edit, or a
batch item left pending. A precise answer that follows an approximate one does not hold a turn
or wake an agent: the agent already has an answer.

Ambient lines (check status, `git: HEAD moved`, environment notices) keep their EYES-r2 rules,
never hold a turn and never wake an agent.

## 3. Interface, envelope and size budget

### 3.1 The narrow in-daemon interface

Producers use one worker-internal trait. The real outbox implements it; a recorder with the same
interface serves boundary tests of every consumer.

```rust
/// The only in-daemon seam between fact producers and the session outbox.
pub(crate) trait SessionOutbox: Send + Sync {
    /// Queues one fact for each recipient session; false when no recipient is active.
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool;
    /// Makes the final reply (or final failure) of the job behind `reference` one job-ready fact
    /// for the job's owning binding. Idempotent per reference: a second call changes nothing.
    fn defer(&self, reference: &str, spec: DeferSpec) -> bool;
    /// Drops every undelivered fact of `binding` silently (displacement); `ide.stop` flushes
    /// into its reply first (§11).
    fn forget(&self, binding: &BindingRef);
    /// Per-session compaction counter (§9.1); None when this channel has no native hook stream.
    fn context_epoch(&self, binding: &BindingRef) -> Option<u64>;
}

/// Producer input; the outbox adds seq, fence and delivery state.
pub(crate) struct FactDraft {
    pub kind: FactKind,              // closed enum, §2
    pub subject: String,             // <= 128 bytes, one line, no control characters
    pub summary: String,             // <= 256 bytes, one line: the outcome in plain words
    pub detail: Option<String>,      // extra lines; summary + detail <= 2048 bytes
    pub detail_ref: Option<String>,  // full output / full answer, read with ide.inspect
    pub failed: bool,                // ordering: failures first (§5)
    pub holds_turn: bool,            // §2
    pub source_bearing: bool,        // §2; detail then holds source-derived text
    pub fence_paths: Vec<FencedPath>, // <= 16 paths the result depends on; empty = whole worktree
}

pub(crate) struct FencedPath { pub path: String, pub digest: Option<[u8; 32]> }

pub(crate) struct DeferSpec {
    pub tool: AssistanceTool,
    pub target: String,              // <= 120 bytes; subject = "<tool> <target>"
    pub fence_paths: Vec<FencedPath>, // M-025: definition file(s) only (§7)
    pub approximate_given: bool,     // true: holds_turn = false
}

/// Test-only recorder: stores every call in order; `set_context_epoch` drives §9.1 in tests.
#[cfg(test)]
pub(crate) struct RecordingOutbox { /* calls: Mutex<Vec<OutboxCall>>, epochs: Mutex<..> */ }
```

`JobEpoch` is the fence the worker captured when the job **started executing** (`Job.native_epoch`
plus the session's `ide.edit` counter); `defer` takes it from the job itself. Exact type names may
change in implementation; the semantics above may not. After the Epic freeze, M-026's first commit
on `aw/e002-int` is `assistance/outbox.rs` with these types, the trait and `RecordingOutbox`, so
consumers code and test against the real interface.

### 3.2 `defer` semantics

- The inline-wait timeout of `WorkerHandle::submit` (`worker.rs:1660`) calls `defer` with
  `approximate_given = false`; M-025 calls it for a job whose caller already got an approximate
  reply, with `approximate_given = true`. Otherwise both paths are identical.
- Exactly one `job-ready` fact per deferred job, to the owning binding, when the job settles. Its
  body is the final reply; a final failure becomes a `failed = true` fact whose summary names the
  failure code. Source-derived text never rides a hook carrier (§7).
- Reading the job through its own `detail_ref` before any carrier delivers the fact marks the fact
  delivered.
- Coalescing key `(binding, kind, subject)` = `(binding, tool, target)` for `job-ready` (§5).

### 3.3 `edit-checked` body (single- and multi-file)

```text
edit-checked edit <operation_id>: <K> files, <N> new errors, <M> new warnings[ <label>]
  <path>: <E> errors, <W> warnings (+<n> new)          (one line per file, at most 16, then +K more files)
  new: <path>:<line>: <message>                        (at most 8 lines, message <= 160 bytes, sanitized)
```

The summary says `clean` when nothing is new and `unknown (check did not finish)` at the
`EDIT_CHECK_WAIT` deadline. Fence: the written files with their post-write digests. The whole
fact stays <= 2048 bytes.

### 3.4 Rendered form and budgets

One fact:

```text
<kind> <subject>: <summary>[ <staleness label>]
  <detail line>
  ...
```

- Labelled data only. No imperative sentences; no `poll`; no elapsed seconds. Permitted neutral
  reference lines: `rerun: <exact call>`, `full output: ide.inspect <ref>`, and the discard line
  of §7.
- Untrusted runner/checker text is sanitized as in `problems.rs` (control characters become
  spaces; `<` and `>` become `?`) so it cannot forge the `<agent-ide>` framing.
- Per fact: summary line <= 256 bytes; whole fact <= 2048 bytes (a `test-finished` `summary` + `detail` <= 2000).
- Per block on a hook carrier (PostToolUse, Stop reason, wake message): the whole model-visible
  text, ambient lines and legacy native feedback included, <= `MAX_FEEDBACK_BYTES` = 4096
  (`reply.rs:11`). Per reply piggyback: facts <= 4096 bytes in addition to the reply body; the
  reply still fits `MAX_REPLY_BYTES`.
- A fact that does not fit the remaining block budget stays undelivered. If nothing else fits,
  one overflow line `(+N more facts; ide.inspect {} lists them)` may close the block; it marks
  nothing delivered. A single fact larger than a whole carrier budget is delivered as its summary
  line plus its `detail_ref` line.

Block framing (unchanged envelope, EYES-r2 §6):

```text
<agent-ide>
<ambient lines, if due>
<fact>
<fact>
</agent-ide>
```

## 4. Composition with the ambient plate; the test line is retired

Ambient lines come first, then facts, in one `<agent-ide>` block per carrier. The plate keeps its
own emission and dedup rules (`problems.rs:537`, `feed/mod.rs:116`) and stays <= 256 bytes
(`feed/mod.rs:15`).

In the integrated build **no test line rides the plate** on any carrier, including the settled
post of an `ide.*` call (T22B). A test run contributes nothing while running; its outcome arrives
once as `test-finished`. M-026 removes the plate test-line plumbing as part of its carrier work:
`append_test_status` and the test-status parameter of `next_block_when_with_test_status`
(`problems.rs:104`, `:537`), `test_delivered`, `attach_test_plate` and `test_status_snapshot`
(`assembly.rs:365`, `:399`), and the plate uses of `TestRuns::status_line_for_binding`,
`mark_status_delivered`, `mark_feed_status_delivered` (`tests.rs:597-675`). The settled post keeps
carrying ambient lines and due facts. M-024 owns only the text: the started line, the status-reply
text and the `test-finished` body.

## 5. Ordering and coalescing

- Each session numbers its facts with a monotonic `seq` starting at 1 per binding generation. The
  number is not rendered.
- Within one block: facts with `failed = true` first, then ascending `seq`. A carrier delivers a
  prefix of that order; it never skips a fact to fit a later one.
- Coalescing key `(binding, kind, subject)`: a new undelivered fact with the same key replaces the
  older undelivered one (the newer `seq` wins); the older job's `detail_ref` stays readable.
  `test-finished` is one per run per recipient; `edit-checked` one per `operation_id`; `job-ready`
  one per `(tool, target)`.
- Bound: at most 32 undelivered facts per session. On overflow the oldest fact with
  `holds_turn = false` is dropped first, then the oldest holding fact; each drop is counted and
  the next block ends with `(N older facts dropped; ide.inspect {} lists recent ones)`.

## 6. Exactly-once per carrier

A fact is handed off **at most once in total**, by the first carrier whose hand-off succeeds.
Selection and marking happen in one step under the outbox lock, so two carriers racing for the
same session (a native hook and a reply, or a reply and a Stop hold) never both deliver it.

| Carrier | Hand-off point | Not a hand-off |
|---|---|---|
| Reply piggyback | The reply reached a live receiver (the job's oneshot send succeeded) **and** the fact survived final fitting into `Envelope::WithStructured` (same rule as `mark_feedback_inline_delivered`, `worker.rs:869`) | A reply whose caller already timed out; a fact trimmed by fitting |
| PostToolUse / PostToolUseFailure hook | The daemon produced the `Feedback` reply for that hook | Hook ingress refused |
| Stop / SubagentStop hold | The daemon produced the `block` reply | A release (exit 0, no output) |
| Wake message | The host transport reported accepted (Claude inbox receipt `held`/`delivered`) or ambiguous (frames written, no receipt) | Definite refusal, unavailable route, session gone |
| `ide.inspect` | The inspect reply reached a live receiver | Same as reply piggyback |

- Delivering a job's result through its own reference (the settled reply, `ide.inspect` of its
  `detail_ref`, `ide.test {"status": N}` or `ide.inspect "tests #N"` of a settled run) marks that
  job's fact delivered, so it is never pushed afterwards.
- Handing a `test-finished` fact to the run's **owner** marks the run observed (collected), exactly
  as a delivered terminal plate line does today (`tests.rs:637`, `:669`): `ide.stop` no longer
  lists it as uncollected and it no longer pins idle exit. A subscriber's hand-off does not.
- An ambiguous hand-off is never retried; the fact stays recoverable (below).
- History: the outbox keeps the summary lines of the last 16 delivered facts per session.
  `ide.inspect {}` lists them when no fact is undelivered (§8.5), and their `detail_ref`s stay
  readable while retained. This is the recovery path for a lost hand-off.

## 7. Source fencing and staleness

- The staleness decision uses only the fact's `fence_paths` (the producer chooses them) and the session's
  `JobEpoch`.
- On a reply or `ide.inspect` the outbox compares each fenced path's current digest with the
  recorded one (a read under the live profile); a fenced path without a digest, or any path when
  the fence is empty, compares through the `ide.edit` counter and native epoch.
- On a hook carrier (PostToolUse, Stop, wake) no file is read (T36B-r, `worker.rs:1954`): a fact is
  stale when the native epoch advanced (a possible-writer native tool ran, `problems.rs:72`) or an
  `ide.edit` after `JobEpoch` touched a fenced path (any path for an empty fence).
- Non-source facts (`test-finished`, `edit-checked`) are never suppressed; they get the label
  `(ran before your later edits to a.rs, b.rs)` when the changed paths are known, else
  `(ran before later changes)`. At most 3 paths are named, then `+K`.
- Source-bearing facts on a hook carrier carry **only** the summary line and the `detail_ref`
  line, never source-derived text; a stale one is labelled `(files changed since)`.
- A source-bearing body delivered by a reply or `ide.inspect` whose fenced path changed is
  replaced by `discarded: <path> changed; repeat <tool> <target>`. M-025 fences only the
  definition file(s), so edits to usage files never discard its answer. The body's own delivery
  re-proof uses the source and provenance the producer recorded on the job's detail
  (`worker.rs:6412`), so a producer that records only the definition file is not discarded by a
  usage-file change there either.

## 8. Carriers

### 8.1 Eligibility by host and mode

| Carrier | Claude interactive | Claude headless (agent-run) | Codex interactive | Codex headless (exec, agent-run app-server) |
|---|---|---|---|---|
| Reply piggyback | yes | yes | yes | yes |
| PostToolUse hook | yes | yes (P4) | yes, when hooks trusted | yes (P6) |
| Stop hold | yes | yes (P3) | `codex exec` yes (P5); TUI unverified (probe Q2) | yes (P5b) |
| SubagentStop hold | yes | foreground subagents only (P1); see §8.3 | not wired in r2 (unprobed) | not wired in r2 (unprobed) |
| Wake message | main session only, via session registry | never | Codex Desktop only, route unavailable in r2 (§8.4) | never |
| `ide.inspect {}` + `wait_ms` | yes | yes | yes | yes |

Claude `ide.*` replies never carried the ambient plate (EYES-r2 §6); they **do** carry outbox
facts, because source-bearing answers must ride a reply or `ide.inspect`.

### 8.2 Reply piggyback and PostToolUse hook (agent working)

- Every terminal `ide.*` reply except `ide.stop` (which flushes, §11) and `pending` placeholders
  carries due facts after any ambient plate, at the start of what the model reads (for Codex the
  `status` field of `structuredContent`, exactly like the plate).
- A paired native PostToolUse/PostToolUseFailure hook carries due facts inside the existing hook
  deadline (250 ms, `codex_hook.rs:32`, `src/main.rs:2090`). It reads in-memory state only.
- Both carriers apply §3.4 budgets and §6 hand-off.

### 8.3 Stop / SubagentStop hold (agent ending its turn)

New hook phases `stop` and `subagent_stop` relay the same fixed fields as today (host, phase,
actor_id, session_id, agent_type) with a synthetic correlation id; no other payload field enters
the transport. Actor: Claude SubagentStop uses `agent_id`; Claude Stop and Codex Stop use the
main-thread actor.

Decision, in order:

1. Release at once (exit 0, no output) unless the session is active and has an undelivered
   holding fact, or an in-flight job that would produce one and was started in this activation.
2. An undelivered holding fact exists: block at once with it.
3. Otherwise hold until the first holding fact is ready, or until the hold deadline
   = min(300 s, the largest remaining budget of the session's holding jobs). A fact: block with
   it. No fact: release. The hold never blocks without a fact and never blocks twice for the same
   fact (§6), so `stop_hook_active` needs no special case.
4. The block reply is `{"decision":"block","reason": <text>}` with
   `<agent-ide> facts </agent-ide>` followed by: `These results you requested arrived as you
   ended your turn. Take them into account, then restate your complete final answer.` The restate
   sentence is mandatory: agent-run keeps the last assistant text as the answer (probe report §3).
5. Holding facts in the reason obey the 4096-byte hook budget; non-holding due facts and ambient
   lines may ride along within it.
6. Claude SubagentStop holds only when the subagent cannot outlive its parent's result. In Claude
   headless runs, background subagents (the 2.1.294 default) turn a hold into a second `result`
   frame, which agent-run rejects as `ambiguous_result` (P1). Until probe Q5 gives a direct
   signal, SubagentStop holds only for a session proven interactive: a session-registry entry
   (§8.4) with `kind == "interactive"` for the hook's `session_id`. Otherwise it releases.

Transport: the hold uses its own daemon lane (bounded to 16 concurrent holds) and a hook-side
deadline of hold cap + 5 s, never the 4-connection native hook lane (`app.rs:54`) or the 250 ms
hook deadline. A hold beyond the lane bound releases at once. Host configuration ships from this
repository (§16): the Claude plugin `hooks/hooks.json` gains `Stop` and `SubagentStop` entries,
and the managed Codex fragment (`src/main.rs:1875`) gains a `Stop` entry, each with
`timeout: 330`.

### 8.4 Wake message (agent idle)

A wake is sent only when all hold:

- the fact is a holding fact of this session (never ambient, never `lost-after-restart`, never a
  precise-after-approximate answer);
- the session state is `idle` (§9) and no Pre hook or MCP call arrived since;
- 2 s passed since the fact became ready, so an imminent hook takes it instead;
- no wake was attempted for this session in the current idle period.

The wake text is one user-role message, <= 4096 bytes:
`<agent-ide>` facts `</agent-ide>` followed by `Results you requested from Agent IDE arrived
while you were idle.` The facts handed off follow §6 (accepted or ambiguous counts as delivered).

The notification layer is one narrow interface; the owner may later replace its implementation
with a shared cross-harness system:

```rust
/// The only seam between the outbox and any host wake route.
pub(crate) trait SessionWaker: Send + Sync {
    fn wake<'a>(&'a self, target: &'a WakeTarget, text: &'a str)
        -> Pin<Box<dyn Future<Output = WakeOutcome> + Send + 'a>>;
}
pub(crate) struct WakeTarget { pub host: HostKind, pub session_id: String, pub actor_is_main: bool }
pub(crate) enum WakeOutcome { Accepted, Ambiguous, Refused, Unavailable }
```

Routes behind the interface in r2:

- **Claude main session** (actor == session_id). Follow agent-run `delivery/claude.rs`: resolve
  `~/.claude/sessions/<pid>.json` by `sessionId`; require `kind == "interactive"` and
  `status == "idle"`; read `peerToken` from `<pid>.*.key`; connect to `messagingSocketPath`;
  prove the peer is that pid and this uid before writing; write the `auth` frame and one `user`
  frame with `session_id`, `msg_id` and a private reply socket as `from`; wait <= 1.5 s for a
  correlated `peer_message_status` receipt. `held`/`delivered` = Accepted, other statuses =
  Refused, written-but-silent = Ambiguous, missing entry or dead socket = Unavailable. Text,
  tokens and paths never enter logs.
- **Claude subagent actor**: Unavailable. The registry names sessions, not subagents; a message
  to the session would reach a different actor (async report §9.1, safety rule 8).
- **Claude headless** (no interactive registry entry): Unavailable. A new turn after the result
  frame breaks agent-run's runtime contract (P2).
- **Codex Desktop**: Unavailable in r2. agent-run reaches threads through
  `send_message_to_thread` from a signed-Node frontend that holds `CODEX_APP_TOOLS_PIPE_PATH`
  (`agent-run/assets/desktop-transport.cjs`); whether the managed agent-ide MCP can use that pipe
  is probe Q6. The Codex idle cover is the Stop hold plus `wait_ms`.
- **Codex exec, app-server, TUI**: Unavailable; covered by the Stop hold and `wait_ms`.

### 8.5 `ide.inspect` consolidation; test status reads stay

Input schema (all fields optional, at most one of `detail_ref` and `detail_refs`):

| Field | Meaning |
|---|---|
| none (`{}`) | Every undelivered fact of this session, full bodies included (source-bearing bodies re-proved, §7); marks them delivered. With nothing undelivered: one line per in-flight job of the session (subject only, no elapsed time) and the summaries of the last 16 delivered facts. |
| `detail_ref` | Unchanged meaning (pending job, page continuation, test handle `tests #N`). |
| `detail_refs` | 1-8 references (test handles included), answered in order within the reply budget, summary first. |
| `wait_ms` | 0-240000, clamped. With `{}`: return as soon as one fact is undelivered, or at the deadline. With refs: return as soon as any named reference settles. A deadline reply says `nothing ready yet` and lists in-flight jobs; it never cancels a job. |

`wait_ms` is served by the MCP facade as a loop of daemon inspections, each bounded below the
10 s method budget (`app.rs:47`). 240 s stays under the Codex 300 s default tool timeout (P7);
Claude is verified to 180 s (P8) and 240 s is probe Q8.

`ide.test {"status": N}` and `ide.inspect "tests #N"` stay, with identical answers, and are not
deprecated in this Epic: an on-demand read of one run. A running run answers `tests #N: running`
(no elapsed seconds, no poll hint); a settled run answers the `test-finished` body. Reading a
settled run marks its fact delivered (§6). Neither is taught as a way to wait.

### 8.6 `ide.edit` and its project check

`ide.edit` answers once its write settles. If the project check finishes inside the 8 s inline
window (`INLINE_REPLY_WAIT`, `worker.rs:68`), the diagnostics are in the reply as today; otherwise
the reply carries `diagnostics: pending` (the reserved `EditDiagnostics::Pending`, `reply.rs:355`)
and the check continues up to `EDIT_CHECK_WAIT` (90 s, `worker.rs:61`). Its outcome, or
`unknown` at that deadline, becomes one `edit-checked` holding fact (§3.3). An edit reply is never
a whole-reply `pending` because of its check.

### 8.7 Reply wording and teaching sentences

Replacement reply texts (no polling call, `assets/mcp/reply.jinja`):

- `:79` pending placeholder:
  `pending: the result arrives by itself in a later <agent-ide> block (detail_ref {{ detail_ref }})`
- `:338` edit with pending diagnostics:
  `edit: {{ outcome }}; path {{ result.path }}; source_ref {{ result.source_ref }}; diagnostics: pending; the project check arrives by itself as an edit-checked fact`

- `ide.test` started-line suffix, both hosts:
  `; the result arrives by itself in a later <agent-ide> block`

These texts are valid only together with M-026's `defer`, `test-finished` and `edit-checked`
delivery, so they land with it on `aw/e002-int`.

One-sentence push wording M-024 may teach (skill, server instructions, tool descriptions):

- Claude: `Results of long work (test runs, project checks after edits, answers that were pending) are pushed to you in an <agent-ide> block after your next tool call or when you end your turn, so never poll for them; if you have nothing else to do, call ide.inspect {"wait_ms": 120000} once.`
- Codex: `Results of long work (test runs, project checks after edits, answers that were pending) are pushed to you in an <agent-ide> block at the top of your next ide.* reply, after your next tool call, or when you end your turn, so never poll for them; if you have nothing else to do, call ide.inspect {"wait_ms": 120000} once.`

## 9. Agent state

Tracked per session from host events only; it is advisory and never authorizes anything.

| State | Entered on |
|---|---|
| `working` | `ide.start`; any Pre/Post hook or MCP call of the session; a Stop that blocked |
| `ending` | Stop or SubagentStop received; while the hold runs |
| `idle` | Stop or SubagentStop released (no block) |

A wake additionally requires the host's own idle signal where one exists (Claude registry
`status == "idle"`).

### 9.1 Compaction signal

`context_epoch(binding)` is a per-session counter that rises when the host compacts or clears the
agent's context, so a consumer (M-027's "unchanged since your read") never claims text the model
no longer holds.

- Claude: the plugin `hooks/hooks.json` gains `PreCompact` (matcher empty: manual and auto) and
  `SessionStart` with matcher `compact|clear`. Both events are in Claude Code 2.1.294.
- Codex: the managed fragment gains `PreCompact`. The event name is in codex-cli 0.160.1's hook
  set (binary inspection), live behaviour is probe Q9.
- An event raises the counter of every live session of that host session (main actor and its
  subagents), because the payload may not name the subagent; this errs towards re-sending.
- `None` means the channel never delivered a native hook (Codex with absent or untrusted hooks,
  or an agent-run Codex harness without the hook entries): no compaction can be observed there,
  and the consumer must not assume the model still holds an earlier read. `Some(n)` assumes the
  installed hook configuration is the one shipped with this binary (`agent-ide doctor` flags a
  plugin version mismatch); §16 lists the agent-run copies that must follow.

## 10. Restart

- The outbox is in memory and dies with the daemon. Facts are not persisted: they describe
  processes and snapshots that died too.
- The MCP facade (one per host session, survives daemon replacement) remembers, per remembered
  actor, up to 16 subjects of requested work it relayed without seeing them resolved: a `pending`
  reply, a `tests #N: started` reply, an edit with `diagnostics: pending`.
- When that actor re-activates after a replacement (the existing recovery path,
  `docs/assistance-host-binding.md` "Restart recovery"), the new daemon queues one
  `lost-after-restart` fact: `restart: the IDE restarted; results not delivered before it may be
  lost: tests #3 (before restart), edit op-7`. The subjects are labelled "before restart" because
  new numbering starts again. A hook that delivered one of them before the restart is invisible to
  the facade, hence "may be".
- Stale `detail_ref`s keep today's answer ("issued before the IDE restarted; re-read").

## 11. `ide.stop` and displacement

- The stop reply carries every undelivered fact as its summary line (and `detail_ref` line),
  bounded to 2048 bytes, after the existing uncollected-runs list (`worker.rs:1711`). The outbox
  of that session is then dropped.
- A displaced session (stale-lease takeover or any committed revoke) is dropped silently through
  `forget`, called from `release_binding_state` (`worker.rs:5838`); nobody can receive a flush.
- After stop or displacement nothing is pushed for that session: no hook, no hold (a Stop hook
  finds no active session and releases), no wake. A test run that settles after stop stays
  readable through `ide.inspect "tests #N"` as today.

## 12. Several actors

- Facts are addressed to sessions and never delivered to another actor or generation.
- Ambient lines fan out per session through the existing feed keys.
- One-run-per-worktree: a session that requests a test while another session's run on the same
  worktree is active becomes a subscriber; it receives its own `test-finished` fact for that run
  with `holds_turn = true`. Producers push to the owner and every subscriber.
- Claude subagents are separate actors with separate outboxes; a SubagentStop hold delivers only
  the subagent's facts.

## 13. Consumer obligations and compatibility checks

### 13.1 M-024 (plates, terminal test fact)

- Provides `M024-M026-TEST-FACT` r2: `terminal_fact(id, result, owns_detail) -> {summary, detail}`
  (`summary` <= 256 bytes, `summary` + `detail` <= 2000 bytes, at most 8 `FAIL` entries, `rerun:`,
  `full output: ide.inspect <ref>`), sanitized, without `poll` or elapsed seconds of a running
  run. M-026 calls it at run settlement and pushes the fact with `fence_paths = []`.
- Owns the started line and the status-reply text (`tests #N: running`, no seconds, no poll);
  does not edit the plate test-line plumbing that §4 retires.
- Uses the §8.7 texts for pending replies, the started-line suffix and teaching.
- Checks: one settled run yields exactly one delivered `test-finished` across a concurrent hook and
  reply; the rendered text never contains `poll` and `summary` + `detail` never exceeds 2000
  bytes; no plate carries a test line; a delivered fact makes `ide.stop` stop listing the run.

### 13.2 M-025 (precise answer after an approximate one)

- Answers at once with a labelled approximate reply, then calls
  `defer(reference, DeferSpec { tool, target, fence_paths: definition file(s), approximate_given: true })`.
- Records only the definition file(s) as the detail's source provenance if usage-file edits must
  not discard the answer.
- Calls nothing extra on takeover: `release_binding_state` forgets the displaced session.
- Checks against `RecordingOutbox` and the real outbox: exactly one `job-ready` per job to the
  owner, including a final failure; a hook carrier shows no source text; `ide.inspect {}` returns
  the precise body; a definition-file edit yields
  `discarded: <path> changed; repeat ide.symbol <target>`, a usage-file edit does not; a second
  `defer` for the same reference changes nothing; the fact never holds a Stop or wakes.

### 13.3 M-027 (pending batch items, multi-file edits, compaction)

- Calls `defer` for each pending batch job: one `job-ready` fact per job, subject
  `batch <detail_ref>: items i,j` (at most 32 indexes), `holds_turn = true`, `source_bearing` by
  tool.
- A multi-file `ide.edit` yields one `edit-checked` fact per `operation_id` in the §3.3 format,
  fenced by the written files and their post-write digests, never a whole-reply `pending`.
- Reads `context_epoch(binding)` (§9.1) for its "unchanged since your read" dedupe and treats
  `None` as "no compaction knowledge".
- Retrieving items with `ide.inspect {"detail_refs": […]}` marks their facts delivered.
- Checks: a batch with two pending items yields one fact naming both; reading them by
  `detail_refs` before any carrier runs leaves nothing to push; a multi-file edit yields exactly
  one `edit-checked` fact with per-file counts and at most 8 `new:` lines; a recorded compaction
  changes `context_epoch`.

## 14. Excluded

agent-run finish and pool waiting; a cross-harness messaging system; `asyncRewake`; background
`wait` commands; MCP notifications as a model carrier (not model-visible, P8); persistence of
facts across daemon restarts.

## 15. Open probes

| Id | Question | Affects |
|---|---|---|
| Q1 | Claude 8-continuation Stop cap (`CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`): reset on a tool call, and behaviour at the cap | §8.3 loop safety |
| Q2 | Codex interactive TUI: Stop block continues the same turn; managed hook trust prompt | §8.1 |
| Q3 | Do agent-run headless Claude sessions register in `~/.claude/sessions` under the runtime HOME, and with which `kind`? (On 2026-10-10 the runtime home's `sessions/` was empty.) | §8.3, §8.4 |
| Q4 | Plugin `hooks.json` `timeout: 330` honoured for Stop on Claude 2.1.29x | §8.3 |
| Q5 | Can a SubagentStop hook tell a background subagent from a foreground one? | §8.3 rule 6 |
| Q6 | Does Codex Desktop hand `CODEX_APP_TOOLS_PIPE_PATH` to the managed agent-ide MCP, and can it call `send_message_to_thread` without a signed-Node frontend? | §8.4 Codex route |
| Q7 | A user frame to an `idle` interactive Claude session starts exactly one turn; to a `busy` one it is held | §8.4 |
| Q8 | A 240 s MCP call on Claude (180 s verified) and inside a Codex code-mode `exec` script | §8.5 |
| Q9 | Claude `PreCompact`/`SessionStart(compact)` and Codex `PreCompact` payloads: fire once per compaction, name the subagent or only the session | §9.1 |

## 16. Deployment of host hook configuration

Shipped from this repository:

- `hooks/hooks.json` (Claude plugin, installed by `self-install` to
  `<prefix>/share/agent-ide/plugin/current`): adds `Stop`, `SubagentStop` (timeout 330),
  `PreCompact` and `SessionStart` (matcher `compact|clear`), all to `claude-hook.sh`.
- `agent-ide codex-hooks print` (`src/main.rs:1875`): adds `Stop` (timeout 330) and `PreCompact`.
- `scripts/install-local.sh` keeps printing the operator lines; they name the new events.

External changes this machine needs for agent-run runs (owner approval; not made by M-026):

1. `~/.agent-run/config.toml` `[harnesses.claude-code].plugins` lists a hand-made copy
   `~/.agent-run/plugins/agent-ide-hooks` (plugin.json version 0.3.16; hooks.json without Stop,
   SubagentStop, PreCompact or SessionStart), not the installer output. Either point it at
   `<prefix>/share/agent-ide/plugin/current`, as the installer already instructs (this also loads
   the agent-ide skill into agent-run Claude runs), or replace the copy's `hooks/hooks.json` with
   the shipped one after each install. Restart agent-run afterwards.
2. `~/.agent-run/config.toml` gains two `[[harnesses.codex.hooks]]` entries with
   `command = ["<prefix>/bin/agent-ide", "codex-hook", "--managed"]`: `event = "Stop"` and
   `event = "PreCompact"`. agent-run writes them with timeout 600 and trust hashes
   (`agent-run-adapters/src/plugins.rs` `hook_groups`, `event_label`), so no timeout field is
   needed.

## Revision history

- r1 (commit `06352778`): initial contract.
- r2: M-024 answers (test line retired from the plate, test status reads stay, reply texts,
  collection on hand-off, teaching sentences); `SessionOutbox` trait with `defer`, `forget`,
  `context_epoch` and `RecordingOutbox`; M-025 definition-file fence and discard text;
  `edit-checked` multi-file body; compaction signal; displacement; deployment list; consumes
  `M024-M026-TEST-FACT` r2 (`terminal_fact`, 2000-byte text).
