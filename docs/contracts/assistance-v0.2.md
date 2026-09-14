# Assistance v0.2 contract

Revision: EDIT-r1 (proposed). Provider: Assistance. Direct consumers: Application, Changes, and host adapters. Vocabulary: [common](common.md).

## Sixth closed MCP method

Assistance owns the sixth and only added MCP method, `ide.edit`, alongside `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop`. Its parameters and results are exactly the canonical request/outcomes in [Changes v0.2](changes-v0.2.md); Assistance validates the closed schema, current host binding, active authority, and bounded rendering, then delegates. It neither resolves paths, writes files, settles receipts, nor interprets Application storage rows.

The only success rendering is a bounded `created`, `replaced`, or `unchanged` result with operation/path references and post-read source reference. `stale_source`, `unsafe_target`, conflicts, no-effect results, unavailable, and unknown are rendered as compact typed outcomes. `outcome_unknown` instructs inspection of the named target before another mutation; it never recommends replay. Tool failure remains fail-open for native work.

Claude uses its existing strict, claimed foreground helper path for every `ide.edit` effect. It cannot borrow a daemon write or an ambient shell; absent claimed helper evidence returns `unavailable_before_dispatch`. Codex uses its already proven host-bound path. These host routes remain adapters, not alternate edit contracts.

Success: a bound active call delegates a complete request and returns `replaced` with the post-read source reference. Error: an unbound Claude call returns `unavailable_before_dispatch` and does not mint a receipt or write.

## Gates

A substitute gate proves only schema closure, rendering, host-binding rejection, and native-fallback presentation. A real integration gate proves the sixth method through the actual closed IPC route and each supported host adapter; it consumes the Workspace/Changes effect gates and does not substitute host fixtures for them.
