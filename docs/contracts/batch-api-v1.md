# Batch API v1 contract

Revision: batch-api-v1 r1 (proposed); tracked boundary id `batch-api-v1`, revision 1. Provider: the Assistance facade (`ide.read`, `ide.outline`, `ide.symbol`, `ide.graph`, `ide.test`, `ide.edit`). Consumers: coding agents; the teaching surface (tool descriptions, server instructions, skill, `ide.start` card), which must show exactly these shapes; the session outbox, which carries pending batch results and the multi-file edit's check. Vocabulary: [common](common.md); tool basics: [tools v0.4](tools-v0.4.md); edit receipts: [Changes v0.2](changes-v0.2.md).

Purpose: an agent names every file, symbol or range it needs in ONE call and gets everything back, and applies one logical change across several files in ONE `ide.edit` with ONE project check.

Current E-003 rules are defined by [symbol read/edit](symbol-read-edit-v1.md) and
[harness replies](harness-reply-v1.md): edit operation ids are optional; reads and
edits print no file-version reference; current text and delivered-row memory guard
edits; checks arrive separately. The historical detailed examples below retain
deprecated supplied-reference compatibility, not required fields or model workflow.

Forms and caps at a glance:

| Tool | Batch key | Entry forms | Caps | Other fields |
|---|---|---|---|---|
| `ide.read` | `items` | `{symbol}`, `{path}`, `{path, lines}` | 1–32 items, ≤16 files | `force` |
| `ide.outline` | `paths` or `glob` | path strings; one pattern | ≤24 files | `kinds` |
| `ide.symbol` | `symbols` | symbol strings | 1–8 | `usages`, `callers`, `callees`, `history` |
| `ide.graph` | `symbols` | symbol strings | 1–8 roots | `direction`, `depth`, `tests` |
| `ide.test` | `targets` | `{symbol}`, `{path}`, `{pattern}` | 1–8 | `budget_s` |
| `ide.edit` | `files` | `{path, changes}`, `{path, create: true, content}`, `{path, content}` | 1–16 files, ≤32 changes per file, ≤128 changes, ≤128 KiB of `content` + `new` | optional `operation_id` |

## 1. Common rules

- Every single-target form keeps its request, reply and errors exactly as before.
- A batch form uses a plural key. Sending a plural key together with its singular form (or two plural forms of one tool) is `invalid_parameters` naming the conflicting fields; nothing runs.
- A malformed batch (wrong shape, over a cap, duplicate edit path) is refused as a whole. A well-formed item that fails (unknown symbol, missing file, range past the end) fails only itself, in place, and the rest are answered.
- Results come back in request order. One batch call is one job with one `detail_ref`.
- The JSON under each form heading below is that form's canonical valid example: it validates against the generated `schemas/tools.json`. Placeholders `…-58` stand for a real `source_ref`/`detail_ref` string and `…` inside `content` for real code.
- New optional fields: only `force` (boolean, `ide.read {items}` only). There is no `budget` and no `detail` field.
- The reply opens with ONE summary line (counts and per-item states), then whole blocks in request order. A block is never cut. `ide.read` delivers every block whole with no reply-size ceiling (output-cap removal). The other batch tools still name blocks that do not fit the 64 KiB reply envelope in a footer with the exact call that fetches them. There is no `budget` parameter.
- Argument limit of the batch forms of `ide.read`, `ide.outline`, `ide.symbol` and `ide.graph`: 16 KiB of serialized JSON (single forms stay at 4 KiB). `ide.test` stays at 24 KiB and `ide.edit` at 136 KiB. Each entry keeps its existing bound (paths and symbol addresses at most 1024 bytes).
- A batch that does not finish within the inline window (8 s) answers with the outbox's pending placeholder and its `detail_ref` (session-outbox-v1 §8.7); its complete result stays retained under that reference and is handed to Bridge once as one `job-ready` fact when configured (session-outbox-v1 §3.2). Source-bearing results are re-proved against their fence and handed to Bridge only while current. A pending batch item names the retained batch reference and conditional Bridge delivery.
- A block whose file changed between observation and delivery is not served stale; it reads as the outbox's discard line `discarded: <path> changed; repeat <tool> <target>` (session-outbox-v1 §7).
- Reply hygiene (M024-M027-REPLY-HYGIENE r2, reply-hygiene-v1 §2): no reply line of these forms contains `poll`, `every few seconds`, `in a few seconds`, `until it completes` or `again every`, and no running or pending line carries elapsed seconds. Lines state facts.

## 2. `ide.read {items}`

```json
{"items":[
  {"symbol":"src/a.rs#Store/transcript"},
  {"path":"src/b.rs","lines":"10-40"},
  {"path":"src/b.rs","lines":"120-160"},
  {"path":"docs/api.md"}],
 "force":false}
```

- An item is exactly one of `{symbol}`, `{path}` (the whole text of any readable text file) or `{path, lines}` (inclusive 1-based range).
- Caps: 1–32 items, at most 16 distinct files. An exactly repeated item renders once (duplicates footer, as today).
- `force` (optional, default `false`) disables the unchanged answer below.
- The existing `symbols` and `path`+`ranges` forms remain and are answered by the same engine; they gain the summary line and keep everything else.

Reply:

```text
read: 4 items in 3 files — 3 delivered, 1 unchanged, 0 not included, 0 failed
<block per item, in request order>
source_ref: …-58 (valid for src/a.rs, src/b.rs, docs/api.md)
```

- ONE `source_ref` is valid as an edit base for every file whose items were all delivered or answered unchanged. A file with a cut item is not covered.
- Item failures in place: `no such symbol: <address> — check ide.outline {"path":"<file>"}`, `no such file: <path>`, `not a readable text file: <path>`, `no such range: lines <a-b> is past the end of <path> (<n> lines)`.
- No item is cut for size: `ide.read` delivers every block whole (output-cap removal); the `not included` footer remains only for the other batch tools.

Unchanged answer. The IDE remembers, per session, the content digest of every block it delivered in an `items` read. When the same item is requested again and its bytes are identical, the block is replaced by exactly one line:

```text
unchanged since your read <ref>: <item> (<n> lines); "force":true returns the text
```

- `<ref>` is the `detail_ref` of the earlier read that delivered the identical block. `<item>` is the item as requested: the symbol address for `{symbol}`, the path for `{path}`, `<path> lines <a-b>` for `{path, lines}`. `<n>` is the block's line count. Example: `unchanged since your read …-41: src/c.rs#Parser/next (34 lines); "force":true returns the text`.
- It counts as `unchanged` in the summary line. The reply's `source_ref` still covers that file, so an edit needs no re-read.
- `force:true` always returns the text. Any byte change, `ide.stop`, and an observed context compaction of the session reset the memory for that session.
- It is enabled only while the session's `context_epoch` (session-outbox-v1 §9.1) is known, and the memory resets whenever that epoch rises (Claude Code: `PreCompact`, and `SessionStart` with matcher `compact|clear`; Codex: `PreCompact`). Where the epoch is unknown (`None`), the unchanged answer is never given and the text is always returned.
- Only the `items` form answers unchanged. Single-target reads and the legacy `symbols` / `path`+`ranges` forms never do.

## 3. `ide.outline {paths | glob}`

```json
{"paths":["src/a.rs","src/b.rs"],"kinds":"fn,struct"}
{"glob":"crates/x/src/**/*.rs","kinds":"fn"}
```

- `paths`: 1–24 files. `glob`: a worktree-relative pattern with `*` (within one path segment), `**` (any depth) and `?`; it never leaves the worktree or the allowed roots, follows no symlink out of them, skips the directories the name index skips (`.git`, `.hg`, `.venv`, `venv`, `node_modules`, `target`, `dist`) and matches regular files only. At most 24 files, sorted by path; more matches are reported as `+<n> more files matched`.
- `kinds` keeps its closed vocabulary and applies to every file.
- Reply: `outline: <n> files — <k> outlined, <m> listed only` then one whole outline per file; a file that does not fit becomes `<path> (<lines> lines, <symbols> symbols) → ide.outline {"path":"<path>"}`.

## 4. `ide.symbol {symbols}`

```json
{"symbols":["src/domain/transcript.rs#TranscriptQuery","src/a.rs#Store/transcript"],"usages":true,"callers":1}
```

- `symbols`: 1–8 entries. `usages`, `callers`, `callees` and `history` keep their meaning and apply to every entry.
- Each entry is answered by the per-item engine of [symbol-graph-item r2](symbol-graph-item-v1.md) §1, so its block equals the single form's reply text, as one card in request order. An `approximate` entry shows its labelled block at once; a `deferred` entry names the retained batch reference and conditional Bridge delivery.
- Follow-up: the batch job re-runs its approximate and deferred entries in `precise` mode, retains the result under the batch `detail_ref` and hands ONE `job-ready` fact naming that reference and the entry indexes to Bridge when configured (session-outbox-v1 §3.2).
- Reply: `symbols: <n> — <k> cards, <a> approximate, <p> pending, <f> failed` then the cards.

## 5. `ide.graph {symbols}`

```json
{"symbols":["src/a.rs#f","src/b.rs#g"],"direction":"callers","depth":1}
```

- `symbols`: 1–8 roots. `direction`, `depth` and `tests` apply to every root.
- One section per root through the same per-item engine, each with the single form's caps (60 nodes, 120 edges). A root whose every requested direction faulted is `unavailable (<cause>)`, never an empty graph. Approximate and deferred roots follow the §4 rules.
- Reply: `graph: <n> roots (<direction>, depth <d>) — <k> graphs, <a> approximate, <p> pending, <u> unavailable, <f> failed`.

## 6. `ide.test {targets}`

```json
{"targets":[{"symbol":"src/a.rs#parse"},{"path":"tests/cli.rs"},{"pattern":"roles_"}],"budget_s":300}
```

- `targets`: 1–8 entries, each exactly one of `symbol`, `path`, `pattern`. `command` stays single-target only. `budget_s` covers the whole run.
- All targets run as ONE job of the worktree (one job per worktree still holds): one runner invocation per language and package when the runner accepts several filters (libtest names, pytest node ids, vitest files), otherwise the targets run one after another inside that job.
- One terminal result in the standard test-result format, with one line per target and one rerun line that reproduces the whole run.

## 7. `ide.edit {files}`

> E-003 (symbol-read-edit-v1): `operation_id` and every `source_ref` are optional (the files' current bytes are the base, still stale-safe at the write); a `{path, content}` entry replaces an existing file; `changes` entries also take `symbol` windows (`file#Type/m:3-5`), `@` regions, `{symbol, old, new}`, `add_import` and `remove_import`; the reply carries no `source_ref` text and lists each file's changed lines by address under its file line; the check is never waited for. The `source_ref`-based rules below remain valid for a caller that still sends one.

```json
{"operation_id":"add-roles-1","source_ref":"…-58",
 "files":[
  {"path":"crates/domain/src/transcript.rs","changes":[{"symbol":"crates/domain/src/transcript.rs#TranscriptQuery","op":"replace","content":"…"}]},
  {"path":"crates/cli/src/cli.rs","source_ref":"…-61","changes":[{"old":"before_cursor: None,","new":"before_cursor: None,\n roles: None,"}]},
  {"path":"crates/store/src/new_helper.rs","create":true,"content":"…"},
  {"path":"docs/roles.md","content":"…"}]}
```

Request:

- `operation_id` (required) and an optional batch `source_ref` that is the default base of every file. No other top-level key.
- `files`: 1–16 entries with unique `path`s. Each entry is exactly one of:
  - `changes`: 1–32 change objects, exactly as the one-file `changes` form (`lines` | `symbol` + `op`/`where` | `old`/`new`/`within`);
  - `content` with `create: true`: a new file; the path must not exist;
  - `content` without `create`: the whole new content of an existing file.
- An entry's own deprecated, unadvertised `source_ref` overrides the batch one. No base is required: an entry without one (including `lines`, `old` and whole-content entries) resolves against the fresh observation of the file's current bytes, as the one-file form does.
- Totals: at most 128 changes and at most 128 KiB in all `content` and `new` strings together; the serialized arguments stay within 136 KiB.

Phase 1 — validate everything, write nothing:

- Every base must name a retained observation of that path in this session whose bytes are still the file's current bytes; a `create` path must still be absent.
- Every change resolves, applies, the file is formatted once and must pass the language's syntax gate, exactly as in the one-file form.
- Any failure refuses the whole call: `error: edit_refused (edit:refused)` naming every failed item as `file <k> <path>: …` or `file <k> <path> change <n>: …` with the exact fix (a stale base reads `file <k> <path>: stale_source (changed since <source_ref>)`), at most 8 sentences then `+<n> more`. Nothing is written, no receipt is kept and the `operation_id` stays unused.

Phase 2 — commit:

- A durable batch receipt is recorded before the first write: the canonical request digest (over the caller's arguments only) and, per file in request order, the path, the base digest and the candidate digest.
- Files are written in request order. Immediately before each write the file's bytes are re-checked against its base. A file that changed meanwhile, or an I/O error, stops the commit; files after it are not written.
- After the last write of the call, ONE project check runs on the final state: one check generation per affected checked language, normally exactly one. Diagnostics are grouped per file: per-file counts and at most 8 `new:` lines in total. A file no IDE language checks reports `not_analysed`. When the check does not finish within the inline window the reply says `diagnostics: pending`; the result stays retained under its reference and Bridge receives one `edit-checked` fact for the whole operation when configured, in the session-outbox-v1 §3.3 format (subject `edit <operation_id>`), fenced by the written files and their post-write digests. The check never turns the edit reply into a whole-reply `pending`; only validation and writes that outlast the inline window do, and then the outcome stays retained and is eligible for one `job-ready` Bridge fact.

Reply (summary first):

```text
edit: applied 3 of 3 files; source_ref …-63 (valid for all 3); diagnostics: current_reported (1 new error)
  1 crates/domain/src/transcript.rs: replaced; change 1: … (now 28–52)
  2 crates/cli/src/cli.rs: replaced; change 1: old text at line 1784 replaced (now 1784–1785)
  3 crates/store/src/new_helper.rs: created
new: crates/cli/src/cli.rs:1790 error[E0063] missing field `roles`
```

```text
edit: partial — written 1 of 3; not written: 2 crates/cli/src/cli.rs (changed after validation), 3 crates/store/src/new_helper.rs (not reached); the same operation_id resumes the rest
```

- Structured result: `operation_id`, `outcome` (`applied` | `partial` | `conflicting_duplicate` | `outcome_unknown`), `source_ref` (valid for every written file), and per file `path`, `state` (`created` | `replaced` | `unchanged` | `not_written` | `unknown`) and, for `not_written`, `reason` (`changed_after_validation` | `io_error` | `not_reached`).
- The post-edit `source_ref` is a valid base for a further edit of any written file.

Retry and recovery:

- Same `operation_id`, same request digest, settled `applied`: the settled result is returned; nothing is written again.
- Same `operation_id`, same request digest, `partial` or interrupted: the commit resumes in request order. A file whose bytes equal its candidate digest is already written. A file whose bytes equal its base digest is written, after its recomputed candidate is checked against the recorded candidate digest. Any other file stops the resume as `not_written: changed_after_validation`. Then one project check runs.
- Same `operation_id`, different request: `conflicting_duplicate`; nothing is written.
- After a daemon crash the receipt and the digests give each file's exact state; resuming needs no in-memory `source_ref`. An edit that may have written before its reply was lost answers `outcome_unknown` for the operation, never `internal`.
- Nothing is ever applied twice and no newer bytes are ever overwritten.

The `rename` operation keeps its request and reply; it uses this commit, so a rename that stops midway reports which files were written and a retry with the same `operation_id` finishes it.

## 8. Limits and reasons

| Limit | Value | Reason |
|---|---|---|
| `ide.edit` files | 16 | One observed logical change edited 20 files with 52 hunks; adding one field touches 8+ construction sites plus tests and docs. 16 covers most changes in one call and matches the read cap, so one batch read's `source_ref` covers every file of one edit. |
| changes per file | 32 | Existing one-file cap, unchanged. |
| changes per call | 128 | 8 per file on average over 16 files; about 2.5× the 52-hunk change. |
| edit content | 128 KiB total; arguments 136 KiB | The arguments travel in one IPC payload of at most 144 KiB together with host metadata, inside the 160 KiB frame; about one turn of model output. Raise only together with the IPC limits, after measurement. |
| read items / files | 32 / 16 | About 30 KiB of reply text holds about 32 typical bodies of about 1 KiB; 16 files mirrors the edit cap. |
| outline files | 24 | A typical file outline is about 1.2 KiB, so 24 fill the reply; the rest are listed in one line each. |
| symbol / graph entries | 8 | A card with 30 usage lines and its callers is about 3–4 KiB; 8 fill the reply. |
| test targets | 8 | One runner invocation; wider selections use `path` or `pattern`. |
| batch argument JSON | 16 KiB | 32 items of up to about 450 bytes each; per-entry bounds are unchanged. |

## 9. Corrections to the design note (batch-api.md, code at 004afb7)

- Detail store: not 8 details per binding. The daemon keeps one result ledger of `limits.details` entries, 64 in the generated launcher configuration (`init.rs:157`; bound 1–128 at `assistance/launcher.rs:350-366`). `FAIR_DETAILS_PER_BINDING = 8` (`assistance/worker.rs:2220`) is the floor protecting other live bindings during eviction; the requester's own settled details go first and pending details are never evicted, so pending work can fill the ledger and refuse new calls (`worker.rs:2070-2090`, `:2317-2370`). One detail per batch stays the rule.
- Edit receipt digest: the one-file `changes`, symbol, create and rename forms put the worker's own new job reference into the receipt's `source_ref` field (`assistance/symbols.rs:2854-2858`, `:3124-3128`, `:3215`, `:3523-3527`), so every call has a new digest. An identical retry of a settled edit never gets its settled result: with a `source_ref` it answers `stale_source` (its base is no longer current), without one `conflicting_duplicate` (`changes/edit.rs:510-517`). Only the full-content form digests the caller's `source_ref` (`assistance/worker.rs:5199-5203`). The batch receipt therefore digests the caller's arguments only.
- Rename is not retryable at all: any admission other than `Prepared`, including a settled duplicate, returns `internal` (`assistance/symbols.rs:3530-3533`). It also skips files outside the worktree silently (`:3511-3517`) and counts a per-file `stale_source` as touched (`:3539-3541`).
- Edit content: the wire `content` is at most 128 KiB per field (`assistance/facade.rs:371`) and the serialized arguments at most 136 KiB (`changes/edit.rs:17`, measured at `facade.rs:775-781`); an internally spliced candidate may be up to 1 MiB (`workspace/edit.rs:22`).
- Reply budget: the batch read already fills the whole envelope measured with the structured copy (`assistance/symbols.rs:505-518`), about 30 KiB of text. A fair share of at least 1.5 KiB per item cannot hold for 32 items (48 KiB), so the whole-block, request-order rule with an exact follow-up footer is kept (`:545-644`) and no `budget` parameter is added.
- No compaction hook exists: the installed hook phases are PreToolUse, PostToolUse, PostToolUseFailure and PermissionDenied (`hooks/hooks.json`). The unchanged answer needs a compaction signal from host-state tracking and is off where none exists.
- `ide.symbol` `usages` is a boolean (`assistance/facade.rs:857-863`), not `"classified"`; usage classification and bare-name resolution are not batch features. `kinds:"pub"` is not in the closed kind vocabulary (`facade.rs:801-814`). The 64 KiB source-scan cap on outline globs is dropped: it would stop at about two real source files; 24 files and the reply envelope bound the work.
- The project check that answers an edit is a check generation forced for the edited file's language (`assistance/worker.rs:5495-5508`); a mixed-language batch needs one generation per affected checked language.
- Validation of 16 files (formatter and syntax gate per file) can exceed the 8 s inline window (`worker.rs:68`) inside the 10 s method budget (`app.rs:47`); such an edit answers `pending` and its outcome is delivered as a fact.
- Line drift: the 16 listed test failures are `MAX_LISTED_FAILURES` at `assistance/worker.rs:7509`. Every other cited line matches 004afb7, whose source is identical to `bbca32f`.

## 10. Ownership of the tool surface

- This contract's provider edits only `tool_schemas()` property text and `validate_call` for the batch parameters above, plus the engines behind them. Tool-level descriptions (the tool doc comments), `SERVER_INSTRUCTIONS` and `SKILL.md` belong to the teaching surface's owner, finalised after this contract is agreed.
- Property order in `schemas/tools.json` is alphabetical, because the schema is serialized from sorted JSON maps; it cannot list the plural key first. Batch-first teaching is carried by the descriptions, not by key order. Each new property's own description names its caps and states that it replaces the singular key.
- `schemas/tools.json` is never hand-edited or hand-merged. Each branch regenerates it with `cargo xtask contract update` for its own gate, and integration regenerates it after every merge, followed by `cargo xtask contract check`.

## 11. Compatibility checks

- `schemas/tools.json` is regenerated (`cargo xtask contract update`) and `cargo xtask contract check` passes after every change to these shapes.
- Every JSON example the teaching surface shows (descriptions, server instructions, skill, `ide.start` card) parses and validates against `schemas/tools.json`; a test enforces it.
- Product tests prove each rule above on the real daemon: request order, per-item failure isolation, caps and conflicting keys refused, every read block delivered whole (no cut footer for `ide.read`), `source_ref` coverage, the unchanged answer and its resets, glob confinement, one test job, and for `ide.edit {files}`: zero writes for a stale file or a failing change at every position, crash injection after each write followed by a resuming retry that applies nothing twice, a native write between validation and write stopping as `partial` without overwrite, and exactly one project check per call.
