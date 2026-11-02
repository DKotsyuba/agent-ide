---
name: agent-ide
description: Batch call forms for the Agent IDE ide.* tools; the server instructions hold the rules.
---

# Agent IDE

The server instructions hold the rules and the tool descriptions define each call; this skill
shows the batch forms. Call `ide.start` first.

Discover only the needed tool. With deferred tool search, request its exact public name.
In code mode, filter to that name's suffix and print matching names only; then inspect just
the exact returned entry, for example `text(ALL_TOOLS.find(t => t.name === "<exact discovered name>"))`.
This loads the needed schema without printing unrelated descriptions.

1. Unfamiliar project: call `ide.tree {}` first; it maps directories and files with the authors'
   one-line purpose, replacing rounds of ls/find/rg; zoom in with `ide.tree {"path":"crates/store"}`.
   Source at known addresses: the read batch below. Structure: `ide.outline` paths/glob.
   Definitions/references: `ide.symbol` symbols. Needed call chains:
   `ide.graph {"symbols":["src/a.rs#Store/get","src/b.rs#load"],"direction":"callers","depth":2}`.
2. Read: `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"docs/b.md","lines":"1-80"}]}`.
3. Edit production code and tests together in one patch:
   `ide.edit {"operation_id":"change-1","patch":"*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** Add File: tests/b.rs\n+fn t() {}\n*** End Patch"}`.
4. Test: `ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"command":["cargo","check"]}],"budget_s":120}`.

The server instructions hold fallback, verification and lifecycle rules.
