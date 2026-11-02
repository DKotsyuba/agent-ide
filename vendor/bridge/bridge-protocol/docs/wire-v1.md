# Universal producer wire v1 — canonical boundary proposal

Contract: `bridge-producer-wire-v1`, revision 3. Provider M-007; consumers M-008 and M-009. This planning artifact fixes proposed names and JSON fields before source implementation. It requires explicit reciprocal agreement and Epic freeze; it does not claim implemented operations or qualification. DOC-015 governs durable semantics; DOC-017 governs optional assertion/lifetime semantics. This artifact supplies their exact DTO/wire boundary. All identifiers/payloads remain product-independent.

## Transport and typed envelope

One UTF-8 NDJSON request and one response per protected Unix stream exchange. Envelope keys are exactly `api_version`, `request_id`, `credential`, `op`, `params`. `api_version` is integer 1. `op` is an operation name below; `params` is its object. No producer identity can be supplied in params. Credential determines identity. Response keys are exactly `api_version`, `request_id`, `success` and exactly one of `result` or `error`. Result object is exactly `op` plus `value`, with the operation-specific value below. A successful result must match the request operation and its immutable request identity. Errors never contain response/body/credential/proof text.

The protocol exports `Request`, `Operation`, `Response`, `OperationResult`, each operation's `*Params` and `*Result`, receipts/status views and the enums named below at crate root. Public `decode_request`, `encode_request`, `decode_response`, `encode_response` and validation methods apply the same bounds; decoding rejects duplicate JSON keys recursively, unknown envelope/DTO fields, incomplete/non-object frames and trailing JSON. The bounded original host metadata object is the only open-map exception. Unknown metadata keys are not caller authority.

Operations: `capabilities`, `register_call`, `publish`, `status`, `close_route`, `consume_assertion`, `retire_delivery_scope`. Last two are separately capability-gated. `host_observations` is false in this baseline and has no operation or implied capability.

## Scalar values, canonicalization and secrets

- Opaque request/operation/event/stream/group/invocation/scope/route/native identifiers: nonempty UTF-8 <=256 bytes, no Unicode controls; exact equality, no trimming or Unicode normalization. Kind is <=64 bytes to match existing core `Kind`.
- Issued route/caller/destination/assertion keys are opaque references, not arbitrary caller addresses or permissions. A delivery scope key is producer-owned and never reused.
- `Payload` is <=65536 UTF-8 bytes, permits tab/LF/CR, rejects other control characters. `Secret` and payload/evidence/assertion-containing DTO Debug output are redacted. Secrets are serializable for the protected exchange but never included in errors or ordinary logs.
- `metadata` is a sorted map with <=16 entries. Key <=64 bytes/value <=256 bytes, nonempty/no controls. `bridge.` keys are reserved. Ordinary source bodies, arguments and outputs are not metadata.
- All present timestamp/sequence/revision integers are 1..i64::MAX. Optional absence/null is no explicit expiry. Mode is `message`, `inject`, `auto`; reply policy is `self`, `shared_session`.
- Policy is tagged `type`: `keep_all`, `latest` with `stream` and `revision`, or `batch` with `group`. Default is `{type:"keep_all"}`. Stream/group opaque keys do not have product meaning.
- Destination is tagged `type`: `route` with `route_key`, or `address` with `address` (exactly one form).
- Canonical address has four slash-separated components: host/native-harness/store/session. Native harness spellings are `codex` and `claude-code`. Components encode `%` as `%25` and `/` as `%2F`; strict percent decoding and component validation must match existing core address behavior. V1 normalization re-encodes canonical uppercase escapes. No core crate dependency enters the public implementation.
- Native metadata `Host` enum wire values are `codex` and `claude_code`. This new DTO maps explicitly to legacy `Harness::Codex`/`ClaudeCode`; it changes no existing envelope/hash serialization.
- Registration frame limit <=128KiB, core frame <=512KiB, response <=4MiB, rendered manifest <=96KiB; capabilities may advertise lower grant/effective limits. Oversize refuses rather than truncating.
- Unordered constraint sets are sorted/deduplicated in canonical fingerprint material; duplicate allowed-mode/kind entries in a wire request are invalid. Omitted `allowed_kinds` is a stable inherit-policy sentinel, not a later grant snapshot. First effective constraints are stored separately.

The protocol provides deterministic SHA-256 normalized publication `intent_hash` from typed normalized `PublishParams`. It excludes outer request ID and credentials, includes destination TAG and route key/canonical address, event ID, kind, payload, mode, policy, sorted metadata, absolute expiry and optional delivery scope. Domain separator is `bridge-producer-intent-v1\n` before canonical UTF-8 JSON. `envelope_hash` remains the distinct legacy core-resolved event hash. No historical hash changes. SDK verifies returned intent hash and event/destination binding before releasing producer intent.

## capabilities

`CapabilitiesParams` is an empty object.

`Capabilities` fields:

| Field | Type / meaning |
|---|---|
| `api_version`, `supported_versions` | Selected integer and supported integer list |
| `operations` | Finite operation-name strings |
| `evidence_variants` | `native`, `sealed` strings actually supported |
| `native_contracts` | `NativeContract[]` as below |
| `modes`, `policies`, `reply_policies` | Finite allowed enum lists; policies are `PolicyKind` names |
| `limits` | `Limits` object below |
| `grant` | `ProducerConstraints` object below |
| `features` | `Features` booleans `consume_assertion`, `delivery_scopes`, `host_observations` |
| `retention` | `Retention` booleans `durable_keys`, `closed_tombstones`, `credential_independent_identity` |

`Limits`: `registration_request_bytes`, `core_request_bytes`, `response_bytes`, `payload_bytes`, `metadata_entries`, `rendered_manifest_bytes`, `open_routes`, `assertions` (nonnegative capacities; byte limits positive).

`ProducerConstraints`: `allowed_modes`, `allowed_policies`, `allowed_kinds`, `reply_policies`, `direct_address` boolean, `native_scopes` bounded opaque configuration strings. These facts disclose only this authenticated producer's configured permissions. They do not authorize operations themselves.

`NativeContract`: `host`, `build_label`, `build_digest` (lowercase 64-hex SHA-256), `paths`. `HostPathCapability`: `path` (`direct_mcp` or `nested_inner_mcp`), `supported` boolean, `reason` optional bounded machine reason. Configured support is not a substitute for a qualification receipt. Baseline nested support is false until independent matching inner-call records are qualified; no outer `functions.exec` proof is shared across calls.

The protected hook's native `tool_name` must positively classify an actual supported host MCP invocation. Known outer wrapper, non-MCP or missing/unqualified name refuses `unsupported_host_path`. This is host grammar, not a producer/tool catalog.

## register_call

`RegisterCallParams` fields:

| Field | Type / default |
|---|---|
| `operation_key` | Required opaque producer operation incarnation |
| `evidence` | Optional `CallEvidence`; omitted only for committed exact request recovery |
| `reply_policy` | `ReplyPolicy`, default `self` |
| `allowed_modes` | Optional unique nonempty enum set, default effective `[message]` |
| `allowed_kinds` | Optional unique nonempty kind set or `*`, default inherit-policy request sentinel |
| `max_payload_bytes` | Optional positive <=65536/current limits |
| `route_expires_at_ms` | Optional absolute timestamp |
| `assertion_audience` | Optional configured generic audience string |
| `invocation_key` | Optional trusted-frontend opaque key; required iff assertion audience supplied |

`CallEvidence` is tagged `type`: `native` with `host` and `metadata` original bounded JSON object, or `sealed` with redacted `proof`. The SDK metadata parser produces an unverified candidate only. Kernel peer/host build/store/protected hook join and one authoritative reserve/commit/finalize evidence-use path are provider responsibilities. No public DTO can itself assert verified authority.

`RegistrationResult`: `receipt: RegistrationReceipt`, `evidence_state: CallerEvidenceState`, `assertion: Option<FreshAssertion>`. Evidence-state strings are `verified_at_registration`, `historical`, `fresh_assertion_available`. Historical returns no fresh assertion. Fresh availability requires an actual still-valid separately issued ephemeral assertion, not receipt lookup.

`RegistrationReceipt`: `producer`, `request_id`, `operation_key`, `route_key`, `caller_scope`, `destination_scope`, `reply_policy`, `constraints: RouteConstraints`, `state: RouteState`, `created_at_ms`, `route_expires_at_ms`. Receipt's initial state/constraints/time are immutable. `RouteConstraints`: `allowed_modes`, `allowed_kinds`, `max_payload_bytes`, `reply_policy`. `RouteState`: `open`, `closed`, `expired`. Current route state belongs in `RouteStatus`, not a rewritten original receipt.

`FreshAssertion`: redacted `assertion_key`, `invocation_key`, `audience`, `observed_at_ms`, `expires_at_ms`, `issuer_boot`, `endpoint_incarnation`. Native caller/call facts are returned only on authenticated consumption. Assertions expire after the separately configured bounded lifetime (initial audience contract 30 seconds), remain per-boot ephemeral and never reconstruct from durable route facts.

Request and operation fingerprints are distinct: request fixes requested constraints/reply policy/audience/invocation linkage, excluding rotating evidence/credential; operation binding fixes derived producer/caller/destination/route constraints, excluding transient assertion request. Exact committed request recovery returns original route without spending proof twice; supplied differing fresh origin cannot be represented as current. A new request for the same operation requires fresh same-origin evidence. Only a new fresh request may issue a new assertion; closed/expired operations never reopen.

## publish

`PublishParams`: `destination: Destination`, `event_id`, `kind`, `payload: Payload`, `mode` default `message`, `policy` default `keep_all`, `metadata` default empty map, `expires_at_ms` optional, `delivery_scope_key` optional (requires feature). Producer authentication/ownership precedes lookup. Exact accepted v1 intent recovery precedes current new-event route/expiry/grant admission; changed intent conflicts. Legacy-only colliding identity is `legacy_event_identity` rather than v1 adoption.

`PublishResult`: `receipt: EventReceipt`, `replayed: bool`.

`EventReceipt`: `producer`, `event_id`, `intent_hash`, `envelope_hash`, `accepted_at_ms`, `acceptance_seq`, `destination: FrozenDestination`.

`FrozenDestination`: canonical `address`, optional `route_key`, optional `caller_scope`, optional `destination_scope`. Route form requires all route/scope facts and must match requested key; direct address form has null route/caller/destination scopes. Receipt acknowledges durable custody only, not native acceptance, context or business processing. Every route publication authenticates producer; key possession alone grants nothing.

## status

`StatusParams`: `selector: StatusSelector`. Selector tagged `type`: `registration` with `request_id`, `route` with `route_key`, or `event` with `event_id`.

`StatusResult` tagged `type`:

- `registration` with `receipt: Option<RegistrationReceipt>` (historical only, no assertions);
- `route` with `route: Option<RouteStatus>`;
- `event` with `event: Option<EventStatus>`.

Null means no committed visible scoped record at observation, never cancellation/non-commit certainty. Foreign scoped identifiers are not disclosed.

`RouteStatus`: original `receipt`, effective `state`, `closed_at_ms` optional.

`EventStatus`: original `receipt`, `custody_state: CustodyState`, `delivery: DeliveryStatus`. Custody states are `pending`, `superseded`, `expired`; disposition belongs to delivery state separately.

`DeliveryStatus`: `delivery: DeliveryState`, `attempt_state: Option<AttemptState>`, `effective_mode: Option<EffectiveMode>`, `note: Option<String>`, `attempts_made: u64`, `attempt: Option<u64>`, `tiers: Vec<EvidenceTier>`, `incident: Option<String>`, `hold_reason: Option<String>`, `blocked_by: Option<String>`. Note/hold reason <=512 bytes, opaque incident ID<=256, blocked-by event key<=256. Tiers are unique finite observed set; no raw evidence bodies, payload or secrets. None attempt/mode means no attempt, not a synthetic state.

- `EffectiveMode`: `inject`, `message`.
- `DeliveryState`: `unhanded`, `reserved`, `invoking`, `held`, `review`, `accepted`, `hook_submitted`, `refused`, `disposed`.
- `AttemptState`: `reserved`, `invoking`, `native_accepted`, `native_held`, `written_no_receipt`, `hook_submitted`, `known_unsent`, `outcome_unknown`, `permanent_refusal`, `closed`.
- `EvidenceTier`: `native_accepted`, `native_held`, `hook_emitted`, `written_no_receipt`, `context_observed`, `application_processed`, `known_unsent`, `receipt_token_possession`, `refused`, `unknown`.

No tier is inferred from another. Status never resends or recovers delivery automatically.

## close_route

`CloseRouteParams`: `route_key`, optional bounded `reason`<=512 bytes. Outer stable request ID fixes route and reason. `CloseResult`: `receipt: CloseReceipt`, `replayed: bool`. `CloseReceipt`: `producer`, `request_id`, `route_key`, effective `state`, `closed_at_ms` optional. First-close timestamp is immutable; expired route remains expired. Close fences new custody only. Previously accepted events retain delivery policy and accepted exact event replay is still historical custody recovery.

## consume_assertion — optional

`ConsumeAssertionParams`: redacted `assertion_key` and required opaque `invocation_key`. Service matches the forwarded key before spending the assertion; mismatched key refuses `assertion_invocation_mismatch`. Both fields fix consumption intention under the stable request identity. Outer request ID is stable consumption identity; configured authenticated backend audience derives from credential and positively qualified peer. No audience supplied in arguments can authorize itself.

`ConsumeResult`: `receipt: ConsumptionReceipt`, `state: InvocationValidity` (`current` or `historical`). `ConsumptionReceipt`: `producer`, `request_id`, `context: InvocationContext`, `consumed_at_ms`. Context fields: `audience`, `caller_scope`, `destination_scope`, canonical actual `caller_address`, canonical frozen `destination_address`, `role: ActorRole` (`root`/`child`), `native_actor_id`, `native_call_id`, `invocation_key`, `observed_at_ms`, `expires_at_ms`, `issuer_boot`, `endpoint_incarnation`.

Current means original validity is still usable under issuer/endpoint/expiry facts; it is not a promise about domain job idempotency, current sandbox/source permissions or future replay. Exact same consumer/request retry preserves original context/expiry and is explicitly historical once expired/revoked; it never admits a second independent business operation or refreshes authority. Service boot loss has no new assertion reconstruction. Historical receipt recovery, when retained per issuer cache, remains historical. No receipt grants edit or workspace authority.

## retire_delivery_scope — optional

`RetireDeliveryScopeParams`: `scope_key`, `action: RetirementAction` (`preserve` or `retire_unhanded`). Stable outer request ID fixes target/action. `RetireResult`: `receipt: RetireReceipt`, `replayed: bool`. `RetireReceipt`: `producer`, `request_id`, `scope_key`, `action`, `retired_at_ms`, `disposed: u64`, `already_terminal: u64`, `preserved_possible_effect: u64`, `preserved_unhanded: u64`.

Four disjoint counters freeze at first retirement snapshot. Any possible/uncertain historical native effect takes precedence even if custody terminal; otherwise safe terminal is already_terminal; safe pending/reserved becomes disposed under retire_unhanded or preserved_unhanded under preserve. Later evidence may change event status, not replay receipt counts. Same-action later request returns original scope retirement facts; changed action conflicts. Retiring an unknown key creates permanent tombstone; a delayed first publish cannot reopen it.

Retirement fences new custody and (for retire_unhanded) new invoking transitions transactionally, including already reserved manifests. Possible invoking/held/written/unknown/accepted effects are preserved, never recalled or automatically resent. Coalescing scope includes delivery lifetime and may not combine retired/unrelated scope members.

## Error and client outcome contract

`ApiError`: `class: ErrorClass`, `reason: String` (bounded machine name), `retry_hint: RetryHint`, optional `effect: MutationEffect`, optional `reconciliation: ReconciliationKey`, `detail: String` <=512 secret-safe bytes. Details are explanatory static text, never parsed native errors/arguments/payload/proofs. Unknown effects require a reconciliation key. Read-only errors omit mutation effect.

- Classes: `auth`, `forbidden`, `invalid`, `unsupported`, `conflict`, `capacity`, `busy`, `unavailable`, `outcome_unknown`.
- Retry hints: `never`, `same_identity`, `fresh_evidence_same_identity`, `observe_same_identity`.
- Mutation effects: `not_committed`, `committed`, `committed_replay`, `unknown`.
- Reconciliation key tagged `type`: `registration` with `request_id`, `event` with `event_id`, `close` with `request_id`, `consume` with `request_id`, `retire` with `request_id`.

Reasons include `unsupported_host_path`, `needs_new_attestation`, `caller_scope_conflict`, `immutable_operation_conflict`, `intent_conflict`, `legacy_event_identity`, `route_not_accessible`, `route_closed`, `route_expired`, `shared_session_policy_required`, `assertion_expired`, `assertion_consumed`, `assertion_audience_mismatch`, `assertion_invocation_mismatch`, and qualified evidence missing/mixed/ambiguous/stale/duplicate reasons. Reasons are generic service/native semantics, never product handlers.

SDK-local `ClientError` distinguishes validated service `ApiError` from invalid local request/config and transport `not_sent`/`outcome_unknown`/read failure. Once any request bytes may have left, missing/malformed/mismatched response is unknown for a mutation, with the original stable reconciliation key. No identity replacement, direct native fallback or automatic native resend. One absolute finite exchange deadline is shared by connect/write/read and at most one exact safe reconnect; no deadline renewal from partial traffic.

## SDK package boundary and executable checks

`bridge-protocol`: std + existing locked serde/serde_json/sha2; public DTOs, strict bounds/canonicalization/fingerprints only. `bridge-sdk`: std synchronous Unix transport + existing locked socket2 0.6.5 for a bounded nonblocking connect + protocol + serde_json as needed; no core, storage, native adapter, host process inspection, framework/runtime or sibling producer dependency. Root alone provisions manifests/dependencies.

Seven SDK modules: `config`, `native_meta`, `registration`, `publication`, `status`, `transport`, `client`. Facade uses caller-persisted IDs and immutable params; no SDK database/domain outbox/background worker/connection pool. Native metadata extraction accepts the trusted framework object explicitly, never arguments/cwd/environment.

Canonical executable fixture artifact after authorized implementation: `crates/bridge-protocol/tests/wire-v1.json`. It covers every op/result, validation failures, normalization/default stability and immutable response binding. Module boundaries require correct-pass/mutant-fail/restored-pass proof. Independent pinned producer builds must exclude core/storage/native/framework; unknown-producer composition and live host receipts are separate M-010/root gates. Existing bridge-client/legacy API and core hashes remain untouched.

## Shared canonical fingerprint helpers

`registration_request_fingerprint(&RegisterCallParams) -> Result<String, ProtocolError>` is the sole protocol-owned SHA-256 definition shared by native in-flight conflict checks and durable core registration. Domain separator is `bridge-producer-registration-v1\n`; fingerprint fixes operation key, normalized requested mode/kind/size/expiry constraints, reply policy and optional assertion audience/invocation key. Evidence/proof/credential/transport are excluded. Omitted modes normalize to `[message]`; unordered supplied sets sort canonically. Omitted kinds normalize to a typed `{type:"inherit_policy"}` sentinel, distinct from `{type:"explicit",kinds:[...]}`. Optional max-payload absence remains a requested inheritance sentinel/null, independent of the later effective grant snapshot. The private core operation-binding fingerprint additionally uses genuinely verified origin/destination and immutable effective route constraints, excludes transient assertion request and remains core-owned.

`publication_intent_fingerprint(&PublishParams) -> Result<String, ProtocolError>` is the sole protocol-owned normalized intent hash described above; SDK and core use the same helper, never separate lookalike algorithms. Canonical helper input is validated typed data and canonical JSON field/map order, not caller JSON formatting. `ProtocolError` represents pure validation/decoding/encoding failures with bounded static field/reason text and no original credential/body/value leakage; it is distinct from authenticated service `ApiError`.

Counts and capacities (`attempts_made`, all four retirement counters, route/assertion capacities) are 0..i64::MAX and may legitimately be zero. Positive timestamp/sequence/revision bounds do not apply to them. Byte limits remain positive. A missing attempt sequence is null, never numeric zero.
