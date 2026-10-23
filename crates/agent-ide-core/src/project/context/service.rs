//! Opt-in model orchestration with project-private requests, replay and finite physical call budget.
//! This operator service is not a host authorization boundary; caller owns root access and cancellation.
use super::embedding::{EmbeddingBatch, EmbeddingClient, EmbeddingConfig};
use super::select::{CandidateView, JevConfig, JevSelector, Selection};
use super::{ContextBundle, ContextIndex, DocumentOptions, PacketOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

/// Explicit models and limits. Omitted models allocate no clients and open no credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub embedding: Option<EmbeddingConfig>,
    pub jev: Option<JevConfig>,
    pub max_new_calls: usize,
    pub include_threshold: f64,
}

/// Measured model calls exclude replayed requests from current physical usage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallRecord {
    pub kind: String,
    pub replayed: bool,
    pub elapsed_ms: f64,
    pub usage: Option<Value>,
    pub error: Option<Value>,
}

/// Source packet plus honest backend coverage and accounting, not an assertion of task completeness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preparation {
    pub packet: ContextBundle,
    pub embedding_used: bool,
    pub embedding_complete: bool,
    pub jev_used: bool,
    pub calls: Vec<CallRecord>,
    pub warnings: Vec<String>,
    pub elapsed_ms: f64,
}

/// One private request journal per project; unknown outcomes block identical paid resubmission.
pub(super) struct Journal {
    directory: PathBuf,
}
impl Journal {
    /// Creates a private directory under the already verified project state directory.
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let directory = root.join(".agent-ide/requests");
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        // SAFETY: geteuid only reads this process's effective user identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "request journal must be a private directory",
            ));
        }
        fs::File::open(root.join(".agent-ide"))?.sync_all()?;
        Ok(Self { directory })
    }
    /// Derives names only from configured scope and exact JSON; no model-created path is used.
    fn path(&self, scope: &str, request: &Value) -> PathBuf {
        let mut hash = blake3::Hasher::new();
        hash.update(scope.as_bytes());
        hash.update(&[0]);
        hash.update(request.to_string().as_bytes());
        self.directory
            .join(format!("{}.json", hash.finalize().to_hex()))
    }
    /// Retrieves a completed response or refuses a pending/failed attempt; never infers success.
    pub(super) fn cached(&self, scope: &str, request: &Value) -> io::Result<Option<Value>> {
        let path = self.path(scope, request);
        let Some(entry) = Self::read_entry(&path)? else {
            return Ok(None);
        };
        if entry["request"] != *request {
            return Err(io::Error::other("request journal identity mismatch"));
        }
        if entry["state"] != "complete" {
            return Err(io::Error::other(
                "prior model outcome unresolved or failed; explicit operator retry required",
            ));
        }
        Ok(Some(entry["response"].clone()))
    }
    /// Reads one bounded private journal entry; no request is sent while recovering it.
    fn read_entry(path: &Path) -> io::Result<Option<Value>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        // SAFETY: geteuid reads only the current effective user identity.
        if metadata.uid() != unsafe { libc::geteuid() }
            || !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.len() > 16 * 1024 * 1024
        {
            return Err(io::Error::other("invalid request journal entry"));
        }
        let mut bytes = Vec::new();
        file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        let entry: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        Ok(Some(entry))
    }

    /// Durable group identity used by per-document reservations across changed batch boundaries.
    pub(super) fn group_key(&self, scope: &str, request: &Value) -> String {
        self.path(scope, request)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// Missing is distinguishable from an admitted pending request; journal creation and entries
    /// are directory-synced before a physical send can begin.
    pub(super) fn group_exists(&self, key: &str) -> io::Result<bool> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(io::Error::other("invalid request group"));
        }
        match fs::symlink_metadata(self.directory.join(format!("{key}.json"))) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Recovers only a confirmed response bound to the original request and configured model space.
    pub(super) fn completed_group(
        &self,
        scope: &str,
        key: &str,
    ) -> io::Result<Option<(Value, EmbeddingBatch)>> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(io::Error::other("invalid request group"));
        }
        let Some(entry) = Self::read_entry(&self.directory.join(format!("{key}.json")))? else {
            return Ok(None);
        };
        let request = entry["request"].clone();
        if self.group_key(scope, &request) != key {
            return Err(io::Error::other("request group identity mismatch"));
        }
        if entry["state"] != "complete" {
            return Ok(None);
        }
        Ok(Some((
            request,
            serde_json::from_value(entry["response"].clone()).map_err(io::Error::other)?,
        )))
    }

    /// Archives an unresolved attempt only for an explicit operator retry; original evidence survives.
    pub(super) fn archive_unresolved(&self, scope: &str, key: &str) -> io::Result<PathBuf> {
        if !self.group_exists(key)? {
            return Err(io::Error::other("unresolved journal missing"));
        }
        let path = self.directory.join(format!("{key}.json"));
        let entry = Self::read_entry(&path)?
            .ok_or_else(|| io::Error::other("unresolved journal missing"))?;
        if self.group_key(scope, &entry["request"]) != key
            || !matches!(entry["state"].as_str(), Some("pending" | "failed"))
        {
            return Err(io::Error::other("not an unresolved request"));
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let archive = self.directory.join(format!(
            "{key}.operator-retry-{}-{nonce}.json",
            std::process::id()
        ));
        // hard_link is a no-overwrite reservation; unlinking the original preserves the archived inode.
        fs::hard_link(&path, &archive)?;
        fs::File::open(&self.directory)?.sync_all()?;
        fs::remove_file(&path)?;
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(archive)
    }

    /// Fsyncs pending state before the transport can attempt a physical send.
    fn begin(&self, scope: &str, request: &Value) -> io::Result<()> {
        let path = self.path(scope, request);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let result = (|| {
            let bytes = serde_json::to_vec(&json!({"state":"pending","request":request}))
                .map_err(io::Error::other)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            drop(file);
            // This call created the file and no HTTP can start before this callback succeeds.
            let _ = fs::remove_file(&path);
            let _ = fs::File::open(&self.directory).and_then(|directory| directory.sync_all());
        }
        result
    }
    /// Replaces only this request's pending entry atomically; failed writes preserve pending state.
    fn finish(
        &self,
        scope: &str,
        request: &Value,
        response: &Value,
        success: bool,
    ) -> io::Result<()> {
        let path = self.path(scope, request);
        let temporary = path.with_extension("next");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let bytes=serde_json::to_vec(&json!({"state":if success {"complete"}else{"failed"},"request":request,"response":response})).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        fs::File::open(&self.directory)?.sync_all()
    }
}

/// Reuses source collection and model clients; no tools, commands or paths come from model output.
pub struct ContextService {
    pub(super) config: ModelConfig,
    pub(super) embedding: Option<EmbeddingClient>,
    jev: Option<JevSelector>,
}
impl ContextService {
    /// Validates explicit request bounds before constructing enabled model clients.
    pub fn new(config: ModelConfig) -> io::Result<Self> {
        if config.max_new_calls > 512
            || !config.include_threshold.is_finite()
            || !(0.0..=1.0).contains(&config.include_threshold)
        {
            return Err(io::Error::other("invalid model request limits"));
        }
        let embedding = config
            .embedding
            .clone()
            .map(EmbeddingClient::new)
            .transpose()
            .map_err(io::Error::other)?;
        let jev = config.jev.as_ref().map(JevSelector::new).transpose()?;
        Ok(Self {
            config,
            embedding,
            jev,
        })
    }

    /// Prepares from an already refreshed index. Model errors remain visible and fall back to
    /// current lexical evidence. The caller must execute source/database phases off worker locks.
    pub async fn prepare(
        &self,
        index: &ContextIndex,
        task: &str,
        packet_options: &PacketOptions,
        document_options: &DocumentOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<Preparation> {
        self.prepare_inner(
            index,
            task,
            packet_options,
            document_options,
            cancel,
            true,
            None,
        )
        .await
    }

    /// Collects complete bounded evidence for a caller-owned carrier. The caller MUST enforce
    /// its actual token/byte ceiling and disclose omissions before publishing this result.
    pub async fn prepare_for_delivery(
        &self,
        index: &ContextIndex,
        task: &str,
        packet_options: &PacketOptions,
        document_options: &DocumentOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<Preparation> {
        self.prepare_inner(
            index,
            task,
            packet_options,
            document_options,
            cancel,
            false,
            None,
        )
        .await
    }

    /// Produces a bounded JSON packet grounded in current public-check observations.
    #[allow(
        clippy::too_many_arguments,
        reason = "existing preparation contract plus explicit public evidence"
    )]
    pub async fn prepare_with_evidence(
        &self,
        index: &ContextIndex,
        task: &str,
        evidence: &super::evidence::PublicEvidence,
        packet_options: &PacketOptions,
        document_options: &DocumentOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<Preparation> {
        let mut result = self
            .prepare_with_evidence_for_delivery(
                index,
                task,
                evidence,
                packet_options,
                document_options,
                cancel,
            )
            .await?;
        result.fit(packet_options)?;
        Ok(result)
    }

    /// Grounds retrieval and Jev in current public-check failures while preserving the owner's task.
    #[allow(
        clippy::too_many_arguments,
        reason = "existing preparation contract plus explicit public evidence"
    )]
    pub async fn prepare_with_evidence_for_delivery(
        &self,
        index: &ContextIndex,
        task: &str,
        evidence: &super::evidence::PublicEvidence,
        packet_options: &PacketOptions,
        document_options: &DocumentOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<Preparation> {
        let query = evidence.retrieval_query(index, task)?;
        let mut result = self
            .prepare_inner(
                index,
                &query,
                packet_options,
                document_options,
                cancel,
                false,
                Some(evidence),
            )
            .await?;
        result.packet.task = task.to_owned();
        result.warnings.push(format!(
            "retrieval grounded in public check {} ({} failure locations)",
            evidence.run_id,
            evidence.failures.len()
        ));
        Ok(result)
    }

    /// Shares retrieval and model effects; only the final public representation's fitting differs.
    #[allow(
        clippy::too_many_arguments,
        reason = "shared preparation includes optional source-validated evidence"
    )]
    async fn prepare_inner(
        &self,
        index: &ContextIndex,
        task: &str,
        packet_options: &PacketOptions,
        document_options: &DocumentOptions,
        cancel: &tokio_util::sync::CancellationToken,
        fit_json: bool,
        evidence: Option<&super::evidence::PublicEvidence>,
    ) -> io::Result<Preparation> {
        let began = Instant::now();
        if task.trim().is_empty() || task.len() > 64 * 1024 {
            return Err(io::Error::other("invalid context task"));
        }
        // A query never initializes or refreshes source/vector state.
        let _ = document_options;
        let ready_space = self
            .config
            .embedding
            .as_ref()
            .map(|c| c.space_id().map_err(io::Error::other))
            .transpose()?
            .unwrap_or_else(|| "lexical".into());
        let ready = index.published_documents(&ready_space)?;
        let journal = (self.embedding.is_some() || self.jev.is_some())
            .then(|| Journal::new(&index.root))
            .transpose()?;
        let mut result = Preparation {
            packet: ContextBundle {
                task: task.into(),
                snapshot: index.snapshot()?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "context_index_not_ready")
                })?,
                windows: Vec::new(),
                file_facts: Vec::new(),
                gaps: Vec::new(),
                estimated_tokens: 0,
                accounting_is_estimated: true,
            },
            embedding_used: false,
            embedding_complete: false,
            jev_used: false,
            calls: Vec::new(),
            warnings: Vec::new(),
            elapsed_ms: 0.0,
        };
        let mut candidates = index.search(task, packet_options.candidates)?;
        let mut remaining = self.config.max_new_calls;
        if let (Some(client), Some(config)) = (&self.embedding, &self.config.embedding) {
            let journal = journal.as_ref().expect("model journal constructed");
            let space = config.space_id().map_err(io::Error::other)?;
            let documents = index.published_documents(&space)?;
            result.warnings.extend(documents.gaps.clone());
            result.embedding_complete = documents.gaps.is_empty();
            let has_vectors = !documents.documents.is_empty();
            if has_vectors {
                match client.request_query(task) {
                    Ok(request) => match self
                        .embedding_call(
                            client,
                            config,
                            journal,
                            &space,
                            &request,
                            &[task.to_owned()],
                            true,
                            &mut remaining,
                            &mut result.calls,
                            cancel,
                        )
                        .await
                    {
                        Ok(response) => {
                            candidates = index.hybrid_search(
                                task,
                                &documents,
                                &space,
                                &response.vectors[0],
                                packet_options.candidates,
                            )?;
                            result.embedding_used = true;
                        }
                        Err(error) => result.warnings.push(error.to_string()),
                    },
                    Err(error) => result.warnings.push(error.to_string()),
                }
            }
            if !result.embedding_complete {
                result.warnings.push(
                    "semantic index coverage is partial; lexical candidates were retained".into(),
                );
            }
        }
        // Deterministic public-evidence tiers by path: failure anchors (0), then the direct
        // imports of anchored tests (1). The selector never drops them and ranks them first.
        let mut pinned: BTreeMap<PathBuf, u8> = BTreeMap::new();
        if let Some(evidence) = evidence {
            // Actual failure locations must reach the selector even if similarity ranks them poorly.
            let records = index.records()?;
            let mut anchors = Vec::new();
            let mut sources = Vec::new();
            for failure in &evidence.failures {
                if anchors
                    .iter()
                    .any(|c: &super::Candidate| c.file.path == failure.path)
                {
                    continue;
                }
                if let Some(file) = records.iter().find(|r| r.path == failure.path) {
                    let mut candidate = candidates
                        .iter()
                        .find(|c| c.file.path == file.path)
                        .cloned()
                        .unwrap_or_else(|| super::Candidate {
                            file: file.clone(),
                            lexical_score: 0,
                            reasons: Vec::new(),
                            preferred_spans: Vec::new(),
                            whole_file: false,
                        });
                    let source = super::packet::read_current(
                        &index.root,
                        file,
                        packet_options.max_file_bytes,
                        &mut 0,
                    )?;
                    let mut offsets = vec![0];
                    offsets.extend(source.match_indices('\n').map(|(at, _)| at + 1));
                    // Failure lines override unrelated semantic windows; packet construction
                    // expands them to complete enclosing declarations where the parser can.
                    let spans: Vec<_> = evidence
                        .failures
                        .iter()
                        .filter(|f| f.path == file.path)
                        .filter_map(|f| f.line)
                        .filter_map(|line| {
                            let start = *offsets.get(line as usize - 1)?;
                            let end = offsets.get(line as usize).copied().unwrap_or(source.len());
                            (start < end).then_some((start, end))
                        })
                        .collect();
                    if !spans.is_empty() {
                        candidate.preferred_spans = spans;
                    }
                    candidate.reasons.insert(0, format!("public check {} failure location; investigation anchor, not a proven defect", evidence.run_id));
                    if let Some(diagnostic) = public_failure_reason(failure) {
                        candidate.reasons.insert(1, diagnostic);
                    }
                    anchors.push(candidate);
                    let test = crate::lang::Language::for_path(&file.path)
                        .is_some_and(|language| language.support().is_test_file(&file.path));
                    sources.push((file.path.clone(), source, test));
                }
            }
            // The code an anchor exercises or depends on is usually what it imports, which a
            // failure inside the anchor itself never names.
            let imported = anchored_imports(
                &index.root,
                &records,
                &candidates,
                &anchors,
                &sources,
                &mut result.warnings,
                cancel,
                task,
            )
            .await?;
            pinned = anchors
                .iter()
                .map(|candidate| (candidate.file.path.clone(), 0))
                .chain(
                    imported
                        .iter()
                        .map(|(candidate, tier)| (candidate.file.path.clone(), *tier)),
                )
                .collect();
            anchors.extend(imported.into_iter().map(|(candidate, _)| candidate));
            // Observed failure files supplement the ranked retrieval quota; they must not
            // evict its tail. Both inputs are already bounded, with a final packet safety cap.
            let (combined, omitted) = merge_anchor_candidates(anchors, candidates);
            if omitted > 0 {
                result.warnings.push(format!(
                    "{omitted} ranked candidates omitted at the 256-file safety limit"
                ));
            }
            candidates = combined;
        }
        let ready_files: BTreeSet<_> = ready.documents.iter().map(|d| &d.path).collect();
        candidates.retain(|c| ready_files.contains(&c.file.path));
        if self.jev.is_none() {
            rank_without_selector(&mut candidates, &pinned);
        }
        if let Some(client) = &self.jev {
            let journal = journal.as_ref().expect("model journal constructed");
            // The main agent's output budget must not hide candidates from the selector.
            let (views, notes) = candidate_evidence(index, &candidates, packet_options);
            result.warnings.extend(notes);
            let scope = format!(
                "jev:{}",
                blake3::hash(
                    serde_json::to_string(self.config.jev.as_ref().unwrap())
                        .map_err(io::Error::other)?
                        .as_bytes()
                )
                .to_hex()
            );
            let mut position = 0;
            let mut assessments = BTreeMap::new();
            while position < views.len() {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "context cancelled",
                    ));
                }
                let mut end = (position + 16).min(views.len());
                while end > position && !client.fits(task, &views[position..end]) {
                    end -= 1;
                }
                if end == position {
                    result.warnings.push(format!(
                        "{}: evidence exceeds Jev request limit",
                        views[position].path.display()
                    ));
                    position += 1;
                    continue;
                }
                let batch = &views[position..end];
                let request = client.request(task, batch);
                let selection = match journal.cached(&scope, &request) {
                    Ok(Some(response)) => serde_json::from_value::<Selection>(response)
                        .map(|value| (value, true))
                        .map_err(io::Error::other),
                    Ok(None) if remaining > 0 => {
                        remaining -= 1;
                        let call = tokio::select! {_ = cancel.cancelled()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"context cancelled")),call=client.assess(task,batch,|request|journal.begin(&scope,request).map_err(|_|()))=>call};
                        match call {
                            Ok(value) => {
                                journal.finish(
                                    &scope,
                                    &request,
                                    &serde_json::to_value(&value).map_err(io::Error::other)?,
                                    true,
                                )?;
                                Ok((value, false))
                            }
                            Err(error) => {
                                if error.attempted {
                                    journal.finish(
                                        &scope,
                                        &request,
                                        &serde_json::to_value(&error).map_err(io::Error::other)?,
                                        false,
                                    )?;
                                }
                                result.calls.push(CallRecord {
                                    kind: "jev".into(),
                                    replayed: false,
                                    elapsed_ms: error.elapsed_ms,
                                    usage: None,
                                    error: Some(serde_json::to_value(&error).unwrap()),
                                });
                                Err(io::Error::other(error))
                            }
                        }
                    }
                    Ok(None) => Err(io::Error::other("model call budget exhausted")),
                    Err(error) => Err(error),
                };
                match selection {
                    Ok((selection, replayed)) => {
                        if selection.request != request
                            || selection.assessments.len() != batch.len()
                            || selection
                                .assessments
                                .iter()
                                .map(|value| &value.id)
                                .collect::<BTreeSet<_>>()
                                .len()
                                != batch.len()
                            || selection.assessments.iter().any(|value| {
                                !batch.iter().any(|view| view.id == value.id)
                                    || !value.relevance.is_finite()
                                    || !value.expansion.is_finite()
                                    || !(0.0..=1.0).contains(&value.relevance)
                                    || !(0.0..=1.0).contains(&value.expansion)
                            })
                        {
                            return Err(io::Error::other("cached Jev assessments are invalid"));
                        }
                        result.calls.push(CallRecord {
                            kind: "jev".into(),
                            replayed,
                            elapsed_ms: if replayed { 0.0 } else { selection.elapsed_ms },
                            usage: selection.usage,
                            error: None,
                        });
                        for assessment in selection.assessments {
                            assessments.insert(assessment.id.clone(), assessment);
                        }
                        result.jev_used = true;
                    }
                    Err(error) => result.warnings.push(error.to_string()),
                }
                position = end;
            }
            apply_assessments(
                &mut candidates,
                &assessments,
                &pinned,
                task,
                self.config.include_threshold,
            );
        }
        // Collect under the unchanged byte/read limits, then apply the actual output budget
        // fairly across files. This private ceiling is not a model context allowance.
        let mut collection = packet_options.clone();
        collection.context_tokens = (collection.max_read_bytes as u64)
            .saturating_mul(8)
            .saturating_add(262144);
        collection.reserved_tokens = 0;
        result.packet = index.prepare_candidates(task, &collection, candidates)?;
        result.packet.gaps.extend(ready.gaps);
        interleave(&mut result.packet.windows);
        result.elapsed_ms = began.elapsed().as_secs_f64() * 1000.0;
        if fit_json {
            result.fit(packet_options)?;
        }
        Ok(result)
    }

    /// Replays only completed validated batches; pending/failed outcomes never trigger automatic retries.
    #[allow(
        clippy::too_many_arguments,
        reason = "one bounded exchange carries explicit accounting and cancellation"
    )]
    pub(super) async fn embedding_call(
        &self,
        client: &EmbeddingClient,
        config: &EmbeddingConfig,
        journal: &Journal,
        scope: &str,
        request: &Value,
        inputs: &[String],
        query: bool,
        remaining: &mut usize,
        records: &mut Vec<CallRecord>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<EmbeddingBatch> {
        let cached = journal.cached(scope, request)?;
        let (batch, replayed) = if let Some(response) = cached {
            (
                serde_json::from_value::<EmbeddingBatch>(response).map_err(io::Error::other)?,
                true,
            )
        } else {
            if *remaining == 0 {
                return Err(io::Error::other("model call budget exhausted"));
            }
            *remaining -= 1;
            let call = async {
                if query {
                    client
                        .embed_query(&inputs[0], |request| {
                            journal.begin(scope, request).map_err(|_| ())
                        })
                        .await
                } else {
                    client
                        .embed_documents(inputs, |request| {
                            journal.begin(scope, request).map_err(|_| ())
                        })
                        .await
                }
            };
            let response = tokio::select! {_ = cancel.cancelled()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"context cancelled")),response=call=>response};
            match response {
                Ok(batch) => {
                    journal.finish(
                        scope,
                        request,
                        &serde_json::to_value(&batch).map_err(io::Error::other)?,
                        true,
                    )?;
                    (batch, false)
                }
                Err(error) => {
                    if error.attempted {
                        journal.finish(
                            scope,
                            request,
                            &serde_json::to_value(&error).map_err(io::Error::other)?,
                            false,
                        )?;
                    }
                    records.push(CallRecord {
                        kind: "embedding".into(),
                        replayed: false,
                        elapsed_ms: error.elapsed_ms,
                        usage: None,
                        error: Some(serde_json::to_value(&error).unwrap()),
                    });
                    return Err(io::Error::other(error));
                }
            }
        };
        if batch.model != config.response_model.as_deref().unwrap_or(&config.model)
            || batch.vectors.len() != inputs.len()
            || batch.vectors.iter().any(|vector| {
                vector.len() != config.dimensions
                    || vector.iter().any(|n| !n.is_finite())
                    || vector.iter().all(|n| *n == 0.0)
            })
        {
            return Err(io::Error::other("cached embedding response is invalid"));
        }
        records.push(CallRecord {
            kind: "embedding".into(),
            replayed,
            elapsed_ms: if replayed { 0.0 } else { batch.elapsed_ms },
            usage: Some(serde_json::to_value(&batch.usage).map_err(io::Error::other)?),
            error: None,
        });
        Ok(batch)
    }
}

impl Preparation {
    /// Bounds the actual serialized response, including accounting and omitted-evidence notes.
    /// Task text is never clipped; metadata that cannot fit produces an explicit error.
    fn fit(&mut self, options: &PacketOptions) -> io::Result<()> {
        let omitted = self.warnings.len().saturating_sub(16);
        self.warnings.truncate(16);
        for warning in &mut self.warnings {
            if warning.len() > 1024 {
                let mut end = 1024;
                while !warning.is_char_boundary(end) {
                    end -= 1;
                }
                warning.truncate(end);
                warning.push_str("... (note truncated)");
            }
        }
        if omitted > 0 {
            self.warnings.push(format!(
                "{omitted} additional coverage notes omitted from this response"
            ));
        }
        loop {
            let encoded = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
            let estimate = crate::project::token_estimate::estimate_tokens(
                &encoded,
                options.characters_per_token,
                options.context_tokens,
                options.reserved_tokens,
            )
            .map_err(io::Error::other)?;
            if estimate.estimated_fits {
                if self.packet.estimated_tokens == estimate.estimated_tokens {
                    return Ok(());
                }
                self.packet.estimated_tokens = estimate.estimated_tokens;
            } else {
                let Some(window) = self.packet.windows.pop() else {
                    return Err(io::Error::other(
                        "task and preparation metadata exceed context budget",
                    ));
                };
                self.packet.gaps.push(format!(
                    "{}: omitted to fit complete response budget",
                    window.path.display()
                ));
                self.packet.file_facts.retain(|facts| {
                    self.packet
                        .windows
                        .iter()
                        .any(|window| window.path == facts.path)
                });
            }
        }
    }
}

/// Gives every retrieved candidate bounded, fresh evidence independently of the delivery budget.
/// Small files are complete; larger cards expose headers/imports/declaration names and source spans.
fn candidate_evidence(
    index: &ContextIndex,
    candidates: &[super::Candidate],
    options: &PacketOptions,
) -> (Vec<CandidateView>, Vec<String>) {
    use crate::project::structure::Detail;
    let mut views = Vec::new();
    let mut notes = Vec::new();
    let mut read = 0;
    for candidate in candidates {
        let file = &candidate.file;
        let remaining = options
            .max_read_bytes
            .saturating_sub(read)
            .min(options.max_file_bytes);
        let source = match super::packet::read_current(&index.root, file, remaining, &mut read) {
            Ok(source) => source,
            Err(error) => {
                notes.push(format!(
                    "{}: selector evidence unavailable: {error}",
                    file.path.display()
                ));
                continue;
            }
        };
        let partial = source.len() > 6144;
        let evidence = if !partial {
            source.clone()
        } else {
            let header = match &file.header {
                Detail::Available(value) => prefix(&value.text, 768),
                _ => "",
            };
            let imports = match &file.imports {
                Detail::Available(value) => value
                    .statements
                    .iter()
                    .map(|value| value.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let mut names = Vec::new();
            if let Some(Detail::Available(symbols)) = &file.symbols {
                for symbol in symbols {
                    symbol.walk(&mut |symbol| {
                        names.push(format!(
                            "{}:{}-{}",
                            symbol.name, symbol.range.start, symbol.range.end
                        ));
                    });
                }
            }
            let names = names.join("\n");
            let mut evidence = format!(
                "Header excerpt:\n{}\nImports excerpt:\n{}\nDeclaration-name excerpt:\n{}\n",
                header,
                prefix(&imports, 768),
                prefix(&names, 1536)
            );
            let spans: Vec<_> = candidate
                .preferred_spans
                .iter()
                .copied()
                .filter(|&(start, end)| {
                    start < end
                        && end <= source.len()
                        && source.is_char_boundary(start)
                        && source.is_char_boundary(end)
                })
                .take(2)
                .collect();
            let spans = if spans.is_empty() {
                vec![(0, source.len())]
            } else {
                spans
            };
            for (start, end) in spans {
                // A stack-frame line alone often hides the operation and its setup.
                // Include nearby code while keeping the existing per-card byte ceiling.
                let (mut start, mut end) = if end - start < 512 {
                    (
                        start.saturating_sub(512),
                        end.saturating_add(512).min(source.len()),
                    )
                } else {
                    (start, end)
                };
                while !source.is_char_boundary(start) {
                    start += 1;
                }
                while !source.is_char_boundary(end) {
                    end -= 1;
                }
                let excerpt = prefix(&source[start..end], 1536);
                evidence.push_str(&format!(
                    "Source bytes {}..{}:\n{}\n",
                    start,
                    start + excerpt.len(),
                    excerpt
                ));
            }
            evidence
        };
        views.push(CandidateView {
            id: file.id.clone(),
            path: file.path.clone(),
            content_hash: file.content_hash.clone().unwrap_or_default(),
            source: evidence,
            partial,
            reasons: candidate.reasons.clone(),
        });
    }
    (views, notes)
}

/// Applies the selector's judgments without changing delivery shape. Unknown candidates, relevant
/// ones and those whose exclusion needs more evidence (`expansion` ≥ 0.5) survive, as do
/// public-evidence tiers and task-named paths. An expansion answer is uncertainty about
/// excluding a candidate, not a request for its whole file: preferred spans and `whole_file`
/// stay as retrieval set them, and the dependency evidence comes from the anchors' direct
/// imports. Both estimates are recorded as reasons.
fn apply_assessments(
    candidates: &mut Vec<super::Candidate>,
    assessments: &BTreeMap<String, super::select::Assessment>,
    pinned: &BTreeMap<PathBuf, u8>,
    task: &str,
    include_threshold: f64,
) {
    candidates.retain(|candidate| {
        pinned.contains_key(&candidate.file.path)
            || task.contains(candidate.file.path.to_string_lossy().as_ref())
            || assessments
                .get(&candidate.file.id)
                .is_none_or(|value| value.relevance >= include_threshold || value.expansion >= 0.5)
    });
    rank_selected(candidates, pinned, task, |candidate| {
        assessments
            .get(&candidate.file.id)
            .map_or(0.5, |value| value.relevance)
    });
    for candidate in candidates {
        if let Some(value) = assessments.get(&candidate.file.id) {
            candidate.reasons.push(format!(
                "Jev relevance estimate: {:.2}; additional evidence: {:.2}",
                value.relevance, value.expansion
            ));
        }
    }
}

/// Rank discovered imports without replacing the existing lexical/hybrid order of other files.
fn rank_without_selector(candidates: &mut [super::Candidate], pinned: &BTreeMap<PathBuf, u8>) {
    let tier = |candidate: &super::Candidate| {
        pinned
            .get(&candidate.file.path)
            .copied()
            .map(import_priority)
            .unwrap_or(5)
    };
    candidates.sort_by(|a, b| {
        let (left, right) = (tier(a), tier(b));
        left.cmp(&right).then_with(|| {
            if matches!(left, 1 | 4) {
                b.lexical_score.cmp(&a.lexical_score)
            } else {
                std::cmp::Ordering::Equal
            }
        })
    });
}

/// Keeps failure anchors in evidence order, then ranks runtime/unknown imports together by
/// relevance regardless of their test/source provenance. Task-named paths, proven type-only
/// imports and other candidates follow. Lexical score breaks relevance ties; reasons stay intact.
fn rank_selected(
    candidates: &mut [super::Candidate],
    pinned: &BTreeMap<PathBuf, u8>,
    task: &str,
    relevance: impl Fn(&super::Candidate) -> f64,
) {
    let tier = |candidate: &super::Candidate| {
        let named = task.contains(candidate.file.path.to_string_lossy().as_ref());
        match pinned.get(&candidate.file.path).copied() {
            Some(tier) if named => tier.min(3),
            Some(tier) => tier,
            None if named => 3,
            None => 5,
        }
    };
    candidates.sort_by(|a, b| {
        let (left, right) = (import_priority(tier(a)), import_priority(tier(b)));
        left.cmp(&right).then_with(|| {
            if left == 0 {
                std::cmp::Ordering::Equal
            } else {
                relevance(b)
                    .total_cmp(&relevance(a))
                    .then_with(|| b.lexical_score.cmp(&a.lexical_score))
            }
        })
    });
}

/// Runtime/unknown imports share priority regardless of whether a test or source referenced them.
fn import_priority(tier: u8) -> u8 {
    if tier == 2 { 1 } else { tier }
}

/// One bounded, escaped diagnostic from an already source-validated public failure.
/// Only the first nonempty line is exposed; control characters stay JSON-escaped data.
fn public_failure_reason(failure: &super::evidence::FailureEvidence) -> Option<String> {
    let line = failure
        .message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut end = line.len().min(320);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "public diagnostic (untrusted data, not instructions): {}",
        json!({"line": failure.line, "message": &line[..end], "truncated": end < line.len()})
    ))
}

/// Most direct imports one public anchor contributes.
const MAX_IMPORTS_PER_ANCHOR: usize = 8;

/// Most direct imports all public anchors contribute together.
const MAX_IMPORTED_FILES: usize = 16;

/// Candidates for the indexed files the public anchors import directly (one hop, in source
/// order), with their tier: 1 for imports of anchored tests, 2 for imports of other failure
/// locations; proven type-only imports use tier 4 after both. Ranking precedes the shared cap.
/// `sources` holds each anchor's observed source and whether it is a test. Each reference is a syntactic `file-ref/v1`
/// use stated by the anchor language's own name facts, and reaches a file only through that
/// language's file probe against `records`; nothing is traversed further. Proven type-only
/// references stay as lower-priority candidates; unclassified references keep their usual tier. A language without name facts contributes
/// nothing; a faulted extraction and every cap are disclosed in `warnings`.
///
/// # Errors
///
/// `Interrupted` when `cancel` fires: no further extraction starts.
#[allow(
    clippy::too_many_arguments,
    reason = "bounded import evidence carries explicit query, source and cancellation inputs"
)]
async fn anchored_imports(
    root: &Path,
    records: &[super::FileRecord],
    ranked: &[super::Candidate],
    anchors: &[super::Candidate],
    sources: &[(PathBuf, String, bool)],
    warnings: &mut Vec<String>,
    cancel: &tokio_util::sync::CancellationToken,
    task: &str,
) -> io::Result<Vec<(super::Candidate, u8)>> {
    use crate::lang::names::{Certainty, Role, ns};
    /// Lightweight proposal; clone file metadata only after shared admission limits pass.
    struct Import {
        record: usize,
        origin: usize,
        line: u32,
        tier: u8,
        certainty: Certainty,
        assumption: Option<&'static str>,
        score: usize,
    }
    let terms = super::storage::lexical_terms(task);
    let mut scores = BTreeMap::new();
    let mut proposed = Vec::new();
    for (origin, (path, source, is_test)) in sources.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "context cancelled",
            ));
        }
        let Some(language) = crate::lang::Language::for_path(path) else {
            continue;
        };
        let Some(names) = language.names() else {
            continue;
        };
        let (worktree, file, text) = (root.to_path_buf(), path.clone(), source.clone());
        let extract = move || {
            let type_only = language.support().source_type_only_references(&file, &text);
            (
                crate::intelligence::names::facts_of(&worktree, language, &file, text.as_bytes()),
                type_only,
            )
        };
        let (facts, ranges) = if crate::intelligence::anchors::routed(language).is_some() {
            match tokio::task::spawn_blocking(extract).await {
                Ok(value) => value,
                Err(_) => {
                    warnings.push(format!(
                        "{}: direct imports not resolved (extraction task failed)",
                        path.display()
                    ));
                    continue;
                }
            }
        } else {
            extract()
        };
        let facts = match facts {
            Ok(facts) => facts.unwrap_or_default(),
            Err(fault) => {
                warnings.push(format!(
                    "{}: direct imports not resolved ({fault})",
                    path.display()
                ));
                continue;
            }
        };
        let type_only: Vec<_> = ranges
            .into_iter()
            .filter(|range| {
                range.start < range.end
                    && range.end <= source.len()
                    && source.is_char_boundary(range.start)
                    && source.is_char_boundary(range.end)
            })
            .collect();
        let offsets: Vec<_> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        for fact in facts {
            if fact.key.namespace != ns::FILE_REF || fact.role != Role::Use {
                continue;
            }
            let Some((target, assumption)) = names.file_probe().reach(&fact.key.name, |name| {
                records.iter().any(|record| record.path == Path::new(name))
            }) else {
                continue;
            };
            if Path::new(&target) == path
                || anchors
                    .iter()
                    .any(|anchor| anchor.file.path == Path::new(&target))
            {
                continue;
            }
            let Some(record) = records
                .iter()
                .position(|record| record.path == Path::new(&target))
            else {
                continue;
            };
            let tier = if reference_is_type_only(&offsets, fact.line, fact.column, &type_only) {
                4
            } else if *is_test {
                1
            } else {
                2
            };
            let score = *scores
                .entry(record)
                .or_insert_with(|| super::storage::lexical_reasons(&records[record], &terms).len());
            proposed.push(Import {
                score,
                record,
                origin,
                line: fact.line,
                tier,
                certainty: fact.certainty,
                assumption,
            });
        }
    }
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "context cancelled",
        ));
    }
    // Rank before applying either cap: an early type-only import must not consume a slot
    // needed by a later runtime reference, including another reference to the same file.
    proposed.sort_by(|a, b| {
        import_priority(a.tier)
            .cmp(&import_priority(b.tier))
            .then_with(|| b.score.cmp(&a.score))
            .then(a.tier.cmp(&b.tier))
    });
    let mut imported = Vec::new();
    let mut seen = BTreeSet::new();
    let mut admitted = vec![0; sources.len()];
    let mut beyond = vec![0; sources.len()];
    for item in proposed {
        if seen.contains(&item.record) {
            continue;
        }
        if admitted[item.origin] == MAX_IMPORTS_PER_ANCHOR || imported.len() == MAX_IMPORTED_FILES {
            beyond[item.origin] += 1;
            continue;
        }
        seen.insert(item.record);
        admitted[item.origin] += 1;
        let record = &records[item.record];
        let (path, _, is_test) = &sources[item.origin];
        let certainty = match (item.certainty, item.assumption) {
            (Certainty::Exact, None) => "exact".to_owned(),
            (Certainty::Exact, Some(reach)) => reach.to_owned(),
            (Certainty::Heuristic(read), None) => read.to_owned(),
            (Certainty::Heuristic(read), Some(reach)) => format!("{read}, {reach}"),
        };
        let mut candidate = ranked
            .iter()
            .find(|candidate| candidate.file.path == record.path)
            .cloned()
            .unwrap_or_else(|| super::Candidate {
                file: record.clone(),
                lexical_score: item.score,
                reasons: super::storage::lexical_reasons(record, &terms),
                preferred_spans: Vec::new(),
                whole_file: false,
            });
        candidate.lexical_score = item.score;
        let anchor = if *is_test {
            "anchored public test"
        } else {
            "public failure location"
        };
        let kind = if item.tier == 4 {
            "; proven type-only"
        } else {
            ""
        };
        candidate.reasons.insert(
            0,
            format!(
                "direct import of {anchor} {} (line {}; syntactic reference, {certainty}{kind})",
                path.display(),
                item.line
            ),
        );
        imported.push((candidate, item.tier));
    }
    for (origin, count) in beyond
        .into_iter()
        .enumerate()
        .filter(|(_, count)| *count > 0)
    {
        warnings.push(format!(
            "{}: {count} further direct imports omitted at the import cap",
            sources[origin].0.display()
        ));
    }
    Ok(imported)
}

/// Match a fact's one-based byte position to exact specifier ranges, never to its whole line.
fn reference_is_type_only(
    offsets: &[usize],
    line: u32,
    column: u32,
    ranges: &[std::ops::Range<usize>],
) -> bool {
    let Some(at) = line
        .checked_sub(1)
        .and_then(|line| offsets.get(line as usize))
        .and_then(|start| {
            column
                .checked_sub(1)
                .and_then(|column| start.checked_add(column as usize))
        })
    else {
        return false;
    };
    if offsets.get(line as usize).is_some_and(|next| at >= *next) {
        return false;
    }
    ranges.iter().any(|range| range.contains(&at))
}

/// Keeps validated anchors first without spending the ranked retrieval quota on them.
/// Duplicate files use the anchor's current failure spans; the packet's hard limit stays 256.
fn merge_anchor_candidates(
    mut anchors: Vec<super::Candidate>,
    ranked: Vec<super::Candidate>,
) -> (Vec<super::Candidate>, usize) {
    for candidate in ranked {
        if !anchors
            .iter()
            .any(|anchor| anchor.file.path == candidate.file.path)
        {
            anchors.push(candidate);
        }
    }
    let omitted = anchors.len().saturating_sub(256);
    anchors.truncate(256);
    (anchors, omitted)
}

/// Returns a byte-bounded UTF-8 excerpt; its enclosing card always declares incompleteness.
fn prefix(text: &str, mut limit: usize) -> &str {
    limit = limit.min(text.len());
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    &text[..limit]
}

/// Gives each selected file one window before spending remaining space on a second window.
fn interleave(windows: &mut Vec<super::SourceWindow>) {
    let mut groups: Vec<std::collections::VecDeque<super::SourceWindow>> = Vec::new();
    for window in std::mem::take(windows) {
        if groups
            .last()
            .is_some_and(|group| group.front().is_some_and(|first| first.path == window.path))
        {
            groups.last_mut().unwrap().push_back(window);
        } else {
            groups.push(std::collections::VecDeque::from([window]));
        }
    }
    loop {
        let before = windows.len();
        for group in &mut groups {
            if let Some(window) = group.pop_front() {
                windows.push(window);
            }
        }
        if windows.len() == before {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selector sees both files even when neither source fits the main agent's small budget.
    #[test]
    fn selector_evidence_survives_a_small_delivery_budget() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("context-evidence");
        let source = "x".repeat(3000);
        for name in ["needle-a.alpha", "needle-b.alpha"] {
            std::fs::write(root.join(name), &source).unwrap();
        }
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&crate::project::structure::StructureOptions::default())
            .unwrap();
        let options = PacketOptions {
            context_tokens: 1600,
            reserved_tokens: 1000,
            ..Default::default()
        };
        assert!(
            index
                .prepare("needle", &options)
                .unwrap()
                .windows
                .is_empty()
        );
        let candidates = index.search("needle", 12).unwrap();
        let (views, notes) = candidate_evidence(&index, &candidates, &options);
        assert!(notes.is_empty());
        assert_eq!(views.len(), 2);
        assert!(
            views
                .iter()
                .all(|view| view.source == source && !view.partial)
        );
    }

    /// Four observed failures do not evict the tail of twelve independently ranked files.
    #[test]
    fn public_anchors_supplement_the_ranked_quota() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("context-anchor-quota");
        for number in 0..16 {
            std::fs::write(root.join(format!("needle-{number:02}.alpha")), "x").unwrap();
        }
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&crate::project::structure::StructureOptions::default())
            .unwrap();
        let all = index.search("needle", 16).unwrap();
        let mut anchors = all[..4].to_vec();
        anchors[0].preferred_spans = vec![(0, 1)];
        let (combined, omitted) = merge_anchor_candidates(anchors.clone(), all[4..].to_vec());
        assert_eq!(omitted, 0);
        assert_eq!(combined.len(), 16);
        assert_eq!(combined.last().unwrap().file.path, all[15].file.path);
        let (deduplicated, _) = merge_anchor_candidates(anchors, all);
        assert_eq!(deduplicated.len(), 16);
        assert_eq!(deduplicated[0].preferred_spans, vec![(0, 1)]);
    }

    /// Expansion closes a large selected file, including facts outside the initial semantic span.
    #[test]
    fn requested_expansion_materializes_the_full_file() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("context-expansion");
        let source = format!("{}\nend", "x".repeat(10000));
        std::fs::write(root.join("needle.alpha"), &source).unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&crate::project::structure::StructureOptions::default())
            .unwrap();
        let mut candidates = index.search("needle", 12).unwrap();
        assert_eq!(candidates.len(), 1);
        candidates[0].preferred_spans = vec![(9000, 9010)];
        candidates[0].whole_file = true;
        let packet = index
            .prepare_candidates("needle", &PacketOptions::default(), candidates)
            .unwrap();
        assert_eq!(packet.windows.len(), 1);
        assert_eq!(packet.windows[0].text, source);
        assert!(packet.windows[0].complete_file && packet.windows[0].unit_complete);
    }

    /// The first retained prefix covers distinct files rather than several windows of one file.
    #[test]
    fn first_windows_cover_distinct_files() {
        let make = |name: &str, start| super::super::SourceWindow {
            path: name.into(),
            content_hash: "hash".into(),
            start_byte: start,
            end_byte: start + 1,
            text: "x".into(),
            complete_file: false,
            reasons: Vec::new(),
            ..Default::default()
        };
        let mut windows = vec![
            make("a", 0),
            make("a", 1),
            make("a", 2),
            make("b", 0),
            make("c", 0),
        ];
        interleave(&mut windows);
        assert_eq!(
            windows
                .iter()
                .map(|window| window.path.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "a", "a"]
        );
    }

    /// Indexes `files` (path, text) lexically under a fresh scratch root.
    async fn lexical_index(
        name: &str,
        files: &[(&str, String)],
    ) -> (crate::scratch::ScratchDir, ContextIndex, ContextService) {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new(name);
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let service = ContextService::new(ModelConfig {
            embedding: None,
            jev: None,
            max_new_calls: 0,
            include_threshold: 0.5,
        })
        .unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        let scope = crate::project::structure::StructureOptions {
            respect_gitignore: false,
            ..Default::default()
        };
        service
            .index_project(
                &mut index,
                &scope,
                &DocumentOptions::default(),
                &super::super::indexer::IndexBudget {
                    max_new_calls: 0,
                    max_input_bytes: 0,
                },
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        (root, index, service)
    }

    /// An anchored public test's direct syntactic imports become candidates after the anchors,
    /// reached through the language's own probe, in source order, labelled with the test and
    /// certainty. Unreachable references, the test itself and anchored files add nothing, and
    /// the per-test cap is disclosed.
    #[tokio::test]
    async fn anchored_tests_contribute_their_direct_imports() {
        let mut refs: Vec<String> = (0..10).map(|n| format!("ref:lib/m{n}")).collect();
        refs.splice(
            0..0,
            [
                "ref:sub/a".into(),
                "ref:missing".into(),
                "ref:test_a.beta".into(),
                "ref:anchor".into(),
            ],
        );
        let test = format!("{}\nfail here\n", refs.join(" "));
        let mut files = vec![
            ("test_a.beta", test.clone()),
            ("sub/a.alpha", "subject\n".to_owned()),
            ("anchor.alpha", "anchored\n".to_owned()),
        ];
        for n in 0..10 {
            files.push((
                Box::leak(format!("lib/m{n}.alpha").into_boxed_str()),
                "m\n".to_owned(),
            ));
        }
        let (root, index, _) = lexical_index("context-anchored-imports", &files).await;
        let records = index.records().unwrap();
        let candidate = |path: &str| super::super::Candidate {
            file: records
                .iter()
                .find(|record| record.path == Path::new(path))
                .unwrap()
                .clone(),
            lexical_score: 0,
            reasons: Vec::new(),
            preferred_spans: Vec::new(),
            whole_file: false,
        };
        let anchors = [candidate("test_a.beta"), candidate("anchor.alpha")];
        let mut warnings = Vec::new();
        let imported = anchored_imports(
            &root,
            &records,
            &[],
            &anchors,
            &[(PathBuf::from("test_a.beta"), test.clone(), true)],
            &mut warnings,
            &tokio_util::sync::CancellationToken::new(),
            "",
        )
        .await
        .unwrap();
        assert!(imported.iter().all(|(_, tier)| *tier == 1));
        let imported: Vec<_> = imported
            .into_iter()
            .map(|(candidate, _)| candidate)
            .collect();
        let paths: Vec<_> = imported
            .iter()
            .map(|candidate| candidate.file.path.to_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "sub/a.alpha",
                "lib/m0.alpha",
                "lib/m1.alpha",
                "lib/m2.alpha",
                "lib/m3.alpha",
                "lib/m4.alpha",
                "lib/m5.alpha",
                "lib/m6.alpha"
            ]
        );
        assert_eq!(
            imported[0].reasons,
            [
                "direct import of anchored public test test_a.beta (line 1; syntactic reference, extension probe)"
            ]
        );
        assert_eq!(
            warnings,
            ["test_a.beta: 3 further direct imports omitted at the import cap"]
        );
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        let refused = anchored_imports(
            &root,
            &records,
            &[],
            &anchors,
            &[(PathBuf::from("test_a.beta"), test, true)],
            &mut Vec::new(),
            &cancelled,
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Interrupted);
    }

    /// End to end on lexical retrieval: a failure inside a public test delivers the file the
    /// test imports, which neither the failure nor the task names.
    #[tokio::test]
    async fn a_failing_test_delivers_its_imported_subject() {
        let mut files = vec![
            ("test_a.beta", "ref:sub/a\nfail here\n".to_owned()),
            ("sub/a.alpha", "subject\n".to_owned()),
        ];
        for n in 0..12 {
            files.push((
                Box::leak(format!("needle-{n:02}.alpha").into_boxed_str()),
                "needle\n".to_owned(),
            ));
        }
        let (_root, index, service) = lexical_index("context-anchored-subject", &files).await;
        let hash = index
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.path == Path::new("test_a.beta"))
            .unwrap()
            .content_hash
            .unwrap();
        let evidence = super::super::evidence::PublicEvidence {
            run_id: "tests-1".into(),
            failures: vec![super::super::evidence::FailureEvidence {
                path: "test_a.beta".into(),
                content_hash: hash,
                line: Some(2),
                message: "expected false to be true\nprivate stack tail".into(),
            }],
        };
        let options = PacketOptions {
            candidates: 4,
            ..Default::default()
        };
        let prepared = service
            .prepare_with_evidence_for_delivery(
                &index,
                "needle",
                &evidence,
                &options,
                &DocumentOptions::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let paths: Vec<_> = prepared
            .packet
            .windows
            .iter()
            .map(|window| window.path.to_str().unwrap())
            .collect();
        assert_eq!(paths[..2], ["test_a.beta", "sub/a.alpha"], "{paths:?}");
        let reasons = &prepared.packet.windows[0].reasons;
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("expected false to be true"))
        );
        assert!(
            reasons
                .iter()
                .all(|reason| !reason.contains("private stack tail"))
        );
        assert!(
            prepared.packet.windows[1].reasons[0]
                .starts_with("direct import of anchored public test test_a.beta")
        );
    }

    /// Diagnostic rendering preserves Unicode boundaries and cannot inject raw output lines.
    #[test]
    fn public_failure_messages_are_bounded_data() {
        let mut failure = super::super::evidence::FailureEvidence {
            path: "test_a.beta".into(),
            content_hash: "unused".into(),
            line: Some(7),
            message: format!("\n\"{}\"\nnot another note", "я".repeat(300)),
        };
        let reason = public_failure_reason(&failure).unwrap();
        assert!(!reason.contains('\n'));
        let value: Value = serde_json::from_str(reason.split_once(": ").unwrap().1).unwrap();
        assert!(value["message"].as_str().unwrap().len() <= 320);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["line"], 7);
        failure.message = " \n\t".into();
        assert!(public_failure_reason(&failure).is_none());
    }

    /// Known type-only references cannot consume the shared cap ahead of later runtime imports.
    #[tokio::test]
    async fn runtime_imports_precede_type_only_imports_before_caps() {
        let first = format!(
            "!types {}\n",
            (0..8)
                .map(|n| format!("ref:t{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let second = format!(
            "!types {}\n",
            (8..16)
                .map(|n| format!("ref:t{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let runtime = "ref:r0 ref:r1\n".to_owned();
        let mut files = vec![
            ("test_a.beta", first.clone()),
            ("test_b.beta", second.clone()),
            ("lib.beta", runtime.clone()),
        ];
        for name in (0..16)
            .map(|n| format!("t{n}.alpha"))
            .chain((0..2).map(|n| format!("r{n}.alpha")))
        {
            files.push((Box::leak(name.into_boxed_str()), "code\n".into()));
        }
        let (root, index, _) = lexical_index("type-import-cap", &files).await;
        let sources = vec![
            ("test_a.beta".into(), first, true),
            ("test_b.beta".into(), second, true),
            ("lib.beta".into(), runtime, false),
        ];
        let mut warnings = Vec::new();
        let imported = anchored_imports(
            &root,
            &index.records().unwrap(),
            &[],
            &[],
            &sources,
            &mut warnings,
            &tokio_util::sync::CancellationToken::new(),
            "",
        )
        .await
        .unwrap();
        assert_eq!(imported.len(), MAX_IMPORTED_FILES);
        assert_eq!(imported[0].0.file.path, Path::new("r0.alpha"));
        assert_eq!(imported[1].0.file.path, Path::new("r1.alpha"));
        assert!(imported[..2].iter().all(|(_, tier)| *tier == 2));
        assert!(imported[2..].iter().all(|(_, tier)| *tier == 4));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("2 further direct imports"))
        );
    }

    /// Byte positions distinguish imports on one line and account for preceding Unicode bytes.
    #[test]
    fn type_only_reference_positions_are_not_line_wide() {
        let source = "α 'a' 'b'\nβ 'c' 'd'";
        let offsets = [0, source.find('\n').unwrap() + 1];
        let a = source.find('a').unwrap();
        let c = source.find('c').unwrap();
        let ranges = [a..a + 1, c..c + 1];
        assert!(reference_is_type_only(&offsets, 1, 5, &ranges));
        assert!(!reference_is_type_only(&offsets, 1, 9, &ranges));
        assert!(reference_is_type_only(&offsets, 2, 5, &ranges));
        assert!(!reference_is_type_only(&offsets, 0, 5, &ranges));
        assert!(!reference_is_type_only(&offsets, 2, 0, &ranges));
        assert!(!reference_is_type_only(
            &offsets,
            1,
            (c + 1) as u32,
            &ranges
        ));
    }

    /// Public-evidence tiers lead the selector's order whatever their relevance: anchors in
    /// evidence order, then direct imports, then task-named paths, then the rest by relevance.
    #[test]
    fn selector_order_keeps_public_evidence_tiers_first() {
        let candidate = |path: &str, score: usize| super::super::Candidate {
            file: super::super::FileRecord {
                id: path.into(),
                path: path.into(),
                content_hash: None,
                language: None,
                header: crate::project::structure::Detail::Absent,
                imports: crate::project::structure::Detail::Absent,
                symbols: None,
                truncated: false,
            },
            lexical_score: score,
            reasons: vec![format!("reason {path}")],
            preferred_spans: Vec::new(),
            whole_file: false,
        };
        let mut candidates = vec![
            candidate("high.x", 9),
            candidate("import.x", 1),
            candidate("source-import.x", 8),
            candidate("named.x", 2),
            candidate("anchor-b.x", 3),
            candidate("low.x", 0),
            candidate("anchor-a.x", 4),
        ];
        let pinned = BTreeMap::from([
            (PathBuf::from("anchor-b.x"), 0),
            (PathBuf::from("anchor-a.x"), 0),
            (PathBuf::from("import.x"), 1),
            (PathBuf::from("source-import.x"), 2),
        ]);
        rank_selected(&mut candidates, &pinned, "fix named.x", |candidate| {
            candidate.lexical_score as f64 / 10.0
        });
        let order: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.file.path.to_str().unwrap())
            .collect();
        assert_eq!(
            order,
            [
                "anchor-b.x",
                "anchor-a.x",
                "source-import.x",
                "import.x",
                "named.x",
                "high.x",
                "low.x"
            ]
        );
        assert_eq!(candidates[0].reasons, ["reason anchor-b.x"]);
        // A semantic retrieval order may intentionally put a low lexical score first.
        let mut hybrid = vec![
            candidate("semantic-first.x", 0),
            candidate("lexical-second.x", 9),
        ];
        rank_without_selector(&mut hybrid, &BTreeMap::new());
        assert_eq!(hybrid[0].file.path, Path::new("semantic-first.x"));
        hybrid.push(candidate("pinned-import.x", 2));
        rank_without_selector(
            &mut hybrid,
            &BTreeMap::from([(PathBuf::from("pinned-import.x"), 2)]),
        );
        assert_eq!(
            hybrid
                .iter()
                .map(|candidate| candidate.file.path.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["pinned-import.x", "semantic-first.x", "lexical-second.x"]
        );
    }

    /// A non-test failure location contributes its own direct imports one tier after the
    /// anchored tests' imports when relevance ties; reasons preserve the kind of anchor.
    #[tokio::test]
    async fn non_test_anchors_contribute_their_imports_after_test_imports() {
        let files = [
            ("lib.beta", "ref:dep\nfail\n".to_owned()),
            ("test_a.beta", "ref:sub/a\nfail\n".to_owned()),
            ("dep.alpha", "dependency\n".to_owned()),
            ("sub/a.alpha", "subject\n".to_owned()),
        ];
        let (root, index, _) = lexical_index("context-non-test-anchor", &files).await;
        let records = index.records().unwrap();
        let candidate = |path: &str| super::super::Candidate {
            file: records
                .iter()
                .find(|record| record.path == Path::new(path))
                .unwrap()
                .clone(),
            lexical_score: 0,
            reasons: Vec::new(),
            preferred_spans: Vec::new(),
            whole_file: false,
        };
        let imported = anchored_imports(
            &root,
            &records,
            &[],
            &[candidate("lib.beta"), candidate("test_a.beta")],
            &[
                (PathBuf::from("lib.beta"), files[0].1.clone(), false),
                (PathBuf::from("test_a.beta"), files[1].1.clone(), true),
            ],
            &mut Vec::new(),
            &tokio_util::sync::CancellationToken::new(),
            "",
        )
        .await
        .unwrap();
        let shape: Vec<_> = imported
            .iter()
            .map(|(candidate, tier)| (candidate.file.path.to_str().unwrap(), *tier))
            .collect();
        assert_eq!(shape, [("sub/a.alpha", 1), ("dep.alpha", 2)]);
        assert_eq!(
            imported[1].0.reasons,
            [
                "direct import of public failure location lib.beta (line 1; syntactic reference, extension probe)"
            ]
        );
    }

    /// End to end: a failure in a non-test source delivers the file it imports, which neither
    /// the failure nor the task names.
    #[tokio::test]
    async fn a_failing_source_delivers_its_imported_dependency() {
        let mut files = vec![
            ("lib.beta", "ref:dep\nfail here\n".to_owned()),
            ("dep.alpha", "dependency\n".to_owned()),
        ];
        for n in 0..12 {
            files.push((
                Box::leak(format!("needle-{n:02}.alpha").into_boxed_str()),
                "needle\n".to_owned(),
            ));
        }
        let (_root, index, service) = lexical_index("context-anchored-dependency", &files).await;
        let hash = index
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.path == Path::new("lib.beta"))
            .unwrap()
            .content_hash
            .unwrap();
        let evidence = super::super::evidence::PublicEvidence {
            run_id: "tests-1".into(),
            failures: vec![super::super::evidence::FailureEvidence {
                path: "lib.beta".into(),
                content_hash: hash,
                line: Some(2),
                message: "TypeError: body unusable".into(),
            }],
        };
        let prepared = service
            .prepare_with_evidence_for_delivery(
                &index,
                "needle",
                &evidence,
                &PacketOptions {
                    candidates: 4,
                    ..Default::default()
                },
                &DocumentOptions::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let paths: Vec<_> = prepared
            .packet
            .windows
            .iter()
            .map(|window| window.path.to_str().unwrap())
            .collect();
        assert_eq!(paths[..2], ["lib.beta", "dep.alpha"], "{paths:?}");
        assert!(
            prepared.packet.windows[1].reasons[0]
                .starts_with("direct import of public failure location lib.beta")
        );
    }

    /// The selector's expansion answer keeps an otherwise irrelevant candidate but never turns it
    /// into a whole-file delivery: its preferred spans stay, and a large file with spans is still
    /// delivered localized. Low relevance without expansion drops; public tiers always stay.
    #[test]
    fn expansion_retains_without_whole_file_delivery() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("context-expansion-shape");
        let source = format!("{}\nneedle\n{}", "x".repeat(6000), "y".repeat(6000));
        std::fs::write(root.join("needle-big.alpha"), &source).unwrap();
        std::fs::write(root.join("needle-other.alpha"), "needle other\n").unwrap();
        std::fs::write(root.join("needle-anchor.alpha"), "needle anchor\n").unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&crate::project::structure::StructureOptions::default())
            .unwrap();
        let mut candidates = index.search("needle", 12).unwrap();
        assert_eq!(candidates.len(), 3);
        let span = (6001, 6008);
        for candidate in &mut candidates {
            if candidate.file.path == Path::new("needle-big.alpha") {
                candidate.preferred_spans = vec![span];
            }
        }
        let id = |path: &str| {
            candidates
                .iter()
                .find(|candidate| candidate.file.path == Path::new(path))
                .unwrap()
                .file
                .id
                .clone()
        };
        let assessment = |path: &str, relevance: f64, expansion: f64| {
            (
                id(path),
                super::super::select::Assessment {
                    id: id(path),
                    relevance,
                    expansion,
                },
            )
        };
        let assessments = BTreeMap::from([
            assessment("needle-big.alpha", 0.1, 0.9),
            assessment("needle-other.alpha", 0.1, 0.1),
            assessment("needle-anchor.alpha", 0.1, 0.1),
        ]);
        let pinned = BTreeMap::from([(PathBuf::from("needle-anchor.alpha"), 0)]);
        apply_assessments(&mut candidates, &assessments, &pinned, "needle", 0.5);
        let kept: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.file.path.to_str().unwrap())
            .collect();
        assert_eq!(kept, ["needle-anchor.alpha", "needle-big.alpha"]);
        let uncertain = &candidates[1];
        assert!(!uncertain.whole_file);
        assert_eq!(uncertain.preferred_spans, [span]);
        assert!(
            uncertain
                .reasons
                .contains(&"Jev relevance estimate: 0.10; additional evidence: 0.90".to_owned())
        );
        let packet = index
            .prepare_candidates("needle", &PacketOptions::default(), vec![uncertain.clone()])
            .unwrap();
        assert!(!packet.windows.is_empty());
        assert!(packet.windows.iter().all(|window| !window.complete_file));
    }
}
