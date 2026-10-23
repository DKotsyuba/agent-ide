//! Bounded API/local embedding requests. A durable before-send callback owns trace and budget.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// Backend choice controls egress policy, not the semantic meaning of the vector.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Api,
    Local,
}

/// Explicit embedding space and endpoint; changing a space requires a separate vector cache.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub backend: Backend,
    pub endpoint: String,
    pub model: String,
    pub response_model: Option<String>,
    pub revision: String,
    pub dimensions: usize,
    pub credential_file: Option<PathBuf>,
    pub egress_allowed: bool,
    pub timeout_ms: u64,
    pub max_batch: usize,
    pub max_input_bytes: usize,
    pub query_input_type: Option<String>,
    pub document_input_type: Option<String>,
}

/// Actual provider accounting; missing cost is unknown, never a synthetic zero.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// One validated batch, in original input order, normalized for cosine similarity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingBatch {
    pub vectors: Vec<Vec<f32>>,
    pub usage: Usage,
    pub elapsed_ms: f64,
    pub model: String,
}

/// Safe failure facts distinguish local rejection from an attempted, possibly billed request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingFailure {
    pub category: String,
    pub attempted: bool,
    pub usage_unknown: bool,
    pub status: Option<u16>,
    pub elapsed_ms: f64,
}
impl std::fmt::Display for EmbeddingFailure {
    /// Emits no key, request text or provider response.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.category)
    }
}
impl std::error::Error for EmbeddingFailure {}
impl EmbeddingFailure {
    /// A rejected configuration or input cannot have incurred provider usage.
    fn before(category: &str) -> Self {
        Self {
            category: category.into(),
            attempted: false,
            usage_unknown: false,
            status: None,
            elapsed_ms: 0.0,
        }
    }
}

/// Reusable connection pool with private credentials; intentionally has no Debug implementation.
pub struct EmbeddingClient {
    config: EmbeddingConfig,
    http: reqwest::Client,
    credential: Option<String>,
}

impl EmbeddingConfig {
    /// Validates shape for local identity/preflight; a sending client additionally requires API permission and credentials.
    fn validate(&self, sending: bool) -> Result<(), EmbeddingFailure> {
        let error = || EmbeddingFailure::before("embedding_config_invalid");
        let url = reqwest::Url::parse(&self.endpoint).map_err(|_| error())?;
        if self.model.is_empty()
            || self.model.len() > 128
            || !self.model.bytes().all(|byte| byte.is_ascii_graphic())
            || self.response_model.as_ref().is_some_and(|model| {
                model.is_empty()
                    || model.len() > 128
                    || !model.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || self.revision.is_empty()
            || self.revision.len() > 128
            || !(1..=8192).contains(&self.dimensions)
            || !(1..=128).contains(&self.max_batch)
            || !(1..=32768).contains(&self.max_input_bytes)
            || !(1..=30000).contains(&self.timeout_ms)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || self
                .query_input_type
                .iter()
                .chain(self.document_input_type.iter())
                .any(|value| value.is_empty() || value.len() > 64)
        {
            return Err(error());
        }
        match self.backend {
            Backend::Api
                if url.scheme() != "https"
                    || (sending && (!self.egress_allowed || self.credential_file.is_none())) =>
            {
                return Err(error());
            }
            Backend::Local => {
                let local = url
                    .host_str()
                    .and_then(|host| {
                        host.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .ok()
                    })
                    .is_some_and(|ip| ip.is_loopback());
                if !local || !matches!(url.scheme(), "http" | "https") {
                    return Err(error());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Credentials and timeouts do not define vector identity; role templates and space settings do.
    pub fn space_id(&self) -> Result<String, EmbeddingFailure> {
        self.validate(false)?;
        let descriptor = json!({"schema":1,"backend":self.backend,"endpoint":self.endpoint,"model":self.model,"response_model":self.response_model,"revision":self.revision,"dimensions":self.dimensions,"query":self.query_input_type,"document":self.document_input_type,"normalization":"l2"});
        Ok(blake3::hash(descriptor.to_string().as_bytes())
            .to_hex()
            .to_string())
    }
}

impl EmbeddingClient {
    /// Constructs only after opt-in. API credentials use the same private-file policy as Jev.
    /// Loopback traffic bypasses proxies so local source data cannot be sent to a remote proxy.
    pub fn new(config: EmbeddingConfig) -> Result<Self, EmbeddingFailure> {
        config.validate(true)?;
        let credential = config
            .credential_file
            .as_deref()
            .map(crate::model_io::credential)
            .transpose()
            .map_err(EmbeddingFailure::before)?;
        let mut builder = crate::model_io::http_builder(Duration::from_millis(config.timeout_ms));
        if config.backend == Backend::Local {
            builder = builder.no_proxy();
        } else {
            builder = builder.https_only(true);
        }
        let http = builder
            .build()
            .map_err(|_| EmbeddingFailure::before("embedding_client_unavailable"))?;
        Ok(Self {
            config,
            http,
            credential,
        })
    }

    /// Embeds documents in one bounded physical request; never retries or silently truncates.
    pub async fn embed_documents(
        &self,
        inputs: &[String],
        before_send: impl FnOnce(&Value) -> Result<(), ()>,
    ) -> Result<EmbeddingBatch, EmbeddingFailure> {
        self.embed(
            inputs,
            self.config.document_input_type.as_deref(),
            before_send,
        )
        .await
    }

    /// Uses the configured query role in the same vector space as document embeddings.
    pub async fn embed_query(
        &self,
        input: &str,
        before_send: impl FnOnce(&Value) -> Result<(), ()>,
    ) -> Result<EmbeddingBatch, EmbeddingFailure> {
        self.embed(
            &[input.to_owned()],
            self.config.query_input_type.as_deref(),
            before_send,
        )
        .await
    }

    /// Builds the exact header-free request for cache lookup and durable replay; no I/O occurs.
    pub fn request_documents(&self, inputs: &[String]) -> Result<Value, EmbeddingFailure> {
        self.request_value(inputs, self.config.document_input_type.as_deref())
    }

    /// Builds the exact query request in the configured document-compatible space.
    pub fn request_query(&self, input: &str) -> Result<Value, EmbeddingFailure> {
        self.request_value(&[input.to_owned()], self.config.query_input_type.as_deref())
    }

    /// Applies the same local bounds before cache lookup and before sending.
    fn request_value(
        &self,
        inputs: &[String],
        role: Option<&str>,
    ) -> Result<Value, EmbeddingFailure> {
        if inputs.is_empty()
            || inputs.len() > self.config.max_batch
            || inputs
                .iter()
                .any(|input| input.trim().is_empty() || input.len() > self.config.max_input_bytes)
        {
            return Err(EmbeddingFailure::before("embedding_input_limit"));
        }
        let mut request = json!({"model":self.config.model,"input":inputs,"dimensions":self.config.dimensions,"encoding_format":"float"});
        if let Some(role) = role {
            request["input_type"] = json!(role);
        }
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| EmbeddingFailure::before("embedding_request_invalid"))?;
        if bytes.len() > 256 * 1024 {
            return Err(EmbeddingFailure::before("embedding_request_limit"));
        }
        if self
            .credential
            .as_ref()
            .is_some_and(|key| crate::model_io::contains_secret(&request, key))
        {
            return Err(EmbeddingFailure::before(
                "embedding_request_contains_secret",
            ));
        }
        Ok(request)
    }

    /// Sends one recorded request and validates model, dimensions, indices and finite vector data.
    async fn embed(
        &self,
        inputs: &[String],
        role: Option<&str>,
        before_send: impl FnOnce(&Value) -> Result<(), ()>,
    ) -> Result<EmbeddingBatch, EmbeddingFailure> {
        let request = self.request_value(inputs, role)?;
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| EmbeddingFailure::before("embedding_request_invalid"))?;
        let mut call = self
            .http
            .post(&self.config.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes);
        if let Some(key) = &self.credential {
            let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| EmbeddingFailure::before("credential_invalid"))?;
            auth.set_sensitive(true);
            call = call.header(reqwest::header::AUTHORIZATION, auth);
        }
        before_send(&request)
            .map_err(|_| EmbeddingFailure::before("embedding_trace_or_budget_refused"))?;
        let began = Instant::now();
        let failed = |category: &str, status| EmbeddingFailure {
            category: category.into(),
            attempted: true,
            usage_unknown: true,
            status,
            elapsed_ms: began.elapsed().as_secs_f64() * 1000.0,
        };
        let mut response = call.send().await.map_err(|error| {
            failed(
                if error.is_timeout() {
                    "embedding_timeout"
                } else {
                    "embedding_network"
                },
                None,
            )
        })?;
        let status = response.status();
        const LIMIT: usize = 8 * 1024 * 1024;
        if response
            .content_length()
            .is_some_and(|length| length > LIMIT as u64)
        {
            return Err(failed("embedding_response_limit", Some(status.as_u16())));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failed("embedding_body_failure", Some(status.as_u16())))?
        {
            if chunk.len() > LIMIT.saturating_sub(bytes.len()) {
                return Err(failed("embedding_response_limit", Some(status.as_u16())));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(failed(
                if status.as_u16() == 429 {
                    "embedding_rate_limited"
                } else {
                    "embedding_http_failure"
                },
                Some(status.as_u16()),
            ));
        }
        let response: Value = serde_json::from_slice(&bytes)
            .map_err(|_| failed("embedding_json_invalid", Some(status.as_u16())))?;
        let vectors = validate_vectors(
            &response,
            self.config
                .response_model
                .as_deref()
                .unwrap_or(&self.config.model),
            self.config.dimensions,
            inputs.len(),
        )
        .map_err(|reason| failed(reason, Some(status.as_u16())))?;
        let input_tokens = response
            .pointer("/usage/prompt_tokens")
            .or_else(|| response.pointer("/usage/input_tokens"))
            .and_then(Value::as_u64);
        let cost_usd = response
            .pointer("/usage/cost")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        Ok(EmbeddingBatch {
            vectors,
            usage: Usage {
                input_tokens,
                cost_usd,
            },
            elapsed_ms: began.elapsed().as_secs_f64() * 1000.0,
            model: response["model"].as_str().unwrap().to_owned(),
        })
    }
}

/// Rejects incomplete, aliased, duplicate-index, nonfinite and zero-vector responses.
fn validate_vectors(
    response: &Value,
    model: &str,
    dimensions: usize,
    count: usize,
) -> Result<Vec<Vec<f32>>, &'static str> {
    if response.get("model").and_then(Value::as_str) != Some(model) {
        return Err("embedding_model_mismatch");
    }
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or("embedding_data_invalid")?;
    if data.len() != count {
        return Err("embedding_count_mismatch");
    }
    let mut ordered = vec![None; count];
    for item in data {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n < count)
            .ok_or("embedding_index_invalid")?;
        if ordered[index].is_some() {
            return Err("embedding_duplicate_index");
        }
        let vector = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or("embedding_vector_invalid")?;
        if vector.len() != dimensions {
            return Err("embedding_dimension_mismatch");
        }
        let values: Vec<f64> = vector
            .iter()
            .map(|v| {
                v.as_f64()
                    .filter(|n| n.is_finite() && n.abs() <= f32::MAX as f64)
                    .ok_or("embedding_value_invalid")
            })
            .collect::<Result<_, _>>()?;
        let norm = values.iter().map(|n| n * n).sum::<f64>().sqrt();
        if norm == 0.0 || !norm.is_finite() {
            return Err("embedding_zero_vector");
        }
        ordered[index] = Some(values.into_iter().map(|n| (n / norm) as f32).collect());
    }
    ordered
        .into_iter()
        .map(|vector| vector.ok_or("embedding_missing_index"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Real input ordering is restored; malformed batches never enter the vector store.
    #[test]
    fn vector_validation_restores_order_and_rejects_bad_batches() {
        let good = json!({"model":"fixture","data":[{"index":1,"embedding":[0,2]},{"index":0,"embedding":[3,0]}]});
        assert_eq!(
            validate_vectors(&good, "fixture", 2, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        let mut bad = good.clone();
        bad["data"][1]["index"] = json!(1);
        assert!(validate_vectors(&bad, "fixture", 2, 2).is_err());
        assert!(validate_vectors(&good, "other", 2, 2).is_err());
        assert!(validate_vectors(&good, "fixture", 3, 2).is_err());
        let zero = json!({"model":"fixture","data":[{"index":0,"embedding":[0,0]}]});
        assert!(validate_vectors(&zero, "fixture", 2, 1).is_err());
    }
}
