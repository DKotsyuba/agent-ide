# Agent-facing MCP content contract

Revision: AGENT-CONTENT-r2. Provider: Assistance content renderer. Consumers: the Assistance facade, retained diff fitting, and every Agent IDE MCP method. The typed result contract remains PeerReply and its nested Changes-owned EditResult.

## Two views of one typed result

Each accepted IDE result produces one MCP CallToolResult built from the same validated typed value, with content always present and structuredContent present per the host-specific projection below:

- content contains exactly one deterministic text block for the coding agent;
- structuredContent, when present, contains the complete unchanged typed result.

The text block does not contain a serialized JSON copy. It starts with the result state, retains only facts needed for the next decision, and names at most one recommended next tool or the native fallback. Rendering does not invoke a model, inspect runtime state, acquire diagnostics, or dispatch differently by host; only the presence of structuredContent varies by host (below).

The renderer does not control whether an MCP host independently exposes structuredContent to its model. It guarantees only the content it creates.

## Host-specific projection

The Claude host hands structuredContent straight to its model in place of content, defeating the compact renderer (observed live in Claude Code, T14B); the renderer cannot change that host behavior, so it stops sending Claude the duplicate it would misuse. Every managed Claude MCP call (`--claude-launcher-template`, and `--auto-launcher-template` once it resolves to Claude) therefore projects content only: structuredContent is entirely absent from the CallToolResult, never emitted as `null` or an empty object. Every other host (Codex, and any other MCP caller) keeps both projections unchanged, and its acceptance evidence still depends on structuredContent.

Because content is Claude's only carrier, it alone must state every fact an accepted reply needs for the next action: the exact detail_ref for a follow-up ide.inspect, the exact detail_ref for Pending, the T08B recovery hint when present (`ide.start` may be repeated; another method needs `ide.start` first and fresh references), truncation, and the Edit outcome with its source_ref. Closed guidance below applies identically to both projections; where it names a fact, that fact is in content, not only in structuredContent.

## Closed guidance

When a configured semantic provider cannot run or cannot verify TypeScript document membership, a path-proven Context uses the normal complete Context content with a lexical mode reason and an editable source reference. The renderer does not turn this Context into a typed error.

Activation points to ide.outline and ide.symbol. Current source Context presents bounded evidence and points to ide.edit with its exact source_ref (the same value as the reply's detail_ref) when available, or the native editor. A `kind: "problems"` Context has no source_ref and preserves the exact v0.2 problems text without edit guidance. A truncated Context or Diff points to ide.inspect only when its typed `continuation` is true; a detail_ref alone is not evidence of another consumable page. Incomplete Context otherwise guides to edit/native work with that same source_ref when one exists, and incomplete Diff to stopping or safe native review. A reviewed Diff points to ide.stop. Stop confirms authority release.

Pending work names ide.inspect with the exact detail_ref. An oversized pending result fails closed.

Edit presents the closed outcome, public path and only the references needed for a safe next action. outcome_unknown requires inspecting the target and forbids replay. When an accepted Edit result carries post-edit diagnostics, current reported diagnostics may point to another ide.edit with its usable source_ref, current clean diagnostics point to ide.diff, and unknown or pending diagnostics point to ide.context or to ide.inspect only when a real detail_ref exists. The renderer never infers clean diagnostics from silence.

For `stale_source`, content states that no write occurred and distinguishes changed target content or presence from an incomplete or unavailable reference. It also states that a newer observation alone does not invalidate identical content. The closed Edit result does not encode a finer stale reason, so the text does not claim which condition occurred.

Typed Error sets isError. A standalone `resolution_unverified` error names the supported configured-project requirement and a later Context retry without claiming that a native tool can substitute for closed TypeScript resolution. Path-proven Context answers use the lexical mode instead. Unavailable, pending, lifecycle, feedback and edit-result states do not become transport errors; each accepted typed reply has one compact content block, and unchanged structuredContent wherever the host-specific projection above includes it. Native fallback is named only when IDE work is unavailable, unsupported, declined or uncertain.

An `execution_profile` refusal with a closed cause renders `error: execution_profile (<tag>); continue with native tools`, preserving the leading code. The tag is one of the fixed, path-free error-log details; for example, `query_policy`. Refusals without a closed cause retain `error: execution_profile; continue with native tools`.

An `outside_allowed_roots` error renders `error: outside_allowed_roots; the working directory is not below any configured allowed root; start the IDE in an allowed directory or add this one to allowed_roots, otherwise continue with native tools`, never naming the path.

## Bounds and privacy

The final serialized CallToolResult, not an intermediate reply, must fit the existing Assistance response ceiling; the renderer measures the exact carrier it is about to emit, so a Claude projection is measured without the structuredContent it omits. Only owner Complete text may shrink, at UTF-8 boundaries, while marking truncation. Closed identifiers, paths, outcomes and references are not partially emitted.

Compact content must not add source text, prompts, credentials, native tool payloads, arbitrary provider or operating-system errors, host metadata or telemetry fields beyond facts already admitted by the typed result.

Every page of a multi-page Context or Claude-captured Diff starts with a position marker line, `page N; bytes A-B of TOTAL`, and the last is `page N (last); bytes A-TOTAL of TOTAL; complete`; a single-page result has none. Page one of a Claude result is delivered by the ticket settlement, so its first `ide.inspect` returns page two; a re-inspect after the last page re-serves the last page. A Context is paged whole up to the 1 MiB source ceiling, and its `source_ref` is refused for `ide.edit` until the last page was delivered. Each Diff hunk is preceded by a `file: <path>` line, and a continuation page that starts inside a file's hunks begins with `file: <path> (continued)`.

Diff and Context pagination (`fit_diff_page`, `ContextPageState::next`) compose a page on the daemon side, before any MCP call has identified its host, so both stay sized to fit the worst-case carrier — content plus structuredContent — even though a Claude call's actual final envelope, once projected, has no structuredContent to fit at all.

## Gates

Contract tests cover every PeerReply state, every Edit outcome, pending with a detail reference, errors, UTF-8 truncation, exact references, final-envelope bounds and structuredContent equality. The eleven public IDE tools and retained Diff pagination use the same renderer. A real Codex and Claude acceptance records compact content, plus the matching typed structured result for Codex and its deliberate absence for Claude.
