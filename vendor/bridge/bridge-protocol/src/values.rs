//! Pure bounded values, canonical native addresses and secret-safe errors.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

/// Public additive API version; independent of legacy envelope/storage schema.
pub const API_VERSION: u16 = 1;
/// Maximum opaque identifier bytes; identifiers are never normalized.
pub const MAX_ID_BYTES: usize = 256;
/// Maximum event-kind bytes, matching the legacy core's validated Kind.
pub const MAX_KIND_BYTES: usize = 64;
/// Maximum UTF-8 payload bytes before manifest/render limits.
pub const MAX_PAYLOAD_BYTES: usize = 65536;
/// Maximum metadata pairs in an immutable publication.
pub const MAX_METADATA_ENTRIES: usize = 16;
/// Native registration request frame ceiling, excluding the final newline.
pub const MAX_REGISTRATION_BYTES: usize = 128 * 1024;
/// Core request frame ceiling, excluding the final newline.
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
/// Response frame ceiling, excluding the final newline.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Positive integer ceiling for timestamps, revisions and durable sequences.
pub const MAX_INTEGER: u64 = i64::MAX as u64;
/// Deterministically ordered opaque publication metadata.
pub type Metadata = BTreeMap<String, String>;

/// Pure validation failure containing static descriptions, never input values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    /// Name of the validated field or boundary, not caller-controlled data.
    pub field: &'static str,
    /// Stable secret-safe explanation of the violated contract.
    pub reason: &'static str,
}
impl ProtocolError {
    /// Creates a side-effect-free error from static safe descriptions.
    pub const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}
impl fmt::Display for ProtocolError {
    /// Displays only static contract descriptions, excluding rejected values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}
impl std::error::Error for ProtocolError {}

/// Validates an exact nonempty opaque identifier; does not trim or normalize.
pub fn validate_id(field: &'static str, value: &str, max: usize) -> Result<(), ProtocolError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(ProtocolError::new(field, "invalid bounded identifier"));
    }
    Ok(())
}
/// Validates a present positive storage-compatible timestamp or sequence.
pub fn validate_positive(field: &'static str, value: u64) -> Result<(), ProtocolError> {
    if value == 0 || value > MAX_INTEGER {
        return Err(ProtocolError::new(field, "integer outside positive range"));
    }
    Ok(())
}
/// Validates nonnegative counters/capacities without inventing a positive floor.
pub fn validate_count(field: &'static str, value: u64) -> Result<(), ProtocolError> {
    if value > MAX_INTEGER {
        return Err(ProtocolError::new(field, "integer outside count range"));
    }
    Ok(())
}
/// Validates an optional timestamp; absence means no explicit expiry.
pub fn validate_time(field: &'static str, value: Option<u64>) -> Result<(), ProtocolError> {
    value.map_or(Ok(()), |value| validate_positive(field, value))
}

/// Protected credential/proof/assertion text; formatting always redacts content.
/// Serialization is intentional only for a protected exchange or private state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);
impl Secret {
    /// Wraps text without logging it; the receiving operation applies its bounds.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Exposes borrowed text for authentication/serialization, never ordinary logs.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    /// Prints a fixed placeholder independent of secret value and length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}
impl fmt::Display for Secret {
    /// Prints the same fixed placeholder; does not disclose protected content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Opaque bounded event body; Debug formatting never discloses source content.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(String);
impl Payload {
    /// Validates UTF-8 byte/control bounds without truncation; writes nothing.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let payload = Self(value.into());
        payload.validate()?;
        Ok(payload)
    }
    /// Revalidates deserialized payload bytes and permitted tab/LF/CR controls.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.0.len() > MAX_PAYLOAD_BYTES
            || self
                .0
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(ProtocolError::new(
                "payload",
                "invalid payload size or controls",
            ));
        }
        Ok(())
    }
    /// Borrows content for delivery or canonical hashing; caller owns disclosure.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Payload {
    /// Shows no body bytes or length; body is never an ordinary diagnostic.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Payload(<redacted>)")
    }
}

/// Supported native host metadata contract, independent of producer identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    /// Qualified Codex MCP metadata/hook contract.
    Codex,
    /// Qualified Claude Code MCP metadata/hook contract.
    ClaudeCode,
}

/// Canonical logical native recipient, containing no authority or kernel identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeAddress {
    /// Actual OS host namespace, bounded independently.
    pub host: String,
    /// Native host product; not the message producer.
    pub harness: Host,
    /// Actual native home/store namespace, never cwd/display name.
    pub store: String,
    /// Native thread/session identifier, independent of endpoint incarnation.
    pub session: String,
}
impl NativeAddress {
    /// Parses four encoded components with the existing core percent grammar.
    /// Unknown native harness, malformed escapes/UTF-8 or invalid components refuse.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let parts: Vec<_> = value.split('/').collect();
        if parts.len() != 4 {
            return Err(ProtocolError::new("address", "expected four components"));
        }
        let harness = match parts[1] {
            "codex" => Host::Codex,
            "claude-code" => Host::ClaudeCode,
            _ => return Err(ProtocolError::new("address", "unsupported native harness")),
        };
        let address = Self {
            host: decode_component(parts[0])?,
            harness,
            store: decode_component(parts[2])?,
            session: decode_component(parts[3])?,
        };
        for part in [&address.host, &address.store, &address.session] {
            validate_id("address", part, MAX_ID_BYTES)?;
        }
        Ok(address)
    }
    /// Returns lossless uppercase canonical percent encoding of the components.
    pub fn canonical(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            encode_component(&self.host),
            match self.harness {
                Host::Codex => "codex",
                Host::ClaudeCode => "claude-code",
            },
            encode_component(&self.store),
            encode_component(&self.session)
        )
    }
}
/// Escapes only separator and escape bytes, matching existing native addressing.
fn encode_component(value: &str) -> String {
    value.replace('%', "%25").replace('/', "%2F")
}
/// Decodes strict percent byte escapes; invalid UTF-8/escapes refuse safely.
fn decode_component(value: &str) -> Result<String, ProtocolError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let byte = bytes
                .get(i + 1..i + 3)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or(ProtocolError::new("address", "malformed escape"))?;
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| ProtocolError::new("address", "invalid decoded UTF-8"))
}
