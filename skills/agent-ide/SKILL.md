---
name: agent-ide
description: Workflow rules for the Agent IDE ide.* tools - one batch per topic, no duplicate native work, pending results. Read before the first ide.* call.
---

# Agent IDE

The tool descriptions define each call; this skill holds the rules. Call `ide.start` first.

## One bounded batch per topic

Collect the related set, then send one call per phase instead of one per file, symbol or hunk.
Split only at an advertised limit or for newly found work.

1. Discover with `ide.outline` paths/glob and `ide.symbol` symbols. For shared signatures,
   indirect faults or wider impact:
   `ide.graph {"symbols":["src/a.rs#Store/get","src/b.rs#load"],"direction":"callers","depth":2}`.
2. Read: `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"docs/b.md","offset":1,"limit":80}]}`.
3. Edit production code and tests together:
   `ide.edit {"files":[{"path":"src/a.rs","changes":[{"symbol":"src/a.rs#Store/get","content":"fn get(&self) {}"}]},{"path":"tests/b.rs","create":true,"content":"fn t() {}"}]}`.
4. Test: `ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"command":["cargo","check"]}],"budget_s":120}`.

## No duplicate work

- Never re-read natively (Read, cat, sed, grep for content) what an IDE reply already delivered.
  An acknowledgement for an unchanged item means: use the copy you hold; `force:true` only after
  lost context. Do not re-read your own successful edit; an edit reply that names committed files
  is the write, so skip ls or git status checks.
- Never refetch a passed test log or a page already delivered; open more output only for a
  failure you need to read.
- Run native tests, builds or type checks only for a gap an IDE reply names (unavailable counts,
  unknown diagnostics, a target ide.test cannot run), and say which gap. Unavailable test counts
  are not a pass.
- `ide.diff` is optional; native `git diff` is valid. Send `ide.stop` in the same tool batch as
  the last `ide.diff`.

## Pending results

A `pending` reply or retained result is not a failure. Plain file reads answer at once; an edit
at the inline wait limit replies `edit_progress` with the files it committed. Continue independent
work; with Bridge delivery, end the turn and the result arrives. When your host keeps the turn
open, send one bounded `ide.inspect` (the reply's `detail_ref` and `wait_s`) before any native
fallback. Never poll, sleep or repeat the original call, and never call finish or `ide.stop`
while a result is pending.

## Failures

Try a refusal's Repair once; if the tool stays refused or unavailable, continue with native tools
and name the call. Verify an uncertain write before another mutation.
