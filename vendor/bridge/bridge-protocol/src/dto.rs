//! Exact additive universal request/result DTOs; no producer/domain types.

use crate::{Host, Metadata, Payload, Secret};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Declares documented strict DTOs; fields carry their own semantic rustdoc.
macro_rules! dto {
    ($name:ident, $doc:literal, {$($field:ident:$ty:ty => $fd:literal),* $(,)?}) => {
        #[doc=$doc]
        #[derive(Debug,Clone,PartialEq,Serialize,Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(#[doc=$fd] pub $field:$ty,)* }
    };
}
/// Declares finite lowercase wire enums with attached type/member documentation.
macro_rules! wire_enum {
    ($name:ident, $doc:literal, {$($variant:ident => $vd:literal),* $(,)?}) => {
        #[doc=$doc]
        #[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
        #[serde(rename_all="snake_case")]
        pub enum $name { $(#[doc=$vd] $variant,)* }
    };
}
wire_enum!(Mode,"Requested native delivery selector; auto is not a third carrier.",{Message=>"Separate native conversation message.",Inject=>"Natural hook-context delivery.",Auto=>"Select carrier from positively observed recipient lifecycle."});
impl Default for Mode {
    /// Default publication/route delivery is message.
    fn default() -> Self {
        Self::Message
    }
}
/// Return policy distinguishes actual actor from shared destination; never authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyPolicy {
    /// Independently addressable actual caller.
    #[serde(rename = "self")]
    SelfActor,
    /// Explicitly granted shared-session destination for a native child.
    SharedSession,
}
impl Default for ReplyPolicy {
    /// Default requires an independently addressable caller.
    fn default() -> Self {
        Self::SelfActor
    }
}
wire_enum!(PolicyKind,"Generic coalescing-policy capability names.",{KeepAll=>"Preserve every distinct event.",Latest=>"Replace monotonic snapshots only.",Batch=>"Losslessly package same-group members."});
wire_enum!(RouteState,"Effective route state; no transition reopens a retired identity.",{Open=>"Allows policy-compliant new custody.",Closed=>"New custody fenced; accepted events remain.",Expired=>"Expiry passed; no extension/reopening."});
wire_enum!(ActorRole,"Actual native actor distinction independent of destination equality.",{Root=>"Addressed session's actual context.",Child=>"Distinct child context, possibly sharing return address."});
wire_enum!(CallerEvidenceState,"Historical routing and current observation/assertion remain distinct.",{VerifiedAtRegistration=>"Actual current observation was verified for this registration.",Historical=>"Durable receipt recovered; no fresh authority.",FreshAssertionAvailable=>"A separate live audience assertion was actually issued."});
wire_enum!(InvocationValidity,"Original consumed assertion validity; never renewed on replay.",{Current=>"Original issuer/endpoint/expiry validity still applies.",Historical=>"Retained consumption facts cannot admit fresh work."});
wire_enum!(RetirementAction,"Immutable producer lifetime retirement choice.",{Preserve=>"Fence only new custody.",RetireUnhanded=>"Also fence invoking and dispose only provably unhanded effects."});
wire_enum!(CustodyState,"Retained custody state, independent of native delivery disposition.",{Pending=>"Accepted event retained for ordinary delivery.",Superseded=>"Monotonic policy replaced this custody member.",Expired=>"Event expiry passed."});
wire_enum!(EffectiveMode,"Concrete carrier chosen for one actual attempt.",{Inject=>"Natural hook-context carrier.",Message=>"Native message carrier."});
wire_enum!(DeliveryState,"Finite retained delivery state, never inferred from custody.",{Unhanded=>"No current reservation or native handoff.",Reserved=>"Manifest reserved without native effect.",Invoking=>"Possible-effect transition committed.",Held=>"Native system holds the handoff.",Review=>"Uncertainty requires explicit recovery.",Accepted=>"Native handoff accepted.",HookSubmitted=>"Hook context emitted.",Refused=>"Permanent native refusal.",Disposed=>"Audited/safe disposition; not a delivery claim."});
wire_enum!(AttemptState,"Durable native attempt state distinguishing uncertainty from no write.",{Reserved=>"Reserved but not invoking.",Invoking=>"Possible effect committed before carrier I/O.",NativeAccepted=>"Native queue accepted.",NativeHeld=>"Native nonterminal hold.",WrittenNoReceipt=>"Write occurred without positive receipt.",HookSubmitted=>"Hook payload emitted.",KnownUnsent=>"Positive no-write evidence.",OutcomeUnknown=>"Possible effect; never automatically resend.",PermanentRefusal=>"Definite permanent refusal.",Closed=>"Reservation reclaimed without invocation."});
wire_enum!(EvidenceTier,"Separate positive evidence kinds; none implies model/business processing.",{NativeAccepted=>"Native queue accepted.",NativeHeld=>"Native holds the event.",HookEmitted=>"Hook payload flushed.",WrittenNoReceipt=>"Written without receipt.",ContextObserved=>"Model context independently observed.",ApplicationProcessed=>"Business processing independently evidenced.",KnownUnsent=>"Positively no native write.",ReceiptTokenPossession=>"Recipient token possession only.",Refused=>"Permanent refusal.",Unknown=>"Effect remains uncertain."});
wire_enum!(ErrorClass,"Closed safe service error classes.",{Auth=>"Authentication refused.",Forbidden=>"Authenticated grant does not permit operation.",Invalid=>"Malformed/bounded contract input.",Unsupported=>"Version/feature/host path unavailable.",Conflict=>"Immutable identity conflicts.",Capacity=>"Hard admission bound reached.",Busy=>"Same-identity work in progress.",Unavailable=>"Service/ephemeral evidence unavailable.",OutcomeUnknown=>"Possible unresolved mutation effect."});
wire_enum!(RetryHint,"Safe recovery instruction; never changes an identity.",{Never=>"Do not automatically retry.",SameIdentity=>"Exact immutable retry only.",FreshEvidenceSameIdentity=>"Obtain genuine evidence without replacing identity.",ObserveSameIdentity=>"Observe/reconcile retained identity."});
wire_enum!(MutationEffect,"Truthful durable mutation effect, separate from delivery evidence.",{NotCommitted=>"Definite no committed mutation.",Committed=>"Authoritative durable receipt.",CommittedReplay=>"Original receipt recovered.",Unknown=>"Commit/effect may exist; reconcile same identity."});
wire_enum!(HostPath,"Native host invocation path; no producer names participate.",{DirectMcp=>"Genuine protected native MCP call.",NestedInnerMcp=>"Separately qualified actual inner MCP call."});

/// Generic publication compaction; no product payload interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Policy {
    /// Preserve independently meaningful events.
    KeepAll,
    /// Monotonic producer-owned snapshot stream.
    Latest {
        /// Opaque durable stream identity.
        stream: String,
        /// Positive authoritative revision, not arrival time.
        revision: u64,
    },
    /// Losslessly batch same-group events.
    Batch {
        /// Opaque producer batch identity.
        group: String,
    },
}
impl Default for Policy {
    /// Default never discards or compacts events.
    fn default() -> Self {
        Self::KeepAll
    }
}
/// Immutable tagged destination form participates in v1 intent hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    /// Producer-owned route reference, always authenticated by service.
    Route {
        /// Issuer-generated opaque return route.
        route_key: String,
    },
    /// Explicit grant-authorized canonical native address.
    Address {
        /// Four-component encoded native address.
        address: String,
    },
}
/// Unverified native metadata or sealed evidence candidate; never itself authority.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallEvidence {
    /// Original bounded framework-native metadata candidate.
    Native {
        /// Native host metadata contract.
        host: Host,
        /// Original object separated from tool arguments.
        metadata: Map<String, Value>,
    },
    /// Registry-issued single-use frontend proof candidate.
    Sealed {
        /// Secret-safe opaque proof reference.
        proof: Secret,
    },
}
impl std::fmt::Debug for CallEvidence {
    /// Never prints original metadata, native values or proof text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallEvidence(<redacted>)")
    }
}
/// Explicit registration intentions, excluding caller-authored recipient claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterCallParams {
    /// Producer operation incarnation chosen before work.
    pub operation_key: String,
    /// Fresh candidate or absent for committed exact request recovery only.
    #[serde(default)]
    pub evidence: Option<CallEvidence>,
    /// Explicit return policy; defaults to actual self actor.
    #[serde(default)]
    pub reply_policy: ReplyPolicy,
    /// Optional unique requested modes; default message only.
    #[serde(default)]
    pub allowed_modes: Option<Vec<Mode>>,
    /// Optional requested kind set; absence is stable inherit-policy sentinel.
    #[serde(default)]
    pub allowed_kinds: Option<Vec<String>>,
    /// Optional requested payload ceiling; absence inherits first effective grant.
    #[serde(default)]
    pub max_payload_bytes: Option<u32>,
    /// Absolute route expiry; no default job lifetime.
    #[serde(default)]
    pub route_expires_at_ms: Option<u64>,
    /// Declaratively configured detached audience, paired with invocation key.
    #[serde(default)]
    pub assertion_audience: Option<String>,
    /// Exact opaque frontend-to-backend RPC incarnation linkage.
    #[serde(default)]
    pub invocation_key: Option<String>,
}
dto!(RouteConstraints,"Immutable first effective route limits; current grants still apply to new events.",{allowed_modes:Vec<Mode> =>"Unique permitted requested delivery modes.",allowed_kinds:Vec<String> =>"Bounded generic permitted kinds/wildcard.",max_payload_bytes:u32=>"Positive effective UTF-8 payload byte ceiling.",reply_policy:ReplyPolicy=>"Frozen return policy."});
dto!(RegistrationReceipt,"Durable immutable route association; never fresh invocation authority.",{producer:String=>"Authenticated configured producer identity.",request_id:String=>"Original committed registration identity.",operation_key:String=>"Immutable producer operation incarnation.",route_key:String=>"Opaque issuer return route reference.",caller_scope:String=>"Stable actual actor scope, distinct from destination.",destination_scope:String=>"Stable frozen return destination scope.",reply_policy:ReplyPolicy=>"Original return policy.",constraints:RouteConstraints=>"First effective immutable constraints.",state:RouteState=>"Original route state; current state lives in RouteStatus.",created_at_ms:u64=>"Original durable creation Unix milliseconds.",route_expires_at_ms:Option<u64> =>"Absolute expiry, absent for no imposed job lifetime."});
dto!(FreshAssertion,"Separate short-lived audience assertion; Debug redacts its secret key.",{assertion_key:Secret=>"Protected issuer assertion reference.",invocation_key:String=>"Frozen frontend RPC incarnation key.",audience:String=>"Configured authenticated detached backend audience.",observed_at_ms:u64=>"Actual protected source observation time.",expires_at_ms:u64=>"Original bounded expiry, never renewed.",issuer_boot:String=>"Opaque per-boot issuer fence.",endpoint_incarnation:String=>"Opaque source process/endpoint revocation fence."});
dto!(RegistrationResult,"Registration receipt plus explicit actual evidence/cache freshness.",{receipt:RegistrationReceipt=>"Immutable durable receipt, never permission.",evidence_state:CallerEvidenceState=>"Historical/current/actually available assertion distinction.",assertion:Option<FreshAssertion> =>"Present only for actual current separately issued assertion."});
/// Immutable publication intent; no payload schema or producer-specific fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishParams {
    /// Tagged route or grant-authorized direct native address.
    pub destination: Destination,
    /// Stable producer event identity across lost acknowledgements.
    pub event_id: String,
    /// Opaque bounded granted event kind.
    pub kind: String,
    /// Opaque content; formatting redacts it.
    pub payload: Payload,
    /// Requested mode; defaults to message.
    #[serde(default)]
    pub mode: Mode,
    /// Generic compaction policy; defaults keep_all.
    #[serde(default)]
    pub policy: Policy,
    /// Deterministically ordered bounded metadata.
    #[serde(default)]
    pub metadata: Metadata,
    /// Absolute event expiry, separate from route expiry.
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    /// Optional producer lifetime key; requires advertised delivery_scopes.
    #[serde(default)]
    pub delivery_scope_key: Option<String>,
}
dto!(FrozenDestination,"Immutable resolved destination facts acknowledged by custody.",{address:String=>"Canonical actual native return address.",route_key:Option<String> =>"Original route key or absent for direct address.",caller_scope:Option<String> =>"Actual actor scope for route form only.",destination_scope:Option<String> =>"Frozen destination scope for route form only."});
dto!(EventReceipt,"Durable custody only, not native acceptance or processing.",{producer:String=>"Authenticated configured producer.",event_id:String=>"Stable accepted event identity.",intent_hash:String=>"Lowercase SHA-256 normalized v1 request intent.",envelope_hash:String=>"Distinct resolved legacy core envelope hash.",accepted_at_ms:u64=>"Original custody commit Unix milliseconds.",acceptance_seq:u64=>"Original positive durable acceptance order.",destination:FrozenDestination=>"Frozen resolved destination facts."});
dto!(PublishResult,"Exact accepted publication result.",{receipt:EventReceipt=>"Original custody receipt.",replayed:bool=>"True when recovering an already accepted immutable intent."});
/// Exactly one authenticated status selector; scope keys grant no access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusSelector {
    /// Historical registration request, without fresh assertion.
    Registration {
        /// Exact original registration request ID.
        request_id: String,
    },
    /// Immutable route plus effective lifecycle.
    Route {
        /// Opaque own return route key.
        route_key: String,
    },
    /// Custody and independently retained native delivery facts.
    Event {
        /// Exact own event identity.
        event_id: String,
    },
}
dto!(StatusParams,"One producer-scoped read selector; no recovery/resend side effect.",{selector:StatusSelector=>"Exactly one owned record selector."});
dto!(RouteStatus,"Original route receipt and current effective state remain separate.",{receipt:RegistrationReceipt=>"Unmodified original receipt.",state:RouteState=>"Effective open/closed/expired state at observation.",closed_at_ms:Option<u64> =>"Original close time, absent before close."});
dto!(DeliveryStatus,"Bounded content-free native delivery facts, never inferred processing.",{delivery:DeliveryState=>"Current retained delivery disposition.",attempt_state:Option<AttemptState> =>"Current attempt state, absent before reservation.",effective_mode:Option<EffectiveMode> =>"Actual chosen carrier, absent before attempt.",note:Option<String> =>"Secret-safe generic explanation, <=512 bytes.",attempts_made:u64=>"Nonnegative durable native attempt count.",attempt:Option<u64> =>"Positive current attempt sequence or absent.",tiers:Vec<EvidenceTier> =>"Unique finite positive evidence kinds.",incident:Option<String> =>"Opaque bounded incident reference, no body.",hold_reason:Option<String> =>"Bounded non-secret generic hold reason.",blocked_by:Option<String> =>"Opaque predecessor event key."});
dto!(EventStatus,"Immutable custody with separately observed event/native states.",{receipt:EventReceipt=>"Original acknowledged custody.",custody_state:CustodyState=>"Retained custody state.",delivery:DeliveryStatus=>"Independent native delivery facts."});
/// Producer-scoped status with explicit absent committed record at observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusResult {
    /// Historical receipt only; no assertion reconstructed.
    Registration {
        /// None means no visible committed own request.
        receipt: Option<RegistrationReceipt>,
    },
    /// Effective own route state.
    Route {
        /// None means no accessible committed own route.
        route: Option<RouteStatus>,
    },
    /// Own event custody/delivery evidence.
    Event {
        /// None means no visible committed own event.
        event: Option<EventStatus>,
    },
}
/// Idempotent route-close intention; close does not recall accepted events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseRouteParams {
    /// Opaque own route key to fence against new custody.
    pub route_key: String,
    /// Optional non-secret reason participates in close request identity.
    #[serde(default)]
    pub reason: Option<String>,
}
dto!(CloseReceipt,"Immutable close request/state receipt; expired stays expired.",{producer:String=>"Authenticated owner identity.",request_id:String=>"Stable original close request identity.",route_key:String=>"Own route identity.",state:RouteState=>"Closed or expired effective state.",closed_at_ms:Option<u64> =>"First close time, absent if merely already expired."});
dto!(CloseResult,"Historical or newly committed route-close result.",{receipt:CloseReceipt=>"Original close receipt/state.",replayed:bool=>"True for historical same-intent recovery."});
dto!(ConsumeAssertionParams,"Audience-bound exact forwarded invocation; no caller-authored authority.",{assertion_key:Secret=>"Protected separately issued assertion reference.",invocation_key:String=>"Required forwarded opaque RPC key matched before spend."});
dto!(InvocationContext,"Actual verified native invocation facts, not domain mutation permission.",{audience:String=>"Authenticated configured backend audience.",caller_scope:String=>"Actual actor scope, never inferred from destination.",destination_scope:String=>"Frozen return destination scope.",caller_address:String=>"Canonical actual caller native address.",destination_address:String=>"Canonical frozen return address.",role:ActorRole=>"Actual native root/child distinction.",native_actor_id:String=>"Actual native actor ID.",native_call_id:String=>"Exact protected native per-call ID.",invocation_key:String=>"Immutable frontend RPC linkage.",observed_at_ms:u64=>"Original native observation Unix milliseconds.",expires_at_ms:u64=>"Original assertion expiry, never renewed.",issuer_boot:String=>"Actual issuer boot fence.",endpoint_incarnation:String=>"Actual source incarnation fence."});
dto!(ConsumptionReceipt,"Original assertion consumption identity/context/time.",{producer:String=>"Authenticated producer identity.",request_id:String=>"Original exact consume request identity.",context:InvocationContext=>"Original verified invocation, historical on expiry/revocation.",consumed_at_ms:u64=>"Original consumption Unix milliseconds."});
dto!(ConsumeResult,"Explicit current/historical consumption; exact retry never renews authority.",{receipt:ConsumptionReceipt=>"Original immutable consumption facts.",state:InvocationValidity=>"Original validity, never refreshed by replay."});
dto!(RetireDeliveryScopeParams,"Immutable producer-owned lifetime closure intention.",{scope_key:String=>"Opaque never-reused producer lifetime key.",action:RetirementAction=>"Preserve or safely retire only provably unhanded."});
dto!(RetireReceipt,"First retirement snapshot; four disjoint counts never rewrite on replay.",{producer:String=>"Authenticated scope owner.",request_id:String=>"Original exact retirement request identity.",scope_key:String=>"Permanently closed producer lifetime key.",action:RetirementAction=>"Immutable first retirement action.",retired_at_ms:u64=>"Original durable fence time.",disposed:u64=>"Safe unhanded members disposed.",already_terminal:u64=>"Safe already-terminal members.",preserved_possible_effect:u64=>"Members with any possible/uncertain historical native effect.",preserved_unhanded:u64=>"Safe unhanded members retained by preserve action."});
dto!(RetireResult,"Committed lifetime fence plus original retirement snapshot.",{receipt:RetireReceipt=>"Original permanent scope retirement.",replayed:bool=>"True for historical same-intent result."});
/// Same-identity mutation reconciliation handle, never an instruction to resend native effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconciliationKey {
    /// Exact registration request.
    Registration {
        /// Original stable request ID.
        request_id: String,
    },
    /// Exact accepted/possibly accepted event intent.
    Event {
        /// Producer-stable event ID.
        event_id: String,
    },
    /// Exact close request.
    Close {
        /// Stable close ID.
        request_id: String,
    },
    /// Exact consumption request.
    Consume {
        /// Stable consume ID.
        request_id: String,
    },
    /// Exact scope retirement request.
    Retire {
        /// Stable retire ID.
        request_id: String,
    },
}
dto!(ApiError,"Bounded structured service error; detail never contains request/secret/body.",{class:ErrorClass=>"Closed service failure class.",reason:String=>"Bounded generic machine reason.",retry_hint:RetryHint=>"Safe same-identity recovery instruction.",effect:Option<MutationEffect> =>"Mutation effect, absent for reads.",reconciliation:Option<ReconciliationKey> =>"Required original identity for unknown mutation.",detail:String=>"Bounded static secret-safe explanation."});
dto!(Features,"Optional implemented boundaries, never availability inferred from draft DTOs.",{consume_assertion:bool=>"Qualified configured audience assertions enabled.",delivery_scopes:bool=>"Durable producer lifetime retirement enabled.",host_observations:bool=>"False in this baseline; no observer operations."});
dto!(Retention,"Namespace identity/tombstone retention promises, not native exactly-once.",{durable_keys:bool=>"Accepted identities retained durably.",closed_tombstones:bool=>"Closed identities cannot become new operations.",credential_independent_identity:bool=>"Token rotation preserves configured identity."});
dto!(Limits,"Effective bounded endpoint/grant admission limits.",{registration_request_bytes:u64=>"Positive registration frame byte ceiling.",core_request_bytes:u64=>"Positive core frame byte ceiling.",response_bytes:u64=>"Positive response frame byte ceiling.",payload_bytes:u64=>"Positive UTF-8 payload ceiling.",metadata_entries:u64=>"Nonnegative metadata entry ceiling.",rendered_manifest_bytes:u64=>"Positive rendered native manifest byte ceiling.",open_routes:u64=>"Nonnegative open-route capacity.",assertions:u64=>"Nonnegative assertion-cache capacity."});
dto!(ProducerConstraints,"Only authenticated producer's configured limits; facts are not permissions.",{allowed_modes:Vec<Mode> =>"Configured native modes.",allowed_policies:Vec<PolicyKind> =>"Configured generic coalescing policies.",allowed_kinds:Vec<String> =>"Bounded configured opaque kinds/wildcard.",reply_policies:Vec<ReplyPolicy> =>"Configured self/shared-session policies.",direct_address:bool=>"Grant allows authorized explicit address publication.",native_scopes:Vec<String> =>"Bounded opaque configured native scope expressions."});
dto!(HostPathCapability,"Truthful configured host acceptance, separate from live qualification receipts.",{path:HostPath=>"Actual native direct/inner path contract.",supported:bool=>"Implemented supported matching-evidence path.",reason:Option<String> =>"Bounded unsupported-path reason."});
dto!(NativeContract,"Configured exact native host/build/digest facts, not producer behavior.",{host:Host=>"Supported native metadata contract.",build_label:String=>"Bounded configured build label.",build_digest:String=>"Exact lowercase SHA-256 configured executable digest.",paths:Vec<HostPathCapability> =>"Finite direct/nested path support facts."});
dto!(Capabilities,"Authenticated additive API/host/limit facts; consumes no caller evidence.",{api_version:u16=>"Selected additive API version.",supported_versions:Vec<u16> =>"Finite supported versions.",operations:Vec<String> =>"Finite generic operation names.",evidence_variants:Vec<String> =>"Implemented native/sealed candidate forms.",native_contracts:Vec<NativeContract> =>"Configured host/build/path facts.",modes:Vec<Mode> =>"Implemented permitted modes.",policies:Vec<PolicyKind> =>"Implemented generic coalescing policies.",reply_policies:Vec<ReplyPolicy> =>"Implemented allowed return policies.",limits:Limits=>"Effective bounded endpoint/grant limits.",grant:ProducerConstraints=>"Only this producer's configuration facts.",features:Features=>"Explicit optional capability flags.",retention:Retention=>"Durable identity/tombstone semantics."});
dto!(
    CapabilitiesParams,
    "Empty authenticated capability read; no evidence consumed.",
    {}
);

/// Closed generic operation set; legacy/operator operations never enter it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Operation {
    /// Authenticated capability read.
    Capabilities(CapabilitiesParams),
    /// Sole native-evidence-consuming registration.
    RegisterCall(RegisterCallParams),
    /// Immutable event custody admission/recovery.
    Publish(PublishParams),
    /// Scoped historical state read.
    Status(StatusParams),
    /// Fence new custody against an own route.
    CloseRoute(CloseRouteParams),
    /// Optional qualified detached invocation transfer.
    ConsumeAssertion(ConsumeAssertionParams),
    /// Optional producer lifetime fence.
    RetireDeliveryScope(RetireDeliveryScopeParams),
}
/// Closed typed response result, always checked against the requested operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OperationResult {
    /// Authenticated capability facts.
    Capabilities(Capabilities),
    /// Durable registration and explicit actual freshness.
    RegisterCall(RegistrationResult),
    /// Custody receipt, never inferred delivery.
    Publish(PublishResult),
    /// Historical scoped record observation.
    Status(StatusResult),
    /// Immutable route-close acknowledgement.
    CloseRoute(CloseResult),
    /// Explicit current/historical assertion consumption.
    ConsumeAssertion(ConsumeResult),
    /// Permanent lifetime fence acknowledgement.
    RetireDeliveryScope(RetireResult),
}
