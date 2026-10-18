# Reply hygiene and terminal test fact v1 contract (HYGIENE-r1)

Revision 1 (2026-10-10). Canonical definition of two things the other stage modules rely on: the
text of the terminal test fact, and the rule that keeps replies, plates, facts and instructions
from inviting polling. Provider: M-024 "Reply hygiene and teaching". Consumers: M-026 (carries the
test fact), M-025 and M-027 (their new reply wording obeys the rule).

Evidence: `agent-run/experiments/agent-ide-async-delivery-20261010/report.md` §2.3 (46 of 90 Opus
plates were `running K s; poll: …` repeats), §5 and §9.1; transcript audit §3 rows 3 and 6.

| Boundary id | Peer | Section |
|---|---|---|
| `M024-M026-TEST-FACT` | M-026 | 1 |
| `M024-M027-REPLY-HYGIENE` | M-027 | 2 |
| `M024-M025-REPLY-HYGIENE` | M-025 | 2 |

Vocabulary of the envelope (`FactDraft`, `test-finished`, carriers, budgets) is
[session-outbox-v1](session-outbox-v1.md); this file does not restate it.

## 1. Terminal test fact (`M024-M026-TEST-FACT`)

A run that is still running has **no fact and no plate line**. Its only text is the `ide.test`
start reply. A run that settles produces exactly one fact; the same function renders the settled
`ide.test` reply body, so a reply and a pushed fact never disagree.

### 1.1 Producer

```rust
/// Terminal text of one settled run; pure and deterministic (no clock, no I/O).
pub(crate) struct TerminalFact { pub summary: String, pub detail: String }
pub(crate) fn terminal_fact(id: u64, result: &RunResult, owns_detail: bool) -> TerminalFact;
```

Mapping to the outbox draft: `kind = test-finished`, `subject = "tests #N"`, `summary` and
`detail` as returned, `detail_ref` = the run's output detail when `owns_detail` (the run's owner
only), `failed = failures > 0 || exit != 0 || stopped || no summary`,
`source_bearing = false`, `fence = []` (whole worktree). Settled-reply body = `tests #N: ` + `summary` +
`\n` + `detail`.

### 1.2 Text

```text
tests #N: <summary>
  FAIL <name>
       <file>:<line>  <message>
  (+K more failed tests in the full output)
  <one line per target of a several-target run, at most 160 bytes each>
  rerun: <command>
  full output: ide.test {"detail_ref":"<detail_ref>/output/1"}
```

| Case | `summary` (one line, at most 256 bytes) |
|---|---|
| counts parsed | `P passed, F failed, S s (exit C)` |
| stopped at its budget | `stopped at budget B s — P passed, F failed so far (exit C)`; unavailable counts are labelled `test counts unavailable` |
| owner cancellation | `cancelled — P passed, F failed so far (exit C)`; unobserved counts or exit remain explicitly unavailable |
| non-zero exit, no counts | `no test results (exit C), S s — runner said: <excerpt, at most 200 bytes>` |
| no recognised summary (only the exit status is known, whatever the status) | `exit C, S s` (`exit unknown` after a signal); `detail` adds `output (tail):` and at most 10 lines and 1024 bytes of the output tail |

`S` is the whole seconds the finished run took: an outcome, not progress. An environment label,
when the run has one, follows the summary as ` · env <label>`; the whole summary is cut on a
character boundary to 256 bytes.

Rules (each one is a unit test):

1. At most 8 `FAIL` entries. An entry is the `FAIL <name>` line (name at most 120 bytes) and one
   optional location/message line (at most 160 bytes). Failures beyond the shown ones are counted
   in one `(+K more failed tests in the full output)` line.
2. `rerun:` is always present. The command is the exact rerun line the reply carries today
   (`cd <dir> && env NAME=VALUE … <argv>` when the run had a directory or environment), cut with
   `…` at 300 bytes on a character boundary.
3. `full output: ide.test {"detail_ref":"<detail_ref>/output/1"}` is present exactly when
   `owns_detail` and the run has retained output. The original reference reads the compact
   completion. Only explicit numbered output references deliver logs, once per page; repeats
   return a one-line pointer. Successful reply handoff advances the cursor; the last retains
   the bounded capture until its detail expires. Explicit `ide.test {"output":"<ref>/output/N"}`
   restores a body after context loss without changing that cursor or running tests again.
4. `summary` plus `detail` is at most 3584 bytes, the `test-finished` body limit of
   session-outbox-v1 (§2, §3.4). When the text is longer, first drop the quoted output lines,
   then the message lines from the last `FAIL` entry backwards, then drop entries from the end
   (the `+K` count grows). Every bounded target outcome, summary, `rerun:` and `full output:`
   survives. Target labels are shortened before their exit/skipped/unavailable outcome.
5. Every cut is on a UTF-8 character boundary. Control characters in runner-provided names,
   messages and excerpts are removed.
6. No imperative sentence, no `poll`, and no elapsed seconds of a *running* run appear anywhere in
   the text (section 2).
7. The text of a fact equals the settled reply body for every run, including
   `exit C, S s` runs without parsed counts. Both use the bounded tail above; the retained
   output remains recoverable separately. The only difference is the outbox's own hand-off
   sanitation of `<` and `>`.
8. Delivery is Bridge's: IDE hands the fact over once and keeps no delivery state; a run counts as
   collected for the `ide.stop` uncollected list only when its owner reads it explicitly, and a
   plate or hook never renders a test line.

### 1.3 Other `ide.test` lines

| Line | Text |
|---|---|
| started (reply only) | `tests #N: started — <argv> [(K tests selected)] (budget B s); the result is retained under its reference; Bridge delivers it automatically` (the wording of session-outbox-v1 §8.7 for a pending result) |
| second start while one is active | `tests #N: still running` |
| `ide.test {status}` or inspect of `tests #N`, run still active | `tests #N: running` |
| the same, run settled | the terminal text of rule 7 |

None of them carries seconds or an instruction. The poll-hint-as-`detail_ref` branch
(`poll_hint_run`) is removed with the hint it recognized.

## 2. Reply hygiene rule (`M024-M027-REPLY-HYGIENE`, `M024-M025-REPLY-HYGIENE`)

Banned phrases, matched case-insensitively as substrings:
`poll`, `every few seconds`, `in a few seconds`, `until it completes`, `again every`.

| Tier | Surfaces | Rule |
|---|---|---|
| A: text the model reads after a call | `assets/mcp/reply.jinja` source; every `PeerReply` state rendered through the content renderer; test lines, plates and facts; the `ide.stop` reply; the `ide.start` signature block; batch reply lines | No banned phrase at all, `poll` included. |
| B: teaching text | the eleven tool descriptions, `SERVER_INSTRUCTIONS`, `SKILL.md`, `agents/ide-reviewer.md`, `skills/agent-ide/agents/openai.yaml` | The four phrases other than `poll` are banned. `poll` may appear only as a negation: `do not poll`, `never poll`, `no polling`. |

Further rules:

- **No seconds on something unfinished.** A line about a running, pending or still-active job never
  contains a seconds count. A finished job's duration is an outcome and is allowed.
- **Data, not orders.** Facts and plates state what happened. The permitted references are a
  `rerun:` command, `full output: ide.test {"detail_ref":"<ref>/output/1"}` and a `detail_ref` handle. Sentences addressed
  to the agent belong to the owner of that surface.
- **Wording ownership.** M-026: `pending` and edit `diagnostics: pending`. M-025:
  `provider_loading`, warming and approximate-answer notices. M-027: batch reply lines. M-024:
  test lines, signature block, descriptions, instructions, skill. Each owner writes its own text;
  this rule only rejects text that invites polling.
- **Guard.** One test, `reply_surfaces_never_invite_polling`, applies the table to the real
  surfaces — the test lines, the `ide.stop` reply, every state of the start card's servers line,
  the batch read and edit reply lines, the several-target test lines, the signature block,
  `reply.jinja`, the descriptions, the instructions and the skill — and rejects a seconds count
  on a line about a running, pending or still-active job (a started line's budget and a finished
  run's duration are allowed). A module's new wording passes it before integration; M-024 re-runs
  it on the integrated candidate.

## 3. Compatibility checks

- Unit tests over `terminal_fact` fixtures: counts only; one failure; twenty failures with long
  names and messages (size, `+K`); budget stop; non-zero exit without counts; zero exit without
  summary; explicit-command tail; multi-byte names cut on a boundary; control characters; an
  oversized rerun command; non-owner (no `full output:`).
- A mutation proves the fact test fails when the failure cap is raised from 8 or when the `rerun:`
  line is dropped, and passes again when restored.
- Product test: a long-running `ide.test` run produces no plate line while running and exactly one
  terminal fact after it, on a reply-carrying host and on a hook-carrying host.
- The guard test above passes on the integrated candidate with the final text of all four modules.
