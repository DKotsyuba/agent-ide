# Harness reply v1 contract

Revision: harness-reply-v1 r5; tracked boundary id `harness-reply-v1`, revision 5 (r5 raises the catalog ceilings of section 3 from 10/7 KiB to 15/13 KiB; the current served catalogs measure 15.1 KB for Claude and 12.9 KB for Codex; every other rule and budget is r4's). Provider: M-030 (`assistance/facade.rs`, `content.rs`, `assets/mcp/reply.jinja`, `signature_card.rs`, `hygiene.rs`, `snapshots.rs`, the skills). Consumers: M-028 (what search, outline, diff and error replies print for an address) and M-029 (what `ide.read` and `ide.edit` answer), both of which follow these rules in every reply they add or change. Consumes: [symbol-address-v1](symbol-address-v1.md) r2 for how a location prints and [symbol-read-edit-v1](symbol-read-edit-v1.md) r2 for the read and edit shapes. Vocabulary: [common](common.md); reply states: [reply-hygiene-v1](reply-hygiene-v1.md) and [agent-content-v0.2](agent-content-v0.2.md), whose Codex `structuredContent` copy this contract removes.

Purpose: an IDE reply costs the model about what the same fact costs from a native tool. It is one block of plain text, says only what the agent does not already know, and names the single next step when something went wrong.

## 1. Rules

- **R1 One text block.** Every successful or failed tool result of every host (Claude, Codex, an unrecognized host) carries exactly one `text` content block and no `structuredContent`. `isError`, the status plate (`<agent-ide>` block) and every retry hint stay in that text. Codex shows `structuredContent` instead of `content` when it is present, so a duplicate would replace the compact text and double the bytes. The typed reply value stays an internal oracle: a `test-seams` build keeps the copy for non-Claude hosts when `AGENT_IDE_TEST_STRUCTURED_REPLIES=1` (set by `.cargo/config.toml` for the workspace tests); a release build never reads that variable.
- **R2 No labels.** No state-label prefix (`complete`, `ok`) and no `Next:` line. A reply that simply delivers what was asked starts with the delivery.
- **R3 Framing only on exceptions.** A summary line, count or trailer appears only when something is not simply delivered: a cut (one trailer line per cut, naming how to continue), a partial result, an approximate answer, a clamp. Never a line that restates the request or the count the text already shows.
- **R4 Locations as the address grammar says.** Wherever a reply names a place it prints the symbol address plus the absolute line exactly as symbol-address-v1 defines, for example `src/x.rs#Type/method:7 (L123)`; regions print as `src/x.rs#@imports`. No reply invents another location form.
- **R5 Refusals.** A refusal is a head sentence of at most 160 bytes, then one `Repair:` line of at most 120 bytes naming the single next call or edit, then optionally an excerpt of the current text (at most 60 lines or 4 KiB). It names no internal reason code, retry count or alternative list.
- **R6 Nothing teaches waiting.** No server paragraph, tool description, schema text, start reply, pending or started wording, skill or doc tells the agent to wait, poll or call `ide.inspect` with `wait_ms`. Results of long work arrive by themselves in an `<agent-ide>` block after the next tool call or at the end of the turn. `wait_ms` stays accepted by the validator but is not advertised. Codex idle cover: results arrive on the next `ide.*` reply or hook; it is a blocker only if a deferred result can be lost.
- **R7 Budgets.** The sizes in section 3 bound descriptions, catalogs, skills and the replies that carry the most framing. M-030 may trim framing lines (R3) later without a revision of this contract; raising a budget needs one.

## 2. Catalog per host

Both hosts still accept every tool and parameter ever advertised (`ide.context`, `ide.graph`, `wait_ms`); only the advertised subset differs.

| Host | Advertised tools |
|---|---|
| Claude, Claude-compatible and unmanaged `mcp` | 11: start, context, diff, inspect, stop, edit, outline, read, symbol, graph, test |
| Codex (managed) | 9: start, diff, inspect (also serves the project problems), stop, edit, outline, read, symbol (callers and callees through its depth parameters), test |

The host is the managed launcher's host; `mcp --host codex|claude` selects it for export. The server paragraph is per host and says the same thing in fewer words for Codex.

## 3. Byte budgets

| Item | Budget |
|---|---|
| One tool description | at most 600 B; hard ceiling 2048 B (the Claude clipping limit) |
| One property description | at most 160 B |
| Server paragraph | at most 700 B |
| Claude catalog (descriptions plus input schemas, as served) | at most 15 KiB (E-002: 21.8 KiB) |
| Codex catalog (as served) | at most 13 KiB |
| Claude skill | at most 6 KiB |
| Codex skill | at most 4 KiB |
| `ide.start` signature block | at most 1.5 KiB |
| Read, outline and diff header | one line, at most 160 B |
| Trailer per cut | one line, at most 160 B |

`facade::catalog_stays_within_the_budgets` enforces the 600 B description, 160 B property, 700 B paragraph and catalog limits for both hosts. The catalog totals were first written as 10 KiB and 7 KiB before the schemas were measured; the advertised schemas carry structure (types, enums, defaults, nesting) that cannot go without losing a parameter, so the ceilings are the measured 15 KiB and 13 KiB. Every other budget is unchanged.

## 4. Baseline

Sizes of the E-002 build (`eaeba10`), the numbers the E-003 changes are measured against:

| Item | Bytes at `eaeba10` |
|---|---|
| Descriptions plus input schemas, 11 tools (`schemas/tools.json`, compact JSON) | 22,345 |
| Longest description (`ide.test`) | 997 |
| Longest input schema (`ide.edit`) | 3,739 |
| Server paragraph | 735 |
| Claude skill (`skills/agent-ide/SKILL.md`) | 33,349 |
| Reply template (`assets/mcp/reply.jinja`) | 37,119 |

The same figures are recomputed from the tree by `python3 -c` over `schemas/tools.json` and `wc -c` over the skill and template; `ide.inspect` after M-030 T-005 is 375 B description plus 396 B schema (462 B and 593 B before).

## 5. Conformance

- R1: a product test per host family (modern Codex without `initialize`, managed auto, an unrecognized host) asserts one text block and no `structuredContent` on a binary run without the test seam; the pinned unrecognized-host assertion states the rule.
- R6: `facade::advertised_text_never_teaches_waiting` scans the server paragraph and the serialized catalog; `signature_card` tests assert the start block names no `wait_ms`.
- R7: the catalog budget test above plus `schemas/tools.json` equal to the binary (`cargo xtask contract check`).
- R2-R5: reply-rendering unit tests of `content.rs` and the M-028/M-029 reply tests, updated in the tasks that change those replies.
