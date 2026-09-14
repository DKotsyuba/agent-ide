# Local telemetry

Telemetry v0.2 stores a closed set of local usage events in the existing Application SQLite owner. It is not remote analytics and has no dashboard or upload path.

The durable event schema contains only fixed tags and enums plus bounded elapsed or output-size summaries already measured by existing code. It never records source, paths, prompts, credentials, tool arguments, commands, stdout, stderr, diagnostic messages, or free-form error text. Invalid, oversized, queued-out, or storage-unavailable events are dropped; an accepted writer timeout remains outcome-unknown rather than being counted as dropped. Coding and native fallback behavior continues unchanged.

Rows are ordered by durable sequence. At writer startup Telemetry loads retained row and byte totals once; each later insert updates those totals and visits only rows that must be evicted when either the configured row count (at most 100,000) or canonical event payload total (at most 16 MiB) would be exceeded. An event's canonical encoding is at most 2 KiB. Query and export use Application's bounded multi-row read; neither opens SQLite independently or changes retention.

See [the telemetry contract](contracts/telemetry-v0.2.md) for the fixed vocabulary and read/export guarantees.

Managed sessions keep Workspace, Changes, receipts, and boot authority in their private runtime Store. Telemetry alone uses a candidate-stable database guarded by a nonblocking process lock: one managed daemon owns durable capture, while another daemon for the same worktree disables only its telemetry sink and retains independent session authority. Native hooks report an unavailable routed boundary through a best-effort fixed one-byte runtime datagram; hooks never open SQLite, and daemon shutdown drains delivered markers and queued telemetry before returning.

`agent-ide telemetry query --database <local.sqlite> [--tag <closed-tag>] [--cursor <next_cursor>]` emits a deterministic page of no more than 1,000 rows. `agent-ide telemetry export --database <local.sqlite> [--tag <closed-tag>]` writes canonical rows until its 4 MiB budget and reports `truncated` plus the first omitted sequence on stderr. Both commands accept only the four closed tags, use a read-only Application Store API, never accept SQL, and never create or migrate a database. Because a read-only process cannot know another writer's in-memory loss count, it reports `dropped: null` instead of inventing zero.
