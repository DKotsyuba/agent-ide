# Universal Bridge producer SDK

This crate is a synchronous client for the standalone v1 contract. It depends on
`bridge-protocol`, serde/serde_json and the already locked socket2 safe Unix
connection API. It has no core, SQLite, native carrier, MCP framework, producer
repository, business outbox, background sender or connection pool dependency.
Legacy `bridge-client` and operator APIs remain separate and unchanged.

## Deployment and identity

Configure a producer credential, permissions and optional backend audience in
Bridge; onboarding a new project changes configuration only. Use explicit
registration and core Unix endpoints inside service-owned private directories.
Do not send credentials/proof/assertion text through model arguments or logs.
`Client::new(Config::new(registration_socket, core_socket, Secret::new(token),
Duration::from_secs(5))?)?` validates configuration without I/O. Credentials may
rotate while the configured producer identity and retained keys remain stable.
Capabilities query the registration endpoint and include authenticated native
host facts. Core handles publish/status/close/retire; adapter handles registration
and optional audience assertions. Unsupported paths/features refuse explicitly.

## Integration sequence

1. At the actual MCP frontend preserve protected `_meta` separately from arguments.
   `extract_native_evidence(host, &original_meta)` yields an **unverified** candidate.
   Only Bridge's qualified peer/build/hook join can verify it. No cwd/environment,
   model address or parser result is authority. Direct and nested paths are separate.
2. Persist a stable registration request ID and opaque operation incarnation before
   `register_call(id, RegisterCallParams { ... })`. Commit the returned route with
   normal local admission. Historical receipt recovery grants no new invocation.
3. If detached admission requires an assertion, request configured audience plus
   opaque forwarded invocation key, then require actual assertion availability.
   Backend `consume_assertion(id, ConsumeAssertionParams { assertion_key,
   invocation_key })` verifies configured audience/peer/linkage. Check original
   expiry, issuer/endpoint fences and current/historical state; local operation
   idempotency/sandbox/source/access authority remain producer-owned.
4. Persist stable event ID and immutable publication intent with your domain outcome.
   `publish(exchange_id, PublishParams { destination: Destination::Route { route_key },
   event_id, kind, payload, mode, policy, metadata, expires_at_ms, delivery_scope_key })`
   returns exact validated custody, not native delivery. Once custody is committed,
   release only that local publication intent and let Bridge own delivery.
5. `status(id, StatusSelector::Event { event_id })` keeps custody/native queue/hook
   emission/context/application evidence separate. No visible record is not
   cancellation. `close_route` fences new events, preserving accepted effects.
   Optional `retire_delivery_scope` fences producer lifetime; possible native
   effects cannot be recalled or automatically replayed.

SDK config is immutable/shareable; each exchange owns a bounded Unix stream. One
absolute deadline covers connect/write/read and optional one identical reconnect.
No partial traffic renews it. A retry preserves request/event IDs and bytes. Lost,
malformed or mismatched acknowledgement after any possible write is unknown;
later failed connects or retry-only preflight rejections do not erase earlier uncertainty. Only an authoritative same-identity committed result can resolve that uncertainty in this client. `ClientError` exposes
structured service effect/hint/reconciliation without logging response bodies.

`examples/producer.rs` is a tiny generic MCP stdin/stdout onboarding probe. Its
tool name, operation/event/kind and payload are runtime inputs; Bridge/SDK contain
no producer catalog. It does not provide a production business outbox. Compile
it with `cargo build -p bridge-sdk --example producer --offline --locked`.
The same source can build against pinned unpacked protocol/SDK artifacts outside
this repository; root integration qualification records those independent pins.

## Packaging and compatibility

Pin BOTH protocol and SDK version plus package checksums, then build the standalone
consumer offline against those artifacts and locked third-party dependencies.
Workspace-relative source checkout paths are development wiring only, not the
integration contract. The source manifests currently inherit workspace metadata;
packaging must materialize standalone manifests and versioned protocol dependency.
API v1, legacy envelope schema and database schema are distinct. Unsupported
versions refuse before mutation; optional capabilities must be checked rather than
inferred from DTO availability. Existing consumer migrations are outside this change.

Local fixture checks prove DTO/SDK boundaries, not live native qualification or
unknown-producer composition. Those separately require frozen common hashes,
real trusted MCP metadata/hooks, raw-protocol comparison and independent arrival.
