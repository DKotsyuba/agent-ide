# Agent-facing MCP content contract

Revision: AGENT-CONTENT-r2. Provider: Assistance content renderer. Consumers: the Assistance facade, retained diff fitting, and every Agent IDE MCP method. The typed result contract remains PeerReply and its nested Changes-owned EditResult.

## One text block per typed result

Each accepted IDE result produces one MCP CallToolResult built from the validated typed value
(`PeerReply`). Per [harness-reply-v1](harness-reply-v1.md) R1, every production result of every
host (Claude, Codex, an unrecognized host) carries exactly one deterministic text block and no
`structuredContent`; the field is absent, never `null` or an empty object. The public catalog is
ten tools on both hosts and the public `ide.context` is removed; its private engine only serves
`ide.inspect` problems and diagnostics. The typed value stays an internal oracle: only a
`test-seams` build keeps the unchanged copy in `structuredContent` for non-Claude hosts, when
`AGENT_IDE_TEST_STRUCTURED_REPLIES=1`; a release build never reads that variable.

The text block does not contain a serialized JSON copy. It leads with the delivery (no state-label
prefix and no `Next:` line), retains only facts needed for the next decision, and names at most one
Repair action when something went wrong. Rendering does not invoke a model, inspect runtime
state, acquire diagnostics, or dispatch differently by host.

Because the text is the only carrier, it alone states every fact an accepted reply needs for the
next action: the exact `detail_ref` of a follow-up `ide.inspect`, the exact `detail_ref` of a
pending result, the recovery hint when present (`ide.start` may be repeated; another method
needs `ide.start` first and fresh references), truncation, and the edit outcome.

## Closed guidance

When a configured semantic provider cannot run or cannot verify TypeScript document membership, a path-proven read or diagnostics answer uses its normal content with a lexical mode reason. The renderer does not turn it into a typed error.

Activation points to ide.outline and ide.symbol. Reads and outlines present bounded evidence addressed by symbol (`src/a.rs#Type/method`); code is edited by symbol, and path line ranges are for non-code text. A source read names no file-version reference and an edit needs none. A truncated read or Diff points to ide.inspect or the originating tool's `detail_ref` only when its typed `continuation` is true; a detail_ref alone is not evidence of another consumable page. Incomplete Diff guides to stopping or safe native review. A reviewed Diff points to ide.stop. Stop confirms authority release.

Deferred graph, test and edit-check results stay retained under their reference. When Bridge is configured it delivers them automatically, owning carrier and timing; IDE has no legacy `Stop`, reply or hook deferred carrier, and no reply teaches waiting or polling. Pending work names its exact detail_ref for explicit retrieval only. An oversized pending result fails closed.

Edit presents the closed outcome, public path and only the facts needed for a safe next action. outcome_unknown requires inspecting the target (`ide.diff`) and forbids replay. When an accepted Edit result carries post-edit diagnostics, current reported diagnostics are listed for another symbol edit, current clean diagnostics need no follow-up, and unknown or pending diagnostics point to ide.inspect only when a real detail_ref exists. The renderer never infers clean diagnostics from silence.

For `stale_source`, content names the path and says no write occurred because the file changed since it was read. It directs the caller to re-read the affected symbol and repeat the edit.

Typed Error sets isError. A standalone `resolution_unverified` error names the supported configured-project requirement and a later retry without claiming that a native tool can substitute for closed TypeScript resolution. Path-proven answers use the lexical mode instead. Unavailable, pending, lifecycle, feedback and edit-result states do not become transport errors; each accepted typed reply has one compact text block. Native fallback is named only when IDE work is unavailable, unsupported, declined or uncertain; every busy, restart or lost-reply refusal names exactly one concrete Repair action, never a two-step retry-then-native rule.

An `execution_profile` refusal with a closed cause renders a plain head naming the rejected operation and the native-tools recovery, without the internal code or cause tag. For example: `The Git query was refused by the execution policy.` then `Repair: Continue with native tools.`

An `outside_allowed_roots` refusal says `This project is outside the directories the IDE may open.` with the Repair `Call ide.start with an allowed root.`; an absent requested root also names its nearest admitted ancestor.

## Bounds and privacy

The final serialized CallToolResult, not an intermediate reply, must fit the existing Assistance response ceiling; the renderer measures the exact carrier it is about to emit, so the measured carrier has no `structuredContent` to fit. Only owner Complete text may shrink, at UTF-8 boundaries, while marking truncation. Closed identifiers, paths, outcomes and references are not partially emitted.

Compact content must not add source text, prompts, credentials, native tool payloads, arbitrary provider or operating-system errors, host metadata or telemetry fields beyond facts already admitted by the typed result.

Every page of a multi-page read or captured Diff starts with a position marker line, `page N; bytes A-B of TOTAL`, and the last is `page N (last); bytes A-TOTAL of TOTAL; complete`; a single-page result has none. Page one is delivered by the ticket settlement, so its first `ide.inspect` returns page two; a re-inspect after the last page re-serves the last page. Each Diff hunk is preceded by a `file: <path>` line, and a continuation page that starts inside a file's hunks begins with `file: <path> (continued)`.

Diff and source pagination (`fit_diff_page`, `ContextPageState::next`) compose a page on the daemon side, before any MCP call has identified its host, so both stay sized to fit the worst-case carrier; the final production envelope is the text block alone.

## Gates

Contract tests cover every PeerReply state, every Edit outcome, pending with a detail reference, errors (one Repair line within the head and byte budgets, no `; otherwise` two-step repair), UTF-8 truncation, exact references and final-envelope bounds. The ten public IDE tools and retained Diff pagination use the same renderer. A real Codex and Claude acceptance records compact single-block content and the deliberate absence of `structuredContent` on both hosts.
