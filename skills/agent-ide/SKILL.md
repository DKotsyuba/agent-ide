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

## Boundaries

Do not assume `ide.check` or `ide.finish` exist. Discover whether `ide.edit` is
offered for this session; its absence never blocks the truthful native fallback.
