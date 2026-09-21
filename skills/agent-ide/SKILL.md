---
name: agent-ide
description: Use Agent IDE `ide.*` tools, including `ide.edit` when offered, for source implementation, debugging, refactoring, and testing. Native host editing remains a supported fallback.
---

# Agent IDE

Agent IDE exposes five lifecycle tools: `ide.start`, `ide.context`, `ide.diff`,
`ide.inspect`, `ide.stop`, plus `ide.edit` when the active product surface offers it. Use this workflow whenever they are
offered and the task implements, debugs, refactors, or writes/fixes source.
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

Each accepted reply carries one compact decision-facing text block that alone states every fact
needed for the next action — state, `detail_ref`, `helper`, `continuation`, `retry`, Edit outcome
and `source_ref`. On Claude, that text is the only carrier: Claude hands `structuredContent`
straight to its model instead of `content`, so the managed Claude MCP never sends it. On a host
that does expose `structuredContent`, treat that complete typed result as the source of truth
instead of parsing the compact text as a second response schema.

## Workflow

1. `ide.start` once per actor per worktree. Do not call it again for later
   edits in the same worktree.
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
5. `ide.diff` before finishing the task, to review the accumulated change.
6. `ide.inspect` with the returned `detail_ref` whenever a reply is `Pending`
   or reports truncated content. Do not repeat the same call instead. A
   `Context` or `Diff` reply with `continuation: true` means the result is
   larger than one reply: call `ide.inspect` with that same `detail_ref` again
   to get the next chunk, and repeat until a chunk reports `continuation:
   false`; concatenate the chunks in order for the complete text.
7. `ide.stop` at handoff to another actor, or when the task ends, to release
   this binding's activation.

## Project problem feed

The `<agent-ide>` block reports project-wide Rust/Python error and warning counts for this
worktree and appears in hook context only when those items change; no block means unchanged, not
finished. When counts rise after your edit, call `ide.context` with `{"kind": "problems"}`
(optionally `"language": "rust" | "python"` and `"offset"`) to read the bounded problem list
before continuing. Fixed texts such as `environment not found`, `outside allowed roots`, or
`checks disabled` mean the feed has no counts for that language — never treat them as a clean
result.

## Pending replies and the Claude foreground helper

A `Pending` reply may carry an exact `helper` command. On Claude, run that
command yourself with the Bash tool in the foreground
(`run_in_background` must stay `false`), without editing, wrapping, or
appending to it. Call `ide.inspect` only after that command has completed —
never before, and never construct the helper command yourself. The helper must be the ONLY command
in its Bash call (no `date;` prefix or other wrapper); take timestamps in a separate call.

If that exact helper reports `unavailable` under Claude's strict macOS sandbox, report the exact
`/private/tmp/ai-c-…/claude-helper.sock` path derived from its `--runtime-dir` argument. The operator
must add that one path to `sandbox.network.allowUnixSockets` in project-local settings before a new
Claude session; never suggest `allowAllUnixSockets`.

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
