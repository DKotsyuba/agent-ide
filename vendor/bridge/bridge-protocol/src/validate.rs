//! Pure shared shape/identity validation; no storage or native authority.
use crate::*;
use serde::Serialize;
use std::collections::BTreeSet;

/// Validates unique finite values using deterministic O(n log n) set insertion.
fn unique<T: Serialize>(
    field: &'static str,
    values: &[T],
    nonempty: bool,
) -> Result<(), ProtocolError> {
    if nonempty && values.is_empty() {
        return Err(ProtocolError::new(field, "empty constraint set"));
    }
    let mut set = BTreeSet::new();
    for value in values {
        let key = serde_json::to_string(value)
            .map_err(|_| ProtocolError::new(field, "invalid set value"))?;
        if !set.insert(key) {
            return Err(ProtocolError::new(field, "duplicate constraint value"));
        }
    }
    Ok(())
}
/// Validates bounded non-secret explanation text; absence is not an empty value.
fn text(field: &'static str, value: &str, max: usize) -> Result<(), ProtocolError> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(ProtocolError::new(field, "invalid bounded text"));
    }
    Ok(())
}
/// Validates optional generic text without inventing a default explanation.
fn optional_text(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Result<(), ProtocolError> {
    value.map_or(Ok(()), |v| text(field, v, max))
}
/// Validates optional opaque identifiers.
fn optional_id(field: &'static str, value: Option<&str>) -> Result<(), ProtocolError> {
    value.map_or(Ok(()), |v| validate_id(field, v, MAX_ID_BYTES))
}
/// Validates lowercase SHA-256 hexadecimal text without reinterpreting identities.
fn hash(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(ProtocolError::new(field, "invalid SHA-256"));
    }
    Ok(())
}
/// Validates a bounded generic kind/wildcard set with no product catalog.
fn kinds(values: &[String]) -> Result<(), ProtocolError> {
    unique("allowed_kinds", values, true)?;
    for value in values {
        validate_id("allowed_kinds", value, MAX_KIND_BYTES)?;
    }
    Ok(())
}
/// Validates a positive effective payload ceiling, never truncating content.
fn payload_limit(value: u32) -> Result<(), ProtocolError> {
    if value == 0 || value as usize > MAX_PAYLOAD_BYTES {
        return Err(ProtocolError::new(
            "max_payload_bytes",
            "invalid payload limit",
        ));
    }
    Ok(())
}
/// Validates canonical address syntax with the existing pure grammar.
fn address(value: &str) -> Result<(), ProtocolError> {
    NativeAddress::parse(value).map(|_| ())
}

impl RegisterCallParams {
    /// Validates request intentions and candidate shape only; never attests callers.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("operation_key", &self.operation_key, MAX_ID_BYTES)?;
        if let Some(m) = &self.allowed_modes {
            unique("allowed_modes", m, true)?;
        }
        if let Some(k) = &self.allowed_kinds {
            kinds(k)?;
        }
        if let Some(max) = self.max_payload_bytes {
            payload_limit(max)?;
        }
        validate_time("route_expires_at_ms", self.route_expires_at_ms)?;
        match (&self.assertion_audience, &self.invocation_key) {
            (None, None) => (),
            (Some(a), Some(k)) => {
                validate_id("assertion_audience", a, MAX_ID_BYTES)?;
                validate_id("invocation_key", k, MAX_ID_BYTES)?;
            }
            _ => {
                return Err(ProtocolError::new(
                    "assertion_audience",
                    "audience and invocation key must be paired",
                ));
            }
        }
        if let Some(evidence) = &self.evidence {
            match evidence {
                CallEvidence::Sealed { proof } => {
                    validate_id("proof", proof.expose(), MAX_ID_BYTES)?
                }
                CallEvidence::Native { metadata, .. } => {
                    let bytes = serde_json::to_vec(metadata)
                        .map_err(|_| ProtocolError::new("evidence", "invalid metadata"))?;
                    if bytes.len() > MAX_REGISTRATION_BYTES {
                        return Err(ProtocolError::new("evidence", "metadata too large"));
                    }
                }
            }
        }
        Ok(())
    }
}
impl RouteConstraints {
    /// Revalidates immutable finite route modes/kinds/byte limits.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        unique("allowed_modes", &self.allowed_modes, true)?;
        kinds(&self.allowed_kinds)?;
        payload_limit(self.max_payload_bytes)
    }
}
impl RegistrationReceipt {
    /// Validates durable identity domains; this does not mint fresh authority.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        for (field, value) in [
            ("producer", &self.producer),
            ("request_id", &self.request_id),
            ("operation_key", &self.operation_key),
            ("route_key", &self.route_key),
            ("caller_scope", &self.caller_scope),
            ("destination_scope", &self.destination_scope),
        ] {
            validate_id(field, value, MAX_ID_BYTES)?;
        }
        self.constraints.validate()?;
        if self.reply_policy != self.constraints.reply_policy {
            return Err(ProtocolError::new("receipt", "reply policy mismatch"));
        }
        validate_positive("created_at_ms", self.created_at_ms)?;
        validate_time("route_expires_at_ms", self.route_expires_at_ms)
    }
}
impl FreshAssertion {
    /// Validates bounded original assertion identity/time without renewing it.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("assertion_key", self.assertion_key.expose(), MAX_ID_BYTES)?;
        for (field, value) in [
            ("invocation_key", &self.invocation_key),
            ("audience", &self.audience),
            ("issuer_boot", &self.issuer_boot),
            ("endpoint_incarnation", &self.endpoint_incarnation),
        ] {
            validate_id(field, value, MAX_ID_BYTES)?;
        }
        validate_positive("observed_at_ms", self.observed_at_ms)?;
        validate_positive("expires_at_ms", self.expires_at_ms)?;
        if self.expires_at_ms < self.observed_at_ms {
            return Err(ProtocolError::new("assertion", "expiry before observation"));
        }
        Ok(())
    }
}
impl RegistrationResult {
    /// Requires actual assertion for available state; historical never contains one.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.receipt.validate()?;
        match (self.evidence_state, &self.assertion) {
            (CallerEvidenceState::FreshAssertionAvailable, Some(a)) => a.validate(),
            (
                CallerEvidenceState::Historical | CallerEvidenceState::VerifiedAtRegistration,
                None,
            ) => Ok(()),
            _ => Err(ProtocolError::new(
                "registration",
                "freshness/assertion mismatch",
            )),
        }
    }
}
impl PublishParams {
    /// Validates immutable generic event intent before custody; never permission.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match &self.destination {
            Destination::Route { route_key } => validate_id("route_key", route_key, MAX_ID_BYTES)?,
            Destination::Address { address: a } => address(a)?,
        }
        validate_id("event_id", &self.event_id, MAX_ID_BYTES)?;
        validate_id("kind", &self.kind, MAX_KIND_BYTES)?;
        self.payload.validate()?;
        match &self.policy {
            Policy::KeepAll => (),
            Policy::Latest { stream, revision } => {
                validate_id("stream", stream, MAX_ID_BYTES)?;
                validate_positive("revision", *revision)?;
            }
            Policy::Batch { group } => validate_id("group", group, MAX_ID_BYTES)?,
        }
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(ProtocolError::new("metadata", "too many pairs"));
        }
        for (k, v) in &self.metadata {
            validate_id("metadata_key", k, 64)?;
            validate_id("metadata_value", v, MAX_ID_BYTES)?;
            if k.starts_with("bridge.") {
                return Err(ProtocolError::new("metadata", "reserved key prefix"));
            }
        }
        validate_time("expires_at_ms", self.expires_at_ms)?;
        optional_id("delivery_scope_key", self.delivery_scope_key.as_deref())
    }
}
impl EventReceipt {
    /// Revalidates custody hash/identity/destination shape without implying delivery.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("producer", &self.producer, MAX_ID_BYTES)?;
        validate_id("event_id", &self.event_id, MAX_ID_BYTES)?;
        hash("intent_hash", &self.intent_hash)?;
        hash("envelope_hash", &self.envelope_hash)?;
        validate_positive("accepted_at_ms", self.accepted_at_ms)?;
        validate_positive("acceptance_seq", self.acceptance_seq)?;
        address(&self.destination.address)?;
        match (
            &self.destination.route_key,
            &self.destination.caller_scope,
            &self.destination.destination_scope,
        ) {
            (Some(r), Some(c), Some(d)) => {
                validate_id("route_key", r, MAX_ID_BYTES)?;
                validate_id("caller_scope", c, MAX_ID_BYTES)?;
                validate_id("destination_scope", d, MAX_ID_BYTES)?;
            }
            (None, None, None) => (),
            _ => {
                return Err(ProtocolError::new(
                    "destination",
                    "partial frozen route facts",
                ));
            }
        }
        Ok(())
    }
}
impl DeliveryStatus {
    /// Revalidates bounded counts/reasons/tier uniqueness, not evidence semantics.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_count("attempts_made", self.attempts_made)?;
        validate_time("attempt", self.attempt)?;
        unique("tiers", &self.tiers, false)?;
        optional_text("note", self.note.as_deref(), 512)?;
        optional_text("hold_reason", self.hold_reason.as_deref(), 512)?;
        optional_id("incident", self.incident.as_deref())?;
        optional_id("blocked_by", self.blocked_by.as_deref())
    }
}
impl StatusSelector {
    /// Validates exactly one opaque owned lookup selector, with no effect.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Registration { request_id } => {
                validate_id("request_id", request_id, MAX_ID_BYTES)
            }
            Self::Route { route_key } => validate_id("route_key", route_key, MAX_ID_BYTES),
            Self::Event { event_id } => validate_id("event_id", event_id, MAX_ID_BYTES),
        }
    }
}
impl CloseRouteParams {
    /// Validates immutable own-route close intention and safe optional reason.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("route_key", &self.route_key, MAX_ID_BYTES)?;
        optional_text("reason", self.reason.as_deref(), 512)
    }
}
impl ConsumeAssertionParams {
    /// Validates assertion reference and exact forwarded invocation key, not authority.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("assertion_key", self.assertion_key.expose(), MAX_ID_BYTES)?;
        validate_id("invocation_key", &self.invocation_key, MAX_ID_BYTES)
    }
}
impl RetireDeliveryScopeParams {
    /// Validates opaque producer lifetime identity; action is a closed enum.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("scope_key", &self.scope_key, MAX_ID_BYTES)
    }
}
impl InvocationContext {
    /// Revalidates original native invocation domains/times without granting work.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        for (field, value) in [
            ("audience", &self.audience),
            ("caller_scope", &self.caller_scope),
            ("destination_scope", &self.destination_scope),
            ("native_actor_id", &self.native_actor_id),
            ("native_call_id", &self.native_call_id),
            ("invocation_key", &self.invocation_key),
            ("issuer_boot", &self.issuer_boot),
            ("endpoint_incarnation", &self.endpoint_incarnation),
        ] {
            validate_id(field, value, MAX_ID_BYTES)?;
        }
        address(&self.caller_address)?;
        address(&self.destination_address)?;
        validate_positive("observed_at_ms", self.observed_at_ms)?;
        validate_positive("expires_at_ms", self.expires_at_ms)?;
        if self.expires_at_ms < self.observed_at_ms {
            return Err(ProtocolError::new("context", "expiry before observation"));
        }
        Ok(())
    }
}
impl ApiError {
    /// Validates safe error domains and mandatory unknown-effect reconciliation.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_id("reason", &self.reason, 64)?;
        text("detail", &self.detail, 512)?;
        if self.effect == Some(MutationEffect::Unknown) && self.reconciliation.is_none() {
            return Err(ProtocolError::new(
                "error",
                "unknown effect without reconciliation",
            ));
        }
        if let Some(k) = &self.reconciliation {
            match k {
                ReconciliationKey::Registration { request_id }
                | ReconciliationKey::Close { request_id }
                | ReconciliationKey::Consume { request_id }
                | ReconciliationKey::Retire { request_id } => {
                    validate_id("request_id", request_id, MAX_ID_BYTES)?
                }
                ReconciliationKey::Event { event_id } => {
                    validate_id("event_id", event_id, MAX_ID_BYTES)?
                }
            }
        }
        Ok(())
    }
}
impl Capabilities {
    /// Validates finite content-free advertised facts; no foreign identities.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.api_version != API_VERSION || !self.supported_versions.contains(&API_VERSION) {
            return Err(ProtocolError::new(
                "capabilities",
                "unsupported selected version",
            ));
        }
        unique("supported_versions", &self.supported_versions, true)?;
        for values in [
            &self.operations,
            &self.evidence_variants,
            &self.grant.native_scopes,
        ] {
            unique("capabilities", values, false)?;
            for value in values {
                validate_id("capability", value, MAX_ID_BYTES)?;
            }
        }
        unique("modes", &self.modes, false)?;
        unique("policies", &self.policies, false)?;
        unique("reply_policies", &self.reply_policies, false)?;
        unique("grant_modes", &self.grant.allowed_modes, false)?;
        unique("grant_policies", &self.grant.allowed_policies, false)?;
        unique("grant_reply", &self.grant.reply_policies, false)?;
        if !self.grant.allowed_kinds.is_empty() {
            kinds(&self.grant.allowed_kinds)?;
        }
        for n in &self.native_contracts {
            validate_id("build_label", &n.build_label, MAX_ID_BYTES)?;
            hash("build_digest", &n.build_digest)?;
            let paths: Vec<_> = n.paths.iter().map(|p| p.path).collect();
            unique("paths", &paths, false)?;
            for p in &n.paths {
                optional_text("path_reason", p.reason.as_deref(), 512)?;
            }
        }
        for (field, value) in [
            (
                "registration_request_bytes",
                self.limits.registration_request_bytes,
            ),
            ("core_request_bytes", self.limits.core_request_bytes),
            ("response_bytes", self.limits.response_bytes),
            ("payload_bytes", self.limits.payload_bytes),
            (
                "rendered_manifest_bytes",
                self.limits.rendered_manifest_bytes,
            ),
        ] {
            validate_positive(field, value)?;
        }
        for (field, value) in [
            ("metadata_entries", self.limits.metadata_entries),
            ("open_routes", self.limits.open_routes),
            ("assertions", self.limits.assertions),
        ] {
            validate_count(field, value)?;
        }
        Ok(())
    }
}
impl Operation {
    /// Validates closed operation inputs without service/native authority.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Capabilities(_) => Ok(()),
            Self::RegisterCall(p) => p.validate(),
            Self::Publish(p) => p.validate(),
            Self::Status(p) => p.selector.validate(),
            Self::CloseRoute(p) => p.validate(),
            Self::ConsumeAssertion(p) => p.validate(),
            Self::RetireDeliveryScope(p) => p.validate(),
        }
    }
}
impl OperationResult {
    /// Validates bounded result shape before matching it to the exact request.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Capabilities(c) => c.validate(),
            Self::RegisterCall(r) => r.validate(),
            Self::Publish(p) => p.receipt.validate(),
            Self::Status(s) => match s {
                StatusResult::Registration { receipt } => receipt
                    .as_ref()
                    .map_or(Ok(()), RegistrationReceipt::validate),
                StatusResult::Route { route } => {
                    if let Some(r) = route {
                        r.receipt.validate()?;
                        validate_time("closed_at_ms", r.closed_at_ms)
                    } else {
                        Ok(())
                    }
                }
                StatusResult::Event { event } => {
                    if let Some(e) = event {
                        e.receipt.validate()?;
                        e.delivery.validate()
                    } else {
                        Ok(())
                    }
                }
            },
            Self::CloseRoute(c) => {
                validate_id("producer", &c.receipt.producer, MAX_ID_BYTES)?;
                validate_id("request_id", &c.receipt.request_id, MAX_ID_BYTES)?;
                validate_id("route_key", &c.receipt.route_key, MAX_ID_BYTES)?;
                if c.receipt.state == RouteState::Open {
                    return Err(ProtocolError::new("close", "open close receipt"));
                }
                validate_time("closed_at_ms", c.receipt.closed_at_ms)
            }
            Self::ConsumeAssertion(c) => {
                validate_id("producer", &c.receipt.producer, MAX_ID_BYTES)?;
                validate_id("request_id", &c.receipt.request_id, MAX_ID_BYTES)?;
                c.receipt.context.validate()?;
                validate_positive("consumed_at_ms", c.receipt.consumed_at_ms)
            }
            Self::RetireDeliveryScope(r) => {
                validate_id("producer", &r.receipt.producer, MAX_ID_BYTES)?;
                validate_id("request_id", &r.receipt.request_id, MAX_ID_BYTES)?;
                validate_id("scope_key", &r.receipt.scope_key, MAX_ID_BYTES)?;
                validate_positive("retired_at_ms", r.receipt.retired_at_ms)?;
                for v in [
                    r.receipt.disposed,
                    r.receipt.already_terminal,
                    r.receipt.preserved_possible_effect,
                    r.receipt.preserved_unhanded,
                ] {
                    validate_count("retire_count", v)?;
                }
                Ok(())
            }
        }
    }
    /// Rejects mixed operations/foreign receipts and altered immutable custody intent.
    pub fn validate_binding(&self, request: &Request) -> Result<(), ProtocolError> {
        let good = match (&request.operation, self) {
            (Operation::Capabilities(_), Self::Capabilities(_)) => true,
            (Operation::RegisterCall(p), Self::RegisterCall(r)) => {
                r.receipt.request_id == request.request_id
                    && r.receipt.operation_key == p.operation_key
                    && r.receipt.reply_policy == p.reply_policy
                    && registration_constraints_match(p, &r.receipt.constraints)
                    && r.receipt.route_expires_at_ms == p.route_expires_at_ms
                    && r.assertion.as_ref().is_none_or(|a| {
                        Some(&a.audience) == p.assertion_audience.as_ref()
                            && Some(&a.invocation_key) == p.invocation_key.as_ref()
                    })
            }
            (Operation::Publish(p), Self::Publish(r)) => {
                r.receipt.event_id == p.event_id
                    && r.receipt.intent_hash == publication_intent_fingerprint(p)?
                    && match &p.destination {
                        Destination::Route { route_key } => {
                            r.receipt.destination.route_key.as_ref() == Some(route_key)
                        }
                        Destination::Address { address: a } => {
                            r.receipt.destination.route_key.is_none()
                                && r.receipt.destination.address
                                    == NativeAddress::parse(a)?.canonical()
                        }
                    }
            }
            (Operation::Status(p), Self::Status(r)) => match (&p.selector, r) {
                (
                    StatusSelector::Registration { request_id },
                    StatusResult::Registration { receipt },
                ) => receipt.as_ref().is_none_or(|r| &r.request_id == request_id),
                (StatusSelector::Route { route_key }, StatusResult::Route { route }) => route
                    .as_ref()
                    .is_none_or(|r| &r.receipt.route_key == route_key),
                (StatusSelector::Event { event_id }, StatusResult::Event { event }) => event
                    .as_ref()
                    .is_none_or(|r| &r.receipt.event_id == event_id),
                _ => false,
            },
            (Operation::CloseRoute(p), Self::CloseRoute(r)) => {
                r.receipt.request_id == request.request_id && r.receipt.route_key == p.route_key
            }
            (Operation::ConsumeAssertion(p), Self::ConsumeAssertion(r)) => {
                r.receipt.request_id == request.request_id
                    && r.receipt.context.invocation_key == p.invocation_key
            }
            (Operation::RetireDeliveryScope(p), Self::RetireDeliveryScope(r)) => {
                r.receipt.scope_key == p.scope_key && r.receipt.action == p.action
            }
            _ => false,
        };
        if !good {
            return Err(ProtocolError::new(
                "response",
                "immutable request binding mismatch",
            ));
        }
        Ok(())
    }
}

/// Rejects a returned route broader than explicit constraints/default message mode.
/// Inherited kinds/size cannot be independently resolved without service policy.
fn registration_constraints_match(
    request: &RegisterCallParams,
    returned: &RouteConstraints,
) -> bool {
    let default = [Mode::Message];
    let modes = request.allowed_modes.as_deref().unwrap_or(&default);
    returned
        .allowed_modes
        .iter()
        .all(|mode| modes.contains(mode))
        && request
            .max_payload_bytes
            .is_none_or(|max| returned.max_payload_bytes <= max)
        && request.allowed_kinds.as_ref().is_none_or(|kinds| {
            kinds.iter().any(|kind| kind == "*")
                || returned
                    .allowed_kinds
                    .iter()
                    .all(|kind| kinds.contains(kind))
        })
}
