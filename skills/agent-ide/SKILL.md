---
name: agent-ide
description: Use Agent IDE `ide.*` tools, including `ide.edit` when offered, for source implementation, debugging, refactoring, and testing. Native host editing remains a supported fallback.
---

# Agent IDE

Agent IDE exposes the lifecycle tools `ide.start`, `ide.context`, `ide.diff`,
`ide.inspect`, `ide.stop`, `ide.edit`, the symbol tools `ide.outline`, `ide.read`,
`ide.symbol`, `ide.graph`, and `ide.test`. Use this workflow whenever they are offered and the task implements,
debugs, refactors, or writes/fixes source.
Skip it for read-only exploration, prose, configuration, or when `ide.*` is
not present — use native host read/write/test tools plus CodeGraph (when
available) instead.

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
  reading any file longer than a screen.
- `ide.symbol {"symbol":"src/x.rs#Type/method"}` or `{"symbol":"Name"}` — a symbol card:
  resolved signature, documentation, the definition with line numbers, usages grouped by
  `src`/`tests` with the source line text, `callers`/`callees` (set `"callers":1`) and, with
  `"history":true`, the last three commits that touched the definition.
  Use `ide.graph` for a bounded multi-level callers/callees tree instead of the symbol card's
  one-level relationships.
  A bare name that matches several symbols answers with the candidate paths; repeat
  with one exact path.
- `ide.read {"symbol":"src/x.rs#Type/method"}` or `{"path":"src/x.rs","lines":"120-180"}`
  — the body with line numbers; its `source_ref` is what `ide.edit` needs.

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
  by the language server; the reply lists every touched file.
- `ide.edit {"operation_id":"…","path":"src/x.rs","lines":"120-180","content":"…"}` — replaces a
  line range when the target is not a symbol (imports, constants, configuration).

Symbol paths are `file#Owner/name`: `#` separates the file, `/` is nesting (impl, class,
namespace, module → member). Inherent `impl Foo` members are addressed as `Foo/method`;
trait impls keep `impl Trait for Foo` as the segment. A `provider_loading` error means
the language server is still loading the workspace: repeat the same call in a few seconds.

## Workflow

1. `ide.start` once per actor per worktree, optionally with `"root": "/absolute/dir"`
   to name the working directory (default: the host's project directory). Its reply
   carries the project card: git state, languages with line counts, the project's build /
   check / test / lint commands with their provenance, toolchain, layout, entry points and
   docs — read it before exploring by hand. The root, the
   Git worktree it belongs to and that worktree's Git directory must all lie inside the
   operator's configured `allowed_roots`; `outside_allowed_roots` means start the IDE in
   an allowed directory (or ask the operator to extend the list) and otherwise continue
   with native tools. Do not call it again for later edits in the same worktree.
2. `ide.context` before each relevant edit, using a workspace-relative path.
   Include `byte_offset` when definitions or references are needed; omitting it
   requests complete lexical source context only.
3. Prefer `ide.edit` for a supported bounded full-content edit when it is offered
   and its Context contract is satisfied. Native host editing — the model's own
   file-edit tool (Claude's editor, Codex `apply_patch`, etc.) — remains available
   whenever `ide.edit` is inactive, unavailable, unsupported, declined, or uncertain.
4. After a known `ide.edit` outcome (`created`, `replaced`, `unchanged`), follow
   its closed diagnostic state: `current_reported` → `ide.edit` with the
   returned `source_ref`; `current_clean` → `ide.diff`; `unknown` diagnostics
   → `ide.context`. A pending diagnostic still requires `ide.inspect` with its
   returned `detail_ref`. An `outcome_unknown` *result* is a different unknown
   from `unknown` *diagnostics*: the edit's own effect, not just its
   diagnostics, is unproven, so inspect the named path with native host tools
   instead — never call `ide.context` to replay it, and never blind-retry.
5. `ide.test` to run the project's tests on request — never the whole suite by reflex:
   `{"symbol":"src/x.rs#Type/method"}` runs the tests that reference the symbol (including
   in-file `mod tests`), `{"path":"src/x.rs"}` the file's tests, `{"pattern":"name"}` a runner
   filter, `{"command":["cargo","test","--lib"]}` an exact argv; optional `budget_s` (default
   120, max 600). The reply is `tests #N: started — <argv> (budget B s)`; poll with
   `{"status":N}` until `tests #N: P passed, F failed, T s` with up to eight `FAIL name` /
   `file:line message` lines, a `rerun:` argv and `full output: ide.inspect <detail_ref>`.
   One job per worktree at a time; a stopped budget says `stopped at budget`. The
   `<agent-ide>` block carries the job's line once while it runs and once when it ends.
6. `ide.diff` before finishing the task, to review the accumulated change.
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
   refused as `stale_source` otherwise. Calling `ide.inspect` again after the
   last page just repeats the last page.
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
last counts may be outdated; the next plate carries the result with `(+N)`/`(-N)` deltas. When
counts rise after your edit, call `ide.context` with `{"kind": "problems"}` (optionally
`"language": "rust" | "python" | "typescript"` and `"offset"`) to read the bounded problem
list before continuing. The `typescript` filter covers `.ts`, `.tsx`, `.js`, and `.jsx`.
An LSP diagnostic push can give only a provisional lower bound for TypeScript/JavaScript;
claim exact Clean only after a completed configured project check.
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
this call once`, repeat that exact same call once before falling back to
native tools.

## Boundaries

Do not assume `ide.check` or `ide.finish` exist. Discover whether `ide.edit` is
offered for this session; its absence never blocks the truthful native fallback.
