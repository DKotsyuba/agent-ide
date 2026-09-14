//! Finite opaque transport values for Assistance hook ingress and closed v0.1/v0.2 method dispatch.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;

const MAX_OPAQUE_ID_BYTES: usize = 128;

/// Limits one finite Assistance transport call without creating a generic event channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookTransportLimits {
    /// Maximum complete v2 JSON frame size, including the transport envelope.
    pub max_frame_bytes: usize,
    /// Maximum serialized sanitized hook observation size inside its transport envelope.
    pub max_observation_bytes: usize,
    /// Total connect, frame, dispatch, and reply budget for one hook submission.
    pub deadline: Duration,
}

impl HookTransportLimits {
    /// Validates strictly positive finite limits before a hook client attempts a connect-only call.
    pub fn new(
        max_frame_bytes: usize,
        max_observation_bytes: usize,
        deadline: Duration,
    ) -> Option<Self> {
        (max_frame_bytes > 0 && max_observation_bytes > 0 && !deadline.is_zero()).then_some(Self {
            max_frame_bytes,
            max_observation_bytes,
            deadline,
        })
    }
}

/// Carries a bounded validated JSON value that Application must not interpret semantically.
#[derive(Clone, PartialEq, Eq)]
pub struct OpaqueJson(String);

impl std::fmt::Debug for OpaqueJson {
    /// Writes only the type name, never private correlations, attachments or JSON content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpaqueJson(..)")
    }
}

impl OpaqueJson {
    /// Validates JSON serialization and a byte cap without inspecting the value's domain meaning.
    pub fn new(value: impl Into<String>, max_bytes: usize) -> Option<Self> {
        let value = value.into();
        (value.len() <= max_bytes && serde_json::from_str::<Value>(&value).is_ok())
            .then_some(Self(value))
    }

    /// Serializes an already parsed JSON value for opaque bounded transport.
    pub fn from_value(value: &Value, max_bytes: usize) -> Option<Self> {
        Self::new(serde_json::to_string(value).ok()?, max_bytes)
    }

    /// Returns the exact validated JSON bytes sent as opaque Application transport content.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names the closed Assistance operations Application may route through wire v2 or v3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistanceMethod {
    /// Separate sanitized hook-observation ingress; never a generic event subscription.
    HookSubmit,
    /// Current v0.1 `ide.start` method dispatch.
    Start,
    /// Current v0.1 `ide.context` method dispatch.
    Context,
    /// Current v0.1 `ide.diff` method dispatch.
    Diff,
    /// Current v0.1 `ide.inspect` method dispatch.
    Inspect,
    /// Current v0.1 `ide.stop` method dispatch.
    Stop,
    /// v0.2 `ide.edit` method dispatch, available only on wire version 3.
    Edit,
}

impl AssistanceMethod {
    /// Parses the closed version-specific method tags; v2 never accepts `edit`.
    pub(crate) fn from_dispatch_tag(value: &str, version: u64) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "context" => Some(Self::Context),
            "diff" => Some(Self::Diff),
            "inspect" => Some(Self::Inspect),
            "stop" => Some(Self::Stop),
            "edit" if version == 3 => Some(Self::Edit),
            _ => None,
        }
    }
}

/// Holds one sanitized hook observation with opaque caller and attachment correlations.
#[derive(Clone, PartialEq, Eq)]
pub struct HookSubmit {
    request_id: String,
    correlation_id: String,
    opaque_attachment: String,
    sanitized_observation_json: OpaqueJson,
}

impl std::fmt::Debug for HookSubmit {
    /// Writes only the type name, never private correlations, attachments or JSON content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HookSubmit(..)")
    }
}

impl HookSubmit {
    /// Validates bounded opaque correlations and a sanitized JSON object supplied by Assistance.
    pub fn new(
        request_id: impl Into<String>,
        correlation_id: impl Into<String>,
        opaque_attachment: impl Into<String>,
        sanitized_observation_json: OpaqueJson,
    ) -> Option<Self> {
        let request_id = request_id.into();
        let correlation_id = correlation_id.into();
        let opaque_attachment = opaque_attachment.into();
        let is_object = serde_json::from_str::<Value>(sanitized_observation_json.as_str())
            .ok()?
            .is_object();
        (valid_id(&request_id)
            && valid_id(&correlation_id)
            && valid_id(&opaque_attachment)
            && is_object)
            .then_some(Self {
                request_id,
                correlation_id,
                opaque_attachment,
                sanitized_observation_json,
            })
    }

    /// Returns the client-supplied transport request ID for reply correlation.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Returns the opaque Assistance call correlation without asserting host identity.
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Returns the opaque attachment handle for Assistance-only interpretation.
    pub fn opaque_attachment(&self) -> &str {
        &self.opaque_attachment
    }

    /// Returns the pre-sanitized opaque JSON object without interpreting its host metadata.
    pub fn sanitized_observation_json(&self) -> &OpaqueJson {
        &self.sanitized_observation_json
    }
}

/// Holds one finite dispatch request for exactly one closed Assistance method.
#[derive(Clone, PartialEq, Eq)]
pub struct MethodDispatch {
    request_id: String,
    correlation_id: String,
    opaque_attachment: String,
    method: AssistanceMethod,
    params_json: OpaqueJson,
}

impl std::fmt::Debug for MethodDispatch {
    /// Writes only the type name, never private correlations, attachments or JSON content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MethodDispatch(..)")
    }
}

impl MethodDispatch {
    /// Validates bounded opaque correlations, a closed method tag, and opaque JSON parameters.
    pub fn new(
        request_id: impl Into<String>,
        correlation_id: impl Into<String>,
        opaque_attachment: impl Into<String>,
        method: AssistanceMethod,
        params_json: OpaqueJson,
    ) -> Option<Self> {
        let request_id = request_id.into();
        let correlation_id = correlation_id.into();
        let opaque_attachment = opaque_attachment.into();
        (method != AssistanceMethod::HookSubmit
            && valid_id(&request_id)
            && valid_id(&correlation_id)
            && valid_id(&opaque_attachment))
        .then_some(Self {
            request_id,
            correlation_id,
            opaque_attachment,
            method,
            params_json,
        })
    }

    /// Returns the client-supplied transport request ID for bounded reply correlation.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Returns the opaque Assistance call correlation without asserting host identity.
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Returns the opaque attachment handle for Assistance-only interpretation.
    pub fn opaque_attachment(&self) -> &str {
        &self.opaque_attachment
    }

    /// Returns the closed method tag selected by the caller.
    pub fn method(&self) -> AssistanceMethod {
        self.method
    }

    /// Returns the opaque JSON method parameters without interpreting tool semantics.
    pub fn params_json(&self) -> &OpaqueJson {
        &self.params_json
    }
}

/// Represents the two finite request shapes Application may forward to Assistance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistanceDispatch {
    /// One sanitized hook observation submitted without any retained queue or subscription.
    HookSubmit(HookSubmit),
    /// One closed v2/v3 method dispatch; Application still enforces the version-specific set.
    MethodDispatch(MethodDispatch),
}

/// Carries an Assistance-owned opaque result for one finite dispatch operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistanceDispatchReply {
    /// Opaque reply associated with one hook submit correlation.
    HookSubmit(OpaqueJson),
    /// Opaque result associated with one current closed method dispatch.
    MethodDispatch(OpaqueJson),
}

/// Signals that Assistance could not provide a bounded transport result without defining its semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistanceDispatchUnavailable;

/// Allows Assistance to receive exactly one bounded dispatch while retaining all host/tool semantics.
pub trait AssistanceDispatcher: Send + Sync {
    /// Initializes optional daemon-owned peers only after Application holds its exclusive lock.
    /// Default dispatchers have no startup effects; failures prevent a partially initialized daemon.
    fn initialize<'a>(
        &'a self,
        _runtime_dir: &'a std::path::Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    /// Cancels daemon-owned work and reaps owned resources before daemon exit.
    /// Default dispatchers own no resources and therefore complete immediately.
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    /// Starts one finite Assistance operation and resolves its opaque result within the caller budget.
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    >;
}

/// Reports a hook connect-only submission without requiring the caller to repair or retry transport.
#[derive(Clone, PartialEq, Eq)]
pub enum HookSubmitTransportResult {
    /// Assistance returned one opaque reply correlated to the submitted hook observation.
    Dispatched {
        /// Opaque caller correlation echoed after bounded dispatch.
        correlation_id: String,
        /// Assistance-owned opaque reply JSON.
        opaque_reply_json: OpaqueJson,
    },
    /// Daemon, framing, deadline, or Assistance dispatch was unavailable; the hook must fail open.
    Unavailable,
}

impl std::fmt::Debug for HookSubmitTransportResult {
    /// Preserves only the outcome tag, hiding the echoed opaque correlation and reply content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Dispatched { .. } => "HookSubmitTransportResult::Dispatched(..)",
            Self::Unavailable => "HookSubmitTransportResult::Unavailable",
        })
    }
}

/// Reports one finite current-method transport call without defining its tool result semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodDispatchTransportResult {
    /// Assistance returned one opaque result for the supplied closed method.
    Dispatched {
        /// Assistance-owned opaque method result JSON.
        opaque_result_json: OpaqueJson,
    },
    /// Daemon, framing, deadline, or Assistance dispatch was unavailable to the caller.
    Unavailable,
}

/// Checks the common bounded opaque identifier invariant without making an identity claim.
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OPAQUE_ID_BYTES
}

/// Ensures direct and nested public transport diagnostics cannot expose opaque host data.
#[test]
fn debug_redacts_private_transport_fields_and_nested_wrappers() {
    let secret = "private-debug-sentinel";
    let raw = format!(r#"{{"private":"{secret}"}}"#);
    let payload = OpaqueJson::new(&raw, 1024).unwrap();
    let hook = HookSubmit::new(secret, secret, secret, payload.clone()).unwrap();
    let method = MethodDispatch::new(
        secret,
        secret,
        secret,
        AssistanceMethod::Start,
        payload.clone(),
    )
    .unwrap();
    let hook_dispatch = AssistanceDispatch::HookSubmit(hook.clone());
    let method_dispatch = AssistanceDispatch::MethodDispatch(method.clone());
    let hook_reply = AssistanceDispatchReply::HookSubmit(payload.clone());
    let method_reply = AssistanceDispatchReply::MethodDispatch(payload.clone());
    let hook_result = HookSubmitTransportResult::Dispatched {
        correlation_id: secret.into(),
        opaque_reply_json: payload.clone(),
    };
    let method_result = MethodDispatchTransportResult::Dispatched {
        opaque_result_json: payload.clone(),
    };
    for value in [
        &payload as &dyn std::fmt::Debug,
        &hook,
        &method,
        &hook_dispatch,
        &method_dispatch,
        &hook_reply,
        &method_reply,
        &hook_result,
        &method_result,
    ] {
        for rendered in [format!("{value:?}"), format!("{value:#?}")] {
            assert!(
                !rendered.contains(secret) && !rendered.contains(&raw),
                "private transport Debug leaked"
            );
        }
    }
}
