//! Strict duplicate-free NDJSON envelopes and correlated response validation.

use crate::*;
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

/// One additive producer request; credential formatting is always redacted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    /// Additive API version, currently exactly 1.
    pub api_version: u16,
    /// Caller-persisted exact exchange/registration identity.
    pub request_id: String,
    /// Protected configured identity credential.
    pub credential: Secret,
    /// Closed generic operation serialized as op and params fields.
    #[serde(flatten)]
    pub operation: Operation,
}
/// Strict raw envelope before decoding the closed op/params variant.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    /// Version validated independently of envelope/schema versions.
    api_version: u16,
    /// Exact caller correlation ID.
    request_id: String,
    /// Protected credential, never error content.
    credential: Secret,
    /// Generic operation spelling.
    op: String,
    /// Typed operation body decoded after strict outer field validation.
    params: Value,
}
impl<'de> Deserialize<'de> for Request {
    /// Decodes strict known envelope fields; use decode_request for duplicate checks.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawRequest::deserialize(d)?;
        let operation =
            serde_json::from_value(serde_json::json!({"op":raw.op,"params":raw.params}))
                .map_err(|_| de::Error::custom("invalid operation body"))?;
        Ok(Self {
            api_version: raw.api_version,
            request_id: raw.request_id,
            credential: raw.credential,
            operation,
        })
    }
}
impl Request {
    /// Builds a validated exact request; caller keeps its stable identity on retry.
    pub fn new(
        request_id: impl Into<String>,
        credential: Secret,
        operation: Operation,
    ) -> Result<Self, ProtocolError> {
        let request = Self {
            api_version: API_VERSION,
            request_id: request_id.into(),
            credential,
            operation,
        };
        request.validate()?;
        Ok(request)
    }
    /// Validates pure bounded shape, never credentials/native evidence authority.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.api_version != API_VERSION {
            return Err(ProtocolError::new("api_version", "unsupported version"));
        }
        validate_id("request_id", &self.request_id, MAX_ID_BYTES)?;
        validate_id("credential", self.credential.expose(), 16384)?;
        self.operation.validate()
    }
    /// Returns same-identity reconciliation target for mutations; reads have none.
    pub fn reconciliation(&self) -> Option<ReconciliationKey> {
        Some(match &self.operation {
            Operation::RegisterCall(_) => ReconciliationKey::Registration {
                request_id: self.request_id.clone(),
            },
            Operation::Publish(p) => ReconciliationKey::Event {
                event_id: p.event_id.clone(),
            },
            Operation::CloseRoute(_) => ReconciliationKey::Close {
                request_id: self.request_id.clone(),
            },
            Operation::ConsumeAssertion(_) => ReconciliationKey::Consume {
                request_id: self.request_id.clone(),
            },
            Operation::RetireDeliveryScope(_) => ReconciliationKey::Retire {
                request_id: self.request_id.clone(),
            },
            Operation::Capabilities(_) | Operation::Status(_) => return None,
        })
    }
}
/// One strictly correlated success or structured service error envelope.
/// Deserialization requires exactly one non-null result/error key; null is not absence.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// Selected public API version, never legacy schema version.
    pub api_version: u16,
    /// Exact request correlation ID.
    pub request_id: String,
    /// True iff result exists and error is absent.
    pub success: bool,
    /// Closed operation result on success only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<OperationResult>,
    /// Structured secret-safe failure on error only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}
impl Response {
    /// Creates a success envelope; caller validates request-specific result binding.
    pub fn ok(request_id: impl Into<String>, result: OperationResult) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: request_id.into(),
            success: true,
            result: Some(result),
            error: None,
        }
    }
    /// Creates an error envelope without copying the request body.
    pub fn error(request_id: impl Into<String>, error: ApiError) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: request_id.into(),
            success: false,
            result: None,
            error: Some(error),
        }
    }
    /// Validates envelope version/id/exclusive shape and bounded result/error DTOs.
    /// Pure validation; request-specific identity binding remains validate_for.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.api_version != API_VERSION {
            return Err(ProtocolError::new("response", "unsupported version"));
        }
        validate_id("request_id", &self.request_id, MAX_ID_BYTES)?;
        match (self.success, &self.result, &self.error) {
            (true, Some(result), None) => result.validate(),
            (false, None, Some(error)) => error.validate(),
            _ => Err(ProtocolError::new(
                "response",
                "invalid success/result/error shape",
            )),
        }
    }
    /// Checks validated envelope/DTOs and the exact immutable request binding.
    pub fn validate_for(&self, request: &Request) -> Result<(), ProtocolError> {
        self.validate()?;
        if self.request_id != request.request_id {
            return Err(ProtocolError::new("response", "correlation mismatch"));
        }
        if let Some(result) = &self.result {
            return result.validate_binding(request);
        }
        if let Some(error) = &self.error {
            if let Some(key) = &error.reconciliation
                && Some(key.clone()) != request.reconciliation()
            {
                return Err(ProtocolError::new("error", "reconciliation mismatch"));
            }
            if request.reconciliation().is_none() && error.effect.is_some() {
                return Err(ProtocolError::new("error", "read has mutation effect"));
            }
        }
        Ok(())
    }
}
/// Strict raw branches preserve top-level key presence without nullable fields.
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum RawResponse {
    /// Exactly one non-null success body, no error key at all.
    Success {
        /// Selected additive version, validated after decoding.
        api_version: u16,
        /// Exact exchange identity, validated after decoding.
        request_id: String,
        /// Success flag, checked by envelope validation.
        success: bool,
        /// Required non-null typed result.
        result: Box<OperationResult>,
    },
    /// Exactly one non-null error body, no result key at all.
    Failure {
        /// Selected additive version, validated after decoding.
        api_version: u16,
        /// Exact exchange identity, validated after decoding.
        request_id: String,
        /// Failure flag, checked by envelope validation.
        success: bool,
        /// Required non-null typed error.
        error: ApiError,
    },
}
impl<'de> Deserialize<'de> for Response {
    /// Preserves exclusive non-null key shape; decode_response additionally checks
    /// duplicate JSON keys, DTO bounds and immutable request-specific correlation.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match RawResponse::deserialize(d)? {
            RawResponse::Success {
                api_version,
                request_id,
                success,
                result,
            } => Self {
                api_version,
                request_id,
                success,
                result: Some(*result),
                error: None,
            },
            RawResponse::Failure {
                api_version,
                request_id,
                success,
                error,
            } => Self {
                api_version,
                request_id,
                success,
                result: None,
                error: Some(error),
            },
        })
    }
}
/// Recursive strict JSON value, retaining no duplicate object keys at any depth.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    /// Visits bounded frame JSON with serde's default recursion ceiling.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictVisitor)
    }
}
/// Visitor reconstructing JSON while rejecting duplicate key overwrite.
struct StrictVisitor;
impl<'de> Visitor<'de> for StrictVisitor {
    /// Recursively reconstructed duplicate-free JSON value.
    type Value = StrictValue;
    /// Safe parser expectation never echoes rejected content.
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("strict JSON")
    }
    /// Preserves booleans without coercion.
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(v)))
    }
    /// Preserves signed JSON integers without truncation.
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(v.into()))
    }
    /// Preserves unsigned JSON integers without wrapping.
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(v.into()))
    }
    /// Preserves finite JSON numbers; typed integer fields subsequently refuse floats.
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(v)
            .map(|n| StrictValue(Value::Number(n)))
            .ok_or_else(|| E::custom("invalid number"))
    }
    /// Preserves string values; field validators own their domain.
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(v.into()))
    }
    /// Preserves owned string values without extra allocation.
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(StrictValue(v.into()))
    }
    /// Preserves JSON null as explicit optional absence.
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }
    /// Recursively reconstructs arrays under the bounded input frame.
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(v) = a.next_element::<StrictValue>()? {
            values.push(v.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }
    /// Rejects duplicate keys before inserting their values, at every map depth.
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut map = serde_json::Map::new();
        let mut keys = BTreeSet::new();
        while let Some(key) = a.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate key"));
            }
            let value = a.next_value::<StrictValue>()?;
            map.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(map)))
    }
}
/// Decodes one strict JSON object with no newline/second frame/trailing JSON.
fn strict_object(bytes: &[u8], limit: usize) -> Result<Value, ProtocolError> {
    if bytes.is_empty() || bytes.len() > limit || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(ProtocolError::new("frame", "invalid frame size or framing"));
    }
    let value: StrictValue = serde_json::from_slice(bytes)
        .map_err(|_| ProtocolError::new("frame", "invalid or duplicate JSON"))?;
    if !value.0.is_object() {
        return Err(ProtocolError::new("frame", "expected object"));
    }
    Ok(value.0)
}
/// Parses a single additive request; caller frames/peer-authenticates separately.
pub fn decode_request(bytes: &[u8]) -> Result<Request, ProtocolError> {
    let value = strict_object(bytes, MAX_REQUEST_BYTES)?;
    let request: Request = serde_json::from_value(value)
        .map_err(|_| ProtocolError::new("request", "invalid known DTO fields"))?;
    request.validate()?;
    if matches!(
        request.operation,
        Operation::RegisterCall(_) | Operation::ConsumeAssertion(_)
    ) && bytes.len() > MAX_REGISTRATION_BYTES
    {
        return Err(ProtocolError::new("frame", "registration frame too large"));
    }
    Ok(request)
}
/// Encodes validated request bytes without newline; never logs serialized secrets.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    request.validate()?;
    let bytes = serde_json::to_vec(request)
        .map_err(|_| ProtocolError::new("request", "encoding failed"))?;
    let max = if matches!(
        request.operation,
        Operation::RegisterCall(_) | Operation::ConsumeAssertion(_)
    ) {
        MAX_REGISTRATION_BYTES
    } else {
        MAX_REQUEST_BYTES
    };
    if bytes.len() > max {
        return Err(ProtocolError::new("frame", "request frame too large"));
    }
    Ok(bytes)
}
/// Parses strict correlated response bytes and validates immutable request binding.
pub fn decode_response(bytes: &[u8], request: &Request) -> Result<Response, ProtocolError> {
    let value = strict_object(bytes, MAX_RESPONSE_BYTES)?;
    let response: Response = serde_json::from_value(value)
        .map_err(|_| ProtocolError::new("response", "invalid known DTO fields"))?;
    response.validate_for(request)?;
    Ok(response)
}
/// Validates and encodes one bounded service envelope without newline.
/// Invalid version/id/exclusive shape/result/error domains refuse before encoding;
/// request-specific binding is checked separately by decode_response. No logging.
pub fn encode_response(response: &Response) -> Result<Vec<u8>, ProtocolError> {
    response.validate()?;
    let bytes = serde_json::to_vec(response)
        .map_err(|_| ProtocolError::new("response", "encoding failed"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::new("response", "frame too large"));
    }
    Ok(bytes)
}
