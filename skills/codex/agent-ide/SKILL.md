---
name: agent-ide
description: Use Agent IDE `ide.*` tools to understand and change source code by symbol address: find an address, read the symbol, replace it or edit lines inside it. Native tools stay for non-code files and as a fallback.
---

# Agent IDE

Work on code by address, not by file and line. An address is `src/a.rs#Type/method`; every IDE
result prints them with the absolute line, `src/a.rs#Type/method:7 (L123)`. File-level code has
regions: `src/a.rs#@header` (leading comments) and `src/a.rs#@imports`.

## Workflow

1. `ide.start` once per task; `ide.stop` when it ends.
2. Find an address in any result: `ide.outline {"paths":["src/a.rs"]}` lists a file's symbols and
   regions; `ide.symbol {"symbols":["src/a.rs#Store/get"],"callers":2}` adds usages and callers;
   a grep hit `path:line` is also an address.
3. Read the symbol, doc plus body, numbered from 1 inside it:
   `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"symbol":"src/a.rs#@imports"}]}`.
   A window is `src/a.rs#Store/get:3-5`. Plain `path` and `lines` are for non-code files.
4. Edit by address; the reply is immediate:
   - an `apply_patch` body works as usual: `ide.edit {"patch":"*** Begin Patch\n…"}`;
   - replace a symbol (doc and body): `ide.edit {"symbol":"src/a.rs#Store/get","content":"…"}`;
   - change text inside it: `ide.edit {"symbol":"src/a.rs#Store/get","old":"x: 0,","new":"x: 1,"}`;
   - replace lines of it: `ide.edit {"symbol":"src/a.rs#Store/get:3-5","content":"…"}`;
   - `op` insert (with `where`), delete or rename; `add_import` and `remove_import` with a `path`.
5. `ide.test {"targets":[{"symbol":"src/a.rs#parse"}]}` runs the tests that matter.
6. `ide.diff` shows what changed since your last diff; give it a `mode` for the whole diff.

Edits need no file-version reference: the current text is the check. A refusal prints the
symbol's current numbered lines and the one repair step.

## Results arrive by themselves

A project check after an edit, a test run and a pending answer arrive in an `<agent-ide>` block at
the top of your next `ide.*` reply, after your next tool call or when you end your turn. Never
poll or wait for them. `ide.inspect {}` hands over what is ready now;
`ide.inspect {"kind":"problems"}` lists the project problems; a reply that was cut ends with the
`ide.inspect {"detail_ref":"…"}` call that continues it. `check failed`, `check timed out` or
`no files analyzed` mean no counts for that language, never a clean result.

## Fallback

An `unavailable` reply, a diagnostics `unknown` or a refused edit is not a clean result: follow
the one repair step it names once, otherwise continue with native tools. Each subagent calls its
own `ide.start`. A `busy` reply means the call did not run: repeat it.
