# Model-facing text inventory (symbol-first audit)

Every string a model reads while using the IDE, with where it lives, its size at the E-002 build
(`eaeba10`) and now, and its status. The rule it serves: find an address in any result
(`src/a.rs#Type/method:7 (L123)`, `src/a.rs#@header`, `src/a.rs#@imports`), read the symbol (doc
plus body), replace it or edit lines inside it; path ranges only for non-code files; nothing teaches
polling, waiting, the file-version reference or a line-first habit. The guard is
`assistance::hygiene::model_text_teaches_only_the_symbol_workflow`: it scans every source marked
**G** below for the retired forms in `STALE_FORMS`, and
`the_stale_form_guard_catches_every_retired_form` proves each form is caught. Sizes are bytes.

Status: **rewritten** (new text), **trimmed** (same content, fewer bytes or old form dropped),
**kept** (already symbol-first or not a teaching surface; reason given), **owned** (the text belongs
to the M-028 address or M-029 read/edit modules and follows their contracts).

## Teaching text

| Source | Where | Before | After | Status |
|---|---|---|---|---|
| Server paragraph, Claude (G) | `facade.rs:4936` `SERVER_INSTRUCTIONS` | 735 | 508 | rewritten |
| Server paragraph, Codex (G) | `facade.rs:4947` `SERVER_INSTRUCTIONS_CODEX` | (none) | 582 | rewritten (no `ide.graph`/`ide.context`) |
| `ide.start` description (G) | `facade.rs:4780` | 438 | 261 | rewritten |
| `ide.context` description (G) | `facade.rs:4793` | 293 | 224 | trimmed (Claude only) |
| `ide.diff` description (G) | `facade.rs:4807` | 184 | 226 | rewritten (since your last diff) |
| `ide.outline` description (G) | `facade.rs:4820` | 559 | 305 | rewritten |
| `ide.read` description (G) | `facade.rs:4835` | 495 | 279 | rewritten |
| `ide.symbol` description (G) | `facade.rs:4849` | 641 | 324 | rewritten |
| `ide.graph` description (G) | `facade.rs:4863` | 469 | 174 | trimmed (Claude only) |
| `ide.test` description (G) | `facade.rs:4875` | 997 | 337 | rewritten |
| `ide.inspect` description (G) | `facade.rs:4889` | 462 | 274 | rewritten (no `wait_ms`; Codex adds problems) |
| `ide.stop` description (G) | `facade.rs:4904` | 292 | 204 | trimmed |
| `ide.edit` description (G) | `facade.rs:4917` | 873 | 451 | rewritten (no `source_ref`) |
| Property texts, all tools (G) | `facade.rs:216` `tool_schemas()` | up to 609 each | at most 160 each | rewritten |
| Advertised schemas as served (G) | `facade.rs:4994` `CatalogHost::catalog`, `slim_schema` | 22,309 total | Claude 14,650; Codex 12,874 | trimmed (bounds dropped, aliases and `source_ref` unadvertised) |
| `ide.start` signature block (G) | `signature_card.rs:16` | 1,145 | 1,012 | rewritten (edit by symbol, regions) |
| Claude skill (G) | `skills/agent-ide/SKILL.md` | 33,349 | 3,243 | rewritten |
| Codex skill (G) | `skills/codex/agent-ide/SKILL.md` | (none) | 2,764 | rewritten (new) |
| Skill UI files (G) | `skills/agent-ide/agents/openai.yaml`, `skills/codex/agent-ide/agents/openai.yaml` | 331 | 344 / 344 | rewritten |
| Reviewer agent (G) | `agents/ide-reviewer.md` | 684 | 761 | rewritten (report addresses) |
| Plugin manifests (G) | `.claude-plugin/plugin.json`, `.codex-plugin/plugin.json` | 313 / 1,017 | 277 / 1,037 | rewritten (Codex: nine tools, `skills/codex`) |

## Reply and refusal text

| Source | Where | Before | After | Status |
|---|---|---|---|---|
| Reply template (G) | `assets/mcp/reply.jinja` | 37,119 | 35,497 | trimmed: no `complete <kind>:` labels, no `Next:` lines, one `Cut; …` trailer; stale writes repeat the call, while an explicitly supplied stale legacy reference keeps its own repair note |
| Read, outline, symbol headers and per-item lines | `batch.rs`, `symbols.rs` (M-029/M-028) | n/a | n/a | owned |
| Edit reply lines and refusals with `Repair:` | `batch.rs`, `batch_edit.rs`, `edit_forms.rs` (M-029) | n/a | n/a | owned |
| Host-binding and capacity refusals | `reply.jinja` `unavailable:` and `error:` branches | single sentence, one action | unchanged shape | kept: each already names one repair step; `wait` advice removed from the two capacity lines |
| Presentation-degraded fallbacks | `content.rs:490` | 2 lines | 2 lines | trimmed (no label) |
| Batch refusal shapes | `batch.rs:35` `ITEM_SHAPE`, `batch.rs:584` `FILE_SHAPE` | quoted `source_ref` | no `source_ref` | trimmed |
| Context target refusals | `facade.rs:678`, `facade.rs:681` | 2 lines | 2 lines | kept (name the one missing field) |
| Diff trailer and no-change line | `snapshots.rs` `render_diff_text`, `incremental_diff_reply` | n/a | new | rewritten: `unchanged since your last diff: K hunks`, `no changes since your last diff`; exact and degraded captures share the filter, returning hunks are new again, and continuation delivery counts as seen |
| Diff provenance `current_tree` lines | `snapshots.rs:27` | 2 long lines | unchanged | kept: only printed with `provenance: true` |

## Hook plates and outbox facts

| Source | Where | Before | After | Status |
|---|---|---|---|---|
| Diagnostics feedback fact (G via render) | `worker.rs:5100` `FeedbackDelta::new`, `facade.rs:2205` `render` | `Fact:/Next:/Freshness:` labels, "latest context result" | one line plus `ide.inspect {"detail_ref":…}`, "Read the flagged symbols with ide.read" | rewritten |
| Problem-feed overflow line | `delivery.rs:322` | `ide.context` with kind problems | `ide.inspect {"kind":"problems"}` | rewritten |
| `test-finished`, `started`, running lines (G) | `test_fact.rs:77`, `test_fact.rs:83` | already "arrives by itself" | unchanged | kept |
| `edit-checked`, `job-ready` facts | `session_outbox.rs`, `delivery.rs` | already unlabelled | unchanged | kept |
| `ide.stop` reply (G) | `worker.rs:7747` | n/a | unchanged | kept |
| Project feed plates (`rust: …`) | `project.rs` states (G via `surfaces()`) | n/a | unchanged | kept |

## Lists

| Source | Where | Status |
|---|---|---|
| `near:` candidates of an unknown symbol | `reply.jinja` `symbol:near` branch, candidates printed by `symbols.rs` | owned: addresses come from the M-028 resolver |
| Ambiguous bare-name candidates | `symbols.rs` | owned (addresses) |
