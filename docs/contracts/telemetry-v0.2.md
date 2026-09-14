# Telemetry v0.2 contract

Revision: TELEMETRY-r1 (proposed; no implementation claim). Provider: Telemetry. Direct consumers: Assistance, Changes, Intelligence, Workspace, and Application. Vocabulary: [common](common.md).

## Closed local event boundary

Telemetry records a closed, privacy-safe set of typed local events for the five public MCP methods and native fallback observations. An event contains only its schema tag, outcome/fallback reason, bounded duration, provider/language/profile revision, cache/diagnostic state, bounded output-size class, and existing measured resource facts. Measured facts are limited to values current Execution or provider paths already produce: elapsed duration, bounded output byte counts and truncation, admission and cancellation state, and descendant-settlement categories. Telemetry neither samples processes nor invents measurements. A tool deadline is an incomplete observation, not a generic failure.

An event never contains source or other content, paths, prompts, credentials, arguments, commands, stdout, stderr, diagnostic messages, free-form error text, private attachments, or identifiers that can encode those values. Unknown tags, fields, or enum values are rejected before ingestion. Callers supply typed values, never JSON blobs. Telemetry does not infer semantics from Application rows.

Ingestion is fail-open and nonblocking: a full queue, unavailable store, or encoding failure drops only that event and cannot delay, change, retry, or deny the originating operation or native fallback. A timeout after receipt-free work was accepted is outcome-unknown because that work may still commit; it is not counted as dropped, and the writer refreshes its retention totals before a later insert. No event creates authority, effects, or a retry right.

## Ownership, retention, and reads

Telemetry owns semantic event rows, schema validation, deterministic ordering, eviction, query interpretation, and export rendering. Application owns SQLite/IPC/configuration mechanics and supplies only a bounded trusted multi-row read for Telemetry's fixed query. Application must not interpret event tags or aggregate semantics. Managed Workspace, Changes, boot authority, and receipts stay in each session's private runtime Store. Only telemetry uses candidate-stable storage, with one nonblocking advisory-lock owner; contention disables capture for the later daemon without opening that stable SQLite database or affecting either session authority.

Rows are ordered by increasing durable sequence; equal sequence is invalid. The exclusive writer scans retained row and logical-byte totals once at startup, then maintains them per insert and visits only rows actually evicted. At insertion, Telemetry evicts oldest rows at the first reached ceiling: 100000 events or 16 MiB of logical payload, including the new row. Logical payload is the canonical encoded event bytes, not SQLite file size or transport framing. An encoded event is at most 2 KiB; an oversized event is dropped with no substitute text.

`query(filter, cursor, limit)` returns a contiguous sequence-ordered page of at most 1000 rows, a next cursor when more matching rows exist, and explicit `truncated` state. The CLI accepts that returned cursor for subsequent pages. A live writer reports its observed `dropped` count; an independent read-only query/export reports `null` because the value is unknown. `export(filter)` is sequence ordered and is at most 4 MiB of canonical UTF-8 rows. It stops before exceeding the cap and returns `truncated: true` with the first omitted sequence; it never silently samples or reorders. Reads and export have no operation receipt and cannot mutate retention.

Native hooks never open or wait on SQLite. After a valid hook reaches an unavailable or deadline boundary, it may send only one fixed byte to the daemon's private nonblocking datagram endpoint. The daemon converts only that marker to `NativeFallback { reason: HookUnavailable }`; all other payload shapes are ignored. Graceful daemon shutdown first drains delivered markers and then drains the telemetry queue so the final accepted event is not lost.

All telemetry settings (enabled state, retention ceilings within the hard maxima, and read/export budgets) are restart-only. A new effective configuration applies only to a new Telemetry owner; no live reload, migration of semantics, remote upload, dashboard, or analytics endpoint is part of this revision.

## Examples

Success: `ToolCompleted { method: "edit", outcome: "replaced", duration_ms: 84, language: "typescript", diagnostics: "changed" }` is accepted if its canonical encoding is within 2 KiB.

Error: `ToolCompleted { path: "src/private.ts", outcome: "replaced" }` is rejected because `path` is not a field. If the local sink is unavailable, the edit result is still returned normally and the event is dropped.

## Gates

A substitute gate uses a fixed clock and scripted store to prove closed-field rejection, first-ceiling eviction, ordering, cursor continuity, and explicit query/export truncation. A real integration gate uses Application's bounded multi-row read and a restart to prove persisted ordering, both retention ceilings, bounded export, and that an unavailable sink leaves an MCP and native fallback path usable. Neither gate proves product adoption or remote analytics.
