# Model-facing text inventory (symbol-first audit)

Every string a model reads while using the IDE, with where it lives, its size at the E-002 build
(`eaeba10`) and the historical r5 draft, and its status. Current owner-directed graph
restoration measurements are recorded separately below. The rule it serves: find an address in any result
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

The table below retains the historical r5 draft figures, not current export measurements.
The owner-directed catalog amendment restores `ide.graph` to Codex and then removes
public `ide.context` from both hosts (ten public names each). Codex permits 14 KiB;
Claude stays at 15 KiB;
paragraphs 700 B, descriptions 600 B, properties 160 B, the start signature 700 B,
and shipped skill ceilings 6/4 KiB stay unchanged. This is an owner amendment,
not an Agent Tasks ratification of a new contract revision.

The graph-only intermediate export completed before the context-removal amendment:
`schemas/tools.json` then had eleven names/15,178 B compact JSON, and
`schemas/tools-codex.json` ten names/14,191 B. Those exact exports remain in the
working tree and are preserved as `graph-intermediate-tools*.json` under the designated
`outline-fix/graph-return` evidence directory. They are historical intermediate artifacts,
not exports of the final source. The latest owner order defers builds, final snapshot
regeneration, tests and test migration; this candidate is source-complete/UNTESTED.

Current literal source text, measured without building: Claude/Codex paragraphs 662/681 B;
read/symbol/graph/inspect description comments 241/268/263/238 B; start signature 690 B;
Claude/Codex skills 4,571/4,073 B; plugin manifests 287/1,032 B. Current served catalog
totals and final exporter equality are **not measured**. Both snapshots must be regenerated
through `cargo xtask contract update` when the owner resumes verification.

The new capability split is source reads/related links through `ide.read`, exact UTF-8
semantic positions through `ide.symbol {path,byte_offset}`, file provider diagnostics
through `ide.inspect {kind:"diagnostics",path}`, and project checks through
`ide.inspect {kind:"problems"}`. Positional cards and file reports reuse the existing
observation, provider and freshness engine, retain continuation/source fences and explicit
unknown/lexical evidence, and do not print source authority/version bookkeeping. Deferred
Inspect queries keep the public name and source-bearing hook suppression. Public
`ide.context` is unregistered; only the private transport/validation engine remains.

The active model workflow in `docs/contracts/tools-v0.4.md` is guarded too: r5 refusal
and deferred-result rules supersede the separately labeled operator compatibility record.
That record keeps unadvertised protocol compatibility without model idle-wait advice.

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
| `ide.inspect` description (G) | `facade.rs` `CatalogHost::catalog` | 462 | 330 | rewritten (no `wait_ms`; both hosts serve problems) |
| `ide.stop` description (G) | `facade.rs:4904` | 292 | 204 | trimmed |
| `ide.edit` description (G) | `facade.rs:4917` | 873 | 451 | rewritten (no `source_ref`) |
| Property texts, all tools (G) | `facade.rs:216` `tool_schemas()` | up to 609 each | at most 160 each | rewritten |
| Advertised schemas as served (G) | `facade.rs` `CatalogHost::catalog`, `slim_schema` | 22,309 total | Claude 15,099; Codex 12,874 | trimmed (bounds dropped, aliases and `source_ref` unadvertised); both hosts advertise inspect problems; real binary export, compact JSON array sizes |
| `ide.start` signature block (G) | `signature_card.rs:16` | 1,145 | 699 | rewritten (edit by symbol, regions; strict T-006 ceiling 700 B) |
| Claude skill (G) | `skills/agent-ide/SKILL.md` | 33,349 | 3,243 | rewritten |
| Codex skill (G) | `skills/codex/agent-ide/SKILL.md` | (none) | 2,764 | rewritten (new) |
| Skill UI files (G) | `skills/agent-ide/agents/openai.yaml`, `skills/codex/agent-ide/agents/openai.yaml` | 331 | 344 / 344 | rewritten |
| Reviewer agent (G) | `agents/ide-reviewer.md` | 684 | 761 | rewritten (report addresses) |
| Plugin manifests (G) | `.claude-plugin/plugin.json`, `.codex-plugin/plugin.json` | 313 / 1,017 | 277 / 1,037 | rewritten (Codex: nine tools, `skills/codex`) |

## Reply and refusal text

| Source | Where | Before | After | Status |
|---|---|---|---|---|
| Reply template (G) | `assets/mcp/reply.jinja` | 37,119 | 23,492 | rewritten: bounded natural refusals; no `complete <kind>:` labels, no `Next:` lines, one `Cut; …` trailer; short hook acknowledgements and exact feedback; pinned read/edit receipts retained |
| Read, outline, symbol headers and per-item lines | `batch.rs`, `symbols.rs` (M-029/M-028) | n/a | n/a | owned |
| Edit reply lines and refusals with `Repair:` | `batch.rs`, `batch_edit.rs`, `edit_forms.rs` (M-029) | n/a | n/a | owned |
| Generic refusals | `reply.jinja` typed branches, `content.rs` `refusal_text`, `facade.rs` transport fallbacks | reason-code labels and alternative actions | head ≤160 UTF-8 bytes, one Repair ≤120 bytes, optional quoted excerpt ≤60 lines/4 KiB | rewritten: current units, held-writer facts and uncertain stop outcomes remain data; reconnect hints replace the repair |
| Presentation-degraded fallbacks | `content.rs:490` | 2 lines | 2 lines | trimmed (no label) |
| Batch refusal shapes | `batch.rs:35` `ITEM_SHAPE`, `batch.rs:584` `FILE_SHAPE` | quoted `source_ref` | no `source_ref` | trimmed |
| Argument refusals | `facade.rs` `ParameterError::message`, `content.rs` | internal reason prefix | natural head, one Repair and quoted field/rule | rewritten (names the missing field without exposing caller values) |
| Diff trailer and no-change line | `snapshots.rs` `render_diff_compact`, `incremental_diff_reply`; `worker.rs` delivery accounting | n/a | new | rewritten: `unchanged since your last diff: K hunks`, `no changes since your last diff`; exact and degraded captures share the filter, returning hunks are new again, and only successful complete-body delivery counts as seen; pending, lost, capped or shrunken carriers preserve unseen hunks |
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
| Region, past-end and nearby addresses of an unknown symbol | `reply.jinja` symbol-detail branches, candidates printed by `symbols.rs` | owned: addresses come from the M-028 resolver |
| Ambiguous bare-name candidates | `symbols.rs` | owned (addresses) |
