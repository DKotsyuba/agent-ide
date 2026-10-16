# Harness reply v1 contract

Revision: harness-reply-v1 r5; tracked boundary id `harness-reply-v1`, revision 5. Historically r5 raised catalog ceilings from 10/7 KiB to 15/13 KiB (then 15.1 KB Claude, 12.9 KB Codex). The owner-directed amendment restores Codex `ide.graph`, authorizes 14 KiB for Codex, and removes public `ide.context` from both hosts (ten names each). This is not an Agent Tasks ratification of a new revision. Export history and current measurements live in [model text inventory](../model-text-inventory.md). Both final schemas have been exported; focused checks have run, while acceptance still requires the complete project gate. Other r5 rules remain. Provider: M-030 (`assistance/facade.rs`, `content.rs`, `assets/mcp/reply.jinja`, `signature_card.rs`, `hygiene.rs`, `snapshots.rs`, the skills). Consumers: M-028 (addresses) and M-029 (read/edit). Consumes: [symbol-address-v1](symbol-address-v1.md) r2 and [symbol-read-edit-v1](symbol-read-edit-v1.md) r2. Vocabulary: [common](common.md); reply states: [reply-hygiene-v1](reply-hygiene-v1.md) and [agent-content-v0.2](agent-content-v0.2.md).

Purpose: an IDE reply costs the model about what the same fact costs from a native tool. It is one block of plain text, says only what the agent does not already know, and names the single next step when something went wrong.

## 1. Rules

- **R1 One text block.** Every successful or failed tool result of every host (Claude, Codex, an unrecognized host) carries exactly one `text` content block and no `structuredContent`. `isError`, the status plate (`<agent-ide>` block) and every retry hint stay in that text. Codex shows `structuredContent` instead of `content` when it is present, so a duplicate would replace the compact text and double the bytes. The typed reply value stays an internal oracle: a `test-seams` build keeps the copy for non-Claude hosts when `AGENT_IDE_TEST_STRUCTURED_REPLIES=1` (set by `.cargo/config.toml` for the workspace tests); a release build never reads that variable.
- **R2 No labels.** No state-label prefix (`complete`, `ok`) and no `Next:` line. A reply that simply delivers what was asked starts with the delivery.
- **R3 Framing only on exceptions.** A summary line, count or trailer appears only when something is not simply delivered: a cut (one trailer line per cut, naming how to continue), a partial result, an approximate answer, a clamp. Never a line that restates the request or the count the text already shows.
- **R4 Locations as the address grammar says.** Wherever a reply names a place it prints the symbol address plus the absolute line exactly as symbol-address-v1 defines, for example `src/x.rs#Type/method:7 (L123)`; regions print as `src/x.rs#@imports`. No reply invents another location form.
- **R5 Refusals.** A refusal is a head sentence of at most 160 bytes, then one `Repair:` line of at most 120 bytes naming the single next call or edit, then optionally an excerpt of the current text (at most 60 lines or 4 KiB). It names no internal reason code, retry count or alternative list.
- **R6 Nothing teaches waiting.** No server paragraph, tool description, schema text, start reply, pending or started wording, skill or doc tells the agent to wait, poll or call `ide.inspect` with `wait_ms`. A deferred graph, test or edit-check result stays retained under its reference (`ide.test {"result":N}`, `detail_ref`); when Bridge is configured it delivers the result automatically, and Bridge owns the carrier and timing. IDE has no `Stop` hold, hook or reply-piggyback deferred carrier, so without a Bridge route the result is read explicitly. `wait_ms` stays accepted by the validator but is not advertised.
- **R7 Budgets.** The sizes in section 3 bound descriptions, catalogs, skills and the replies that carry the most framing. M-030 may trim framing lines (R3) later without a revision of this contract; raising a budget needs one.

## 2. Catalog per host

Both hosts advertise the same ten public names, including `ide.graph`. The owner-authorized
breaking amendment removes public `ide.context`, including direct calls; its private engine
and transport remain reusable. Legacy parameter compatibility such as `wait_ms` remains.
The native `ide.edit` `patch` property is advertised only to Codex; Claude keeps its old/new and
address edit forms, and the backend still accepts patches on either host for compatibility.

| Host | Advertised tools |
|---|---|
| Claude, Claude-compatible and unmanaged `mcp` | 10: start, diff, inspect, stop, edit, outline, read, symbol, graph, test |
| Codex (managed) | 10: start, diff, inspect (also serves the project problems), stop, edit, outline, read, symbol, graph, test |

The host is the managed launcher's host; `mcp --host codex|claude` selects it for export. The server paragraph is per host and says the same thing in fewer words for Codex.

`ide.symbol {"symbol":"a.rs#f","callers":1}` gives the symbol card and immediate
neighbors; values 1–3 do not add levels. `ide.graph {"symbol":"a.rs#f","depth":2}`
traverses call chains for impact, architecture or indirect faults. Use graph for
signature/public changes, shared refactors or cross-language connections; tiny edits
need only the card. Start depth 2, expand within 1–3 only as needed; callers traces impact,
callees traces execution/dependencies, both traces both sides, and tests:true expands tests.

Former context source reads use `ide.read {"path":"a.rs"}`; skeletons use `ide.outline`.
`ide.symbol {"path":"a.rs","byte_offset":42}` resolves an exact UTF-8 use-site,
returning definitions/references or explicit lexical/unavailable evidence without a file dump
or raw authority/version bookkeeping. Invalid UTF-8 positions refuse; offsets are never
continuation cursors. `ide.inspect {"kind":"diagnostics","path":"a.rs"}` reads provider
reports using the existing source/generation/version fences; unknown stays unknown,
unversioned nonempty counts stay provisional lower bounds. `kind:"problems"` remains the
project check feed. Neither query starts a build/check, and bounded continuation uses
`ide.inspect {"detail_ref":"…"}`. Deferred Inspect queries retain their public tool name and,
like other deferred results, are retained and delivered only through Bridge.

## 3. Byte budgets

| Item | Budget |
|---|---|
| One tool description | at most 600 B; hard ceiling 2048 B (the Claude clipping limit) |
| One property description | at most 160 B |
| Server paragraph | at most 700 B |
| Claude catalog (descriptions plus input schemas, as served) | at most 15 KiB (E-002: 21.8 KiB) |
| Codex catalog (as served) | at most 14 KiB (owner-directed graph amendment; historical r5: 13 KiB) |
| Claude skill | at most 6 KiB |
| Codex skill | at most 4 KiB |
| `ide.start` signature block | at most 700 B (T-006; historical r5 broader budget: 1.5 KiB) |
| Read, outline and diff header | one line, at most 160 B |
| Trailer per cut | one line, at most 160 B |

`facade::catalog_stays_within_the_budgets` enforces the 600 B description, 160 B property,
700 B paragraph and 15/14 KiB catalog limits. The historical 10/7 KiB ceilings became
15/13 KiB in r5 after measurement; the owner's graph restoration raises only Codex to
14 KiB. Description, property, paragraph, skill and strict 700 B signature ceilings stay unchanged.

## 4. Baseline

Historical sizes of the E-002 build (`eaeba10`, eleven tools including `ide.context`), the numbers the E-003 changes were measured against; the current catalog has ten tools:

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
