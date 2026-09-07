# Application v0.1 contract

Revision: v0.1-r1. Provider: Application. Direct consumers: Assistance (private daemon IPC), Workspace (durable transaction mechanics), and Execution (effective bounded runtime/store settings when it consumes them). Vocabulary: [common](common.md).

## Scope and ownership

Application currently provides one Rust binary/library with a private Unix health daemon, immutable in-memory configuration, and one bundled-SQLite owner thread. It owns local endpoint/lock permissions, daemon generation, bounded IPC serving, the configuration merge, SQLite connection setup, and `application_operation_receipts`.

It does not authenticate actors, grant Workspace authority, read Git or source, start or supervise Git/provider processes, choose LSP profiles, interpret diffs, expose MCP tools, render feedback, or own domain schemas and transitions. Assistance owns host proof and MCP presentation; Workspace owns authority and its persistent meanings; Execution owns physical process admission and supervision.

## Private IPC and executable modes

[`core-ipc.md`](core-ipc.md) is the complete wire contract. It owns the v1 health frame, request and response shape, frame limit, endpoint ownership, peer-UID check, stale-socket recovery, and the exact `daemon`/`doctor` CLI modes. This contract does not restate that wire format.

`RuntimeDir::prepare_for_daemon` creates or validates one final private real directory. `run_daemon` holds its nonblocking lock, binds one private Unix socket, generates a fresh daemon generation, and serves only health. It does not start Workspace, Execution, Intelligence, or Assistance activity. `doctor` receives a plain path, never creates a directory or starts a daemon, and returns `Healthy { daemon_generation }` or `Unavailable`.

UID equality is only a local-user transport boundary; it is not actor proof or authority. Assistance must validate any host attachment before it asks a domain to activate. A stale lock pathname, PID, socket closure, or successful local connection alone never establishes host binding.

## Immutable configuration

`effective_config(generation, layers)` returns one immutable `EffectiveConfig` with per-key `ConfigProvenance`, or a validation error. The only current keys are:

- private IPC connection deadline and concurrent-connection cap;
- SQLite submission queue capacity, busy timeout, caller request deadline, and durable receipt capacity.

Layers must be strictly ordered from `Host` through `User`, `Project`, and `Session`. Every implemented setting is a named ceiling: a lower-authority layer may narrow it but cannot expand an earlier value. Zero limits and invalid ordering are rejected. `EffectiveConfig::defaults()` is generation one and uses no external file.

There is no configuration-file parser, unknown-key handling, live reload, drain protocol, provider/toolchain compatibility group, or generic configuration framework in v0.1-r1. Consumers build a new generation and restart the affected Application component explicitly. `Store::open` independently rejects zero direct `StoreConfig` limits, so callers cannot bypass this validation.

## SQLite transaction mechanics

`Store::open(database_path, config)` starts one dedicated owner thread with one bundled SQLite connection, WAL journal mode, foreign keys, bounded submission channel, and configured busy timeout. It creates only the Application mechanics table `application_operation_receipts`; it creates no domain table.

`Store::execute(operation, sql)` admits one valid opaque `OperationId` and calls `sql` with an Application-owned `rusqlite::Transaction`. The closure may perform domain SQL and return a typed value, but may not manually settle the top-level transaction. Application writes `started` and `committed` in the same transaction; it returns the typed value only after that commit succeeds. A domain SQL error rolls back and returns `RolledBack`. SQLite busy returns `Busy` without running domain SQL.

An accepted operation ID has a durable receipt. A duplicate ID returns `DuplicateOperation { existing }` and never calls the supplied closure. `Store::outcome(operation)` performs reconciliation without replaying SQL. Queued/started receipts found on store restart become `OutcomeUnknown`; missing or corrupt receipt data is also unknown. Caller timeout after acceptance likewise returns `OutcomeUnknown` and does not authorize a retry. Receipt capacity is a hard admission cap: no receipt is evicted, including terminal entries.

Application's transaction covers only SQLite. It does not make Git, filesystem, subprocess, host-delivery, or external effects atomic. Domain modules own their SQL tables, domain receipts, semantic transitions, and any decision based on this mechanics result.

## Current checks and consumer obligations

`app_ipc_contract` uses real Unix sockets and daemon processes for health correlation, private modes, invalid/truncated/oversized frames, stale recovery, lock contention, and doctor non-autostart. `app_config_contract` checks ordered ceiling merging and provenance. `app_store_contract` checks commit-before-value delivery, rollback, duplicate suppression, restart unknown, hard receipt capacity, and direct store-limit validation.

Assistance may depend on the health transport only after its own accepted host-binding proof; those tests do not prove D01. Workspace may use `Store` only with its own stable operation IDs and reconcile `OutcomeUnknown` rather than replaying effects. Execution may consume the current explicit limits and store mechanics for its own durable receipts, but no Application API admits or launches its processes. The tests prove local IPC/config/SQLite mechanics only and do not prove D03 sandbox propagation, provider isolation, host fail-open behavior, or a complete IDE.

## Explicit unresolved v0.1 boundaries

No cache-directory or cache-retirement API exists yet. Its contract needs Workspace's canonical worktree-incarnation and verified closure/reset signal plus Intelligence's provider-compatibility and semantic-validity decision. Application must not infer closure from a missing or moved path, or claim provider-cache reuse.

No versioned migration allocator, schema-compatibility gate, backup/restore flow, or upgrade rollback contract exists yet. A domain's first required migration must negotiate that interface with Application; no future tables or migration filenames are reserved now.

The current doctor checks only the private health endpoint. Packaging, install/remove behavior, richer capability/schema reporting, actual host integration, and product assembly remain separate unimplemented work.
