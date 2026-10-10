# Symbol and graph item v1 contract

Revision: symbol-graph-item r2; tracked boundary id `symbol-graph-item`, revision 2. Provider: the
single-target symbol tools `ide.symbol {symbol}` and `ide.graph {symbol}`
(`crates/agent-ide-core/src/assistance/symbols.rs`). Consumers: the batch forms
`ide.symbol {symbols}` and `ide.graph {symbols}` ([batch API v1](batch-api-v1.md) §4-§5), which
answer every entry exactly as the single form answers it. Vocabulary: [tools v0.4](tools-v0.4.md);
deferred delivery: [session outbox v1](session-outbox-v1.md).

Purpose: one per-item engine decides how a symbol or graph root resolves, which usages it lists,
and how it says that an answer is approximate or unavailable. The batch forms call it once per
entry and never re-implement it.

## 1. Item engine

- `symbols.rs` exposes one per-item function for `ide.symbol` and one for `ide.graph`. The single
  form calls it once; a batch form calls it once per entry with the shared options (`usages`,
  `callers`, `callees`, `history`; `direction`, `depth`, `tests`).
- Mode:
  - `allow_approximate`: may answer at once while the file's language server is loading (§4);
  - `precise`: waits for the server within the caller's job deadline, as the single form did in
    0.10.8.
- Outcome, exactly one of:

| Outcome | Meaning | What the caller does later |
|---|---|---|
| `precise(text)` | Final answer. | Nothing. |
| `approximate(text)` | Labelled approximate answer (§4). | Calls the engine again in `precise` mode once the server is ready, and delivers that answer. |
| `deferred` | No answer yet: the server is starting and the language has no exact source outline. | Same as `approximate`, but the caller received nothing for this entry. |
| `unavailable(cause, text)` | Graph only: every requested direction of the root faulted (§6). `text` is the single form's reply. | Nothing. |
| `failed(code, detail)` | Closed failure codes, unchanged (`unknown_symbol`, `ambiguous_symbol`, `provider_unavailable`, `source_unavailable`, …). | Nothing. |

- The item text is exactly the single form's reply text for that address and those options, one
  bounded block. Rust names are fixed when the provider lands; the semantics above change only
  with a new revision.
- The engine creates no reply, no `detail_ref`, no job and no outbox registration; its caller does.
  A batch keeps one job and one `detail_ref` for all entries.
- Batch framing, per-entry isolation, ordering, reply budget, the counting of approximate and
  unavailable entries, and the follow-up of approximate or deferred entries are the batch
  contract's. The batch re-runs those entries in `precise` mode inside its own job, and the
  follow-up fact names the batch `detail_ref` and the entry indexes (session outbox v1 §13.3). The
  single form's follow-up is §5.

## 2. Bare names

- Address forms are unchanged: `file#Owner/name`, a bare `name`, a sigil address.
- The candidates of a bare name (workspace symbols, outline scan, name index) are collapsed by
  rendered address before anything is decided; a declaration wins over the impl blocks that share
  its address.
- Exactly one address left: the entry is answered for it in the same call.
- Two or more: `ambiguous_symbol: <requested> matches <N> symbols; …` with `N >= 2` and the
  candidate list, wording otherwise unchanged. `matches 1 symbols` never appears.
- None: unchanged (`unknown_symbol`).
- While the server is loading (§4), a bare name resolves from the source outlines of the
  worktree's files of that language with the same collapse rule, and the entry is approximate.

## 3. Usages and construction sites

- Usage rows keep their form: `  <file>:<line>  [<tag> ]<line text>`.
- For a symbol of kind `struct`, `enum` or `class`, a row is a construction site when, at a
  whole-word occurrence of the name in its line, after optional spaces and one optional generic
  list (`<…>` or `::<…>`), the text continues with:
  - `{` (struct literal), or `(` (tuple struct, class call);
  - `::new(`, `::default(` or `::from(`;
  - for an enum, `::<Variant>(` or `::<Variant> {`, or the bare variant path;
  - or the occurrence is preceded by `new ` (TypeScript and JavaScript).
- Never a construction site: the definition line; a line whose trimmed text starts with `use `,
  `import `, `from `, `impl`, `struct `, `enum `, `class `, `trait `, `type ` or a visibility
  prefix of those; an occurrence directly preceded by `->`.
- Tag `construction ~`. The existing legend line `(~ = heuristic match, not proven)` prints
  whenever a tag contains `~`.
- Construction rows come first within the source group and within the test group.
- Summary line: `usages: <N> in <F> files (src <S>, tests <T>; <C> constructions)`; the
  `; <C> constructions` part appears only for those kinds and only when `C >= 1`.
- The classification is lexical and per line: macros, `use … as` aliases and patterns
  (`Name { a, .. } =>`) can be misclassified, which is why every such row carries `~`.

## 4. Approximate answers while the server loads

Applies when the file's language server is loading and the language's source outline is exact
(`outline_while_loading`; Rust only in this revision).

- First line of the text:
  `approximate: <server> is still indexing; usages and callers below are name matches from source, marked ~`.
  The single form appends `; the precise answer follows as a fact`.
- Symbol card:
  - heading, signature, doc and definition come from the source outline;
  - usages are every whole-word occurrence of the name in the worktree's files of that language,
    except the definition line, each tagged `~` (or `construction ~`, §3). Files come from the
    name index's listing (Git tracked plus untracked, not ignored; a bounded walk without Git),
    at most 20 000 files, each at most the source byte limit, within a 2 s scan budget. When a
    bound stops the scan, the usages line ends with `(scan stopped: <bound>; partial)`;
  - callers, when requested: the enclosing callable (source outline, kinds function, method,
    constructor, test) of every occurrence whose name is directly followed by `(` (after an
    optional `::<…>`), declarations excluded, one row each, tagged `~`;
  - callees, when requested: `callees: follows with the precise answer`;
  - history: unchanged (Git).
- Graph:
  - root from the source outline; depth 1 only;
  - callers as above, each enclosing callable once, edges to the root, tagged `[~]`;
  - callees: `callees: follows with the precise answer`;
  - header `graph: callers of <root> (approximate, depth 1 of <d>, <n> nodes, <m> edges)`.
- A language without an exact source outline (Python, TypeScript in this revision) never parks
  inside the call while its server starts: the entry is `deferred`. The single form answers at
  once with the ordinary `pending` shape and its `detail_ref`; the outbox delivers the answer as
  the generic pending `job-ready` fact (`holds_turn = true`).
- No entry text asks the agent to poll, wait or repeat.

## 5. Precise follow-up of the single form

- The job that answered approximately keeps running in `precise` mode, re-checking the server
  every 300 ms for up to 10 minutes after the call.
- At most 4 such background jobs per session. Beyond that, the approximate text ends with
  `precise answer not scheduled: 4 already pending for this session` and no job continues.
- When it settles, the precise reply (or its final failure: `provider_loading` after 10 minutes,
  `provider_unavailable` when the server failed) replaces the job's retained detail and is pushed
  as one `job-ready` fact under session outbox v1 §13.2:

| Field | Value |
|---|---|
| subject | `symbol <requested>` or `graph <requested>` |
| holds_turn | `false` |
| source_bearing | `true` |
| fence_paths | `[<definition file>]` |
| detail_ref | the job's own `detail_ref` |
| failed | `true` for a final failure |

- If the approximate reply never reached a live caller (the caller's inline wait ended first and
  it received `pending`), the job's single fact is the generic pending one (`holds_turn = true`).
  A job yields one fact either way.
- `ide.inspect` of the `detail_ref` returns the precise answer once settled, re-proved against the
  definition file.

## 6. Faulted call hierarchy

A failed call-hierarchy request (prepare, incoming or outgoing) is reported where it happened and
is never rendered as an absence of calls.

| Where | Rendering |
|---|---|
| Symbol card | `callers: unavailable (<cause>)` or `callees: unavailable (<cause>)` (unchanged) |
| Graph root, one direction | that direction's section reads `callers: unavailable (<cause>)` (or `callees:`); its edges are not counted |
| Graph root, every requested direction | outcome `unavailable`; single form: `graph: callers of <root> unavailable (<cause>)`; batch: the root is `unavailable (<cause>)` (batch API v1 §5) |
| Deeper graph node | its row ends with ` (callers unavailable: <cause>)` (or `callees`) |

- Causes, closed: `prepare call hierarchy request failed`, `incoming calls request failed`,
  `outgoing calls request failed`, `<server> transport gone`.
- Such a reply is journaled as degraded.

## 7. Compatibility checks

Provider:

1. A struct with impl blocks resolves by its bare name in one `ide.symbol` call and one `ide.graph`
   call.
2. The construction classifier matrix: literal, tuple call, `::new(`, `new X(`, enum variant;
   and not: definition, `use`/`import`, impl header, `-> Name {`.
3. A forced call-hierarchy fault (test seam) renders `unavailable (<cause>)`, never 0 edges; a
   mutation that restores the empty-list fault path makes the test fail.
4. A loading Rust server yields an approximate entry at once and the precise reply later, pushed
   once with the §5 fields; a loading Python or TypeScript server yields `pending` at once.
5. No text added by this contract contains a poll, wait or repeat instruction.

Consumer: each batch entry's block equals the single form's reply text for the same address and
options, except the follow-up sentence of §4 and the batch framing.
