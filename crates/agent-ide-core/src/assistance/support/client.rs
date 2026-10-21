//! Bounded, opt-in System One transport configured by the operator; no retries or fallback.
//!
//! Wire contracts: <https://dev.opencode.ai/v2/docs/console/models>,
//! <https://docs.typesafe.ai/primitives/choice> and
//! <https://docs.typesafe.ai/primitives/noul>. Only the selected route's exact
//! model identifier is accepted; no undocumented response alias mapping.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

/// Operator-configured compatible System One transport; no provider registry or fallback.
#[derive(Clone)]
pub(super) struct Transport {
    name: String,
    endpoint: String,
    model: String,
    response_model: String,
    pub(super) input_usd_per_million: Option<f64>,
}

impl Transport {
    /// Validates a credential-free HTTPS base and relative API path without I/O.
    pub(super) fn new(
        name: &str,
        base: &str,
        path: &str,
        model: &str,
        response_model: Option<&str>,
        input_price: Option<f64>,
    ) -> Result<Self, &'static str> {
        let valid_model = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        };
        let expected = response_model.unwrap_or(model);
        if name.len() > 64
            || !valid_model(model)
            || !valid_model(expected)
            || base.len() > 2048
            || path.is_empty()
            || path.len() > 256
            || path.starts_with('/')
            || path.contains(['?', '#', ':', '\\'])
            || path.split('/').any(|part| part == ".." || part == ".")
            || input_price.is_some_and(|price| !price.is_finite() || price < 0.0)
        {
            return Err("transport_config_invalid");
        }
        let mut base = reqwest::Url::parse(base).map_err(|_| "transport_config_invalid")?;
        if base.scheme() != "https"
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err("transport_config_invalid");
        }
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        let endpoint = base.join(path).map_err(|_| "transport_config_invalid")?;
        if endpoint.origin() != base.origin() {
            return Err("transport_config_invalid");
        }
        Ok(Self {
            name: name.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            response_model: expected.into(),
            input_usd_per_million: input_price,
        })
    }
    /// Exact HTTPS request target derived only from trusted configuration.
    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    /// Exact configured model sent to the provider.
    pub(super) fn model(&self) -> &str {
        &self.model
    }
    /// Exact configured response identity; no aliases are guessed.
    pub(super) fn response_model(&self) -> &str {
        &self.response_model
    }
    /// Operator label for private accounting, not a provider capability selector.
    pub(super) fn name(&self) -> &str {
        &self.name
    }
}

/// Test fixtures only: production endpoints/models must come from launcher configuration.
#[cfg(test)]
pub(super) fn fixture_transport(name: &str) -> Transport {
    let (base, path, model, expected, price) = match name {
        "typesafe" => (
            "https://api.typesafe.ai",
            "v1/systemone",
            "jev-1.13.0",
            "jev-1.13.0",
            0.042,
        ),
        "openrouter" => (
            "https://openrouter.ai/api",
            "alpha/decisions",
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917",
            0.042,
        ),
        _ => (
            "https://opencode.ai",
            "inference/systemone/v1/systemone",
            "jev-1.13-free",
            "jev-1.13-free",
            0.0,
        ),
    };
    Transport::new(name, base, path, model, Some(expected), Some(price)).unwrap()
}
/// Total connect, headers and streamed-body deadline, including a stalled body.
const DEADLINE: Duration = Duration::from_millis(1_500);
/// Maximum serialized request bytes, checked before any HTTP admission.
const REQUEST_LIMIT: usize = 24 * 1024;
/// Maximum response bytes accumulated from bounded streaming chunks.
const RESPONSE_LIMIT: usize = 16 * 1024;
/// Maximum credential-file bytes, including an optional trailing line ending.
const CREDENTIAL_LIMIT: usize = 4096;

/// One locally authorized option; its ID is local, while its description supplies semantics.
pub(super) struct Candidate {
    /// Nonempty unique local identifier, at most 128 bytes; never used as a wire option name.
    pub(super) id: String,
    /// Explicit action meaning shown to the model, bounded by the total request ceiling.
    pub(super) description: String,
    /// Marks the sole optional existing test whose independent usefulness Noul is requested.
    pub(super) auto_test: bool,
}

/// Validated decision and bounded redacted evidence; grants no execution authority itself.
pub(super) struct Selection {
    /// Original local candidate identifier corresponding to the selected opaque wire key.
    pub(super) selected_id: String,
    /// Native Choice confidence, finite and within the inclusive interval 0–1.
    pub(super) confidence: f64,
    /// Native Noul probability for the offered test, absent when no test was offered.
    pub(super) auto_test_probability: Option<f64>,
    /// Exact JSON sent, excluding headers; requests containing the credential are refused.
    pub(super) request: Value,
    /// Bounded response JSON, with any credential substrings redacted recursively.
    pub(super) response: Value,
    /// Reported usage with unsigned input/output token counts; absent means billing unknown.
    pub(super) usage: Option<Value>,
    /// Facts measured at the transport boundary for this single physical attempt.
    pub(super) attempt: Attempt,
}

/// Header-free transport observations; zero/default represents a request never sent.
#[derive(Debug, Default)]
pub(super) struct Attempt {
    /// Monotonic milliseconds from immediately before send through final body/result handling.
    pub(super) total_ms: f64,
    /// HTTP status if response headers arrived, otherwise unknown.
    pub(super) status: Option<u16>,
    /// Body bytes actually received, including a chunk rejected for exceeding the ceiling.
    pub(super) response_bytes: usize,
    /// True once send is attempted; connection failures may still have an unknown remote effect.
    pub(super) physical_attempt: bool,
}

impl std::fmt::Debug for Selection {
    /// Formats statistics only; contextual payloads and local identifiers remain private.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Selection")
            .field("confidence", &self.confidence)
            .field("auto_test_probability", &self.auto_test_probability)
            .finish_non_exhaustive()
    }
}

/// Safe fallback category and bounded evidence; transport error text and headers are discarded.
pub(super) struct Failure {
    /// Closed local category; contains neither provider text nor filesystem paths.
    pub(super) category: &'static str,
    /// Serialized request when admitted under the byte ceiling, without credentials or headers.
    pub(super) request: Option<Value>,
    /// Complete bounded JSON when available, recursively redacted; partial/raw bodies are absent.
    pub(super) response: Option<Value>,
    /// True if an attempted send may have been billed without validated reported usage.
    pub(super) usage_unknown: bool,
    /// Actual bounded transport observations, or default for a pre-send refusal.
    pub(super) attempt: Attempt,
}

impl std::fmt::Debug for Failure {
    /// Formats only safe category and accounting uncertainty, never contextual evidence.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Failure")
            .field("category", &self.category)
            .field("usage_unknown", &self.usage_unknown)
            .finish()
    }
}

impl Failure {
    /// Constructs a pre-send refusal without payload, side effects or billing uncertainty.
    fn before_send(category: &'static str) -> Self {
        Self {
            category,
            request: None,
            response: None,
            usage_unknown: false,
            attempt: Attempt::default(),
        }
    }
}

/// Shared async HTTP pool and protected credential; intentionally has no Debug implementation.
/// Dropping a decide future closes its exchange without retrying possible remote effects.
pub(super) struct JevClient {
    /// One reused reqwest pool retaining system proxy and standard certificate verification.
    http: reqwest::Client,
    /// Read once from the explicitly configured private file; never exported or logged.
    credential: String,
    /// Trusted route fixes both request model and strict response-model validation.
    route: Transport,
    /// Exact configured HTTPS endpoint; unit fixtures alone may replace it with loopback HTTP.
    endpoint: String,
}

impl JevClient {
    /// Loads one absolute canonical regular owner-only credential file, at most 4096 bytes.
    /// Requires canonical path spelling and a no-follow leaf; rejects foreign ownership, group/world access,
    /// empty/non-ASCII/header-unsafe contents and I/O failures without exposing the path.
    /// `route` fixes the destination/model; the file must contain that service's key.
    /// Call only after explicit opt-in: construction reads the file but performs no network I/O.
    pub(super) fn from_credential_file(path: &Path, route: &Transport) -> Result<Self, Failure> {
        let invalid = || Failure::before_send("credential_invalid");
        if !path.is_absolute() || path.canonicalize().map_err(|_| invalid())? != path {
            return Err(invalid());
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| invalid())?;
        let metadata = file.metadata().map_err(|_| invalid())?;
        // SAFETY: geteuid only reports this process's effective user ID.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > CREDENTIAL_LIMIT as u64
        {
            return Err(invalid());
        }
        let mut credential = String::new();
        file.take(CREDENTIAL_LIMIT as u64 + 1)
            .read_to_string(&mut credential)
            .map_err(|_| invalid())?;
        if credential.len() > CREDENTIAL_LIMIT {
            return Err(invalid());
        }
        let credential = credential.trim_end_matches(['\r', '\n']).to_owned();
        if credential.is_empty() || !credential.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid());
        }
        let http = http_builder()
            .https_only(true)
            .build()
            .map_err(|_| Failure::before_send("client_unavailable"))?;
        Ok(Self {
            http,
            credential,
            route: route.clone(),
            endpoint: route.endpoint().to_owned(),
        })
    }

    /// Evaluates bounded state against 2–255 uniquely identified local options, at most one test.
    /// Sends one Choice plus an independent Noul describing that specific offered test.
    /// A single 1.5-second HTTP/body deadline and byte ceilings apply; no retry or fallback
    /// occurs. Errors after attempted send retain unknown billing unless valid usage arrived.
    /// Caller owns episode admission, cancellation, freshness and all action/confidence gates.
    /// `before_send` must durably record the exact header-free request and pending billing before
    /// returning Ok. Its refusal prevents HTTP entirely; its own I/O is outside the HTTP timer.
    pub(super) async fn decide(
        &self,
        state: &str,
        candidates: &[Candidate],
        before_send: impl FnOnce(&Value) -> Result<(), ()>,
    ) -> Result<Selection, Failure> {
        let request = make_request(state, candidates, &self.route)?;
        let bytes =
            serde_json::to_vec(&request).map_err(|_| Failure::before_send("request_invalid"))?;
        if bytes.len() > REQUEST_LIMIT {
            return Err(Failure::before_send("request_too_large"));
        }
        if contains_secret(&request, &self.credential)
            || candidates
                .iter()
                .any(|candidate| candidate.id.contains(&self.credential))
        {
            return Err(Failure::before_send("request_contains_secret"));
        }
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.credential))
                .map_err(|_| Failure::before_send("credential_invalid"))?;
        authorization.set_sensitive(true);
        if before_send(&request).is_err() {
            return Err(Failure {
                request: Some(request),
                ..Failure::before_send("trace_unavailable")
            });
        }
        let began = std::time::Instant::now();
        let attempt = |status, response_bytes| Attempt {
            total_ms: began.elapsed().as_secs_f64() * 1000.0,
            status,
            response_bytes,
            physical_attempt: true,
        };
        let failed = |category, response: Option<Value>, status, response_bytes| Failure {
            category,
            request: Some(request.clone()),
            usage_unknown: response.as_ref().and_then(valid_usage).is_none(),
            response,
            attempt: attempt(status, response_bytes),
        };
        let mut response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|error| failed(network_category(&error), None, None, 0))?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > RESPONSE_LIMIT as u64)
        {
            return Err(failed("response_too_large", None, Some(status.as_u16()), 0));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            failed(
                network_category(&error),
                None,
                Some(status.as_u16()),
                body.len(),
            )
        })? {
            if chunk.len() > RESPONSE_LIMIT.saturating_sub(body.len()) {
                return Err(failed(
                    "response_too_large",
                    None,
                    Some(status.as_u16()),
                    body.len().saturating_add(chunk.len()),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let raw: Value = serde_json::from_slice(&body).map_err(|_| {
            failed(
                "response_json_invalid",
                None,
                Some(status.as_u16()),
                body.len(),
            )
        })?;
        let response = redact(raw, &self.credential);
        if !status.is_success() {
            return Err(failed(
                match status.as_u16() {
                    300..=399 => "redirect_refused",
                    429 => "rate_limited",
                    _ => "http_failure",
                },
                Some(response),
                Some(status.as_u16()),
                body.len(),
            ));
        }
        let (selected_id, confidence, auto_test_probability) =
            validate_response(&response, candidates, &self.route).map_err(|category| {
                failed(
                    category,
                    Some(response.clone()),
                    Some(status.as_u16()),
                    body.len(),
                )
            })?;
        let usage = valid_usage(&response);
        Ok(Selection {
            selected_id,
            confidence,
            auto_test_probability,
            request,
            response,
            usage,
            attempt: attempt(Some(status.as_u16()), body.len()),
        })
    }
}

/// Builds the common deadline/no-redirect/no-retry policy without changing system proxy defaults.
fn http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(DEADLINE)
}

/// Maps transport failures to safe categories without retaining request URLs or provider errors.
fn network_category(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else {
        "network_failure"
    }
}

/// Creates one bounded request with sorted opaque option keys; semantics live in descriptions.
/// Rejects invalid local options and oversized inputs before copying unbounded state.
fn make_request(
    state: &str,
    candidates: &[Candidate],
    route: &Transport,
) -> Result<Value, Failure> {
    if state.len() > REQUEST_LIMIT {
        return Err(Failure::before_send("request_too_large"));
    }
    if !(2..=255).contains(&candidates.len())
        || candidates
            .iter()
            .filter(|candidate| candidate.auto_test)
            .count()
            > 1
    {
        return Err(Failure::before_send("candidates_invalid"));
    }
    let mut criteria = Map::new();
    let mut bytes = state.len();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.id.is_empty()
            || candidate.id.len() > 128
            || candidate.description.trim().is_empty()
            || candidates[..index]
                .iter()
                .any(|previous| previous.id == candidate.id)
        {
            return Err(Failure::before_send("candidates_invalid"));
        }
        bytes = bytes.saturating_add(candidate.description.len());
        if bytes > REQUEST_LIMIT {
            return Err(Failure::before_send("request_too_large"));
        }
        criteria.insert(format!("c{index:03}"), json!(candidate.description));
    }
    let mut questions = json!({"q0": {"type": "choice",
        "instructions": "Which offered action best helps the current edit task? Choose the described no-action option when support would add no useful evidence.",
        "criteria": criteria}});
    if let Some(test) = candidates.iter().find(|candidate| candidate.auto_test) {
        questions["q1"] = json!({"type": "noul", "instructions": format!(
            "Would running this specific existing test provide useful verification evidence for the current edit task? Evaluate this test independently of any other question or choice. Offered test: {}",
            test.description)});
    }
    Ok(json!({"model": route.model(), "state": state, "questions": questions}))
}

/// Checks model, exact question/option IDs, native types and all finite 0–1 statistics.
/// Returns local selection, native confidence and optional independent test probability.
fn validate_response(
    response: &Value,
    candidates: &[Candidate],
    route: &Transport,
) -> Result<(String, f64, Option<f64>), &'static str> {
    if response.get("model").and_then(Value::as_str) != Some(route.response_model()) {
        return Err("response_model_invalid");
    }
    let answers = response
        .get("answers")
        .and_then(Value::as_object)
        .ok_or("response_answers_invalid")?;
    let has_test = candidates.iter().any(|candidate| candidate.auto_test);
    if answers.len() != if has_test { 2 } else { 1 }
        || !answers.contains_key("q0")
        || (has_test && !answers.contains_key("q1"))
    {
        return Err("response_answers_invalid");
    }
    let choice = &answers["q0"];
    if choice.get("type").and_then(Value::as_str) != Some("choice") {
        return Err("response_type_invalid");
    }
    let confidence = probability(choice.get("confidence"))?;
    let selected = choice
        .get("choice")
        .and_then(Value::as_str)
        .ok_or("response_choice_invalid")?;
    let selected_index = (0..candidates.len())
        .find(|index| selected == format!("c{index:03}"))
        .ok_or("response_choice_invalid")?;
    let probabilities = choice
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or("response_probabilities_invalid")?;
    if probabilities.len() != candidates.len() {
        return Err("response_probabilities_invalid");
    }
    let mut total = 0.0;
    let selected_probability = probability(probabilities.get(selected))?;
    for index in 0..candidates.len() {
        let value = probability(probabilities.get(&format!("c{index:03}")))?;
        if value > selected_probability + 0.001 {
            return Err("response_probabilities_invalid");
        }
        total += value;
    }
    if (total - 1.0).abs() > 0.001 {
        return Err("response_probabilities_invalid");
    }
    let auto_test_probability = if has_test {
        let test = &answers["q1"];
        if test.get("type").and_then(Value::as_str) != Some("noul") {
            return Err("response_type_invalid");
        }
        Some(probability(test.get("noul"))?)
    } else {
        None
    };
    if response.get("usage").is_some() && valid_usage(response).is_none() {
        return Err("response_usage_invalid");
    }
    Ok((
        candidates[selected_index].id.clone(),
        confidence,
        auto_test_probability,
    ))
}

/// Accepts only actual finite JSON numbers in the inclusive probability interval.
fn probability(value: Option<&Value>) -> Result<f64, &'static str> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && (0.0..=1.0).contains(number))
        .ok_or("response_probability_invalid")
}

/// Retains reported usage only with both documented unsigned token counts; no estimates.
fn valid_usage(response: &Value) -> Option<Value> {
    let usage = response.get("usage")?;
    usage.get("input_tokens")?.as_u64()?;
    usage.get("output_tokens")?.as_u64()?;
    Some(usage.clone())
}

/// Detects a credential substring anywhere in JSON strings or keys before request admission.
fn contains_secret(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(items) => items.iter().any(|item| contains_secret(item, secret)),
        Value::Object(items) => items
            .iter()
            .any(|(key, item)| key.contains(secret) || contains_secret(item, secret)),
        _ => false,
    }
}

/// Removes entire response strings/map keys containing credentials before evidence escapes.
/// Empty replacements cannot reintroduce a key via the redaction marker or joined substrings.
/// Other JSON values remain unchanged; redacted invalid protocol fields still fail validation.
fn redact(value: Value, secret: &str) -> Value {
    match value {
        Value::String(text) if text.contains(secret) => Value::String(String::new()),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| redact(item, secret)).collect())
        }
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, item)| {
                    (
                        if key.contains(secret) {
                            String::new()
                        } else {
                            key
                        },
                        redact(item, secret),
                    )
                })
                .collect(),
        ),
        other => other,
    }
}

/// Offline protocol, policy and credential checks; all HTTP stays on loopback with fake keys.
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Two semantically described local options, including one offered existing test.
    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                id: "none".into(),
                description: "Do nothing; no useful additional evidence.".into(),
                auto_test: false,
            },
            Candidate {
                id: "test".into(),
                description: "Run the already admitted regression test for the edited function."
                    .into(),
                auto_test: true,
            },
        ]
    }

    /// Native response fixture with Choice confidence, independent Noul and actual usage shape.
    fn response() -> Value {
        json!({"model": fixture_transport("opencode").model(), "answers": {
            "q0": {"type": "choice", "choice": "c001", "confidence": 0.9, "probabilities": {"c000": 0.1, "c001": 0.9}},
            "q1": {"type": "noul", "noul": 0.95}},
            "usage": {"input_tokens": 40, "output_tokens": 9}})
    }

    /// Confirms option semantics are visible and the Noul contains the particular offered test.
    #[test]
    fn request_is_native_independent_and_bounded() {
        let options = candidates();
        let request =
            make_request("edited function", &options, &fixture_transport("opencode")).unwrap();
        assert_eq!(request["model"], fixture_transport("opencode").model());
        assert_eq!(
            request["questions"]["q0"]["criteria"]["c001"],
            options[1].description
        );
        let instruction = request["questions"]["q1"]["instructions"].as_str().unwrap();
        assert!(instruction.contains(&options[1].description));
        assert!(instruction.contains("independently"));
        assert_eq!(
            make_request(
                &"x".repeat(REQUEST_LIMIT + 1),
                &options,
                &fixture_transport("opencode")
            )
            .unwrap_err()
            .category,
            "request_too_large"
        );
        let mut invalid = candidates();
        invalid[1].id = invalid[0].id.clone();
        assert_eq!(
            make_request("state", &invalid, &fixture_transport("opencode"))
                .unwrap_err()
                .category,
            "candidates_invalid"
        );
        invalid = candidates();
        invalid[0].auto_test = true;
        assert_eq!(
            make_request("state", &invalid, &fixture_transport("opencode"))
                .unwrap_err()
                .category,
            "candidates_invalid"
        );
    }

    /// Rejects missing/foreign IDs, wrong types/model, invalid statistics and malformed usage.
    #[test]
    fn response_contract_rejects_untrusted_fields() {
        let options = candidates();
        assert_eq!(
            validate_response(&response(), &options, &fixture_transport("opencode")).unwrap(),
            ("test".into(), 0.9, Some(0.95))
        );
        for (pointer, invalid) in [
            ("/model", json!("jev-1.13")),
            ("/model", json!("jev-1.13.0")),
            ("/answers/q0/type", json!("score")),
            ("/answers/q0/choice", json!("test")),
            ("/answers/q0/choice", json!(null)),
            ("/answers/q0/confidence", json!("0.9")),
            ("/answers/q0/confidence", json!(1.1)),
            ("/answers/q0/confidence", json!(-0.1)),
            ("/answers/q0/confidence", json!(null)),
            ("/answers/q0/probabilities/c001", json!(0.1)),
            ("/answers/q1/type", json!("choice")),
            ("/answers/q1/noul", json!(false)),
            ("/answers/q1/noul", json!(2)),
            ("/answers/q1/noul", json!(null)),
            ("/usage/input_tokens", json!(-1)),
            ("/usage/output_tokens", json!("9")),
        ] {
            let mut value = response();
            *value.pointer_mut(pointer).unwrap() = invalid;
            assert!(
                validate_response(&value, &options, &fixture_transport("opencode")).is_err(),
                "accepted invalid {pointer}"
            );
        }
        let mut value = response();
        value["answers"].as_object_mut().unwrap().remove("q1");
        assert!(validate_response(&value, &options, &fixture_transport("opencode")).is_err());
        let mut value = response();
        value["answers"]["unexpected"] = json!({});
        assert!(validate_response(&value, &options, &fixture_transport("opencode")).is_err());
        let mut value = response();
        value["answers"]["q0"]["probabilities"] = json!({"c000": 0.1, "foreign": 0.9});
        assert!(validate_response(&value, &options, &fixture_transport("opencode")).is_err());
        assert!(serde_json::from_str::<Value>("{bad JSON}").is_err());
        let mut value = response();
        value.as_object_mut().unwrap().remove("usage");
        assert!(validate_response(&value, &options, &fixture_transport("opencode")).is_ok());
        assert!(valid_usage(&value).is_none());
    }

    /// Redaction never introduces the credential through a fixed marker or joined substrings.
    #[test]
    fn secret_redaction_removes_complete_values_and_keys() {
        for secret in ["REDACTED", "abcd", "offline-fixture-secret"] {
            let value =
                json!({format!("before {secret} after"): [format!("ab{secret}cd"), "safe"]});
            assert!(!contains_secret(&redact(value, secret), secret));
        }
    }

    /// Fixed routes require their published response model; latest and other versions refuse.
    #[test]
    fn fixed_routes_validate_only_the_selected_model() {
        let options = candidates();
        assert_eq!(
            fixture_transport("opencode").endpoint(),
            "https://opencode.ai/inference/systemone/v1/systemone"
        );
        assert_eq!(
            fixture_transport("typesafe").endpoint(),
            "https://api.typesafe.ai/v1/systemone"
        );
        for route in [
            &fixture_transport("opencode"),
            &fixture_transport("typesafe"),
            &fixture_transport("openrouter"),
        ] {
            assert_eq!(
                make_request("state", &options, route).unwrap()["model"],
                route.model()
            );
            let mut value = response();
            value["model"] = json!(route.response_model());
            assert!(validate_response(&value, &options, route).is_ok());
            value["model"] = json!("jev-latest");
            assert_eq!(
                validate_response(&value, &options, route).unwrap_err(),
                "response_model_invalid"
            );
        }
        assert!(validate_response(&response(), &options, &fixture_transport("typesafe")).is_err());
    }

    /// Fixture server owner: termination is signaled explicitly and its task must be joined.
    struct Server {
        /// Loopback URL; never exported through production configuration.
        endpoint: String,
        /// Signals the accept loop to terminate after the completed exchange.
        stop: oneshot::Sender<()>,
        /// Owns the server task and returns captured synthetic requests on termination.
        task: tokio::task::JoinHandle<Vec<Vec<u8>>>,
    }

    impl Server {
        /// Starts a bounded loopback responder; `{location}` points back at the same fixture.
        /// A short declared body can deliberately stall; stop still interrupts the accept loop.
        async fn start(reply: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/systemone", listener.local_addr().unwrap());
            let reply = reply.replace("{location}", &endpoint);
            let (stop, mut stopped) = oneshot::channel();
            let task = tokio::spawn(async move {
                let mut requests = Vec::new();
                loop {
                    let (mut socket, _) = tokio::select! {
                        _ = &mut stopped => break,
                        result = listener.accept() => result.unwrap(),
                    };
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    loop {
                        let count =
                            tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                                .await
                                .unwrap()
                                .unwrap();
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        assert!(request.len() <= REQUEST_LIMIT + 8192);
                        if let Some(split) =
                            request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..split]);
                            let length: usize = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|length| length.trim().parse().unwrap())
                                })
                                .unwrap();
                            if request.len() >= split + 4 + length {
                                break;
                            }
                        }
                    }
                    requests.push(request);
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    // Keeping the socket alive until stop lets an incomplete body time out.
                    if !reply.contains("Connection: close") {
                        let _ = (&mut stopped).await;
                        break;
                    }
                }
                requests
            });
            Self {
                endpoint,
                stop,
                task,
            }
        }

        /// Uses production transport policy against loopback, with synthetic key and no proxy.
        fn client(&self) -> JevClient {
            JevClient {
                http: http_builder().no_proxy().build().unwrap(),
                credential: "offline-fixture-secret".into(),
                route: fixture_transport("opencode"),
                endpoint: self.endpoint.clone(),
            }
        }

        /// Stops and joins the server, asserting a single physical HTTP attempt and native body.
        async fn finish(self) {
            self.finish_with_model("jev-1.13-free").await;
        }

        /// Joins the responder and checks the exact configured request-model identity.
        async fn finish_with_model(self, expected_model: &str) {
            let _ = self.stop.send(());
            let requests = tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                requests.len(),
                1,
                "redirect/retry made another HTTP attempt"
            );
            let request = &requests[0];
            let split = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: Value = serde_json::from_slice(&request[split..]).unwrap();
            assert_eq!(body["model"], expected_model);
            assert!(body.get("messages").is_none());
        }
    }

    /// Two distinct configured model/endpoint pairs use the same client code without inference.
    #[tokio::test]
    async fn configured_transports_switch_without_provider_code() {
        for (name, model, expected) in [
            ("provider-a", "classifier-a", "snapshot-a"),
            ("provider-b", "classifier-b", "snapshot-b"),
        ] {
            let route = Transport::new(
                name,
                "https://example.invalid/custom",
                "decide",
                model,
                Some(expected),
                None,
            )
            .unwrap();
            assert_eq!(route.endpoint(), "https://example.invalid/custom/decide");
            let mut value = response();
            value["model"] = json!(expected);
            let server = Server::start(http_response("200 OK", &value.to_string(), "")).await;
            let mut client = server.client();
            client.route = route;
            let selected = client
                .decide("same state", &candidates(), |_| Ok(()))
                .await
                .unwrap();
            assert_eq!(selected.request["model"], model);
            assert_eq!(selected.response["model"], expected);
            server.finish_with_model(model).await;
        }
        for base in [
            "http://example.invalid",
            "https://secret@example.invalid",
            "https://example.invalid/?key=x",
        ] {
            assert!(Transport::new("label", base, "decide", "classifier", None, None).is_err());
        }
        for path in [
            "../decide",
            "//other.invalid/decide",
            "https://other.invalid/",
            "decide?key=x",
        ] {
            assert!(
                Transport::new(
                    "label",
                    "https://example.invalid",
                    path,
                    "classifier",
                    None,
                    None
                )
                .is_err()
            );
        }
    }

    /// Creates a finite HTTP response with explicit byte length and connection closure.
    fn http_response(status: &str, body: &str, extra_headers: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    /// Exercises real streamed HTTP success and redaction in values and keys before traces.
    #[tokio::test]
    async fn http_success_redacts_credentials_and_rejects_secret_requests() {
        let mut value = response();
        value["echo"] = json!({"offline-fixture-secret": ["prefix offline-fixture-secret suffix"]});
        let server = Server::start(http_response("200 OK", &value.to_string(), "")).await;
        let client = server.client();
        let result = client
            .decide("edited function", &candidates(), |request| {
                assert_eq!(request["model"], fixture_transport("opencode").model());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(result.selected_id, "test");
        assert_eq!(result.confidence, 0.9);
        assert_eq!(result.auto_test_probability, Some(0.95));
        assert!(result.attempt.physical_attempt);
        assert_eq!(result.attempt.status, Some(200));
        assert_eq!(result.attempt.response_bytes, value.to_string().len());
        assert!(result.attempt.total_ms > 0.0);
        assert_eq!(result.usage.unwrap()["input_tokens"], 40);
        assert!(
            !result
                .response
                .to_string()
                .contains("offline-fixture-secret")
        );
        assert!(
            !result
                .request
                .to_string()
                .contains("offline-fixture-secret")
        );
        let failure = client
            .decide("offline-fixture-secret", &candidates(), |_| {
                panic!("secret refusal precedes callback")
            })
            .await
            .unwrap_err();
        assert_eq!(failure.category, "request_contains_secret");
        assert!(!failure.usage_unknown);
        assert!(!failure.attempt.physical_attempt);
        assert!(failure.request.is_none());
        server.finish().await;
    }

    /// Covers bad JSON, rate limits, redirects and malformed/oversized responses without retry.
    #[tokio::test]
    async fn http_failures_preserve_uncertainty_without_retry_or_leak() {
        for (reply, expected) in [
            (
                http_response("200 OK", "{bad JSON offline-fixture-secret}", ""),
                "response_json_invalid",
            ),
            (
                http_response(
                    "429 Too Many Requests",
                    r#"{"error":"offline-fixture-secret"}"#,
                    "Retry-After: 0\r\n",
                ),
                "rate_limited",
            ),
            (
                http_response("307 Temporary Redirect", "{}", "Location: {location}\r\n"),
                "redirect_refused",
            ),
            (
                http_response("503 Service Unavailable", "{}", ""),
                "http_failure",
            ),
            (
                http_response("200 OK", &"x".repeat(RESPONSE_LIMIT + 1), ""),
                "response_too_large",
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    RESPONSE_LIMIT + 1,
                    "x".repeat(RESPONSE_LIMIT + 1)
                ),
                "response_too_large",
            ),
        ] {
            let server = Server::start(reply).await;
            let failure = server
                .client()
                .decide("state", &candidates(), |_| Ok(()))
                .await
                .unwrap_err();
            assert_eq!(failure.category, expected);
            assert!(failure.usage_unknown);
            assert!(failure.attempt.physical_attempt);
            assert!(failure.attempt.total_ms > 0.0);
            assert!(failure.attempt.status.is_some());
            assert!(failure.request.is_some());
            assert!(!format!("{failure:?}").contains("offline-fixture-secret"));
            assert!(
                !failure
                    .response
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_default()
                    .contains("offline-fixture-secret")
            );
            server.finish().await;
        }
    }

    /// Proves the 1.5-second total deadline includes a stalled response body; no retries occur.
    #[tokio::test]
    async fn total_deadline_covers_body_and_keeps_unknown_billing() {
        let server = Server::start("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{".into()).await;
        let failure = tokio::time::timeout(
            Duration::from_secs(5),
            server.client().decide("state", &candidates(), |_| Ok(())),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(failure.category, "timeout");
        assert!(failure.usage_unknown);
        assert!(failure.response.is_none());
        assert_eq!(failure.attempt.status, Some(200));
        assert_eq!(failure.attempt.response_bytes, 1);
        assert!(failure.attempt.total_ms >= DEADLINE.as_secs_f64() * 1000.0);
        server.finish().await;
    }

    /// A durable pre-send recording refusal prevents even a loopback connection attempt.
    #[tokio::test]
    async fn pre_send_refusal_has_no_physical_attempt() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = JevClient {
            http: http_builder().no_proxy().build().unwrap(),
            credential: "offline-fixture-secret".into(),
            route: fixture_transport("opencode"),
            endpoint: format!("http://{}/systemone", listener.local_addr().unwrap()),
        };
        let failure = client
            .decide("state", &candidates(), |request| {
                assert!(request["questions"]["q0"].is_object());
                Err(())
            })
            .await
            .unwrap_err();
        assert_eq!(failure.category, "trace_unavailable");
        assert!(!failure.attempt.physical_attempt);
        assert!(!failure.usage_unknown);
        assert_eq!(failure.attempt.total_ms, 0.0);
        assert!(failure.request.is_some());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    /// Proves credential file mode/size/symlink checks without network or real credentials.
    #[test]
    fn credential_file_requires_private_regular_bounded_owner_file() {
        let temporary = std::env::var_os("TMPDIR").expect("pilot checks set explicit TMPDIR");
        let root =
            std::path::PathBuf::from(temporary).join(format!("jev-client-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("credential");
        std::fs::write(&path, "offline-fixture-secret\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(JevClient::from_credential_file(&path, &fixture_transport("opencode")).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            JevClient::from_credential_file(&path, &fixture_transport("opencode"))
                .err()
                .unwrap()
                .category,
            "credential_invalid"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.join("link");
        symlink(&path, &link).unwrap();
        assert!(JevClient::from_credential_file(&link, &fixture_transport("opencode")).is_err());
        assert!(JevClient::from_credential_file(&root, &fixture_transport("opencode")).is_err());
        for contents in [
            "".to_owned(),
            "key\nheader".into(),
            "x".repeat(CREDENTIAL_LIMIT + 1),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                JevClient::from_credential_file(&path, &fixture_transport("opencode")).is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
