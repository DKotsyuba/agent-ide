# Telemetry v0.2 contract

Revision: TELEMETRY-r1 (proposed; no implementation claim). Provider: Telemetry. Direct consumers: Assistance, Changes, Intelligence, Workspace, and Application. Vocabulary: [common](common.md).

## Closed local event boundary

Telemetry records a closed, privacy-safe set of typed local events for the six public MCP methods and native fallback observations. An event contains only its schema tag, outcome/fallback reason, bounded duration, provider/language/profile revision, cache/diagnostic state, bounded output-size class, and existing measured resource facts. Measured facts are limited to values current Execution or provider paths already produce: elapsed duration, bounded output byte counts and truncation, admission and cancellation state, and descendant-settlement categories. Telemetry neither samples processes nor invents measurements.

An event never contains source or other content, paths, prompts, credentials, arguments, commands, stdout, stderr, diagnostic messages, free-form error text, private attachments, or identifiers that can encode those values. Unknown tags, fields, or enum values are rejected before ingestion. Callers supply typed values, never JSON blobs. Telemetry does not infer semantics from Application rows.

Ingestion is fail-open and nonblocking: a full queue, unavailable store, encoding failure, or deadline expiry drops only that event and cannot delay, change, retry, or deny the originating operation or native fallback. No event creates authority, effects, or a retry right.

## Ownership, retention, and reads

Telemetry owns semantic event rows, schema validation, deterministic ordering, eviction, query interpretation, and export rendering. Application owns SQLite/IPC/configuration mechanics and supplies only a bounded trusted multi-row read for Telemetry's fixed query. Application must not interpret event tags or aggregate semantics.

Rows are ordered by increasing durable sequence; equal sequence is invalid. At insertion, Telemetry evicts oldest rows at the first reached ceiling: 100000 events or 16 MiB of logical payload, including the new row. Logical payload is the canonical encoded event bytes, not SQLite file size or transport framing. An encoded event is at most 2 KiB; an oversized event is dropped with no substitute text.

`query(filter, cursor, limit)` returns a contiguous sequence-ordered page of at most 1000 rows, a next cursor when more matching rows exist, and explicit `truncated`/`dropped` counts. `export(filter)` is sequence ordered and is at most 4 MiB of canonical UTF-8 rows. It stops before exceeding the cap and returns `truncated: true` with the first omitted sequence; it never silently samples or reorders. Reads and export have no operation receipt and cannot mutate retention.

All telemetry settings (enabled state, retention ceilings within the hard maxima, and read/export budgets) are restart-only. A new effective configuration applies only to a new Telemetry owner; no live reload, migration of semantics, remote upload, dashboard, or analytics endpoint is part of this revision.

## Examples

Success: `ToolCompleted { method: "edit", outcome: "replaced", duration_ms: 84, language: "typescript", diagnostics: "changed" }` is accepted if its canonical encoding is within 2 KiB.

Error: `ToolCompleted { path: "src/private.ts", outcome: "replaced" }` is rejected because `path` is not a field. If the local sink is unavailable, the edit result is still returned normally and the event is dropped.

## Gates

A substitute gate uses a fixed clock and scripted store to prove closed-field rejection, first-ceiling eviction, ordering, cursor continuity, and explicit query/export truncation. A real integration gate uses Application's bounded multi-row read and a restart to prove persisted ordering, both retention ceilings, bounded export, and that an unavailable sink leaves an MCP and native fallback path usable. Neither gate proves product adoption or remote analytics.
