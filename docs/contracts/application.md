# Local application infrastructure

Revision: r2. Provider: Application. Consumers: Workspace, Execution, Intelligence, Changes, Assistance. Inputs are each domain's configuration/persistence/lifecycle requirements. Vocabulary: [common](common.md).

## Runtime and composition

One Rust library and binary offer daemon, MCP frontend, CLI and hook modes. Assistance owns rmcp/tool/host behavior; Application owns executable wiring, local connection lifecycle and root assembly. An idle/off frontend must not cause domain activation. One private user runtime endpoint/lock identifies a daemon incarnation; stale discovery entries are not proof of a live owned process. Connecting to a compatible daemon does not start a new LSP. Daemon restart creates new connection generation and requires Workspace/Assistance rebinding before active operations.

Application owns runtime-directory/socket permissions, Unix peer-user checks and IPC version negotiation. Execution owns process/cgroup adapters; Workspace owns Git/watch adapters; Assistance owns host-specific identity and delivery. No generic bucket of all OS adapters is assigned here. Start with one crate; do not create a service framework or one crate per folder.

## IPC

Bounded local Unix framing carries protocol version, message type, request ID, deadline, verified connection binding and typed payload. Reject incompatible version, oversized/truncated frame, duplicate-invalid request, unknown required field or unauthorized connection before domain effects. No public TCP fallback. Opaque bound attachment references must be checked through Assistance; same UID alone does not grant another actor's authority. Cancel and disconnect are protocol events, not proof of domain effect rollback or logical stop.

Public MCP text travels through Assistance renderer; internal IPC uses typed serialized records. Success/error envelopes carry common freshness/effects semantics without exposing secret proof material. IPC compatibility is independent of database schema and provider-profile revisions.

## Effective configuration

`load/reload` takes defaults and applicable host/user/project/session layers; produces one immutable EffectiveConfig generation with value provenance or an error retaining the prior configuration. Reject unknown keys, invalid types/units/ranges, contradictory limits and forbidden scope overrides. Each key has explicit merge/authority/reload rules: ceiling, lower-bound, replace, intersection or other justified operation. There is no universal min or generic append of unsafe commands/read roots.

Host ceilings govern project/session requests. Settings classify immediate presentation reload, drain-only limits and semantic compatibility changes. A lower limit prevents new excess admission; incompatible semantic/toolchain changes create a new Intelligence compatibility group. Parser publication alone does not claim every domain has finished a drain; ReloadReport records effective generation and transition state.

Named configuration groups must cover runtime worker/blocking threads; LSP processes/views/starts/warmups; memory/headroom/CPU; short/heavy/owner queues and fairness; request/start/check deadlines; watcher handles/paths/debounce/reconcile/scan budget; lifecycle leases/heartbeat/retention/stop grace; retry/TTL; edit size/journal/recovery; hook budgets and stop nudges; render items/source/characters/token estimates; cache/store/log/event retention; IPC/LSP frame/header limits; DB queue/busy timeout; delivery concurrency/backoff; child output; per-check build parallelism. Units and constraints are schema metadata. Defaults/presets are explicit reviewed values, not magic numbers spread through handlers. Protocol constants are not operational tuning.

## SQLite infrastructure

One long-lived owner thread holds the SQLite connection; bounded submissions and replies connect it to Tokio. Domain operations own their SQL/transactions and versioned tables; Application owns connection initialization, migration ordering/allocation, schema compatibility and backup/restore mechanics. A submitted synchronous transaction cannot await peers or perform network/process/filesystem effects while holding the DB lock.

Store submission distinguishes queued, started, committed/rolled_back, busy and outcome_unknown. A caller timeout after a transaction started does not prove rollback. Stable operation IDs and domain receipts allow lookup/reconciliation without blind repeat. Cancellation cannot accidentally interrupt the next transaction on the shared connection. A transaction covers only its SQL; it does not atomically combine Git, file rename, spawn or external message delivery.

Use bundled SQLite with a declared durability/journal policy and bounded busy handling. Each domain receives allocated migration filenames from the Application owner; peers do not race a shared registry. Migration failure or incompatible older binary refuses active service and preserves recoverable data; it does not silently erase/recreate the DB. Upgrade backup and rollback compatibility have real tests.

## Preparation and distribution

Application alone owns Cargo.toml/Cargo.lock/rust-toolchain.toml, src/lib.rs and main entry registrations, shared small test support, CI and package assembly. Domain-specific types/fixtures stay domain-owned; shared vocabulary constructors and module exports are coordinated in one preparation commit before dependent coding. Exact dependency versions/MSRV/features are validated and pinned during preparation. No source/manifest exists yet; these are planned deliverables.

Local doctor reports effective configuration/capabilities, endpoint/schema health and unsupported controls without starting workspace analysis or running checks. Installation/removal changes only declared artifacts and preserves unrelated host configuration. Default shell completions/GUI/network update service are not required. Integration entrypoints use ordinary cargo integration-test files (not unregistered nested files).

Tests: app_config_contract, app_store_contract, app_ipc_contract, app_lifecycle_e2e, app_upgrade_e2e, daemon_assembly and acceptance_e2e. Config and IPC examples are fixed fixtures. SQLite restart, real Unix endpoint permissions, supported host invocations, actual language providers and platform package smoke tests need real dependencies. Application owns assembly checks; domain owners supply their evidence and root verifies combined product acceptance. A green substitute suite does not complete a module or this product.

## Frontend survival and owned cache infrastructure

The lightweight executable frontend owns connect/start/response/hook deadlines independent of daemon, LSP, SQLite and domain waiters. Assistance's static registry and a minimal degraded response work without daemon connection. A hanging accepted socket, failed startup, corrupt database or crashed domain cannot hold native tools/host turn completion beyond the supported configured host/frontend budget. Hook exit is permissive on IDE failure. If the frontend process itself dies, the host must surface bounded tool failure under its tested MCP timeout configuration; this is a host integration gate, not an unverified guarantee of rmcp.

Application owns daemon discovery/connect/start retries and breaker only. It propagates one total attempt/deadline budget, never reissues an operation that may be accepted, and leaves provider/channel retries to their designated owners. Open-circuit calls receive fast degraded outcomes and deduplicated notices. Fault injection must prove native continuity and honest unavailable/effect_unknown verification.

Application owns bounded cache directories, provenance envelopes, owned-artifact integrity/quarantine and quota/eviction mechanics under `src/app/cache/`; Intelligence defines semantic/native-cache validity and reuse. Actor/session termination does not delete compatible workspace/provider caches. Restore discovery after compatible daemon restart does not activate analysis. Only inactive owned entries can be evicted under policy; borrowed or live opaque provider caches are never modified generically. Corrupt metadata/native validation failure is explicit unavailable/cold, never successful analysis. All deadlines, retry/cooldown, hot-retention, disk quota and eviction values are typed named configuration.

Additional ordinary targets: app_frontend_fail_open and app_cache_lifecycle. Use actual hanging Unix test endpoints/processes and isolated owned cache fixtures for wrapper/SQLite/cache behavior; Assistance owns actual supported-host continuation proof. Static-fixture success cannot establish native-tool continuity or actual persisted LSP index restore.

Owned persistent cache namespaces are keyed by canonical worktree incarnation and provider compatibility; their default lifetime is the worktree's. Do not apply session TTL/stop cleanup to them. Hot-process retention TTL and per-request artifacts/log retention are separate settings. Retire a namespace only on verified worktree deletion or explicit reset; transient unavailable/moved paths require reconciliation. Quota handling may discard obsolete/invalid entries or deny growth, but not silently delete valid worktree cache. Cache cleanup is ownership-checked and never follows an untrusted path outside the owned cache root.
