//! Codex-specific transport validation for an MCP invocation and its native hook lifecycle.
//!
//! A parsed candidate is not an authority claim. A matching trusted `PreToolUse` yields a
//! [`ValidatedInvocation`] before MCP result delivery; later `PostToolUse` is settlement
//! evidence. Consumers still decide whether the host transport and execution profile prove
//! the authority they require.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::Deserialize;
use serde_json::{Map, Value};

const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_HOOK_METADATA_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 128;
const MAX_COMPLETED: usize = 1024;
const MAX_BINDINGS: usize = 64;
const MAX_SANDBOX_STATE_BYTES: usize = 64 * 1024;
const TURN_METADATA: &str = "x-codex-turn-metadata";
const SANDBOX_STATE_METADATA: &str = "codex/sandbox-state-meta";
const SANDBOX_STATE_FIELDS: &[&str] = &[
    "permissionProfile",
    "codexLinuxSandboxExe",
    "sandboxCwd",
    "useLegacyLandlock",
];

/// Identifies one candidate invocation extracted only from trusted MCP request metadata.
///
/// `actor_id` originates from `_meta.threadId` and `call_id` from `_meta.callId`; neither
/// field comes from tool arguments. This candidate is transport observation, not authority.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CandidateInvocation {
    actor_id: String,
    call_id: String,
}

impl CandidateInvocation {
    /// Returns the host thread identity claimed by trusted request metadata.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the host tool-call identity claimed by trusted request metadata.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
}

/// Identifies the trusted Application channel-session carrying a hook or MCP invocation.
///
/// Its value is opaque outside Assistance and is accepted only from the private transport
/// attachment after Application has established its endpoint and connection generation.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct ChannelSessionRef(String);

impl fmt::Debug for ChannelSessionRef {
    /// Redacts the opaque private transport attachment from diagnostic output.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChannelSessionRef(..)")
    }
}

/// Parses one bounded opaque Application attachment into a channel-session reference.
///
/// The attachment must be valid UTF-8 and meet the common identifier bound. Its contents are
/// never logged, rendered, or treated as actor identity by this parser.
pub fn parse_channel_session(attachment: &[u8]) -> Result<ChannelSessionRef, BindingUnavailable> {
    let value = std::str::from_utf8(attachment)
        .map_err(|_| BindingUnavailable::InvalidAttachment)?
        .to_owned();
    checked_identifier(value, "opaque attachment")
        .map(ChannelSessionRef)
        .map_err(|_| BindingUnavailable::InvalidAttachment)
}

/// Identifies one revocable Assistance binding generation for an actor and channel-session.
///
/// Consumers may retain and pass this opaque value back to Assistance but cannot construct,
/// inspect, or turn it into Workspace authority, an Execution permit, or host evidence.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BindingRef {
    actor_id: String,
    channel: ChannelSessionRef,
    generation: u64,
}

impl BindingRef {
    /// Returns a stable opaque persistence key without exposing actor/channel fields or minting proof.
    /// The domain and length framing preserve distinct actor, channel, and generation identities.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"assistance-binding-identity-v1");
        for value in [
            self.actor_id.as_bytes(),
            self.channel.0.as_bytes(),
            &self.generation.to_le_bytes(),
        ] {
            hash.update(&(value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        *hash.finalize().as_bytes()
    }
}

/// Represents one successful liveness consume for a particular [`BindingRef`] generation.
///
/// It is a transient, non-authorizing result. Consumers must acquire a fresh value at each
/// scoped admission because a later stop can revoke the underlying binding generation.
#[derive(Debug, Eq, PartialEq)]
pub struct ActiveBindingUse(BindingRef);

impl ActiveBindingUse {
    /// Returns the opaque binding reference whose active state was consumed.
    pub fn binding_ref(&self) -> &BindingRef {
        &self.0
    }
}

/// Identifies the two native Codex hook lifecycle points that can validate a candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookPhase {
    /// The native host is about to execute the selected tool call.
    Pre,
    /// The native host has finished the selected tool call.
    Post,
}

/// Holds the bounded, selected fields extracted from one native Codex hook payload.
///
/// The parser discards tool input, tool response, cwd, transcript paths, and all other hook
/// payload fields before returning this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEvent {
    phase: HookPhase,
    actor_id: String,
    call_id: String,
}

impl HookEvent {
    /// Returns whether this event occurred before or after its native tool execution.
    pub fn phase(&self) -> HookPhase {
        self.phase
    }

    /// Returns the selected hook actor: child `agent_id` or root `session_id`.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the selected hook `tool_use_id`, which must equal MCP `_meta.callId`.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
}

/// Describes why host binding is unavailable without retaining or reporting raw host data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingUnavailable {
    /// The metadata payload was not valid bounded JSON object input.
    InvalidMetadata,
    /// A required supported field was absent.
    MissingField(&'static str),
    /// A supported field had an empty, non-string, or over-limit value.
    InvalidField(&'static str),
    /// The private Application attachment was not valid bounded opaque channel-session data.
    InvalidAttachment,
    /// The hook phase was not the native `PreToolUse` or `PostToolUse` form.
    UnsupportedHookPhase,
    /// A hook could not be linked exactly to one registered MCP candidate.
    Mismatch,
    /// MCP validation arrived before its exact pre-hook, or a post-hook had no invocation.
    MissingPre,
    /// A post-hook arrived after pre-observation but before MCP validation completed.
    MissingInvocation,
    /// An ordinary MCP invocation had no active matching actor/channel-session binding.
    InactiveBinding,
    /// A sandbox-state parser was called without the required advertised capability.
    CapabilityNotAdvertised,
    /// The host did not return the required bounded sandbox-state object.
    MissingSandboxState,
    /// The host returned a sandbox-state object missing a required outer field or over the cap.
    InvalidSandboxState,
    /// A consumed active binding did not belong to the supplied validated invocation.
    BindingUseMismatch,
    /// The candidate was observed twice or after it was already validated.
    Replay,
    /// Bounded pending or completed lifecycle storage is full.
    CapacityExceeded,
}

/// Reports the lifecycle strength available to a Workspace or Execution consumer.
///
/// `PreObserved` is not an authority claim. `Validated` proves the exact native pre-hook was
/// observed before MCP acceptance; `Settled` records its later matching post-hook evidence.
/// Native observations are recheck hints only; no result attests authority or sandbox enforcement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingStatus {
    /// A native pre-hook was retained while awaiting matching trusted MCP metadata.
    PreObserved,
    /// Trusted MCP metadata matched a pre-hook before the handler returned its result.
    Validated(ValidatedInvocation),
    /// The matching native post-hook arrived after a validated MCP invocation.
    Settled(ValidatedInvocation),
    /// A complete native hook lifecycle marks this active binding for registered-path recheck.
    /// It proves neither an MCP invocation nor a source effect or successful native command.
    NativeObserved(BindingRef),
    /// Required host data was absent, malformed, mismatched, replayed, or unavailable.
    Unavailable(BindingUnavailable),
}

/// Captures the exact actor, call, and active binding whose pre-hook matched trusted MCP metadata.
///
/// This value is bounded and contains no model arguments, source, prompt, or hook payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedInvocation {
    actor_id: String,
    call_id: String,
    binding: BindingRef,
}

impl ValidatedInvocation {
    /// Returns the exact actor shared by MCP metadata and its pre-hook event.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the exact call id shared by MCP metadata and its pre-hook event.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Returns the immutable active binding generation that this invocation matched.
    pub fn binding_ref(&self) -> &BindingRef {
        &self.binding
    }
}

/// States why Assistance accepts a sandbox-state observation from trusted request metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxStateProvenance {
    /// The server advertised `codex/sandbox-state-meta` and the request returned that object.
    AdvertisedAndReturned,
}

/// Holds the complete bounded host sandbox-state object without assigning field semantics.
///
/// Execution may parse this JSON under its own versioned policy. Assistance does not interpret
/// it as an operator profile, a permit, sandbox enforcement proof, or authority grant.
#[derive(Clone, PartialEq)]
pub struct OpaqueSandboxState(Value);

impl fmt::Debug for OpaqueSandboxState {
    /// Redacts the complete host state while preserving explicit access for Execution parsing.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueSandboxState(..)")
    }
}

impl OpaqueSandboxState {
    /// Returns the complete host-returned JSON object for Execution's bounded parser.
    pub fn as_json(&self) -> &Value {
        &self.0
    }
}

/// Correlates a complete opaque sandbox-state observation to one validated host invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedSandboxState {
    actor_id: String,
    call_id: String,
    binding: BindingRef,
    provenance: SandboxStateProvenance,
    state: OpaqueSandboxState,
}

impl ObservedSandboxState {
    /// Returns the actor shared with the validated invocation that returned this state.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the call id shared with the validated invocation that returned this state.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Returns the active binding reference required for every Execution admission consume.
    pub fn binding_ref(&self) -> &BindingRef {
        &self.binding
    }

    /// Returns why this otherwise opaque state object was accepted from the host.
    pub fn provenance(&self) -> SandboxStateProvenance {
        self.provenance
    }

    /// Returns the complete bounded opaque host state for Execution policy parsing.
    pub fn state(&self) -> &OpaqueSandboxState {
        &self.state
    }
}

/// Tracks bounded pre/MCP/post calls plus revocable actor/channel binding generations.
///
/// One guard belongs to one host connection scope. It never silently reactivates a stopped
/// binding: only `establish_start` creates a generation, while ordinary calls use
/// `validate_active`. Completed call capacity is never evicted because it is replay evidence.
#[derive(Debug, Default)]
pub struct HostBindingGuard {
    pre_observed: BTreeSet<(CandidateInvocation, ChannelSessionRef)>,
    settling: BTreeMap<(CandidateInvocation, ChannelSessionRef), BindingRef>,
    completed: BTreeSet<(CandidateInvocation, ChannelSessionRef)>,
    bindings: BTreeMap<(String, ChannelSessionRef), BindingRef>,
    next_generation: u64,
    /// Coalesced native lifecycle hints, at most one per active binding and no raw tool data.
    native_hints: BTreeSet<BindingRef>,
    /// Failed or native-only identities cannot be reused for MCP validation in this daemon lifetime.
    rejected: BTreeSet<(CandidateInvocation, ChannelSessionRef)>,
}

impl HostBindingGuard {
    /// Establishes an explicit start binding after its exact native pre-hook is already observed.
    ///
    /// Missing pre-observation rejects this invocation permanently within the daemon lifetime.
    /// A current binding for the same actor/channel is reused for an idempotent explicit start.
    /// After `stop_binding` or `stop`, a later explicit start creates a fresh generation.
    pub fn establish_start(
        &mut self,
        candidate: CandidateInvocation,
        channel: ChannelSessionRef,
    ) -> BindingStatus {
        let invocation = (candidate.clone(), channel.clone());
        if self.rejected.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        if self.rejected.contains(&invocation)
            || self.settling.contains_key(&invocation)
            || self.completed.contains(&invocation)
        {
            return BindingStatus::Unavailable(BindingUnavailable::Replay);
        }
        if !self.pre_observed.contains(&invocation) {
            self.rejected.insert(invocation);
            return BindingStatus::Unavailable(BindingUnavailable::MissingPre);
        }
        if self.settling.len() >= MAX_PENDING || self.completed.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        let binding_key = (candidate.actor_id.clone(), channel.clone());
        let binding = if let Some(existing) = self.bindings.get(&binding_key) {
            existing.clone()
        } else {
            if self.bindings.len() >= MAX_BINDINGS {
                return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
            }
            let Some(generation) = self.next_generation.checked_add(1) else {
                return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
            };
            self.next_generation = generation;
            let binding = BindingRef {
                actor_id: candidate.actor_id.clone(),
                channel,
                generation,
            };
            self.bindings.insert(binding_key, binding.clone());
            binding
        };
        self.pre_observed.remove(&invocation);
        self.settling.insert(invocation, binding.clone());
        BindingStatus::Validated(validated(candidate, binding))
    }

    /// Validates an ordinary MCP candidate against its active matching actor/channel binding.
    ///
    /// Failed ordering is retained as bounded replay evidence and cannot be repaired by late hooks.
    /// This path never creates or revives a binding. Its pre-observation is consumed even when
    /// no active binding exists, so a later start cannot validate an old ordinary call.
    pub fn validate_active(
        &mut self,
        candidate: CandidateInvocation,
        channel: ChannelSessionRef,
    ) -> BindingStatus {
        let invocation = (candidate.clone(), channel.clone());
        if self.rejected.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        if self.rejected.contains(&invocation)
            || self.settling.contains_key(&invocation)
            || self.completed.contains(&invocation)
        {
            return BindingStatus::Unavailable(BindingUnavailable::Replay);
        }
        if !self.pre_observed.remove(&invocation) {
            self.rejected.insert(invocation);
            return BindingStatus::Unavailable(BindingUnavailable::MissingPre);
        }
        let Some(binding) = self
            .bindings
            .get(&(candidate.actor_id.clone(), channel))
            .cloned()
        else {
            self.rejected.insert(invocation);
            return BindingStatus::Unavailable(BindingUnavailable::InactiveBinding);
        };
        if self.settling.len() >= MAX_PENDING || self.completed.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        self.settling.insert(invocation, binding.clone());
        BindingStatus::Validated(validated(candidate, binding))
    }

    /// Buffers a native pre-hook or records post-hook settlement for a channel-bound invocation.
    ///
    /// Duplicate pre-hooks and post-before-MCP lifecycles prevent later MCP validation.
    /// For an already active binding, a complete Pre/Post without MCP also coalesces one
    /// native-change hint. Failed commands and edit/delete/rename lifecycles use the same hint;
    /// tool payloads are never interpreted as proof of effects.
    /// A pre-hook is not an authority claim. A post-hook never gates MCP result acceptance: it
    /// only settles a call that was already validated by `establish_start` or `validate_active`.
    pub fn observe_hook(&mut self, event: HookEvent, channel: ChannelSessionRef) -> BindingStatus {
        let candidate = CandidateInvocation {
            actor_id: event.actor_id,
            call_id: event.call_id,
        };
        let invocation = (candidate.clone(), channel);
        if self.rejected.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        if self.rejected.contains(&invocation) {
            return BindingStatus::Unavailable(BindingUnavailable::Mismatch);
        }
        match event.phase {
            HookPhase::Pre
                if self.pre_observed.contains(&invocation)
                    || self.settling.contains_key(&invocation)
                    || self.completed.contains(&invocation) =>
            {
                self.pre_observed.remove(&invocation);
                self.settling.remove(&invocation);
                self.rejected.insert(invocation);
                BindingStatus::Unavailable(BindingUnavailable::Replay)
            }
            HookPhase::Pre => {
                if self.pre_observed.len() + self.settling.len() >= MAX_PENDING
                    || self.completed.len() >= MAX_COMPLETED
                {
                    return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
                }
                self.pre_observed.insert(invocation);
                BindingStatus::PreObserved
            }
            HookPhase::Post => {
                if self.completed.contains(&invocation) {
                    return BindingStatus::Unavailable(BindingUnavailable::Replay);
                }
                if self.pre_observed.remove(&invocation) {
                    let binding = self
                        .bindings
                        .get(&(candidate.actor_id, invocation.1.clone()))
                        .cloned();
                    self.rejected.insert(invocation);
                    if let Some(binding) = binding {
                        self.native_hints.insert(binding.clone());
                        return BindingStatus::NativeObserved(binding);
                    }
                    return BindingStatus::Unavailable(BindingUnavailable::MissingInvocation);
                }
                let Some(binding) = self.settling.remove(&invocation) else {
                    self.rejected.insert(invocation);
                    return BindingStatus::Unavailable(BindingUnavailable::Mismatch);
                };
                if self.completed.len() >= MAX_COMPLETED {
                    self.settling.insert(invocation, binding);
                    return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
                }
                self.completed.insert(invocation);
                BindingStatus::Settled(validated(candidate, binding))
            }
        }
    }

    /// Consumes a coalesced native lifecycle hint for one currently active binding.
    ///
    /// `true` requests a fresh bounded registered-path reconciliation, never an assumed source
    /// effect. `false` means no unconsumed hint. Stopped/stale generations return unavailable;
    /// no path, command success, Workspace authority or Execution permit is inferred.
    pub fn take_native_change_hint(
        &mut self,
        binding: &BindingRef,
    ) -> Result<bool, BindingUnavailable> {
        self.check_active(binding)?;
        Ok(self.native_hints.remove(binding))
    }

    /// Checks whether one immutable binding generation remains active at this exact boundary.
    pub fn check_active(&self, binding: &BindingRef) -> Result<(), BindingUnavailable> {
        let key = (binding.actor_id.clone(), binding.channel.clone());
        (self.bindings.get(&key) == Some(binding))
            .then_some(())
            .ok_or(BindingUnavailable::InactiveBinding)
    }

    /// Consumes current liveness for one scoped Workspace or Execution admission.
    ///
    /// Calls are serialized with `stop_binding` and `stop`: a consume after their revocation
    /// point fails. The returned value is not a durable permit and must not be reused.
    pub fn consume_active(
        &mut self,
        binding: &BindingRef,
    ) -> Result<ActiveBindingUse, BindingUnavailable> {
        self.check_active(binding)?;
        Ok(ActiveBindingUse(binding.clone()))
    }

    /// Revokes one generation and rejects its pending pre-hooks; later post settlement remains valid.
    pub fn stop_binding(&mut self, binding: &BindingRef) -> Result<(), BindingUnavailable> {
        let key = (binding.actor_id.clone(), binding.channel.clone());
        if self.bindings.get(&key) != Some(binding) {
            return Err(BindingUnavailable::InactiveBinding);
        }
        self.bindings.remove(&key);
        self.native_hints.remove(binding);
        self.pre_observed.retain(|invocation| {
            if invocation.0.actor_id == binding.actor_id && invocation.1 == binding.channel {
                if self.rejected.len() < MAX_COMPLETED {
                    self.rejected.insert(invocation.clone());
                }
                false
            } else {
                true
            }
        });
        Ok(())
    }

    /// Revokes every active binding and discards unmatched pre-hooks in this host scope.
    ///
    /// Later explicit starts remain permitted and receive fresh generations; later post-hooks
    /// can still settle calls that were validated before this stop.
    pub fn stop(&mut self) {
        self.pre_observed.clear();
        self.bindings.clear();
        self.native_hints.clear();
    }
}

/// Builds a public invocation record from a bounded candidate and immutable binding reference.
fn validated(candidate: CandidateInvocation, binding: BindingRef) -> ValidatedInvocation {
    ValidatedInvocation {
        actor_id: candidate.actor_id,
        call_id: candidate.call_id,
        binding,
    }
}

/// Parses only Codex's trusted MCP `_meta` fields needed for a candidate lifecycle.
///
/// Call this only at trusted MCP ingress after rmcp has separated request metadata from tool
/// arguments. The required `x-codex-turn-metadata` object is checked for host support but not
/// retained, and arbitrary `_meta` or argument fields cannot contribute identity.
pub fn parse_candidate(
    meta: &Map<String, Value>,
) -> Result<CandidateInvocation, BindingUnavailable> {
    if !meta.get(TURN_METADATA).is_some_and(Value::is_object) {
        return Err(BindingUnavailable::MissingField(TURN_METADATA));
    }
    Ok(CandidateInvocation {
        actor_id: required_identifier(meta, "threadId")?,
        call_id: required_identifier(meta, "callId")?,
    })
}

/// Parses one bounded Codex hook payload while retaining only phase, actor, and call id.
///
/// Child hooks must provide `agent_id`; root hooks must provide `session_id`. A payload with
/// both or neither, or duplicate known JSON keys, is unavailable because identity is ambiguous.
/// The parser never returns tool input/output, source, cwd, transcript paths, or unknown fields.
pub fn parse_hook_event(payload: &[u8]) -> Result<HookEvent, BindingUnavailable> {
    if payload.len() > MAX_HOOK_METADATA_BYTES {
        return Err(BindingUnavailable::InvalidMetadata);
    }
    let payload: CodexHookPayload =
        serde_json::from_slice(payload).map_err(|_| BindingUnavailable::InvalidMetadata)?;
    let phase = match payload.hook_event_name.as_str() {
        "PreToolUse" => HookPhase::Pre,
        "PostToolUse" => HookPhase::Post,
        _ => return Err(BindingUnavailable::UnsupportedHookPhase),
    };
    let actor_id = match (payload.agent_id, payload.session_id) {
        (Some(actor), None) | (None, Some(actor)) => checked_identifier(actor, "hook actor")?,
        (Some(_), Some(_)) => return Err(BindingUnavailable::InvalidField("hook actor")),
        (None, None) => return Err(BindingUnavailable::MissingField("hook actor")),
    };
    Ok(HookEvent {
        phase,
        actor_id,
        call_id: checked_identifier(payload.tool_use_id, "tool_use_id")?,
    })
}

/// Selects only Codex correlation fields and rejects duplicate known JSON keys while discarding extras.
#[derive(Deserialize)]
struct CodexHookPayload {
    /// Native lifecycle name, restricted to PreToolUse or PostToolUse after decoding.
    hook_event_name: String,
    /// Root actor identity; absent/null for a child event, bounded after decoding.
    session_id: Option<String>,
    /// Child actor identity; absent/null for a root event, bounded after decoding.
    agent_id: Option<String>,
    /// Exact native tool-call identifier, nonempty and bounded after decoding.
    tool_use_id: String,
}

/// Parses a complete bounded host sandbox-state observation for one consumed active invocation.
///
/// The caller must pass `advertised_capability` only when the same server session advertised
/// `codex/sandbox-state-meta`. All four source-proven outer names are required, while nested
/// values remain opaque for Execution's own versioned parser and never become a permit.
pub fn parse_observed_sandbox_state(
    meta: &Map<String, Value>,
    invocation: &ValidatedInvocation,
    active_use: &ActiveBindingUse,
    advertised_capability: bool,
) -> Result<ObservedSandboxState, BindingUnavailable> {
    if !advertised_capability {
        return Err(BindingUnavailable::CapabilityNotAdvertised);
    }
    if active_use.binding_ref() != invocation.binding_ref() {
        return Err(BindingUnavailable::BindingUseMismatch);
    }
    let Some(state) = meta.get(SANDBOX_STATE_METADATA) else {
        return Err(BindingUnavailable::MissingSandboxState);
    };
    let Some(object) = state.as_object() else {
        return Err(BindingUnavailable::InvalidSandboxState);
    };
    if !SANDBOX_STATE_FIELDS
        .iter()
        .all(|field| object.contains_key(*field))
    {
        return Err(BindingUnavailable::InvalidSandboxState);
    }
    if serde_json::to_vec(state)
        .map_err(|_| BindingUnavailable::InvalidSandboxState)?
        .len()
        > MAX_SANDBOX_STATE_BYTES
    {
        return Err(BindingUnavailable::InvalidSandboxState);
    }
    Ok(ObservedSandboxState {
        actor_id: invocation.actor_id.clone(),
        call_id: invocation.call_id.clone(),
        binding: invocation.binding.clone(),
        provenance: SandboxStateProvenance::AdvertisedAndReturned,
        state: OpaqueSandboxState(state.clone()),
    })
}

/// Reads a required bounded string field without reporting its raw value.
fn required_identifier(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<String, BindingUnavailable> {
    let Some(value) = object.get(field) else {
        return Err(BindingUnavailable::MissingField(field));
    };
    let Some(value) = value.as_str() else {
        return Err(BindingUnavailable::InvalidField(field));
    };
    checked_identifier(value.to_owned(), field)
}

/// Enforces the fixed identifier memory bound and rejects empty values.
fn checked_identifier(value: String, field: &'static str) -> Result<String, BindingUnavailable> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(BindingUnavailable::InvalidField(field));
    }
    Ok(value)
}
