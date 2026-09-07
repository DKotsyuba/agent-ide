use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

const MAX_INPUT: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_FIELDS: usize = 256;
const MAX_RECORD: usize = 32 * 1024;
const MAX_LOG: u64 = 1024 * 1024;
const MAX_SANDBOX_STATE: usize = 64 * 1024;
const SKIPPED_SUBTREES: &[&str] = &["tool_input", "arguments", "content", "environment"];
const STARTUP_ENV: &[&str] = &["CODEX_THREAD_ID", "CODEX_TURN_ID", "CODEX_SESSION_ID"];
const SANDBOX_STATE_META: &str = "codex/sandbox-state-meta";
const SANDBOX_STATE_FIELDS: &[&str] = &[
    "permissionProfile",
    "codexLinuxSandboxExe",
    "sandboxCwd",
    "useLegacyLandlock",
];

/// Runs the test-only recorder as `host_probe <mcp|hook> LOG KEY [SANDBOX_STATE_OUTPUT]`.
///
/// The fourth path is accepted only in MCP mode. A bad input, key, or output path is non-fatal
/// so the diagnostic does not change host flow.
fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let mode = args.next();
    let log = args.next().map(PathBuf::from);
    let key = args.next().map(PathBuf::from);
    match mode.as_deref() {
        Some("hook") => hook(log, key),
        Some("mcp") => mcp(log, key, args.next().map(PathBuf::from)),
        _ => {
            eprintln!("usage: host_probe <mcp|hook> LOG KEY [SANDBOX_STATE_OUTPUT]");
            Ok(())
        }
    }
}

/// Holds the bounded observation destination, BLAKE3 key, and private sandbox-state output.
///
/// The recorder is inert unless both paths are supplied and the key file is exact length.
#[derive(Clone)]
struct Recorder {
    log: Option<PathBuf>,
    key: Option<[u8; 32]>,
    sandbox_state_output: Option<PathBuf>,
}

impl Recorder {
    /// Builds a recorder and fingerprints only allowlisted public startup identifiers.
    fn new(
        log: Option<PathBuf>,
        key_path: Option<PathBuf>,
        sandbox_state_output: Option<PathBuf>,
    ) -> Self {
        let recorder = Self {
            log,
            key: key_path.as_deref().and_then(read_key),
            sandbox_state_output,
        };
        let mut values = Map::new();
        for name in STARTUP_ENV {
            if let Ok(value) = env::var(name) {
                values.insert((*name).to_owned(), Value::String(value));
            }
        }
        if !values.is_empty() {
            recorder.record(&Value::Object(values));
        }
        recorder
    }

    /// Projects a transient JSON value and ignores every observation failure.
    fn record(&self, value: &Value) {
        if let (Some(path), Some(key)) = (self.log.as_deref(), self.key) {
            let _ = write_record(path, &observe(value, &key));
        }
    }

    /// Records a fixed status without preserving malformed hook bytes.
    fn marker(&self, status: &str) {
        self.record(&json!({ "status": status }));
    }

    /// Creates the operator-selected private sandbox fixture from the exact request metadata.
    fn record_sandbox_state(&self, request_meta: &Value) {
        if let Some(path) = self.sandbox_state_output.as_deref() {
            save_sandbox_state(path, request_meta);
        }
    }
}

/// Reads exactly one 32-byte BLAKE3 key; short, long, or unreadable keys are rejected.
fn read_key(path: &Path) -> Option<[u8; 32]> {
    fs::read(path).ok()?.try_into().ok()
}

/// Reads one bounded hook payload and always returns success without model-facing stdout.
fn hook(log: Option<PathBuf>, key: Option<PathBuf>) -> io::Result<()> {
    let recorder = Recorder::new(log, key, None);
    let mut input = Vec::new();
    let complete = io::stdin()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut input)
        .is_ok();
    record_hook_input(&recorder, complete.then_some(input.as_slice()));
    Ok(())
}

/// Classifies a hook payload without copying it into any log record.
fn record_hook_input(recorder: &Recorder, input: Option<&[u8]>) {
    let Some(input) = input.filter(|input| input.len() <= MAX_INPUT) else {
        recorder.marker("malformed_or_oversized");
        return;
    };
    match serde_json::from_slice(input) {
        Ok(value) => recorder.record(&value),
        Err(_) => recorder.marker("malformed_or_oversized"),
    }
}

/// Produces type descriptions plus keyed fingerprints for scalar values and dynamic keys.
///
/// The result is ASCII-only: payload subtrees are skipped and recursive traversal is capped.
fn observe(value: &Value, key: &[u8; 32]) -> String {
    let mut fields = Vec::new();
    walk(value, "$", 0, key, &mut fields);
    fields.join(";")
}

/// Adds no more than `MAX_FIELDS` safe descriptions of a JSON value.
fn walk(value: &Value, path: &str, depth: usize, key: &[u8; 32], fields: &mut Vec<String>) {
    if depth > MAX_DEPTH || fields.len() >= MAX_FIELDS {
        return;
    }
    let scalar = scalar_fingerprint(value, key)
        .map(|fingerprint| format!(" fp={fingerprint}"))
        .unwrap_or_default();
    fields.push(format!("{path}:{}{}", value_kind(value), scalar));
    if depth == MAX_DEPTH || fields.len() >= MAX_FIELDS {
        return;
    }
    match value {
        Value::Object(object) => {
            for (name, child) in object {
                if fields.len() >= MAX_FIELDS {
                    break;
                }
                let child_path = format!("{path}.k{}", fingerprint(name.as_bytes(), key));
                if SKIPPED_SUBTREES.contains(&name.as_str()) {
                    fields.push(format!("{child_path}:skipped"));
                } else {
                    walk(child, &child_path, depth + 1, key, fields);
                }
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                if fields.len() >= MAX_FIELDS {
                    break;
                }
                walk(child, &format!("{path}[{index}]"), depth + 1, key, fields);
            }
        }
        _ => {}
    }
}

/// Returns a stable non-content label for a JSON value.
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Returns a keyed scalar digest while leaving arrays and objects without a content digest.
fn scalar_fingerprint(value: &Value, key: &[u8; 32]) -> Option<String> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Some(fingerprint(value.to_string().as_bytes(), key))
        }
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Returns a domain-separated BLAKE3 hex digest for an unlogged input fragment.
fn fingerprint(input: &[u8], key: &[u8; 32]) -> String {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(b"host_probe/v1\0");
    hasher.update(input);
    hasher.finalize().to_hex().to_string()
}

/// Appends one bounded record without taking the log beyond `MAX_LOG`.
///
/// A nonblocking advisory lock protects the cooperative metadata-size-check-and-append span.
/// Oversized projections end with an explicit ASCII truncation marker.
fn write_record(path: &Path, record: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if file.try_lock().is_err() {
        return Ok(());
    }
    let line = if record.len() < MAX_RECORD {
        format!("{record}\n")
    } else {
        let suffix = ";truncated\n";
        format!("{}{}", &record[..MAX_RECORD - suffix.len()], suffix)
    };
    let enough_room = file
        .metadata()
        .map(|metadata| metadata.len().saturating_add(line.len() as u64) <= MAX_LOG)
        .unwrap_or(false);
    if enough_room {
        let _ = file.write_all(line.as_bytes());
    }
    let _ = file.unlock();
    Ok(())
}

/// Selects exactly the four Codex sandbox-state fields from the opt-in request metadata.
///
/// Other `_meta` fields, including arguments and host-private extensions, are discarded.
fn sandbox_state(request_meta: &Value) -> Option<Value> {
    let input = request_meta.get(SANDBOX_STATE_META)?.as_object()?;
    let mut output = Map::new();
    for field in SANDBOX_STATE_FIELDS {
        if let Some(value) = input.get(*field) {
            output.insert((*field).to_owned(), value.clone());
        }
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

/// Creates one private, bounded sandbox-state fixture without replacing an existing file.
///
/// The file contains only the selected `codex/sandbox-state-meta` object. Serialization,
/// size, creation, permission, and write failures are intentionally inert for the MCP host.
fn save_sandbox_state(path: &Path, request_meta: &Value) {
    let Some(state) = sandbox_state(request_meta) else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&state) else {
        return;
    };
    if bytes.len() > MAX_SANDBOX_STATE {
        return;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(path) else {
        return;
    };
    let _ = file.write_all(&bytes);
}

/// Serves the only test diagnostic tool over stdio until ordinary protocol EOF.
///
/// The requested experimental capability makes Codex attach sandbox state to request `_meta`.
#[tokio::main]
async fn mcp(
    log: Option<PathBuf>,
    key: Option<PathBuf>,
    sandbox_state_output: Option<PathBuf>,
) -> io::Result<()> {
    use rmcp::{ServiceExt, handler::server::tool::ToolRouter, model::*, service::RequestContext};

    /// Owns the generated one-tool router and the shared diagnostic recorder.
    #[derive(Clone)]
    struct Probe {
        router: ToolRouter<Self>,
        recorder: Recorder,
    }

    #[rmcp::tool_router]
    impl Probe {
        /// Returns the deliberately non-authoritative result for every probe call.
        #[rmcp::tool(
            description = "Record available host metadata; authority binding remains unproven."
        )]
        async fn probe_observe(&self) -> String {
            "binding_unproven".to_owned()
        }
    }

    #[rmcp::tool_handler(router = self.router)]
    impl rmcp::ServerHandler for Probe {
        /// Describes only the diagnostic tool capability exposed by this test server.
        fn get_info(&self) -> ServerInfo {
            let mut experimental = ExperimentalCapabilities::new();
            experimental.insert(SANDBOX_STATE_META.to_owned(), Default::default());
            ServerInfo::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_experimental_with(experimental)
                    .build(),
            )
            .with_instructions("test-only host metadata probe")
        }

        /// Records the safe request context before ordinary generated router dispatch.
        fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<CallToolResponse, rmcp::ErrorData>> + Send + '_ {
            async move {
                let request_meta = serde_json::to_value(&context.meta).unwrap_or(Value::Null);
                self.recorder.record_sandbox_state(&request_meta);
                self.recorder.record(&json!({
                    "request_id": &context.id,
                    "request_meta": request_meta,
                    "client_info": context.client_info(),
                    "client_capabilities": context.client_capabilities(),
                }));
                self.router
                    .call(rmcp::handler::server::tool::ToolCallContext::new(
                        self, request, context,
                    ))
                    .await
            }
        }
    }

    Probe {
        router: Probe::tool_router(),
        recorder: Recorder::new(log, key, sandbox_state_output),
    }
    .serve(rmcp::transport::io::stdio())
    .await
    .map_err(|error| io::Error::other(error.to_string()))?
    .waiting()
    .await
    .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    static TEST_ID: AtomicUsize = AtomicUsize::new(0);

    /// Creates a unique disposable directory under the platform temporary directory.
    fn test_dir() -> PathBuf {
        let path = env::temp_dir().join(format!(
            "host-probe-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    /// Enforces exact key length and hides unicode secret names and values.
    #[test]
    fn exact_key_and_private_unicode_are_not_logged() {
        let dir = test_dir();
        let short = dir.join("short");
        let exact = dir.join("exact");
        fs::write(&short, [7_u8; 31]).unwrap();
        fs::write(&exact, [7_u8; 32]).unwrap();
        assert!(read_key(&short).is_none());
        assert_eq!(read_key(&exact), Some([7_u8; 32]));
        let secret = "private-marker-ユニコード";
        let record = observe(
            &json!({ secret: secret, "arguments": { secret: secret } }),
            &[7; 32],
        );
        assert!(!record.contains(secret));
        assert!(record.contains(":skipped"));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Proves traversal, record, and total-log caps plus distinct dynamic-key digests.
    #[test]
    fn caps_and_dynamic_key_fingerprints_hold() {
        let mut object = Map::new();
        for index in 0..(MAX_FIELDS + 20) {
            object.insert(format!("dynamic-{index}"), Value::String("value".into()));
        }
        let record = observe(&Value::Object(object), &[3; 32]);
        assert!(record.split(';').count() <= MAX_FIELDS);
        assert_ne!(fingerprint(b"one", &[3; 32]), fingerprint(b"two", &[3; 32]));
        let dir = test_dir();
        let log = dir.join("log");
        for _ in 0..100 {
            write_record(&log, &"x".repeat(MAX_RECORD)).unwrap();
        }
        assert!(fs::metadata(&log).unwrap().len() <= MAX_LOG);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Keeps malformed and oversized hook payloads out of the observation log.
    #[test]
    fn hook_rejects_malformed_and_oversized_without_payload() {
        let dir = test_dir();
        let log = dir.join("log");
        let key = dir.join("key");
        fs::write(&key, [9_u8; 32]).unwrap();
        let recorder = Recorder::new(Some(log.clone()), Some(key), None);
        record_hook_input(&recorder, Some(b"{private-marker"));
        record_hook_input(&recorder, Some(&vec![b'x'; MAX_INPUT + 1]));
        let output = fs::read_to_string(log).unwrap();
        assert!(!output.contains("private-marker"));
        assert!(!output.contains(&"x".repeat(32)));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Saves only opt-in sandbox profile fields in a new owner-private fixture.
    #[test]
    fn sandbox_fixture_is_private_and_excludes_other_metadata() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir();
        let output = dir.join("sandbox-state.json");
        let private_marker = "private-marker";
        save_sandbox_state(
            &output,
            &json!({
                SANDBOX_STATE_META: {
                    "permissionProfile": "managed",
                    "codexLinuxSandboxExe": true,
                    "sandboxCwd": "/tmp/fixture",
                    "useLegacyLandlock": false,
                    "unrecognized": private_marker,
                },
                "arguments": { "secret": private_marker },
            }),
        );
        let saved: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(saved.get("permissionProfile"), Some(&json!("managed")));
        assert!(saved.get("unrecognized").is_none());
        assert!(
            !fs::read_to_string(&output)
                .unwrap()
                .contains(private_marker)
        );
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        save_sandbox_state(&output, &json!({ SANDBOX_STATE_META: {} }));
        fs::remove_dir_all(dir).unwrap();
    }
}
