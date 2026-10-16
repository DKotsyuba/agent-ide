---
name: agent-ide
description: Batch source discovery, reads, atomic edits and tests. In Bridge mode graph/test completion arrives automatically; end an idle turn and never poll.
---

# Agent IDE

Call `ide.start` once per agent/worktree. Paths are relative. An address is
`src/a.rs#Type/method`; `:3-5` selects a symbol window; `src/a.rs#@imports` is a region.
Ten tools: start, stop, outline, symbol, graph, read, edit, test, diff, inspect.

## Batch workflow

Collect the related set first. Use one bounded batch per phase, rather than one call per
file/symbol/hunk. Batch outline paths/glob, symbol/graph symbols, read items, edits and test
targets. Review changed paths in one diff; fetch known detail_refs together only when needed.
Start/stop are once-per-task lifecycle calls.

1. Discover related changes together. `ide.symbol` gives a card and **direct** neighbors.
   Positive callers/callees values never request deeper traversal. For a precise UTF-8
   use-site use `path` + `byte_offset`.
2. For shared signatures, indirect faults or wider impact:
   `ide.graph {"symbols":["src/a.rs#Store/get","src/b.rs#load"],"direction":"callers","depth":2}`.
   Callers show impact, callees show dependencies, both shows both sides. Depth is 1–3;
   `tests:true` expands tests. In Bridge mode graph always accepts an asynchronous job,
   including a warm cache; acceptance is not the graph. Approximate/lexical edges and
   unavailable coverage remain explicit, never exact negative evidence.
3. Read related code together:
   `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"docs/b.md","lines":"1-80"}]}`
   (path ranges only for non-code text).
   In an observed context epoch, unchanged delivered items return an acknowledgement.
   Reuse prior code; `force:true` handles lost context or an intentional reread. Unknown
   context keeps suppression off.
4. Apply related production/tests edits in one atomic call:
   `ide.edit {"patch":"*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** Update File: src/b.rs\n@@\n-before\n+after\n*** End Patch"}`.
   Add File/Update File work; deletion/moves are unsupported.
   Optional `operation_id` makes retries safe. A refusal writes nothing; use its current
   source. Successful writes and the later project-check result are separate.
5. Launch related tests/checks together:
   `ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"command":["cargo","check"]}],"budget_s":120}`.
   One run ID, ordered steps, one budget and per-target outcomes. Commands use root cwd.
   Unavailable counts are not passing tests.

## Passive completion

In Bridge mode graph/test/deferred-check results arrive automatically. Continue independent
work; otherwise **end the current model turn** and keep activation/run alive. Never poll,
sleep, self-message, call `finish` or `ide.stop` while waiting. Bridge wakes a qualified live
receiver. Delivery failure does not permit another IDE carrier or fabricated wake support.

`ide.test {"result":3}` reads a saved result; `ide.test {"history":true}` lists retained runs;
`ide.test {"detail_ref":"…"}` continues output. History is actor/channel/worktree scoped,
across activations, up to ten minutes/four completed runs in this daemon; restart loses it.
`ide.test {"cancel":3}` cancels your current-generation run; completion follows group cleanup.
Ending a turn does not cancel work. Extra pages use `ide.graph {"detail_ref":"…"}` or
`ide.read {"detail_ref":"…"}`; stale source may require recomputation.

`ide.inspect` remains for diagnostics/problems and starts no checks. Never poll with it.
Unknown never means clean. `ide.diff` is optional for incremental/paged changes; Git diff is
valid. Call `ide.stop` at task end, after collecting required outcomes.

Discover missing tools once. Try a refusal's Repair once; if the tool is unavailable, still refused
or gives no Repair, continue with native tools. Verify uncertain writes before another mutation.
Preserve freshness; never invent graph coverage, test counts or delivery success.
