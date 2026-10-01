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
  resolved signature, documentation, the definition with line numbers, usages grouped by
  `src`/`tests` with the source line text, `callers`/`callees` (set `"callers":1`) and, with
  `"history":true`, the last three commits that touched the definition.
  Use `ide.graph` for a bounded multi-level callers/callees tree instead of the symbol card's
  one-level relationships; only functions and methods are nodes, tests collapse to one
  `+N tests` line per parent unless you pass `"tests":true`.
  A bare name that matches several symbols answers with the candidate paths; repeat
  with one exact path.
- `ide.read {"symbol":"src/x.rs#Type/method"}` or `{"path":"src/x.rs","lines":"120-180"}`
  — the body with line numbers; its `source_ref` is what `ide.edit` needs. Each line reads
  `NNN<TAB>code`: one tab separates the number from the code, so stripping the gutter leaves
  the code exactly as it is in the file.

Edit by symbol with the same paths; every form formats the candidate with the project's
formatter, then runs the project check (cargo check, pyright or tsc) and answers with the
edited file's problems from it — `current_reported` lists `path:line:col severity [code]
message` lines and the check's duration, `current_clean` means the completed check named
none. A broken edit is usually reported within seconds; the check waits at most 90 s, after
which the reply says `unknown` and `ide.context` shows the result when it lands.

- `ide.edit {"operation_id":"…","op":"replace","symbol":"src/x.rs#Type/method","content":"…"}`
  — new body for the symbol; `content` is the whole symbol including its doc comment and
  attributes.
- `ide.edit {"operation_id":"…","op":"insert","symbol":"src/x.rs#Type/anchor","where":"before"|"after"|"first"|"last","content":"…"}`
  — a new symbol placed relative to the anchor (`first`/`last` inside a container such as an
  impl, class or module); indentation follows the neighbours.
- `ide.edit {"operation_id":"…","op":"delete","symbol":"…"}` — removes the symbol with its header.
- `ide.edit {"operation_id":"…","op":"rename","symbol":"…","new_name":"…"}` — project-wide rename
  by the language server; the reply reads `edit: renamed` and its second line lists every touched
  file with its site count.
- `ide.edit {"operation_id":"…","path":"src/x.rs","lines":"120-180","source_ref":"…","content":"…"}` —
  replaces a line range when the target is not a symbol (imports, constants, configuration).
  `source_ref` is required: the `ide.read` the lines came from. The edit is refused
  `stale_source` (no write) when the file changed or the reference did not cover a complete read.
  Retry with the newest `source_ref` in the reply; if none is given, re-read with `ide.read` and
  inspect every page before retrying. The symbol forms take `source_ref` too
  (optional; validated when given).
  After `ide.edit`, base the next edit of that file on the `source_ref` in the edit reply.
- `ide.edit {"operation_id":"…","path":"src/new.rs","content":"…"}` — creates a file that does
  not exist yet (no `source_ref`: there is nothing to read) and answers `edit: created` with the
  same check — prefer it over native creation so the diagnostics arrive in-reply. The same call
  on an existing file is refused with no write: read it first and pass its `source_ref`.
- When a reply's last line reads `formatted: +N lines after line X; use source_ref …`, the
  formatter moved lines: any further line-range edit must start from a fresh `ide.read`, not
  from the line numbers you held before the edit.

Symbol paths are `file#Owner/name`: `#` separates the file, `/` is nesting (impl, class,
namespace, module → member). Inherent `impl Foo` members are addressed as `Foo/method`;
trait impls keep `impl Trait for Foo` as the segment. A `provider_loading` error means
the language server is still loading the workspace: repeat the same call in a few seconds.
While it loads — and when it is unavailable (its workspace failed to load) or its
documentSymbols request failed — `ide.outline`, `ide.read {symbol}` and symbol edits of a
Rust file may already answer from the lexical outline (the reply says `outline: from
source, exact (<server> still indexing; no need to repeat)` while it loads, `outline:
from source, exact (<server> unavailable; no need to repeat)` when it failed, `outline:
from source, exact (<server> request failed: <short cause>; no need to repeat)` when the
exchange did — the outline is exact, so do not repeat the call); a file the lexical
outline cannot reproduce exactly, and an address it does not contain, wait for the
server while it loads and answer `provider_unavailable` when it failed. `ide.symbol`
waits while the server loads; when it is unavailable it answers the definition-only card
(signature, doc, definition from the outline) with `unavailable (<server> workspace
failed to load)` notes for usages and callers. `ide.graph` and rename always wait for
the server. A `provider_unavailable` refusal on these paths names its stage — e.g.
`(rust-analyzer: workspace load failed)`, `(rust-analyzer: documentSymbols request
failed)` — and says what still works (outline/read from source), what does not
(usages, callers) and how to recover (retry later; if it keeps failing, fix what stops the project from loading — `ide.context {"kind":"problems"}` shows the project check).

## Workflow

1. `ide.start` once per actor per worktree, optionally with `"root": "/absolute/dir"`
   to name the working directory (default: the host's project directory). Its reply
   carries the project card: Git state when available, languages with line counts, the project's build /
   check / test / lint commands with their provenance, toolchain, layout, entry points and
   docs — read it before exploring by hand. The root, the
   detected Git worktree and its Git directory must also lie inside the
   operator's configured `allowed_roots`; `outside_allowed_roots` means start the IDE in
   an allowed directory (or ask the operator to extend the list) and otherwise continue
   with native tools. A refusal names its cause: a not-yet-created root includes its nearest
   existing allowed ancestor; Git discovery, unresolved worktree, durable state, worktree-holder,
   and provider-cache conflicts have separate recovery advice. A repeated start by the same actor
   and binding reuses and names the existing activation. Do not call it again for later edits in
   the same worktree.
   Directories without Git work; `ide.diff` and requested symbol history there answer
   `not a git repository: no git data`.
   `ide.diff {"mode": "task"}` includes committed and uncommitted changes since activation plus names of untracked paths that pass Git's standard ignore rules. If no activation commit was recorded, it directs the caller to `head`; when one result is too large, narrow with `head`, `staged`, or `unstaged`, or use native Git.
2. Before an edit, prefer `ide.outline` the file (or `ide.symbol` the target) for its skeleton,
   then `ide.read` the exact symbol or line range you are about to change — its `source_ref` is
   the bounded reference `ide.edit` needs. `ide.context {path}` with no `byte_offset` still works
   and still mints a usable `source_ref` (its first line is now a hint toward `ide.outline`/
   `ide.read`); `ide.context {path, byte_offset}` still answers definitions/references at that
   exact position when needed. Path-only context is the file text; use `ide.symbol` or
   `ide.read {symbol}` when definitions or references are needed.
3. Prefer `ide.edit` for a supported bounded full-content edit when it is offered
   and its Context contract is satisfied. Native host editing — the model's own
   file-edit tool (Claude's editor, Codex `apply_patch`, etc.) — remains available
   whenever `ide.edit` is inactive, unavailable, unsupported, declined, or uncertain.
4. After a known `ide.edit` outcome (`created`, `replaced`, `unchanged`; the first line
   names the operation instead when it was an insert, delete or rename), follow
   its closed diagnostic state: `current_reported` → `ide.edit` with the
   returned `source_ref`; `current_clean` → `ide.diff`; `unknown` diagnostics
   → `ide.context`; `not_analysed` means the project check never compiled the
   file (a Rust file no `mod` declares) — declare it, then edit again. A pending diagnostic still requires `ide.inspect` with its
   returned `detail_ref`. An `outcome_unknown` *result* is a different unknown
   from `unknown` *diagnostics*: the edit's own effect, not just its
   diagnostics, is unproven, so inspect the named path with native host tools
   instead — never call `ide.context` to replay it, and never blind-retry.
5. `ide.test` to run the project's tests on request — never the whole suite by reflex:
   `{"symbol":"src/x.rs#Type/method"}` runs the tests that reference the symbol (including
   in-file `mod tests`), `{"path":"src/x.rs"}` the file's tests, `{"pattern":"name"}` a runner
   filter, `{"command":["cargo","test","--lib"]}` an exact argv; optional `budget_s` (default
   120, max 600). The reply is `tests #N: started — <argv> (budget B s); poll: call ide.test with
   {"status":N}`; call `ide.test {"status":N}` to poll until `tests #N: P passed, F failed, T s` with up to
   eight `FAIL name` / `file:line message` lines, a `rerun:` argv and `full output:
   ide.inspect <detail_ref>`. `ide.inspect` also accepts a test-run handle (`tests #N`,
   `tests-N`, `#N`, `N`) and answers that run's status line.
   After `ide.stop`, `ide.test {"status":N}` is refused; its reply names `ide.inspect
   {"detail_ref":"tests #N"}` as the way to read that run, which still answers.
   When a run has no parsed test counts and exits non-zero, its status line includes the runner's
   first error line (or final non-empty line), bounded to about 200 bytes, followed by the full
   output inspection route.
   One job per worktree at a time; a stopped budget says `stopped at budget`. The
   `<agent-ide>` block carries the job's line once while it runs and once when it ends.
6. `ide.diff` before finishing the task, to review the accumulated change.
   Before finishing a task whose work you committed, review it with `ide.diff {mode: task}`.
7. `ide.inspect` with the returned `detail_ref` whenever a reply is `Pending`
   or reports truncated content. Do not repeat the same call instead. A
   `Context` or `Diff` reply with `continuation: true` means the result is
   larger than one reply: call `ide.inspect` with that same `detail_ref` again
   to get the next chunk (the reply you already hold is page one, so the
   first call returns page two), and repeat until a chunk reports
   `continuation: false`; concatenate the chunks in order for the complete
   text. Every page starts with `page N; bytes A-B of TOTAL`; the last one says
   `(last)` and `complete`. A whole source up to 1 MiB is delivered this way;
   do not `ide.edit` from a `Context` until its last page arrived — the edit is
   refused as `stale_source` otherwise; use the newest `source_ref` when supplied, or re-read all
   pages before retrying. Calling `ide.inspect` again after the
   last page just repeats the last page.
   If inspect reports `source_changed`, finish reading every page before any write, then call
   `ide.context` again. Diff provenance's `current_tree` line says what the capture established
   and to call `ide.diff` again if the tree moved. A root-less start refused for `missing_pre`
   may also include a `retry` hint with the current directory as an explicit root.
7. `ide.stop` at handoff to another actor, or when the task ends, to release
   this binding's activation.

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
Python semantic imports use the same worktree interpreter as checks; a root `.venv` also applies to
nested Python files when no root Python manifest exists.
When counts rise after your edit, call `ide.context` with `{"kind": "problems"}` (optionally
`"language": "rust" | "python" | "typescript"` and `"offset"`) to read the bounded problem
list before continuing. The `typescript` filter covers `.ts`, `.tsx`, `.js`, and `.jsx`.
An LSP diagnostic push can give only a provisional lower bound for TypeScript/JavaScript;
claim exact Clean only after a completed configured project check.
A `git: HEAD moved … outside Agent IDE` line (delivered once) means another process switched the
worktree's branch: symbol answers you already hold may describe the old branch, so re-read before
editing from them.
`check failed (<reason>)`, `environment not found`, `no files analyzed`, `outside allowed roots`
or `checks disabled` mean the feed has no counts for that language — never treat them as a clean
result.

## Pending replies

Both Codex and Claude call the IDE tool directly. When a reply is `pending`, call `ide.inspect`
with the given `detail_ref`; never run a helper command.

## Independence and fallback

Each child or subagent activates `ide.*` independently: a parent's
`ide.start` does not cover its children, and each must call its own
`ide.start` before relying on the workflow. A `stale`, `unavailable`, or
`unknown` outcome is not a clean result — treat it exactly like
`unavailable`. When `ide.*` is inactive, unsupported, or its reply is not
usable, fall back immediately to native host tools and CodeGraph or native
search; never block source work waiting for `ide.*` to become available.
When an `unavailable` reply carries the retry fact `daemon restarted; repeat
this call once` or `session re-rooted to the requested root; repeat this call
once`, repeat that exact same call once before falling back to
native tools. When it carries `session re-rooted to the requested root; repeat
this call once, or call ide.start without root`, repeat that call once, and if
it stays unavailable call `ide.start` with no `root` — that returns the session
to the host's project directory.

## Boundaries

Do not assume `ide.check` or `ide.finish` exist. Discover whether `ide.edit` is
offered for this session; its absence never blocks the truthful native fallback.
