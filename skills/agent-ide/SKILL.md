---
name: agent-ide
description: Use the Agent IDE `ide.*` MCP tools (ide.start, ide.context, ide.diff, ide.inspect, ide.stop) for source implementation, debugging, refactoring, and testing when they are offered. Not for read-only exploration, prose, or when `ide.*` is absent.
---

# Agent IDE

Agent IDE exposes exactly five bounded MCP tools: `ide.start`, `ide.context`,
`ide.diff`, `ide.inspect`, `ide.stop`. Use this workflow whenever they are
offered and the task implements, debugs, refactors, or writes/fixes source.
Skip it for read-only exploration, prose, configuration, or when `ide.*` is
not present — use native host read/write/test tools plus CodeGraph (when
available) instead.

## Workflow

1. `ide.start` once per actor per worktree. Do not call it again for later
   edits in the same worktree.
2. `ide.context` before each relevant edit, to see current source and
   diagnostics before changing it.
3. Edit with the native host writer — the model's own file-edit tool (Claude's
   editor, Codex `apply_patch`, etc.). Agent IDE has no `ide.edit`; the native
   writer is the only source-mutation path this release supports.
4. `ide.context` again after the edit, to confirm the change landed and refresh
   diagnostics against the new bytes.
5. `ide.diff` before finishing the task, to review the accumulated change.
6. `ide.inspect` with the returned `detail_ref` whenever a reply is `Pending`
   or reports truncated content. Do not repeat the same call instead.
7. `ide.stop` at handoff to another actor, or when the task ends, to release
   this binding's activation.

## Pending replies and the Claude foreground helper

A `Pending` reply may carry an exact `helper` command. On Claude, run that
command yourself with the Bash tool in the foreground
(`run_in_background` must stay `false`), without editing, wrapping, or
appending to it. Call `ide.inspect` only after that command has completed —
never before, and never construct the helper command yourself.

## Independence and fallback

Each child or subagent activates `ide.*` independently: a parent's
`ide.start` does not cover its children, and each must call its own
`ide.start` before relying on the workflow. A `stale`, `unavailable`, or
`unknown` outcome is not a clean result — treat it exactly like
`unavailable`. When `ide.*` is inactive, unsupported, or its reply is not
usable, fall back immediately to native host tools and CodeGraph or native
search; never block source work waiting for `ide.*` to become available.

## Boundaries

This skill covers exactly the five current tools listed above. Do not assume
`ide.edit`, `ide.check`, or `ide.finish` exist — none of them ship in this
release.
