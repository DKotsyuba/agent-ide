---
name: agent-ide
description: Task-based Agent IDE workflow, compatible batches, native fallback and automatic async delivery.
---

# Agent IDE

The tool descriptions define each call; this skill holds the rules. Call `ide.start` first.

In an unfamiliar project, call `ide.tree {}` first: one call maps directories and files with
sizes and the authors' one-line purpose, replacing rounds of ls/find/rg; no source bodies. Zoom
in with `ide.tree {"path":"crates/store"}`; then outline or read the files that matter.

## One bounded batch per topic

Collect the related set, then send one call per phase instead of one per file, symbol or hunk.
Split only at an advertised limit or for newly found work.

1. Choose the needed information: a known address goes directly to `ide.read`, needed structure
   to `ide.outline` paths/glob, definitions/references to `ide.symbol` symbols. No preliminary
   outline or graph is required. For needed call chains:
   `ide.graph {"symbols":["src/a.rs#Store/get","src/b.rs#load"],"direction":"callers","depth":2}`.
2. Read: `ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"docs/b.md","offset":1,"limit":80}]}`.
3. Edit production code and tests together:
   `ide.edit {"files":[{"path":"src/a.rs","changes":[{"symbol":"src/a.rs#Store/get","content":"fn get(&self) {}"}]},{"path":"tests/b.rs","create":true,"content":"fn t() {}"}]}`.
4. Test: `ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"command":["cargo","check"]}],"budget_s":120}`.

## No duplicate work

- Reuse sufficient delivered source, check results and diffs. Additional or native reads/tests
  need a named gap, changed source, refusal or unavailability; required independent or broader
  verification remains valid.
  An acknowledgement for an unchanged item means: use the copy you hold; `force:true` only after
  lost context. Do not re-read your own successful edit; an edit reply that names committed files
  is the write, so skip ls or git status checks.
- Do not refetch sufficient passed logs or delivered pages. Explicit output recovery after
  context loss is valid. Unavailable test counts are not a pass.
- `ide.diff` is optional after an equivalent reviewed Git diff. Group independent compatible
  protocol calls; preserve dependencies. Any needed final diff completes before `ide.stop`.

## Pending results

A `pending` reply or retained result is not a failure. Plain file reads answer synchronously.
A committed edit receipt identifies actual written files independently of background checks;
zero-commit `edit_progress` is unresolved. Bridge automatically delivers accepted async results.
Continue independent work or end the turn while retaining the task/session. Never poll, sleep,
repeat the original call, finish or `ide.stop` with required work still owed. One bounded
`ide.inspect` with the reply's `detail_ref` and `wait_s` (1..30) is recovery only, never an
ordinary read/edit step.

## Failures

Try a refusal's Repair once; if the tool stays refused or unavailable, continue with native tools
and name the call. Verify an uncertain write before another mutation.
