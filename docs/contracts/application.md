# Application v0.1 contract

Revision: v0.1-r3. Provider: Application. Direct consumers: Assistance (private daemon IPC), Workspace (durable transaction and migration mechanics), and Execution (effective bounded runtime/store settings when it consumes them). Vocabulary: [common](common.md).

## Scope and ownership

Application currently provides one Rust binary/library with a private Unix health daemon, immutable in-memory configuration, and one bundled-SQLite owner thread. It owns local endpoint/lock permissions, daemon generation, bounded IPC serving, the configuration merge, SQLite connection setup, and `application_operation_receipts`.

It does not authenticate actors, grant Workspace authority, read Git or source, start or supervise Git/provider processes, choose LSP profiles, interpret diffs, expose MCP tools, render feedback, or own domain schemas and transitions. Assistance owns host proof and MCP presentation; Workspace owns authority and its persistent meanings; Execution owns physical process admission and supervision.

## Private IPC and executable modes

[`core-ipc.md`](core-ipc.md) is the complete wire contract. It owns the unchanged v1 health frame and the finite v2 Assistance ingress, including frame and field limits, endpoint ownership, peer-UID check, stale-socket recovery, and the exact `daemon`/`doctor` CLI modes. This contract does not restate that wire format.

`RuntimeDir::prepare_for_daemon` creates or validates one final private real directory. `run_daemon` holds its nonblocking lock, binds one private Unix socket, generates a fresh daemon generation, and serves only health. It does not start Workspace, Execution, Intelligence, or Assistance activity. On Unix SIGINT or SIGTERM, both daemon modes stop accepting, cancel active connection tasks, invoke bounded Assistance shutdown when present, and remove their owned endpoint before returning. SIGKILL cannot run cleanup and carries no cleanup claim. `doctor` receives a plain path, never creates a directory or starts a daemon, and returns `Healthy { daemon_generation }` or `Unavailable`.

UID equality is only a local-user transport boundary; it is not actor proof or authority. Assistance must validate any host attachment before it asks a domain to activate. A stale lock pathname, PID, socket closure, or successful local connection alone never establishes host binding.

The accepted r2 addition exposes exactly `assistance.hook_submit` and `assistance.method_dispatch`; the latter has the closed v0.1 tool enum `start | context | diff | inspect | stop`. Application dispatches one correlated request to Assistance and returns one correlated opaque reply. It has no generic event bus, pub/sub, retained hook queue, host identity, authority, rendering, or method semantics. `submit_hook_if_running` is connect-only and returns bounded `Unavailable` on every transport fault; it never starts/retries a daemon and its caller must fail open.

The proposed v0.2 additions are limited to the mechanics named by [Core IPC](core-ipc.md), [TELEMETRY-r1](telemetry-v0.2.md), and [EDIT-r1](changes-v0.2.md). Application offers a bounded trusted multi-row read to Telemetry and transaction/receipt persistence to Changes; it does not parse telemetry rows, edit requests, paths, source references, or outcomes. The v0.2 configuration additions are restart-only and do not introduce live reload.

The operator `init` command bootstraps the per-user tree in one shot: it creates the effective home's `.agent-ide` directory with mode `0700` (refusing a non-directory) and, when the launcher configuration (the effective home's `.config/agent-ide/launcher.json`, overridable with `--config`) is absent, writes the minimal valid version-one template with mode `0600` — documented default limits, the given `--allowed-root`s (else `~/projects` when it exists, else the home) as `allowed_roots`, and one rebindable single target carrying the measured platform `git` — so `agent-ide launcher check` accepts the result immediately. Existing files are never modified and a symlinked config path is refused with exit 2 and one reason line; success prints one `{"home", "config", "created"}` JSON line.

The `doctor` command with no arguments is the installation counterpart of the daemon doctor (read-only except for retiring the product's own abandoned runtime directories, below): it prints one JSON report `{version, home, checked_at, findings}` where each finding is `{code, severity, component, detail}`, capped at 256 findings, exiting 0 when no finding has severity `error` and 2 otherwise. It reuses the daemon's own launcher read-and-verify for the configuration, probes each declared toolchain with a 5-second `--version`, checks the standalone `current` release pinning (with `COMPLETE` and this binary's version) and the launcher shim, the host plugin manifest version (mismatch warns: restart agent-run after install), Codex/Claude host wiring, this user's product runtime directories (exact names `ai-` or `ai-r-` plus sixteen lowercase hex digits, owned by this user, private, below the temp root or `/private/tmp`): a live daemon is listed with its version, and a directory older than a day with no listening daemon is removed — only while doctor holds its exclusive `agent-ide.lock`, with the directory's device, inode, owner, mode and age checked again first; a held lock (a daemon still starting) or an unsafe lock file keeps it — and reported as `stale_runtime_pruned` (`stale_runtime` when removal fails) — no other temporary entry, including the `ai-k-` key caches, is counted, probed or removed — and the last day's error-journal volume; `doctor --runtime-dir <dir>` keeps the daemon form unchanged.

Both connect-only clients compute one absolute deadline at call entry. Connecting and the
subsequent frame exchange consume that same `HookTransportLimits.deadline` budget; a slow
connection never grants a second full exchange interval. The fake-socket regression uses
Tokio's test-only paused clock to spend 60ms before connect completes and 50ms in a silent
exchange under a 100ms total budget, for both hook and method requests.

The r2 Rust surface is `HookTransportLimits { max_frame_bytes, max_observation_bytes, deadline }`, `HookSubmit { request_id, correlation_id, opaque_attachment, sanitized_observation_json }`, `AssistanceMethod::HookSubmit`, `AssistanceDispatch`, `AssistanceDispatchReply`, `AssistanceDispatcher::{dispatch, shutdown}`, `submit_hook_if_running(runtime_dir, request, limits)`, and `run_daemon_with_assistance(runtime_dir, dispatcher, config)`. The shutdown default is an immediate no-op for dispatchers with no owned resources. `AssistanceMethod` has no extensible string variant. `OpaqueAttachmentRef`, request/correlation IDs, and opaque JSON results are bounded transport values; they are not authority assertions or Application-owned semantic types.

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

`Store::read_one(static_select, parameters, decode)` reads at most one domain row on the same bounded owner queue without allocating an operation receipt. The statement must start with `SELECT` and SQLite must classify it as read-only before stepping. The caller owns the trusted query and row semantics. Missing rows return `None`; queue/decode/timeout failures remain explicit. This supports exact Workspace receipt reconciliation and currentness checks without a second connection or unbounded mechanics receipts.

Application's transaction covers only SQLite. It does not make Git, filesystem, subprocess, host-delivery, or external effects atomic. Domain modules own their SQL tables, domain receipts, semantic transitions, and any decision based on this mechanics result.

## Current checks and consumer obligations

`app_ipc_contract` uses real Unix sockets and daemon processes for health correlation, private modes, invalid/truncated/oversized frames, stale recovery, lock contention, and doctor non-autostart. `app_config_contract` checks ordered ceiling merging and provenance. `app_store_contract` checks commit-before-value delivery, rollback, duplicate suppression, restart unknown, hard receipt capacity, and direct store-limit validation.

Assistance may depend on the health transport only after its own accepted host-binding proof; those tests do not prove D01. Workspace may use `Store` only with its own stable operation IDs and reconcile `OutcomeUnknown` rather than replaying effects. Execution may consume the current explicit limits and store mechanics for its own durable receipts, but no Application API admits or launches its processes. The tests prove local IPC/config/SQLite mechanics only and do not prove D03 sandbox propagation, provider isolation, host fail-open behavior, or a complete IDE.

## Domain migration admission

The accepted r2 migration API is `Store::open_with_backup_root(database_path, backup_root, config)`, `Store::admit_migration(DomainMigration) -> MigrationAdmission`, and `Store::migration_admission(domain, key) -> MigrationAdmission`. `DomainMigration` contains a validated `DomainName`, stable `MigrationKey`, trusted up SQL, and expected immutable digest. Application computes the digest, serializes admission on its one owner thread, and allocates the next monotonically increasing version within that domain.

`MigrationAdmission` is exactly `Applied { version, digest, backup }`, `AlreadyApplied { version, digest }`, `Incompatible { version, existing_digest }`, `OutcomeUnknown { key }`, or `BackupUnavailable`. `backup` is `Option<BackupRef>`: it is `None` only for a fresh initialization and otherwise names the fsynced owned SQLite backup. No successful or failed migration result is inferred from a missing receipt.

The immutable `(domain,key)` mapping rejects changed digest as `Incompatible`; a matching completed digest returns `AlreadyApplied` without executing SQL. `Applied` returns allocated version, digest, and an optional durable backup reference. Commit ambiguity returns `OutcomeUnknown` and never permits blind replay. Workspace resolves that state only through `migration_admission(domain, key)`: an applied ledger row returns `AlreadyApplied`; missing or corrupt ledger evidence remains `OutcomeUnknown`. Trusted up SQL and its applied migration ledger row (domain, key, allocated version, digest, and backup reference) commit together in one SQLite transaction. Application never assigns a version or reserves a future migration outside an accepted admission.

Before upgrading a nonfresh database, Application makes and fsyncs a SQLite backup inside its owned `backup_root`, then persists the `BackupRef` with the migration result. A Store without a safe writable backup root, or any backup failure, returns `BackupUnavailable` without executing migration SQL. Fresh means no non-Application schema object and no migration-ledger entry for the requested domain. `application_operation_receipts` and Application's own migration ledger do not make a database nonfresh. Unknown legacy schema is nonfresh, never assumed fresh. Workspace supplies trusted migration content; Application does not decide Workspace closure, cache policy, or domain semantics.

## Explicit unresolved v0.1 boundaries

`cache::CacheRoot` creates private `0700` roots and bounded opaque ASCII namespace components. `retain` only creates or reopens that namespace, preserving its contents. `CacheNamespace::retire` consumes a `VerifiedCacheRetirement` token whose single field is private and whose constructor is crate-internal, so retirement authority cannot be forged by naming a reason; Application is told nothing about which lifecycle fact was verified. The peer that mints it must have matched Workspace's own `VerifiedWorktreeClosure` against the exact canonical worktree incarnation owning the namespace, after admission revocation and provider quiescence. There is no reset spelling: an unsupported reset path stays unavailable rather than accepting a caller-chosen enum. Stop, handoff, actor exit, a missing or moved path, daemon failure, and provider incompatibility never retire a namespace, and a failed deletion retains the handle for an identical retry. Application does not derive canonical worktree identity, closure, cache compatibility, provider policy, or semantic validity.

`agent-ide doctor --runtime-dir PATH` reports default effective configuration, runtime/endpoint/lock observations, health-v1 and Assistance-v2 compatibility, and explicit unsupported controls. It never creates runtime paths, autostarts a daemon, scans a workspace, opens an LSP, or starts a peer dispatcher. Unsafe/non-directory runtime paths and non-socket endpoints report unavailable before any Unix socket connection attempt. Assistance-v2 remains unavailable without a supplied peer dispatcher; the standalone binary does not fabricate it. Packaging, install/remove behavior, actual host integration, and product assembly remain separate unimplemented work.
