# Symbol read and edit v1 contract

Revision: symbol-read-edit-v1 r2; tracked boundary id `symbol-read-edit-v1`, revision 2. Provider: M-029 (`ide.read`, `ide.edit`). Consumers: coding agents; M-030 (tool descriptions, server instructions, skill, harness-specific reply rendering), which must teach exactly these shapes. Consumes: [symbol-address-v1](symbol-address-v1.md) (M-028) for the address grammar and unit resolution, [session-outbox-v1](session-outbox-v1.md) for the `edit-checked` fact. Vocabulary: [common](common.md); tool basics: [tools v0.4](tools-v0.4.md); the multi-file and batch forms stay as in [batch-api-v1](batch-api-v1.md) except where this contract says otherwise.

Purpose: an agent works the way it naturally does. It finds an address (`file#Type/method[:N]`, or `path:line` from grep), reads the symbol, then replaces the whole symbol, replaces lines inside it, or replaces exact old text with new text scoped to it. Nothing needs a file-version reference, nothing is refused for a reason the native tools accept, and every edit answers at once.

## 0. Rules that hold for every form

- **No file-version reference.** `source_ref` is never required by any read or edit form and no reply prints one. A supplied `source_ref` keeps its pre-v1 meaning (an extra, strict check) and is deprecated; the teaching surface does not mention it. Replies still carry a `detail_ref` for continuation and for the `edit-checked` fact.
- **The current text is the freshness check.** An edit resolves its address against the file's bytes at the moment the call is processed. `old` must occur exactly once in its scope, a window must lie inside the unit's current extent. A call that no longer matches writes nothing and answers with the unit's current numbered text and ONE repair step (§3.5). A window edit (E2) carries a light guard (§3.2 rule 3): against the session's remembered read of that unit when there is one, and an echo of the replaced old lines in the reply when there is not.
- **Atomic.** A call with several changes or files resolves every change before the first write; any refusal writes nothing and consumes nothing.
- **Reply at once.** A write answers as soon as it is durable. The project check is never waited for: its result stays retained under the reply's reference and Bridge delivers one `edit-checked` fact automatically when configured (session-outbox-v1 §3.3).
- **One address language.** The `symbol` field of `ide.read` and `ide.edit` (and each entry of `symbols`, `items[].symbol`, `changes[].symbol`) takes the same strings. What a result prints is what a request accepts.

## 1. Addresses accepted (restated from symbol-address-v1)

```text
ADDRESS  := FILE '#' UNIT [':' WINDOW]        src/a.rs#Store/transcript:3-5
UNIT     := a symbol path  (Type/method, nested as printed by ide.outline)
          | a region name   (@header, @imports, and the others symbol-address-v1 defines; regions carry an @ prefix)
WINDOW   := N | N '-' M | N '-'               lines counted inside the unit; 1 is the unit's first line
LOCATION := PATH ':' LINE [':' COL]           /abs/repo/src/a.rs:130   src/a.rs:130   (no '#')
```

- A unit starts at its header: doc comments, attributes and decorators are lines of the unit, so line 1 of a documented method is its doc comment. A symbol replacement carries doc and body.
- `N-` means "to the unit's last line". A window may not extend past the unit on edits (§3.2); on reads it is clamped (§2.2).
- `LOCATION` is what `grep -n` prints (`path:line`, the trailing `:col` or `:text` of other tools is ignored). `PATH` is absolute inside the worktree or relative to it; an absolute path outside the worktree is refused. `LOCATION` names the smallest unit that holds the line (a symbol or a region; every line has exactly one, symbol-address-v1). Read returns that WHOLE unit; edit treats it as the unit's address with no window.
- Sigil addresses of the name index (`.btn`, `##main`, `--brand`) keep working on `ide.read {symbol}` as today.
- A bare file path in `symbols` keeps its meaning (the whole text of a file no IDE language reads).

## 2. `ide.read`

Forms (unchanged keys; the address language of `symbol`/`symbols`/`items[].symbol` widens as in §1):

```json
{"symbol":"src/a.rs#Store/transcript"}
{"symbol":"src/a.rs#Store/transcript:3-5"}
{"symbol":"/repo/src/a.rs:130"}
{"symbol":"src/a.rs#@imports"}
{"symbols":["src/a.rs#@header","src/a.rs#Store/new","/repo/src/b.rs:88"]}
{"items":[{"symbol":"src/a.rs#Store/transcript:10"},{"path":"docs/x.md","lines":"1-40"}]}
{"path":"src/a.rs","lines":"281-315"}
```

### 2.1 Reply for an address

```text
src/a.rs#Store/transcript  (34 lines; file lines 120-153)
 1	/// Returns the transcript.
 2	pub fn transcript(&self) -> Vec<Turn> {
...
34	}
```

- The header is the unit's canonical address (symbol-address-v1 printing) and its extent. Lines are numbered inside the unit from 1; the number column is padded to the widest shown number. Numbers never continue the file's numbering.
- Window: `src/a.rs#Store/transcript:3-5  (lines 3-5 of 34; file lines 122-124)` and only those lines, numbered 3 to 5.
- `LOCATION`: the whole unit, header `src/a.rs#Store/transcript  (34 lines; file lines 120-153; your line 130 is line 11)`; locations render as `src/a.rs#Store/transcript:11 (L130)` (symbol-address-v1).
- Region: same shape, e.g. `src/a.rs#@imports  (6 lines; file lines 3-8)`.
- No `source_ref:` line. A read reply is never paged: it carries the whole unit regardless of size (output-cap removal).
- Batch forms keep today's summary line and block-per-item structure; each block is the reply above, minus the repeated trailer. Item failures stay per item (`no such symbol: <address> — check ide.outline {"path":"<file>"}`).

### 2.2 Plain path ranges: clamped, never refused for length

- `{"path":P,"lines":"a-b"}`, `ranges` entries and `items[{path,lines}]` accept `N`, `N-M` and `N-`. `M` past the end is clamped to the last line: `src/a.rs  (lines 281-314 of 314)`.
- `a` past the end of the file is an answer, not an error: `src/a.rs: file has 314 lines; nothing at 400-420`.
- `M < N` is the only malformed range; it is `invalid_parameters` naming `lines`.
- Window on an address is clamped the same way: `src/a.rs#Store/transcript: unit has 34 lines; nothing at 40-50`.

### 2.3 Errors (short, one repair step)

| Case | Reply |
|---|---|
| unknown symbol | `no such symbol: <address>; ide.outline {"path":"<file>"} lists the symbols` |
| unknown file | `no such file: <path>` |
| absolute path outside the worktree | `<path> is outside the project; read it with a native tool` |
| binary / unreadable | unchanged (`not a readable text file …`) |
| `LOCATION` line past the end | `<path>: file has <n> lines; nothing at <line>` |

## 3. `ide.edit`

`operation_id` becomes OPTIONAL. Without one the call gets a fresh id (no idempotence across calls, which no refusal needs). With one, today's meaning holds: a repeated id never applies twice, a settled one answers its settled result.

### 3.1 Forms

Exactly one of these per call. All except `patch` and `files` also appear as entries of `changes` (one file) and of `files[].changes`; there `path` is the entry's file and need not repeat.

| # | Request | Effect |
|---|---|---|
| E1 | `{"symbol":ADDRESS, "content":TEXT}` | Replace the whole unit (doc comment and body, or the region) with `content`. |
| E2 | `{"symbol":ADDRESS with WINDOW, "content":TEXT}` | Replace lines N..M of the unit (`:N` is one line). Empty `content` deletes them. |
| E3 | `{"symbol":ADDRESS, "old":TEXT, "new":TEXT}` | Replace the exact text `old` with `new`. `old` must occur exactly once INSIDE the unit (inside the window if one is given). The Claude-native shape; the match may start or end mid-line and include line terminators. |
| E4 | `{"path":P, "old":TEXT, "new":TEXT}` | Same with the whole file as scope (exactly once in the file). `file_path`, `old_string`, `new_string` are accepted as aliases of `path`, `old`, `new`. |
| E5 | `{"symbol":ANCHOR, "op":"insert", "where":"before"\|"after"\|"first"\|"last", "content":TEXT}` | Insert a new symbol (doc included) next to a symbol or region (`where` as today). |
| E6 | `{"symbol":ADDRESS, "op":"delete"}` | Delete the unit with its header. |
| E7 | `{"symbol":ADDRESS, "op":"rename", "new_name":NAME}` | Unchanged. |
| E8 | `{"path":P, "add_import":TEXT}` / `{"path":P, "remove_import":TEXT}` | Add one import statement (idempotent: already present is `unchanged`) / remove the one that equals `TEXT`. `TEXT` is the statement as written in the file, one logical statement. |
| E9 | `{"path":P, "content":TEXT}` | Create the file, or replace an existing file's whole content. |
| E10 | `{"patch":TEXT}` | Apply a Codex `apply_patch` body (§5). |
| E11 | `{"path":P, "changes":[ENTRY…]}` | 1–32 entries of E1–E6, E8, one file, resolved against the same bytes. |
| E12 | `{"files":[{"path":P,"changes":[…]} \| {"path":P,"create":true,"content":T} \| {"path":P,"content":T}…]}` | 1–16 files, one operation. Unchanged apart from §0. |

Notes:

- E2 numbers belong to the unit's CURRENT text; two entries of one `changes` array address the same pre-edit numbering (as `lines` entries do today) and must not overlap.
- E1 with `ADDRESS` = a region replaces that region (the file header comment, the import block).
- E8 places a new import with the other imports (end of the `@imports` region), or after the `@header` region when the file has none. It never reorders existing imports.
- The pre-v1 forms keep working: `{"path":P,"lines":"a-b","content":T}` (file line numbers), `within` on an `old` entry (scope = that symbol), the `symbol`+`op` forms, `source_ref` everywhere. None of them requires `source_ref` any more.
- Caps unchanged: `symbol` 1024 bytes, `content` 128 KiB, 32 changes per file, 128 per call, 16 files.
- Closed errors for shape: two forms in one call (`ide.edit: use one of …` naming the offending keys), `old` without `new`, a window on E4, `add_import` with a newline.

### 3.2 Resolution rules

1. Parse ADDRESS (symbol-address-v1). Unknown symbol or region → refusal §3.5.
2. Unit extent = the unit's current header-inclusive line range. A WINDOW is relative to its first line.
3. E2: `1 <= N <= M <= unit length`. Otherwise refusal "unit has L lines". Light guard: the session remembers the unit text of its last read of that unit, with the unit lines that read showed (any read form that delivered it, including a window or `path:line`). A read reply is never paged or cut, so every row it carries counts as read. This session's own successful edits refresh the memory of units it had read whole (so a window copied from an edit reply is not stale) and forget units it had read only in part. If a remembered text exists, lines N..M lie inside the lines that read showed, and lines N..M of it differ from lines N..M of the unit's current text, the call is refused with the unit's current numbered text ("lines N-M of <unit> changed since your read") and the repair step. If nothing is remembered for those lines (never read, read only elsewhere in the unit, memory reset by `ide.stop` or a context compaction), the edit is applied and the reply echoes the replaced old lines (§3.3). The guard compares only the window's lines, so edits elsewhere in the unit never refuse it.
4. E3: find `old` as exact bytes inside the scope. 0 matches → refusal with current text; 2+ matches → refusal naming the first matches as windows (`:12-14`, `:30-32`). A candidate that does not parse is refused by the existing syntax gate; the project formatter still runs once on the result.
5. Two changes of one call must not overlap in the pre-edit text (existing rule); touching is fine.
6. The write is stale-safe against the exact bytes the call resolved on. If the file changed in that instant, the call answers `edit refused: <path> changed while the edit was applied` and the same call may simply be repeated. A `patch` computes each updated file from the bytes it read and writes it only while the file still has exactly those bytes, so a change between the patch's read and the write is refused the same way and never overwritten.

### 3.3 Replies

Success (the first line states the outcome and the check, one line per change follows; the lines name the CHANGED LINES AFTER the edit as addresses). A single-file call:

```text
edit: replaced; path src/a.rs; diagnostics: pending; result retained under <detail_ref>; Bridge delivers it automatically when configured
  src/a.rs#Store/transcript:12-16 (L131-135)  replaced (3 lines → 5)
  src/a.rs#@imports:7 (L9)  added
lines after 12 moved +2
```

A multi-file call opens `edit: applied <n> of <n> files; diagnostics: …`, then one line per file (`  1 src/a.rs: replaced; diagnostics pending`) with that file's change lines indented under it. The trailing `lines after <N> moved <±k>` is a fact line, present only when the edit moved lines. Diagnostics words are `pending`, `unknown`, `not_analysed (<reason>)`, `current_clean` and `current_reported (…)` when the answer is already known.

- Verbs: `replaced`, `inserted`, `deleted`, `added`, `removed`, `created`, `unchanged`. A deletion names the address that now holds the join point (`src/a.rs#Store:40`) or, for a deleted symbol, the address it had (`src/a.rs#Store/old_fn  deleted (22 lines)`).
- At most 12 change lines, then `+<k> more`.
- A window edit applied without a remembered read of its lines, and every file-numbered `lines` edit (`{"path":P,"lines":"a-b","content":T}`, which has no unit to guard against), adds the replaced old lines under its change line (at most 8 lines, numbered as before the edit, then `+<k> more`), so a wrong window is visible at once:

```text
  src/a.rs#Store/transcript:12-13  replaced (2 lines → 3); was:
    12	    let turns = self.turns.clone();
    13	    turns
```
- The addresses are valid input for `ide.read` at once; the reply prints no source text.
- Diagnostics words: `pending` (the check stays retained under its reference and Bridge delivers its fact when configured), `not_analysed (<reason>)` for a file no IDE language checks. A check that cannot be held (no open outbox) reports `unknown`. The reply never contains a polling instruction.
- Structured result (hosts that carry it; M-030 decides the envelope): the existing `state`, `result`, `diagnostics` fields plus `changed: [{"address","verb","removed","added"}]`, and for multi-file the existing per-file states.
- `detail_ref` stays in the structured result and in `edit-checked` facts; no `source_ref` text.

### 3.4 The check and tests

- A scheduled project check is never waited for, in any form and for any language that has one: the reply is sent right after the write is observed. The provider (language-server) diagnostics refresh of a single-file edit no longer delays the reply; it only fills in the fact if the project check yields nothing for the file.
- One `edit-checked` fact per operation, unchanged format (session-outbox-v1 §3.3), handed to Bridge when configured. Tests are never started or named by an edit (out of scope for E-003): they run through `ide.test` as today, stay retained under their references and use `test-finished` for Bridge delivery.

### 3.5 Refusals (nothing written, nothing consumed)

Each refusal is one or two sentences and ends with the single repair step. Where the cause is text that no longer matches, the reply carries the unit's current text, numbered as in §2.1 (at most 60 lines or 4 KiB; a longer unit shows the nearest 20 lines and says so).

```text
edit refused, nothing written: old text not found in src/a.rs#Store/transcript. Closest lines:
12	    let turns = self.turns.clone();
13	    turns
Repair: copy `old` from these lines, or edit src/a.rs#Store/transcript:12-13.
```

```text
edit refused, nothing written: old text occurs 2 times in src/a.rs#Store/transcript (src/a.rs#Store/transcript:12-12, :30-30).
Repair: make `old` longer, or address one window.
```

```text
edit refused, nothing written: src/a.rs#Store/transcript has 34 lines, not 40. Current text:
 1	/// Returns the transcript.
…
Repair: resend with a window inside 1-34, or without a window to replace the whole symbol.
```

| Case | Sentence (then the repair step) |
|---|---|
| unknown symbol/region | `no such symbol: <address>` — `ide.outline {"path":"<file>"} lists them` |
| unknown file (not E9) | `no such file: <path>` |
| overlap | `changes <a> and <b> overlap at <address>` — merge them or split the lines |
| candidate does not parse | existing syntax-gate sentence naming the line |
| patch hunk does not match | §5 |
| file changed under the edit | `<path> changed while the edit was applied` — repeat the call |
| read-only activation | unchanged |

## 4. Compatibility and migration

- Tool names and the closed top-level field vocabulary grow only by `patch`, `old`, `new`, `add_import`, `remove_import` (plus the three optional aliases of E4) and lose nothing. `operation_id` stops being required.
- Replies lose the `source_ref` text and gain the changed-address lines. `EditResult.source_ref` stays in the structured result for pre-v1 callers and is `null` when no read base was retained.
- `edit_refused`, `stale_source`, `conflicting_duplicate`, `outcome_unknown`, `unavailable_before_dispatch` keep their outcome words.

## 5. `patch` (Codex apply_patch)

```text
*** Begin Patch
*** Add File: src/new.rs
+pub fn value() -> u32 { 1 }
*** Update File: src/a.rs
@@ pub fn transcript(&self)
-    let turns = self.turns.clone();
+    let turns = self.turns.iter().cloned().collect();
 turns
*** End Patch
```

- Grammar exactly as the Codex tool: `*** Add File: P` (every following line `+`-prefixed), `*** Update File: P` with one or more hunks (`@@` optionally followed by a context line; space = context, `-` = removed, `+` = added; `*** End of File` anchors a hunk at the file end).
- A hunk is located by its context and removed lines, searched forward from the previous hunk: exact lines first, then ignoring trailing whitespace, then ignoring surrounding whitespace, then normalising common Unicode punctuation (the Codex rule). All hunks of all files are resolved before the first write; any miss writes nothing. A hunk with no removed or context lines (add-only) goes after the line its `@@` anchor names when that line is found, else at the end of the file. A file that uses CRLF line endings keeps them on every line the patch writes.
- `*** Delete File:` and `*** Move to:` are refused in v1 with `patch: delete and move are not supported; use a native tool`.
- The result is applied through the multi-file pipeline (same atomicity, one `edit-checked` fact) and replied as §3.3, the change lines naming the addresses of the lines each hunk changed.
- Miss: `patch refused, nothing written: hunk 2 of src/a.rs does not match. Nearest text (lines 40-48 of src/a.rs):` followed by the numbered lines, then `Repair: resend the hunk with these lines as context.`

## 6. Consumed from symbol-address-v1 (what M-029 needs, to be confirmed by M-028)

1. `Address::parse(&str)` and `Display` for `FILE#UNIT[:WINDOW]`, round-tripping, with the window typed (`Option<LineRange>`, unit-relative).
2. `Outline::unit(&SymbolPath) -> Option<Unit>` for symbols AND regions, `Unit { path, kind, range (header-inclusive), body }`.
3. `Outline::unit_at(line) -> Option<Unit>` total over `1..=line_count`, and `Address::at(&Outline, line)` giving the canonical string for a file line (`unit` plus `:N`).
4. `outline_from_source` (the in-process / module source outline, no language server) returns the same regions, so the addresses of an edited candidate can be printed at once.
5. `insert_site` accepts a region as anchor: after `@imports` appends an import, after `@header` for a file without imports.
6. Duplicate names (impl blocks, overloads, same-named test fns) have an address form that parses back to exactly that unit.

## 7. Boundary cases (the contract test, `tests/symbol_read_edit/mod.rs`, included from `tests/product_mcp_contract.rs`)

Inputs are a fixed fixture file (Rust); every row is an exact request → exact reply assertion, with the file on disk asserted after each edit.

| # | Request | Reply |
|---|---|---|
| R1 | read `f.rs#Foo/bar` | header with extent, lines numbered 1..n, doc comment is line 1, no `source_ref` |
| R2 | read `f.rs#Foo/bar:3-5` | lines numbered 3..5 only |
| R3 | read `<abs>/f.rs:130` (inside `bar`) | the whole unit of `bar`, header names `your line 130 is line K` |
| R4 | read `f.rs#@header`, `f.rs#@imports`, a constant | each reads as a unit |
| R5 | read path lines `281-315` of a 314-line file | lines 281-314, success |
| R6 | read path lines `400-420` of a 314-line file | `file has 314 lines; nothing at 400-420`, success |
| R7 | batch `symbols` mixing R1–R4 forms and one unknown symbol | blocks in order; only the unknown item fails |
| E1 | replace `f.rs#Foo/bar` with new doc and body | file bytes changed only in that unit; reply names `f.rs#Foo/bar:1-n` |
| E2 | replace `f.rs#Foo/bar:3-5` | only those lines change |
| E3 | `old` unique inside `bar` but also present in another function | success (uniqueness is per unit) |
| E4 | `old` twice inside `bar` | refusal naming both windows; file unchanged |
| E5 | `old` absent | refusal with closest lines; file unchanged |
| E6 | window `40-50` of a 34-line unit | refusal "has 34 lines" with current text |
| E6a | window edit after this session read `bar`, then lines inside the window changed (by an earlier edit or outside) | refusal with current text; file unchanged |
| E6b | window edit of a unit never read in this session | applied; reply echoes the replaced old lines |
| E6c | window edit after a read, change only outside the window | applied |
| E7 | insert after `f.rs#Foo/bar` | new symbol after it, reply names its address |
| E8 | `add_import` twice | first `added`, second `unchanged`; import block otherwise untouched |
| E9 | `remove_import` | statement gone |
| E10 | `content` replace of an existing file, no `source_ref` | replaced |
| E11 | any form with no `source_ref` and no `operation_id` | success |
| E12 | patch with Update + Add | both applied; one refused hunk anywhere → nothing written |
| E13 | multi-file call with one failing `old` | nothing written in any file |
| E14 | a project check held for 12 s | the reply returns before the check ends; the `edit-checked` fact arrives once |
| E15 | stale `source_ref` supplied | refused exactly as before (deprecated path) |
