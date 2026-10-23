//! Shared credential and HTTP policy for opt-in model clients; no provider-specific decisions.
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};

pub(crate) const CREDENTIAL_LIMIT: usize = 4096;

/// Reads one private canonical owner-only credential file without following its final link.
/// Only safe static error categories are returned; key bytes never enter errors.
pub(crate) fn credential(path: &Path) -> Result<String, &'static str> {
    let bad = || "credential_invalid";
    if !path.is_absolute() || path.canonicalize().map_err(|_| bad())? != path {
        return Err(bad());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| bad())?;
    let metadata = file.metadata().map_err(|_| bad())?;
    // SAFETY: geteuid only reads the current effective identity and retains no pointer.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > CREDENTIAL_LIMIT as u64
    {
        return Err(bad());
    }
    let mut text = String::new();
    file.take(CREDENTIAL_LIMIT as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|_| bad())?;
    if text.len() > CREDENTIAL_LIMIT {
        return Err(bad());
    }
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(bad());
    }
    Ok(text.into())
}

/// Retains standard TLS/system-proxy behavior while disabling redirects and automatic retries.
pub(crate) fn http_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
}

/// Prevents a credential from entering header-free journals through JSON strings or keys.
pub(crate) fn contains_secret(value: &serde_json::Value, secret: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(secret),
        serde_json::Value::Array(values) => {
            values.iter().any(|value| contains_secret(value, secret))
        }
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(secret) || contains_secret(value, secret)),
        _ => false,
    }
}
