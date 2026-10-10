# Session outbox and delivery v1 contract (OUTBOX-r1)

Revision 1 (2026-10-10). Canonical definition of the deferred-fact outbox that every IDE session
owns, the carriers that deliver its facts, and the obligations of the modules that produce facts.
Provider: M-026 "Session outbox and delivery". Consumers: M-024 (terminal test fact and plates),
M-025 (precise answer after an approximate one), M-027 (pending batch items).

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
| Session | One `BindingRef` (host + actor + trusted channel + generation, `host_binding.rs:96`). Created by `ide.start`, ended by `ide.stop`, a daemon restart or MCP EOF. |
| Actor | Claude: `agent_id`, else `session_id` (main thread). Codex: `threadId` / `agent_id`. |
| Outbox | Per-session, in-memory, bounded queue of facts plus a short delivered history. Lives in the daemon worker beside the `Ledger`. |
| Fact | One state change the agent did not receive in the reply of the call that caused it (§2). |
| Ambient line | The existing status plate (check counts), git and environment notices (EYES-r2 §6). Not an outbox fact; it shares the block (§4). |
| Carrier | A path that hands text to the model: reply piggyback, PostToolUse hook, Stop/SubagentStop hold, wake message, `ide.inspect`. |
| Hand-off | The single point at which a carrier counts a fact as delivered (§6). |
| Holding fact | A fact with `holds_turn = true` (§2). Only holding facts may hold a Stop or wake an idle agent. |

## 2. Fact kinds

The kind set is closed. Adding a kind is a contract revision.

| Kind | Producer (module) | Subject form | `holds_turn` | Source-bearing | Notes |
|---|---|---|---|---|---|
| `test-finished` | test run settlement (text: M-024, push: M-026) | `tests #N` | true | no | One per run per recipient binding. |
| `edit-checked` | edit project check (M-026; batch edit: M-027) | `edit <operation_id> <path>[ +K files]` | true | no | Replaces the whole-reply `pending` of `ide.edit` (§8.4). |
| `job-ready` | any submitted job whose initial reply was `pending` (M-026), a pending batch job (M-027), or a precise answer after an approximate one (M-025) | `<tool> <target>` or `batch <detail_ref>: items i,j` | true, except **false** for a precise-after-approximate answer | yes for context/outline/read/symbol/graph/diff answers; no for edit, start, test | The model-visible body of a source-bearing fact never rides a hook carrier (§7). |
| `lost-after-restart` | session re-activation after a daemon replacement (M-026) | `restart` | false | no | At most one per actor per replacement (§10). |

`holds_turn` is true exactly when the call that requested the work returned **no** answer for this
subject: a `pending` reply, a `tests #N: started` reply, `diagnostics: pending` on an edit, or a
batch item left pending. A precise answer that follows an approximate one does not hold a turn
or wake an agent: the agent already has an answer.

Ambient lines (check status, `git: HEAD moved`, environment notices) keep their EYES-r2 rules,
never hold a turn and never wake an agent.

## 3. Envelope and size budget

A producer submits a draft; the outbox assigns the sequence and fence:

```rust
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
    pub fence_paths: Vec<String>,    // <= 8 worktree-relative paths the result depends on; empty = whole worktree
}

impl Outbox {
    /// Queues one fact for each recipient session; returns false when no recipient is active.
    pub(crate) fn push(&self, recipients: &[BindingRef], draft: FactDraft, job_epoch: JobEpoch) -> bool;
}
```

Exact type names may change in implementation; the semantics above may not. `JobEpoch` is the
fence captured by the worker when the job **started executing** (§7), never by the producer later.

Rendered form, one fact:

```text
<kind> <subject>: <summary>[ <staleness label>]
  <detail line>
  ...
```

Rules:

- Labelled data only. No imperative sentences; no `poll`; no elapsed seconds. Permitted neutral
  reference lines: `rerun: <exact call>` and `full output: ide.inspect {"detail_ref":"<ref>"}`.
- Untrusted runner/checker text is sanitized as in `problems.rs` (control characters become
  spaces; `<` and `>` become `?`) so it cannot forge the `<agent-ide>` framing.
- Per fact: summary line <= 256 bytes; whole fact <= 2048 bytes. A test fact carries at most 8
  `FAIL` lines.
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

## 4. Composition with the ambient plate

Ambient lines come first, then facts. The plate keeps its own emission and dedup rules
(`problems.rs:537` `next_block_when_with_test_status`, `feed/mod.rs:116`); the outbox does not
re-implement them. Facts use the same `<agent-ide>` block: when both are due on one carrier they
are emitted as one block. The plate stays <= 256 bytes (`feed/mod.rs:15`); facts fit after it.

From this revision the running test line is not an ambient line: a test run contributes nothing
while running, and its outcome arrives once as `test-finished` (M-024 obligation, §12.1).

## 5. Ordering and coalescing

- Each session numbers its facts with a monotonic `seq` starting at 1 per binding generation. The
  number is not rendered.
- Within one block: facts with `failed = true` first, then ascending `seq`. A carrier delivers a
  prefix of that order; it never skips a fact to fit a later one.
- Coalescing key `(kind, subject)`: a new undelivered fact with the same key replaces the older
  undelivered one (the newer `seq` wins). `test-finished` and `job-ready` are terminal, so at most
  one exists per run or job; `edit-checked` is one per `operation_id`.
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

- Delivering a job's result through its own `detail_ref` (an `ide.inspect {"detail_ref": …}` of a
  pending job, or the settled reply itself) marks that job's fact delivered, so it is never pushed
  afterwards.
- An ambiguous hand-off is never retried; the fact stays recoverable (below).
- History: the outbox keeps the summary lines of the last 16 delivered facts per session.
  `ide.inspect {}` lists them when no fact is undelivered (§8.5), and their `detail_ref`s stay
  readable while retained. This is the recovery path for a lost hand-off.

## 7. Source fencing and staleness

- `JobEpoch` = the session's native epoch (`Ledger.native_epoch`, `worker.rs:515`) plus the
  session's `ide.edit` counter, both captured when the job began executing.
- At hand-off the fact is **stale** when the native epoch advanced (a possible-writer native tool
  ran, `problems.rs:72` `may_write`) or an `ide.edit` after `JobEpoch` touched a fenced path (any
  path when `fence_paths` is empty).
- Non-source facts (`test-finished`, `edit-checked`) are never suppressed for staleness; they get
  the label `(ran before your later edits to a.rs, b.rs)` when the edited paths are known, else
  `(ran before later changes)`. At most 3 paths are named, then `+K`.
- Source-bearing facts on a hook carrier (PostToolUse, Stop, wake) carry **only** the summary
  line and its `detail_ref`, never source-derived text (T36B-r: the hook boundary cannot re-prove
  source authority, `worker.rs:1954`). A stale one is labelled `(files changed since)`.
- When a source-bearing body is delivered through a reply or `ide.inspect`, the existing re-proof
  applies (`source_matches`, `worker.rs:6412`); a changed file yields
  `discarded: file changed; repeat` instead of the body.

## 8. Carriers

### 8.1 Eligibility by host and mode

| Carrier | Claude interactive | Claude headless (agent-run) | Codex interactive | Codex headless (exec, agent-run app-server) |
|---|---|---|---|---|
| Reply piggyback | yes | yes | yes | yes |
| PostToolUse hook | yes | yes (P4) | yes, when hooks trusted | yes (P6) |
| Stop hold | yes | yes (P3) | `codex exec` yes (P5); TUI unverified (probe Q2) | yes (P5b) |
| SubagentStop hold | yes | foreground subagents only (P1); see §8.3 | not wired in r1 (unprobed) | not wired in r1 (unprobed) |
| Wake message | main session only, via session registry | never | Codex Desktop only, route unavailable in r1 (§8.4) | never |
| `ide.inspect {}` + `wait_ms` | yes | yes | yes | yes |

Claude `ide.*` replies never carried the ambient plate (EYES-r2 §6); they **do** carry outbox
facts, because source-bearing answers must ride a reply or `ide.inspect`.

### 8.2 Reply piggyback and PostToolUse hook (agent working)

- Every terminal `ide.*` reply except `ide.stop` (which flushes, §11) and `pending` placeholders
  carries due facts after any ambient plate, at the start of what the model reads (for Codex the
  `status` field of `structuredContent`, exactly like the plate).
- A paired native PostToolUse/PostToolUseFailure hook carries due facts inside the existing hook
  deadline (250 ms, `codex_hook.rs:32`, `src/main.rs:2090`). It reads in-memory state only.
- Both carriers apply §3 budgets and §6 hand-off.

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
hook deadline. A hold beyond the lane bound releases at once. Host configuration: the Claude
plugin `hooks/hooks.json` gains `Stop` and `SubagentStop` entries and the managed Codex fragment
(`src/main.rs:1875`) gains a `Stop` entry, each with `timeout: 330`. agent-run deployments copy
these entries into their own plugin and harness hook configuration (probe report §3 item 6).

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

Routes behind the interface in r1:

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
- **Codex Desktop**: Unavailable in r1. agent-run reaches threads through
  `send_message_to_thread` from a signed-Node frontend that holds `CODEX_APP_TOOLS_PIPE_PATH`
  (`agent-run/assets/desktop-transport.cjs`); whether the managed agent-ide MCP can use that pipe
  is probe Q6.
- **Codex exec, app-server, TUI**: Unavailable; covered by the Stop hold and `wait_ms`.

### 8.5 `ide.inspect` consolidation

Input schema (all fields optional, at most one of `detail_ref` and `detail_refs`):

| Field | Meaning |
|---|---|
| none (`{}`) | Every undelivered fact of this session, full bodies included (source-bearing bodies re-proved, §7); marks them delivered. With nothing undelivered: one line per in-flight job of the session (subject only, no elapsed time) and the summaries of the last 16 delivered facts. |
| `detail_ref` | Unchanged meaning (pending job, page continuation, test handle). |
| `detail_refs` | 1-8 references, answered in order within the reply budget, summary first. |
| `wait_ms` | 0-240000, clamped. With `{}`: return as soon as one fact is undelivered, or at the deadline. With refs: return as soon as any named reference settles. A deadline reply says `nothing ready yet` and lists in-flight jobs; it never cancels a job. |

`wait_ms` is served by the MCP facade as a loop of daemon inspections, each bounded below the
10 s method budget (`app.rs:47`). 240 s stays under the Codex 300 s default tool timeout (P7);
Claude is verified to 180 s (P8) and 240 s is probe Q8.

### 8.6 `ide.edit` and its project check

`ide.edit` answers once its write settles. If the project check finishes inside the 8 s inline
window (`INLINE_REPLY_WAIT`, `worker.rs:68`), the diagnostics are in the reply as today; otherwise
the reply carries `diagnostics: pending` (the reserved `EditDiagnostics::Pending`, `reply.rs:355`)
and the check continues up to `EDIT_CHECK_WAIT` (90 s, `worker.rs:61`). Its outcome, or
`unknown` at that deadline, becomes one `edit-checked` holding fact. An edit reply is never a
whole-reply `pending` because of its check.

## 9. Agent state

Tracked per session from host events only; it is advisory and never authorizes anything.

| State | Entered on |
|---|---|
| `working` | `ide.start`; any Pre/Post hook or MCP call of the session; a Stop that blocked |
| `ending` | Stop or SubagentStop received; while the hold runs |
| `idle` | Stop or SubagentStop released (no block) |

A wake additionally requires the host's own idle signal where one exists (Claude registry
`status == "idle"`).

## 10. Restart

- The outbox is in memory and dies with the daemon. Facts are not persisted: they describe
  processes and snapshots that died too.
- The MCP facade (one per host session, survives daemon replacement) remembers, per remembered
  actor, up to 16 subjects of requested work it relayed without seeing them resolved: a `pending`
  reply, a `tests #N: started` reply, an edit with `diagnostics: pending`.
- When that actor re-activates after a replacement (the existing recovery path,
  `docs/assistance-host-binding.md` "Restart recovery"), the new daemon queues one
  `lost-after-restart` fact: `restart: the IDE restarted; results not delivered before it may be
  lost: tests #3 (before restart), edit op-7 src/a.rs`. The subjects are labelled "before
  restart" because new numbering starts again. A hook that delivered one of them before the
  restart is invisible to the facade, hence "may be".
- Stale `detail_ref`s keep today's answer ("issued before the IDE restarted; re-read").

## 11. `ide.stop`

- The stop reply carries every undelivered fact as its summary line (and `detail_ref` line),
  bounded to 2048 bytes, after the existing uncollected-runs list (`worker.rs:1711`). The outbox
  of that session is then dropped.
- After stop nothing is pushed for that session: no hook, no hold (a Stop hook finds no active
  session and releases), no wake. A test run that settles after stop stays readable through
  `ide.inspect "tests #N"` as today.

## 12. Several actors

- Facts are addressed to sessions and never delivered to another actor or generation.
- Ambient lines fan out per session through the existing feed keys.
- One-run-per-worktree: a session that requests a test while another session's run on the same
  worktree is active becomes a subscriber; it receives its own `test-finished` fact for that run
  with `holds_turn = true`. Producers push to the owner and every subscriber.
- Claude subagents are separate actors with separate outboxes; a SubagentStop hold delivers only
  the subagent's facts.

## 13. Consumer obligations

### 13.1 M-024 (plates, terminal test fact)

- Provide the `test-finished` text: summary `P passed, F failed[, S skipped]` (or the runner's
  failure reason), at most 8 `FAIL <name>` lines, a `rerun:` line and a `full output:` line,
  within §3 budgets, sanitized, without `poll` or elapsed seconds.
- Stop emitting running-test lines as ambient lines; the started line stays in the `ide.test`
  reply only.
- Wording of `pending` replies may say the result arrives automatically only together with
  M-026's `job-ready` delivery (integration order).
- Compatibility checks: one settled run yields exactly one delivered `test-finished` across a
  concurrent hook and reply; the rendered fact never contains `poll` and never exceeds 2048 bytes.

### 13.2 M-025 (precise answer after an approximate one)

- The approximate answer is the initial reply, labelled approximate. The precise answer is one
  `job-ready` fact: `holds_turn = false`, `source_bearing = true`, `fence_paths` = the files the
  answer is built from, `detail_ref` = the precise answer.
- Compatibility checks: a hook carrier shows only the summary line and `detail_ref`, never source
  text; `ide.inspect {}` returns the precise body; a fenced file edited before delivery yields
  `discarded: file changed; repeat`; the fact never holds a Stop or wakes.

### 13.3 M-027 (pending batch items)

- Items still pending when a batch reply is sent are left to the outbox: one `job-ready` fact per
  pending job, subject `batch <detail_ref>: items i,j` naming at most 32 item indexes,
  `holds_turn = true`, `source_bearing` by tool.
- A multi-file `ide.edit` produces one `edit-checked` fact per `operation_id` covering all files.
- Retrieving items with `ide.inspect {"detail_refs": […]}` marks their facts delivered.
- Compatibility checks: a batch with two pending items yields one fact naming both; reading them
  by `detail_refs` before any carrier runs leaves nothing to push.

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
