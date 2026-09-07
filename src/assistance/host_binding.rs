//! Codex-specific transport validation for an MCP invocation and its native hook lifecycle.
//!
//! A parsed candidate is not an authority claim. Only a matching trusted `PreToolUse` and
//! `PostToolUse` pair yields a [`ValidatedInvocation`]; consumers still decide whether the
//! host transport and any execution profile prove the authority they require.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_HOOK_METADATA_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 128;
const MAX_COMPLETED: usize = 1024;
const TURN_METADATA: &str = "x-codex-turn-metadata";

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
    /// The hook phase was not the native `PreToolUse` or `PostToolUse` form.
    UnsupportedHookPhase,
    /// A hook could not be linked exactly to one registered MCP candidate.
    Mismatch,
    /// A post-hook arrived before its exact pre-hook.
    MissingPre,
    /// The candidate was observed twice or after it was already validated.
    Replay,
    /// Bounded pending or completed lifecycle storage is full.
    CapacityExceeded,
    /// The guard was stopped and must be replaced for a fresh host lifecycle.
    Stopped,
}

/// Reports the lifecycle strength available to a Workspace or Execution consumer.
///
/// `Candidate` is only a parsed host claim. `Validated` proves an exact native hook pair for
/// the candidate, but does not itself attest host transport authority or sandbox enforcement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingStatus {
    /// Trusted MCP metadata was parsed but native lifecycle validation is still pending.
    Candidate(CandidateInvocation),
    /// A matching trusted pre/post lifecycle pair completed for one candidate.
    Validated(ValidatedInvocation),
    /// Required host data was absent, malformed, mismatched, replayed, or unavailable.
    Unavailable(BindingUnavailable),
}

/// Captures the exact actor and call pair whose native hook lifecycle completed.
///
/// This value is bounded and contains no model arguments, source, prompt, or hook payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedInvocation {
    actor_id: String,
    call_id: String,
}

impl ValidatedInvocation {
    /// Returns the exact actor shared by MCP metadata and both native hook events.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the exact call id shared by MCP metadata and both native hook events.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
}

/// Tracks a bounded one-shot candidate lifecycle and rejects replay after stop or validation.
///
/// One guard belongs to one host connection/turn scope. When its completed-call capacity is
/// exhausted, it reports unavailable instead of evicting replay evidence; make a fresh guard
/// only after the surrounding trusted host lifecycle has changed.
#[derive(Debug, Default)]
pub struct HostBindingGuard {
    pending: BTreeMap<CandidateInvocation, bool>,
    completed: BTreeSet<CandidateInvocation>,
    stopped: bool,
}

impl HostBindingGuard {
    /// Registers a parsed candidate as a one-shot lifecycle awaiting its native pre/post hooks.
    pub fn register(&mut self, candidate: CandidateInvocation) -> BindingStatus {
        if self.stopped {
            return BindingStatus::Unavailable(BindingUnavailable::Stopped);
        }
        if self.pending.contains_key(&candidate) || self.completed.contains(&candidate) {
            return BindingStatus::Unavailable(BindingUnavailable::Replay);
        }
        if self.pending.len() >= MAX_PENDING || self.completed.len() >= MAX_COMPLETED {
            return BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded);
        }
        self.pending.insert(candidate.clone(), false);
        BindingStatus::Candidate(candidate)
    }

    /// Matches a bounded native hook event against one registered candidate lifecycle.
    ///
    /// A pre-hook leaves the candidate pending. Only its matching post-hook returns `Validated`.
    /// Unknown, duplicate, mismatched, and stopped events remain unavailable and grant nothing.
    pub fn observe_hook(&mut self, event: HookEvent) -> BindingStatus {
        if self.stopped {
            return BindingStatus::Unavailable(BindingUnavailable::Stopped);
        }
        let candidate = CandidateInvocation {
            actor_id: event.actor_id,
            call_id: event.call_id,
        };
        let Some(pre_seen) = self.pending.get_mut(&candidate) else {
            return BindingStatus::Unavailable(if self.completed.contains(&candidate) {
                BindingUnavailable::Replay
            } else {
                BindingUnavailable::Mismatch
            });
        };
        match event.phase {
            HookPhase::Pre if *pre_seen => BindingStatus::Unavailable(BindingUnavailable::Replay),
            HookPhase::Pre => {
                *pre_seen = true;
                BindingStatus::Candidate(candidate)
            }
            HookPhase::Post if !*pre_seen => {
                BindingStatus::Unavailable(BindingUnavailable::MissingPre)
            }
            HookPhase::Post => {
                self.pending.remove(&candidate);
                self.completed.insert(candidate.clone());
                BindingStatus::Validated(ValidatedInvocation {
                    actor_id: candidate.actor_id,
                    call_id: candidate.call_id,
                })
            }
        }
    }

    /// Stops this lifecycle, discarding incomplete candidates and rejecting all later input.
    pub fn stop(&mut self) {
        self.pending.clear();
        self.stopped = true;
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
/// both or neither is unavailable because the actor origin is ambiguous. The parser never
/// returns tool input/output, source, cwd, transcript paths, or unknown fields.
pub fn parse_hook_event(payload: &[u8]) -> Result<HookEvent, BindingUnavailable> {
    if payload.len() > MAX_HOOK_METADATA_BYTES {
        return Err(BindingUnavailable::InvalidMetadata);
    }
    let Value::Object(object) =
        serde_json::from_slice(payload).map_err(|_| BindingUnavailable::InvalidMetadata)?
    else {
        return Err(BindingUnavailable::InvalidMetadata);
    };
    let phase = match required_identifier(&object, "hook_event_name")?.as_str() {
        "PreToolUse" => HookPhase::Pre,
        "PostToolUse" => HookPhase::Post,
        _ => return Err(BindingUnavailable::UnsupportedHookPhase),
    };
    let actor_id = match (
        optional_identifier(&object, "agent_id")?,
        optional_identifier(&object, "session_id")?,
    ) {
        (Some(actor), None) | (None, Some(actor)) => actor,
        (Some(_), Some(_)) => return Err(BindingUnavailable::InvalidField("hook actor")),
        (None, None) => return Err(BindingUnavailable::MissingField("hook actor")),
    };
    Ok(HookEvent {
        phase,
        actor_id,
        call_id: required_identifier(&object, "tool_use_id")?,
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

/// Reads an optional bounded string field without reporting its raw value.
fn optional_identifier(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, BindingUnavailable> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => checked_identifier(value.clone(), field).map(Some),
        Some(_) => Err(BindingUnavailable::InvalidField(field)),
    }
}

/// Enforces the fixed identifier memory bound and rejects empty values.
fn checked_identifier(value: String, field: &'static str) -> Result<String, BindingUnavailable> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(BindingUnavailable::InvalidField(field));
    }
    Ok(value)
}
