# Changes v0.2 edit contract

Revision: EDIT-r1 (proposed; extends [Changes v0.1](changes-v0.1.md)). Provider: Changes. Direct consumer: Assistance. Changes consumes the Workspace mutation boundary and Application persistence mechanics. Vocabulary: [common](common.md).

## Canonical request and durable settlement

The entire `ide.edit` request is exactly `operation_id`, relative UTF-8 `path`, same-binding `source_ref`, and full UTF-8 `content`. `source_ref` names either a completed `ide.context` or a prior successful `ide.edit` (`created`, `replaced`, or `unchanged`) for the exact same path within this binding; either chains the identical post-read source binding, so a successful edit needs no context round trip before the next one. There are no optional fields, patch syntax, rename, multi-file journal, shell, `check`, or `finish` operation. Content is at most 48 KiB and the canonical serialized edit arguments are at most 56 KiB; enclosing MCP/IPC limits remain independent and may refuse an otherwise valid request.

Changes owns the `Prepared` and settled durable one-file receipt, duplicate/recovery semantics, orchestration, and public effects result. A receipt stores the canonical request fingerprint, target path, prepared/settled state, and exact settled outcome. It first prepares durably, obtains Workspace's one-use permit, and settles the receipt only from Workspace's typed result. Application provides transaction/receipt mechanics only; it does not decide edit semantics or filesystem effects.

An exact duplicate returns its settled receipt. A changed request with the same operation ID is `conflicting_duplicate`. After restart or an ambiguous accepted effect, Changes reconciles the prepared receipt and Workspace's target-specific post-state; it never sends the write again. Before dispatch it may return `unavailable_before_dispatch`, `cancelled_no_effect`, `deadline_no_effect`, or `capacity_no_effect`. A possibly dispatched write that cannot be conclusively settled returns `outcome_unknown(operation_id, path)`; callers must inspect the named current target and must not blind-retry.

## Public outcomes and fallback

The public outcomes are exactly `created`, `replaced`, `unchanged`, `stale_source`, `conflicting_duplicate`, `unsafe_target`, `cancelled_no_effect`, `deadline_no_effect`, `capacity_no_effect`, `outcome_unknown`, and `unavailable_before_dispatch`. Success includes the path and post-read source reference; errors include only the bounded outcome and operation/path references, never content or diagnostic text.

Success: a prepared request for a missing eligible `notes/todo.ts` settles `created` with its post-read source reference.

Error: the same operation ID with different content returns `conflicting_duplicate`; a crash after Workspace may have replaced `src/a.ts` returns `outcome_unknown(operation_id, "src/a.ts")` until inspected.

Native editing remains available when this operation is inactive, unavailable, unsupported, declined, or uncertain. Assistance may guide toward `ide.edit`, but never blocks a native fallback. Telemetry may observe the fallback under [TELEMETRY-r1](telemetry-v0.2.md) without recording its content or path.

## Gates

A substitute gate proves request fingerprinting, durable prepare/settle ordering, duplicate conflict, every no-effect outcome, and no replay after an unknown result. A real integration gate uses Application persistence plus Workspace descriptor operations to prove recovery after process interruption and native fallback remains usable. It does not prove an atomic multi-file transaction.
