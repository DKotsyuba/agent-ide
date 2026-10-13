//! One shared canonical semantic hash definition; no native/persistence facts.
use crate::*;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 of canonical typed JSON plus fixed domain separator; no diagnostics.
fn fingerprint<T: Serialize>(domain: &str, value: &T) -> Result<String, ProtocolError> {
    let value = sort_json(
        serde_json::to_value(value)
            .map_err(|_| ProtocolError::new("fingerprint", "encoding failed"))?,
    );
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| ProtocolError::new("fingerprint", "encoding failed"))?;
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}
/// Shared registration-intent hash; excludes rotating evidence/credentials.
/// Absent kinds/size remain inheritance sentinels; requested modes default message.
/// Used by native in-flight claims and core request replay, never as authority.
pub fn registration_request_fingerprint(
    params: &RegisterCallParams,
) -> Result<String, ProtocolError> {
    params.validate()?;
    let mut value = serde_json::to_value(params)
        .map_err(|_| ProtocolError::new("fingerprint", "encoding failed"))?;
    let map = value
        .as_object_mut()
        .ok_or(ProtocolError::new("fingerprint", "expected object"))?;
    map.remove("evidence");
    let mut modes: Vec<Value> = params
        .allowed_modes
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|x| serde_json::to_value(x).expect("enum serializes"))
                .collect()
        })
        .unwrap_or_else(|| vec![Value::String("message".into())]);
    modes.sort_by_key(Value::to_string);
    map.insert("allowed_modes".into(), Value::Array(modes));
    let kinds = match &params.allowed_kinds {
        None => serde_json::json!({"type":"inherit_policy"}),
        Some(k) => {
            let mut k = k.clone();
            k.sort();
            serde_json::json!({"type":"explicit","kinds":k})
        }
    };
    map.insert("allowed_kinds".into(), kinds);
    fingerprint("bridge-producer-registration-v1\n", &value)
}
/// Shared normalized publication hash, including tagged destination and lifetime.
/// Outer credential/request ID and resolved core envelope identity are excluded.
pub fn publication_intent_fingerprint(params: &PublishParams) -> Result<String, ProtocolError> {
    params.validate()?;
    let mut normalized = params.clone();
    if let Destination::Address { address } = &mut normalized.destination {
        *address = NativeAddress::parse(address)?.canonical();
    }
    fingerprint("bridge-producer-intent-v1\n", &normalized)
}

/// Sorts every object recursively, independent of serde_json preserve_order feature.
fn sort_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.into_iter().map(|(k, v)| (k, sort_json(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        other => other,
    }
}
