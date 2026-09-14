# Agent-facing MCP content contract

Revision: AGENT-CONTENT-r1. Provider: Assistance content renderer. Consumers: the Assistance facade, retained diff fitting, and every Agent IDE MCP method. The typed result contract remains PeerReply and its nested Changes-owned EditResult.

## Two views of one typed result

Each accepted IDE result produces one MCP CallToolResult with two projections from the same validated typed value:

- content contains exactly one deterministic text block for the coding agent;
- structuredContent contains the complete unchanged typed result.

The text block does not contain a serialized JSON copy. It starts with the result state, retains only facts needed for the next decision, and names at most one recommended next tool or the native fallback. Rendering does not invoke a model, inspect runtime state, acquire diagnostics, or vary by host.

The renderer does not control whether an MCP host independently exposes structuredContent to its model. It guarantees only the content it creates.

## Closed guidance

Activation points to ide.context. Current Context presents bounded evidence and points to ide.edit when available or the native editor. A truncated Context or Diff points to ide.inspect only when its typed `continuation` is true; a detail_ref alone is not evidence of another consumable page. Incomplete Context otherwise guides to edit/native work, and incomplete Diff to stopping or safe native review. A reviewed Diff points to ide.stop. Stop confirms authority release.

Pending work preserves an exact helper command when present, requires foreground execution, and then names ide.inspect with the exact detail_ref. The helper and references are never silently shortened. An oversized pending result fails closed.

Edit presents the closed outcome, public path and only the references needed for a safe next action. outcome_unknown requires inspecting the target and forbids replay. When an accepted Edit result carries post-edit diagnostics, current reported diagnostics may point to another ide.edit with its usable source_ref, current clean diagnostics point to ide.diff, and unknown or pending diagnostics point to ide.context or to ide.inspect only when a real detail_ref exists. The renderer never infers clean diagnostics from silence.

Typed Error sets isError. `resolution_unverified` names the supported configured-project requirement and a later Context retry without claiming that a native tool can substitute for closed TypeScript resolution. Unavailable, pending, lifecycle, feedback and edit-result states do not become transport errors; each accepted typed reply has one compact content block and unchanged structuredContent. Native fallback is named only when IDE work is unavailable, unsupported, declined or uncertain.

## Bounds and privacy

The final serialized CallToolResult, not an intermediate reply, must fit the existing Assistance response ceiling. Only owner Complete text may shrink, at UTF-8 boundaries, while marking truncation. Closed identifiers, paths, outcomes, helper commands and references are not partially emitted.

Compact content must not add source text, prompts, credentials, native tool payloads, arbitrary provider or operating-system errors, host metadata or telemetry fields beyond facts already admitted by the typed result.

## Gates

Contract tests cover every PeerReply state, every Edit outcome, pending with and without a helper, errors, UTF-8 truncation, exact references, final-envelope bounds and structuredContent equality. The six public IDE tools and retained Diff pagination use the same renderer. A real Codex and Claude acceptance records compact content plus the matching typed structured result.
