# Local telemetry

Telemetry v0.2 stores a closed set of local usage events in the existing Application SQLite owner. It is not remote analytics and has no dashboard or upload path.

The durable event schema contains only fixed tags and enums plus bounded elapsed or output-size summaries already measured by existing code. It never records source, paths, prompts, credentials, tool arguments, commands, stdout, stderr, diagnostic messages, or free-form error text. Invalid, oversized, queued-out, or storage-unavailable events are dropped; coding and native fallback behavior continues unchanged.

Rows are ordered by durable sequence. At insertion, the oldest rows are evicted as soon as either the configured row count (at most 100,000) or canonical event payload total (at most 16 MiB) would be exceeded. An event's canonical encoding is at most 2 KiB. Query and export use Application's bounded multi-row read; neither opens SQLite independently or changes retention.

See [the telemetry contract](contracts/telemetry-v0.2.md) for the fixed vocabulary and read/export guarantees.

Managed sessions retain telemetry in their candidate-stable local Store rather than the disposable runtime directory. `agent-ide telemetry query --database <local.sqlite> [--tag <closed-tag>]` emits a deterministic page of no more than 1,000 rows. `agent-ide telemetry export --database <local.sqlite> [--tag <closed-tag>]` writes canonical rows until its 4 MiB budget and reports `truncated` plus the first omitted sequence on stderr. Both commands accept only the four closed tags, use a read-only Application Store API, never accept SQL, and never create or migrate a database.
