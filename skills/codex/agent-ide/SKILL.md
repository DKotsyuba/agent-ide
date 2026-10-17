---
name: agent-ide
description: Batch call forms for the Agent IDE ide.* tools; the server instructions hold the rules.
---

# Agent IDE

The server instructions hold the rules and the tool descriptions define each call; this skill
shows the batch forms. Call `ide.start` first.

Collect the related set, then send one call per phase instead of one per file, symbol or hunk.
Split only at an advertised limit or for newly found work.

1. Discover with `ide.outline` paths/glob and `ide.symbol` symbols. For shared signatures,
   indirect faults or wider impact:
   `ide.graph {"symbols":["src/a.rs#Store/get","src/b.rs#load"],"direction":"callers","depth":2}`.
2. Read: `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"docs/b.md","lines":"1-80"}]}`.
3. Edit production code and tests together in one patch:
   `ide.edit {"patch":"*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** Add File: tests/b.rs\n+fn t() {}\n*** End Patch"}`.
4. Test: `ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"command":["cargo","check"]}],"budget_s":120}`.

Try a refusal's Repair once; if the tool stays refused or unavailable, continue with native tools
and name the call. Verify an uncertain write before another mutation.
