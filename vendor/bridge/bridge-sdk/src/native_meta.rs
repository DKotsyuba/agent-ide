//! Bounded framework metadata extraction produces candidates, never authority.
use bridge_protocol::{
    CallEvidence, Host, MAX_ID_BYTES, MAX_REGISTRATION_BYTES, ProtocolError, validate_id,
};
use serde_json::Value;

/// Extracts an UNVERIFIED candidate from the original framework-protected _meta.
/// Caller must supply that object separately from arguments; no parser can prove
/// provenance. Mixed host fields/missing IDs/malformed turn object refuse locally.
/// Unknown bounded host metadata is preserved for the actual service contract.
pub fn extract_native_evidence(
    host: Host,
    metadata: &Value,
) -> Result<CallEvidence, ProtocolError> {
    let map = metadata.as_object().ok_or(ProtocolError::new(
        "native_meta",
        "expected original metadata object",
    ))?;
    let bytes = serde_json::to_vec(map)
        .map_err(|_| ProtocolError::new("native_meta", "encoding failed"))?;
    if bytes.len() > MAX_REGISTRATION_BYTES {
        return Err(ProtocolError::new("native_meta", "metadata too large"));
    }
    match host {
        Host::Codex => {
            if map.contains_key("claudecode/toolUseId") {
                return Err(ProtocolError::new("native_meta", "mixed host evidence"));
            }
            for key in ["threadId", "callId"] {
                let value = map
                    .get(key)
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::new(
                        "native_meta",
                        "missing native call identifier",
                    ))?;
                validate_id("native_meta", value, MAX_ID_BYTES)?;
            }
            if !map
                .get("x-codex-turn-metadata")
                .is_some_and(Value::is_object)
            {
                return Err(ProtocolError::new(
                    "native_meta",
                    "missing original turn object",
                ));
            }
        }
        Host::ClaudeCode => {
            if map.contains_key("threadId")
                || map.contains_key("callId")
                || map.contains_key("x-codex-turn-metadata")
            {
                return Err(ProtocolError::new("native_meta", "mixed host evidence"));
            }
            let value = map
                .get("claudecode/toolUseId")
                .and_then(Value::as_str)
                .ok_or(ProtocolError::new(
                    "native_meta",
                    "missing native tool-use identifier",
                ))?;
            validate_id("native_meta", value, MAX_ID_BYTES)?;
        }
    }
    Ok(CallEvidence::Native {
        host,
        metadata: map.clone(),
    })
}
