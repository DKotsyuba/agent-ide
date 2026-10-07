---
name: agent-ide
description: Use Agent IDE `ide.*` tools for understanding and changing source code — `ide.outline`/`ide.symbol`/`ide.graph` instead of reading files and grep, `ide.edit` with a project check in the reply, `ide.test` by symbol. Native read/grep/edit remain for non-source files and as a fallback.
---

# Agent IDE

Agent IDE exposes the lifecycle tools `ide.start`, `ide.context`, `ide.diff`,
`ide.inspect`, `ide.stop`, `ide.edit`, the symbol tools `ide.outline`, `ide.read`,
`ide.symbol`, `ide.graph`, and `ide.test`. Use this workflow whenever they are offered and the
task touches source code — understanding it (where is X defined, who calls it, what does this
file contain) as much as implementing, debugging, refactoring or testing. For a source
question, `ide.outline` / `ide.symbol` / `ide.graph` answer from the language server in one
call where grep and file reads take several and miss dynamic dispatch. When `ide.*` is offered
it is the required path for source: native read/grep/edit tools are for prose, configuration and
data files, and for source only when `ide.*` is absent or a named call answered `unavailable` —
say which call in your report.

## Deferred tool discovery

When this skill is required for source work and `ide.*` is not already visible,
use the host's tool discovery once before declaring Agent IDE unavailable. With
`ToolSearch`, search for `Agent IDE ide.start ide.context ide.diff ide.inspect
ide.stop` and load the exact tool references it returns; do not assume whether
the MCP server name uses a hyphen or underscore. A configured MCP missing from a
code-mode `ALL_TOOLS` snapshot is not sufficient evidence of unavailability,
because deferred MCP tools may be omitted there. If discovery is absent, returns
no matching tools, or a discovered tool fails, continue with the fail-open rule
below. Do not repeat discovery in a loop.

Tool schemas loaded earlier in a long or resumed conversation can predate an Agent IDE
upgrade. When a reply or this skill names a parameter your loaded schema lacks (for example
`read_only` or `environment` on `ide.start`, `cwd` or `env` on `ide.test`), load the tool
definitions again once; the server accepts every current parameter either way.

Each accepted reply carries one compact decision-facing text block that states every fact needed
for the next action — state, `detail_ref`, `continuation`, `retry`, Edit outcome and `source_ref`.
Replies come back complete when the work finishes within a few seconds (the usual case on a
warm language server); only a longer job answers `pending` — then call `ide.inspect` with its
`detail_ref` to retrieve the result.

## Understanding code by symbols

Prefer the symbol tools over reading whole files; they answer from the language server
and cost a fraction of the context:

- `ide.outline {"path":"src/assistance/"}` — a directory: subdirectories with file counts, then
  files with line counts and the first documentation line, one level deep.
- `ide.outline {"path":"src/x.rs"}` — the file skeleton: every symbol with its signature,
  doc line and line numbers, members indented, test modules collapsed. Use it before
  reading any file longer than a screen. Add `"kinds":"fn,method"` (comma list over the closed
  kind vocabulary) to cut a big file down to the kinds you need; a container of a selected
  member still shows, and the footer states the cut (`showing N by kinds …`).
- `ide.symbol {"symbol":"src/x.rs#Type/method"}` or `{"symbol":"Name"}` — a symbol card:
  resolved signature, documentation, the definition's address and line range (read its body
  with `ide.read`), usages grouped by `src`/`tests` with the source line text, one level of
  callers (on by default; `"callers":0` omits them), one level of callees with `"callees":1`
  and, with `"history":true`, the last three commits that touched the definition.
  Use `ide.graph` for a bounded multi-level callers/callees tree instead of the symbol card's
  one-level relationships; functions, methods and constructors are nodes, and tests collapse
  to one `+N tests` line per parent unless you pass `"tests":true`. Python has no call
  hierarchy here: `ide.graph` answers `callers/callees unavailable for python (pyright has no
  call hierarchy); use ide.symbol usages`, and the card's callers read `unavailable` — use the
  card's usages instead.
  A bare name that matches several symbols answers with the candidate paths; repeat
  with one exact path. CSS and HTML names are addressed by sigil (`.btn`, `##main`,
  `--brand`): `ide.symbol` answers a name card with every defining file, `ide.read` reads the
  first definition; the `symbols` batch form takes file paths only.
- A file no IDE language reads (`Cargo.toml`, a shell script, a template) answers
  `unsupported_file: <path>` from `ide.outline`, `ide.read {symbol}`, `ide.symbol` and
  `ide.graph`; read it with `ide.read {"path":…,"lines":…}` or native tools.
- `ide.read {"symbol":"src/x.rs#Type/method"}` or `{"path":"src/x.rs","lines":"120-180"}`
  — the body with line numbers; its `source_ref` is what `ide.edit` needs. Each line reads
  `NNN<TAB>code`: one tab separates the number from the code, so stripping the gutter leaves
  the code exactly as it is in the file.
- `ide.read {"symbols":["src/x.rs#A","src/x.rs#A/b","src/y.rs#C"]}` — several bodies in one
  reply, request order, one `source_ref` valid for every file included; unknown symbols are
  listed per item without failing the rest. Over-budget blocks are named in a footer
  (`not included (over the reply budget): …`) with the exact call to fetch them; a first block
  too large for one reply is delivered alone and paged through `ide.inspect` — request the
  other items again. `{"path":…,"ranges":["10-20","44-60"]}` reads several line ranges the
  same way.
- `ide.edit {"operation_id":"…","path":"src/x.rs","source_ref":"…","changes":[…]}` — 1–32
  changes to one file in one call: entries by `lines` (numbers of the version you read;
  empty `content` deletes the range), by `symbol` (`op` `replace`/`insert`/`delete` with the
  same `where` as the single forms; rename is a single-call form only), or by `old`/`new`
  exact text (must match once; add `"within":"sym"` to scope it). Exactly the matched bytes
  are replaced — `old` may start or end mid-line and may include the line terminator, and
  `"new":""` deletes the match; several `old` changes on one line apply together unless their
  matched bytes intersect. All addresses resolve against the version `source_ref` names;
  overlaps, no-match and multi-match are refused with the exact per-change fix and nothing
  is written — retry with the same `operation_id`. The edit is refused the same way when the
  result would not parse and the file parsed before (the reply shows the line and the change
  that caused it); a file that already had a syntax error is edited anyway, with a note
  saying so. The success reply lists where each change landed in the final
  file (`3 changes applied: change 1: lines 12–20 replaced (now 12–24); …`). After line movement, re-read before line-range edits; `old` and symbol edits can use the reply source. Prefer one read + one batch edit over a call per change.

The single-call edit forms use the same symbol paths:

- `ide.edit {"operation_id":"…","op":"replace","symbol":"src/x.rs#Type/method","content":"…"}`
  — new body for the symbol; `content` is the whole symbol including its doc comment and
  attributes.
- `ide.edit {"operation_id":"…","op":"insert","symbol":"src/x.rs#Type/anchor","where":"before"|"after"|"first"|"last","content":"…"}`
  — a new symbol placed relative to the anchor (`first`/`last` inside a container such as an
  impl, class or module); indentation follows the neighbours.
- `ide.edit {"operation_id":"…","op":"delete","symbol":"…"}` — removes the symbol with its header.
- `ide.edit {"operation_id":"…","op":"rename","symbol":"…","new_name":"…"}` — project-wide rename
  by the language server (it waits while the server loads); the first line reads
  `edit: renamed`, and a later line `renamed old → new; N sites in M files: a.rs (2), b.rs (1)`
  names up to five files (`; +K more` for the rest). Its diagnostics, `path` and `source_ref`
  cover only the last file written: check the rest with `ide.context {"kind":"problems"}` and
  re-read any other touched file before editing it.
- `ide.edit {"operation_id":"…","path":"src/x.rs","lines":"120-180","source_ref":"…","content":"…"}` —
  replaces a line range when the target is not a symbol (imports, constants, configuration).
  `source_ref` is required: the `ide.read` the lines came from. The edit is refused
  `stale_source` (no write) when the file changed or the reference did not cover a complete read.
  Re-read with `ide.read` and inspect every page before retrying. The symbol forms take `source_ref` too
  (optional; validated when given).
  After `ide.edit`, base `old` and symbol edits on the reply `source_ref`; after an edit that moved lines, line-range edits need a fresh `ide.read` of the current file.
- `ide.edit {"operation_id":"…","path":"src/new.rs","content":"…"}` — creates a file that does
  not exist yet (no `source_ref`: there is nothing to read) and answers `edit: created` with the
  same check — prefer it over native creation so the diagnostics arrive in-reply. The same call
  on an existing file is refused with no write: read it first and pass its `source_ref`.
- When a reply carries `lines after X moved ±N`, any further line-range edit must start from a fresh
  `ide.read`, not from the line numbers you held before the edit.

Every edit form formats the candidate with the project's formatter, then runs the project
check (cargo check, pyright or tsc) and answers with the edited file's problems from it:

- `current_reported (project check 1.2s: E new errors, W new warnings; X pre-existing errors,
  Y pre-existing warnings)` followed by up to eight `new: path:line:col severity [code]
  message` or `pre-existing: …` lines — fix the `new:` ones. When the project check could not
  analyse the file (a Python root with no environment, a check that could not run), the
  language server's diagnostics for the exact edited text answer the same way, labelled
  `project check did not analyse this file: <reason>`.
- `current_clean` — the completed check named nothing in this file. When it adds `for this
  file; the project check reports N errors in other files`, read them with
  `ide.context {"kind":"problems"}`: the project still does not build.
- `not_analysed (<reason>)` — nothing checked this file; follow the step the reason names.
- `unknown` — a broken edit is usually reported within seconds, but the check waits at most
  90 s; after that the reply says `diagnostics: unknown` and `ide.context {"kind":"problems"}`
  shows the result when it lands.

Symbol paths are `file#Owner/name`: `#` separates the file, `/` is nesting (impl, class,
namespace, module → member). Inherent `impl Foo` members are addressed as `Foo/method`;
trait impls keep the server's label as the segment, paths and generics as written
(`impl fmt::Display for Foo/fmt`) — copy it from `ide.outline`. When `Type/name` names both a
field of the type and a method of one of its blocks, it resolves to the method; edit the
field through its type. A `provider_loading` error means the language server is still loading
the workspace: repeat the same call in a few seconds.
While it loads — and when it is unavailable (its workspace failed to load) or its
documentSymbols request failed — `ide.outline`, `ide.read {symbol}` and symbol edits of a
Rust file may already answer from the lexical outline (the reply says `outline: from
source, exact (<server> still indexing; no need to repeat)` while it loads, `outline:
from source, exact (<server> unavailable; no need to repeat)` when it failed, `outline:
from source, exact (<server> request failed: <short cause>; no need to repeat)` when the
exchange did — the outline is exact, so do not repeat the call). A TypeScript file whose
module resolution the server cannot verify answers the same way (`… project resolution
unverified: <cause> …`). A file the lexical outline cannot reproduce exactly, and an address
it does not contain, wait for the server while it loads (a symbol edit answers
`provider_loading` at once instead — repeat it in a few seconds) and answer
`provider_unavailable` when it failed. `ide.symbol` waits while the server loads; when it is
unavailable it answers the definition-only card (signature, doc, definition from the outline)
with `unavailable (<server> workspace failed to load)` notes for usages and callers.
`ide.graph` and rename always wait for the server. A `provider_unavailable` refusal on these
paths names its stage and what still works: a stage ending `outline and read answer from
source` (Rust) means `ide.outline` and `ide.read` still work while usages and callers do
not; a stage ending `use native reads` (other languages, and a Rust file the lexical outline
cannot reproduce) means no source outline answers either — read with native tools. Recover
the same way in both cases: retry later; if it keeps failing, fix what stops the project from
loading — `ide.context {"kind":"problems"}` shows the project check.

## Workflow

1. `ide.start` once per actor per worktree, optionally with `"root": "/absolute/dir"`
   to name the working directory (default: the host's project directory), and optionally
   with `"activation_id": "<stable id>"` — omitting it is fine: the session then keeps a
   stable default, so repeating the start still returns the same activation. Orchestrators
   and reviewers should start with `"read_only": true`; omit it or set `false` for the
   worktree's one writer. Its reply carries the project card: Git state when available,
   languages with line counts, the project's build / check / test / lint commands with their
   provenance, toolchain, environment, layout, entry points and docs — read it before
   exploring by hand. The root, the detected Git worktree and its Git directory must also lie
   inside the operator's configured `allowed_roots`; `outside_allowed_roots` means start the IDE
   in an allowed directory (or ask the operator to extend the list) and otherwise continue
   with native tools. A refusal names its cause: a not-yet-created root includes its nearest
   existing allowed ancestor; Git discovery, unresolved worktree, durable state, worktree-holder,
   provider-cache and cache-lease (`start:cache_lease`) conflicts have separate recovery advice.
   A repeated start by the same actor and binding reuses and names the existing activation.
   Do not call it again for later edits in the same worktree.
   A `read_only: true` activation can read alongside a writer and other readers. Its
   `ide.edit` and `ide.test` calls are refused with one read-only message naming the current
   writer when there is one. Starting the same activation without `read_only` upgrades it
   when the writer slot is free; a writer can downgrade by starting with `read_only: true`.
   Each Python project root has one current environment, used for semantic imports, the
   check, tests and formatting; the card shows it with its source and a `choose:` hint for the
   other candidates. A nested root without its own environment uses the worktree root's.
   Switch with `ide.start {"environment": {"python": "<candidate or path>"}}`
   (`python:<root>` for a nested root, `auto` resets); the choice persists for the worktree,
   and a `read_only` activation cannot change it. A selector outside the worktree and every
   allowed root is refused `outside_allowed_roots (environment <root> <selector>)`: select the
   project's own venv directory (the one that holds `bin/python`), never a base interpreter.
   Directories without Git work; `ide.diff` and requested symbol history there answer
   `not a git repository: no git data`.
2. Before an edit, prefer `ide.outline` the file (or `ide.symbol` the target) for its skeleton,
   then `ide.read` the exact symbol or line range you are about to change — its `source_ref` is
   the bounded reference `ide.edit` needs. `ide.context {path}` with no `byte_offset` still works
   and still mints a usable `source_ref` (its first line is a hint toward `ide.outline`/
   `ide.read`); `ide.context {path, byte_offset}` answers definitions/references at that
   exact position when needed. Path-only context is the file text; use `ide.symbol` or
   `ide.read {symbol}` when definitions or references are needed.
3. Prefer `ide.edit` (symbol, `lines`, `changes` or new-file form) whenever it is offered.
   Native host editing — the model's own file-edit tool (Claude's editor, Codex
   `apply_patch`, etc.) — remains available when `ide.edit` is inactive, unavailable, refused
   with advice to continue natively, or uncertain.
4. After a known `ide.edit` outcome (`created`, `replaced`, `unchanged`; the first line
   names the operation instead when it was an insert, delete or rename), follow
   its diagnostic state: `current_reported` → fix the `new:` lines with `ide.edit` and the
   returned `source_ref`; `current_clean` → `ide.diff`; `current_clean for this file; the
   project check reports N errors in other files` → `ide.context` with `kind: "problems"`
   first; `unknown` → `ide.context` with `kind: "problems"`; `not_analysed (<reason>)` → no
   completed project check covers the file, so follow the reason's next step: an undeclared
   Rust file → declare its `mod`; a Rust file built only under a `cfg`/`#[path]` gate → check
   it with a build or `ide.test` command that enables it; a Python file with no covering root
   or environment → create one or pick it with `ide.start` `environment`; `no IDE language
   checks this file type` → nothing to check, continue. A pending diagnostic still requires
   `ide.inspect` with its returned `detail_ref`. An `outcome_unknown` *result* is a different
   unknown from `unknown` *diagnostics*: the edit's own effect, not just its diagnostics, is
   unproven, so inspect the named path with native host tools instead — never call
   `ide.context` to replay it, and never blind-retry.
5. `ide.test` to run the project's tests on request — never the whole suite by reflex:
   `{"symbol":"src/x.rs#Type/method"}` runs the tests that reference the symbol (including
   in-file `mod tests`), `{"path":"src/x.rs"}` the file's tests, `{"pattern":"name"}` a runner
   filter, `{"command":["cargo","test","--lib"]}` an exact argv; optional `budget_s` (default
   120, max 600). Explicit commands may also set `cwd` relative to the worktree root (it must
   resolve to a directory inside the worktree) and `env` as up to 32 bounded environment
   overrides. The combined command arguments are limited to 16 KiB. A `path` or `symbol`
   target runs the runner of that file's language — in a mixed worktree a `.py` test path
   selects pytest, never cargo — while a bare pattern or command uses the first detected
   project. The reply is `tests #N: started — <argv> [(K tests selected)] (budget B s); poll:
   call ide.test with {"status": N}`. A `symbol` target may first answer `pending` (use
   `ide.inspect`); an explicit `command` that ends within a few seconds answers its result
   directly; while a run is active in the worktree, another start answers `tests #N: still
   running …` for that run. Call `ide.test {"status":N}` to poll until `tests #N: P passed,
   F failed, T s` with up to sixteen `FAIL name` / `file:line message` entries (more add
   `(+N more failed tests in the full output)`), a `rerun:` line that reproduces the run
   (`cd <dir> && env NAME=VALUE … <argv>` when `cwd` or `env` was given) and `full output:
   ide.inspect <detail_ref>`. `ide.inspect` also accepts a test-run handle (`tests #N`,
   `tests-N`, `#N`, `N`) and answers that run's current result; a worktree keeps at most four
   finished runs. After `ide.stop`, `ide.test {"status":N}` is refused; its reply names
   `ide.inspect {"detail_ref":"tests #N"}` as the way to read that run, which still answers.
   Known pytest, cargo test, vitest/jest, and go test summaries are parsed from output regardless
   of whether the command called the runner directly or through a wrapper. When no known test
   summary appears, the result gives the exit code and a bounded output tail labelled `output`;
   long output has a full-output inspection route. Status plates show only this activation's own
   test jobs, so another actor's earlier run is not attributed to this session.
   One job per worktree at a time; a stopped budget says `stopped at budget`. The
   `<agent-ide>` block carries the job's line while it runs (it may repeat with the elapsed
   time updated) and once when it ends.
6. `ide.diff` before finishing the task, to review the accumulated change; before finishing a
   task whose work you committed, review it with `ide.diff {"mode":"task"}`, which includes
   commits since activation (if no activation commit was recorded it directs you to `head`).
   In `head`, `unstaged` and `task` modes untracked text appears as additions against empty,
   captured best-effort: a file that is binary, oversized, unreadable, special or changing
   during capture stays name-only with a note and never breaks the diff. In `staged` mode
   untracked paths are listed by name only, and staged pages ignore worktree edits. `paths`
   (1–16 literal worktree-relative files or directories, for example
   `{"mode":"head","paths":["src","tests/new.rs"]}`) limits capture and budgets to them. The
   path inventory is separate from the hunks a page delivers. A hunk larger than one reply
   arrives in parts labelled `hunk N of T, lines A-B of L; continues on the next page`, the
   final one `; last part`; a single line too long for any reply becomes a notice with the
   `ide.read {path, lines}` that shows it, and paging continues past it. When retention is
   full, a page says to recapture the remaining lines instead of offering a continuation.
   Capture limits direct you to narrower `paths` or native Git. With `"provenance": true` the
   header's `current_tree` line says what the capture established; call `ide.diff` again
   after committing, staging or adding files.
7. `ide.inspect` with the returned `detail_ref` whenever a reply is `pending` or says
   `Output is truncated; use ide.inspect with detail_ref …` (a diff: `hunks: N more
   (ide.inspect …)` or a hunk part that continues). Do not repeat the original call instead.
   The reply you hold is page one, so the first inspect returns page two; repeat until the
   last page and concatenate them in order. Context, outline, read, symbol, graph and
   test-output pages start with `page N; bytes A-B of TOTAL`, and the last reads `page N
   (last); …; complete`. Diff pages start with their `diff (<mode>): …` summary. Claude
   receives text only, so rely on these markers, not on a `continuation` field. A whole source
   up to 1 MiB is delivered this way; do not `ide.edit` from a `Context` until its last page
   arrived — the edit is refused as `stale_source` otherwise; use the newest `source_ref` when
   supplied, or re-read before retrying. Calling `ide.inspect` again after the last page just
   repeats the last page. If inspect reports `source_changed`, the remaining pages are gone and
   nothing from them is safe to edit from: call `ide.read` or `ide.context` on that path again.
   A diff page refused with `source_stale` means a tracked file changed after capture: call
   `ide.diff` again. A root-less start refused for `missing_pre` may also include a `retry`
   hint with the current directory as an explicit root.
8. `ide.stop` at handoff to another actor, or when the task ends, to release this binding's
   activation (`activation_id` is optional). Its reply lists test runs you have not collected;
   read them with `ide.inspect {"detail_ref":"tests #N"}` before reporting results.

## Project problem feed

The `<agent-ide>` block is a status plate for the worktree's Rust, Python, and
TypeScript/JavaScript checks. It arrives
whenever the status changes and not while it stays the same, so no block means unchanged, not
finished. On Claude it is sent in hook context. On Codex it may arrive in native hook context
right after one of your actions (the operator must have installed and trusted the hooks; a plate
is delivered by whichever channel fires first and never repeated by the other) — otherwise a due
plate leads a terminal `ide.*` reply instead: the plate is the first thing you read, followed by a
newline and the normal reply text, and the `structuredContent` object carries it verbatim as its
`status` field. `ide.stop` replies and `pending` placeholders carry no plate. `rust: checking
(first check)` means no result yet this session;
`rust: checking (files changed; last result: 0 errors, 1 warning)` means a check is running and the
last counts may be outdated; the next plate carries the result with `(+N)`/`(-N)` deltas.
When counts rise after your edit, call `ide.context` with `{"kind": "problems"}` (optionally
`"language": "rust" | "python" | "typescript"` and `"offset"`) to read the bounded problem
list before continuing. The `typescript` filter covers `.ts`, `.tsx`, `.mts`, `.cts`, `.js`,
`.jsx`, `.mjs` and `.cjs`.
An LSP diagnostic push can give only a provisional lower bound for TypeScript/JavaScript;
claim exact Clean only after a completed configured project check.
A `git: HEAD moved … outside Agent IDE` line (delivered once) means another process switched the
worktree's branch: symbol answers you already hold may describe the old branch, so re-read before
editing from them. A change of a Python root's environment (selected, recreated or gone) is
also announced once, and its check runs again.
`check failed (<reason>)`, `check timed out`, `tool not found`, `environment not found` (or the
environment's own missing-cause line), `no files analyzed`, `outside allowed roots`,
`unavailable: read_restricted` or `checks disabled` mean the feed has no counts for that
language — never treat them as a clean result.

## Pending replies

Both Codex and Claude call the IDE tool directly. When a reply is `pending`, call `ide.inspect`
with the given `detail_ref`, and again every few seconds while it stays `pending`; never run a
helper command.

## Independence and fallback

Each child or subagent activates `ide.*` independently: a parent's
`ide.start` does not cover its children, and each must call its own
`ide.start` before relying on the workflow. An `unknown` diagnostic, a
`stale_source` edit or an `unavailable` reply is not a clean result. Follow the
recovery the reply names once (`unknown` → `ide.context {"kind":"problems"}`;
`stale_source` → re-read with `ide.read` and retry; `Call ide.start, then repeat
this call` → do that); otherwise, and whenever `ide.*` is inactive, unsupported
or its reply is not usable, fall back immediately to native host tools and
CodeGraph or native search — never block source work waiting for `ide.*`.
A `retry` fact names the exact recovery: `daemon restarted; repeat this call
once` (on `ide.start`) or `session re-rooted to the requested root; repeat this
call once` → repeat that exact call once before falling back; `daemon restarted;
call ide.start first, then repeat this call with fresh references` → call
`ide.start`, then repeat with new `detail_ref`/`source_ref` values from fresh
reads; `session re-rooted to the requested root; repeat this call once, or call
ide.start without root` → repeat once, and if it stays unavailable call
`ide.start` with no `root` to return to the host's project directory. A start
reply may end with `daemon: <version> still serving (another session is
active); restart that session or wait for it to stop for <current>`: another
session keeps the older daemon alive, so this session uses its behaviour until
that session stops or restarts and this session starts again. `daemon: older
than 0.6.7 still serving` means that daemon must be stopped manually or left to
idle out.

## Boundaries

Do not assume `ide.check` or `ide.finish` exist. Discover whether `ide.edit` is
offered for this session; its absence never blocks the truthful native fallback.

Claude Code's built-in read-only reviewer agents have a fixed tool list. To let a custom agent use
Agent IDE, add the `mcp__agent-ide__ide_*` tools it needs to that agent's `tools:` frontmatter and
include `ide.start` when it needs an activation. The bundled `ide-reviewer` agent is an example.
