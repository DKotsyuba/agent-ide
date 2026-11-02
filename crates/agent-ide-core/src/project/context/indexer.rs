//! Background-only document embeddings, incremental publication and network-free preflight.
use super::{
    ContextIndex, DocumentBatch, DocumentOptions,
    embedding::EmbeddingConfig,
    service::{CallRecord, ContextService, Journal},
};
use crate::project::structure::StructureOptions;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};
use tokio_util::sync::CancellationToken;

/// Upper bounds for one admitted pass; its supervisor owns the cumulative activation allowance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBudget {
    pub max_new_calls: usize,
    pub max_input_bytes: usize,
}

/// A local estimate, not provider usage: tokens use four UTF-8 bytes per token and requests may split.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexEstimate {
    /// Measured local preparation time, without credentials or network.
    pub elapsed_ms: f64,
    pub files: usize,
    pub changed_files: usize,
    pub fragments: usize,
    pub missing_fragments: usize,
    pub input_bytes: usize,
    pub estimated_input_tokens: u64,
    pub estimated_requests: usize,
    pub excluded_files: usize,
    pub gaps: Vec<String>,
}

/// Durable pass outcome; unknown usage is never represented as a zero-dollar success.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexingReport {
    /// Preflight plus embedding/publication time, excluding idle time between admitted passes.
    pub elapsed_ms: f64,
    pub generation: u64,
    pub changed_files: usize,
    pub fragments_embedded: usize,
    pub fragments_pending: usize,
    pub new_calls: usize,
    pub input_tokens: Option<u64>,
    pub input_bytes: usize,
    pub budget_exhausted: bool,
    pub outcome: String,
    pub estimate: IndexEstimate,
    pub calls: Vec<CallRecord>,
    pub warnings: Vec<String>,
}

/// Refreshes only local facts and fragment caches; does not construct clients or read credentials.
pub fn estimate_indexing(
    index: &mut ContextIndex,
    scope: &StructureOptions,
    options: &DocumentOptions,
    embedding: Option<&EmbeddingConfig>,
    cancel: &CancellationToken,
) -> io::Result<IndexEstimate> {
    index.require_writer()?;
    let began = std::time::Instant::now();
    let previous_records = if index.snapshot()?.is_some() {
        index.records()?
    } else {
        Vec::new()
    };
    let previous: BTreeMap<_, _> = previous_records
        .into_iter()
        .filter(|r| r.content_hash.is_some())
        .map(|r| (r.path, r.content_hash))
        .collect();
    index.refresh_cancellable(scope, &|| cancel.is_cancelled())?;
    let mut options = options.clone();
    if let Some(config) = embedding {
        options.max_input_bytes = options.max_input_bytes.min(config.max_input_bytes);
    }
    let documents = index.documents_cancellable(&options, &|| cancel.is_cancelled())?;
    let mut estimate = estimate_documents(index, &documents, embedding)?;
    let current: BTreeMap<_, _> = index
        .records()?
        .into_iter()
        .filter(|r| r.content_hash.is_some())
        .map(|r| (r.path, r.content_hash))
        .collect();
    estimate.changed_files = current
        .iter()
        .filter(|(p, h)| previous.get(*p) != Some(*h))
        .count()
        + previous
            .keys()
            .filter(|p| !current.contains_key(*p))
            .count();
    estimate.elapsed_ms = began.elapsed().as_secs_f64() * 1000.0;
    Ok(estimate)
}

/// Counts unique missing payloads; unchanged content is reusable even when source offsets move.
fn estimate_documents(
    index: &ContextIndex,
    documents: &DocumentBatch,
    embedding: Option<&EmbeddingConfig>,
) -> io::Result<IndexEstimate> {
    let records = index.records()?;
    let space = embedding
        .map(|c| c.space_id().map_err(io::Error::other))
        .transpose()?;
    let mut seen = BTreeSet::new();
    let mut missing = 0;
    let mut bytes = 0;
    for document in &documents.documents {
        if !seen.insert(&document.id) {
            continue;
        }
        if let (Some(config), Some(space)) = (embedding, space.as_deref())
            && index
                .vector(space, &document.id, config.dimensions)?
                .is_none()
        {
            missing += 1;
            bytes += document.text.len();
        }
    }
    Ok(IndexEstimate {
        elapsed_ms: 0.0,
        files: records.iter().filter(|r| r.content_hash.is_some()).count(),
        changed_files: index
            .snapshot()?
            .map_or(0, |s| s.files.saturating_sub(s.reused_files)),
        fragments: seen.len(),
        missing_fragments: missing,
        input_bytes: bytes,
        estimated_input_tokens: bytes.div_ceil(4) as u64,
        estimated_requests: embedding.map_or(0, |c| missing.div_ceil(c.max_batch.max(1))),
        excluded_files: records.iter().filter(|r| r.content_hash.is_none()).count(),
        gaps: documents
            .gaps
            .iter()
            .filter(|s| !s.ends_with(": path-only source"))
            .cloned()
            .collect(),
    })
}

impl ContextService {
    /// Runs one cancellable background pass. Completed vectors survive interruption; only fully
    /// embedded, freshly checked files publish. Errors are reported once, never automatically retried.
    pub async fn index_project(
        &self,
        index: &mut ContextIndex,
        scope: &StructureOptions,
        options: &DocumentOptions,
        budget: &IndexBudget,
        cancel: &CancellationToken,
    ) -> io::Result<IndexingReport> {
        let estimate = estimate_indexing(
            index,
            scope,
            options,
            self.config.embedding.as_ref(),
            cancel,
        )?;
        self.index_prepared(index, options, budget, cancel, estimate)
            .await
    }

    /// Consumes a locally prepared preflight without repeating the directory scan; publication
    /// still revalidates source hashes, and the supervisor retains ownership of cancellation.
    pub async fn index_prepared(
        &self,
        index: &mut ContextIndex,
        options: &DocumentOptions,
        budget: &IndexBudget,
        cancel: &CancellationToken,
        estimate: IndexEstimate,
    ) -> io::Result<IndexingReport> {
        index.require_writer()?;
        let began = std::time::Instant::now();
        let mut options = options.clone();
        if let Some(config) = &self.config.embedding {
            options.max_input_bytes = options.max_input_bytes.min(config.max_input_bytes);
        }
        let documents = index.documents_cancellable(&options, &|| cancel.is_cancelled())?;
        let space = self
            .config
            .embedding
            .as_ref()
            .map(|c| c.space_id().map_err(io::Error::other))
            .transpose()?
            .unwrap_or_else(|| "lexical".into());
        let mut report = IndexingReport {
            elapsed_ms: 0.0,
            generation: documents.generation,
            changed_files: estimate.changed_files,
            fragments_embedded: 0,
            fragments_pending: estimate.missing_fragments,
            new_calls: 0,
            input_tokens: Some(0),
            input_bytes: 0,
            budget_exhausted: false,
            outcome: "ok".into(),
            warnings: estimate.gaps.clone(),
            calls: Vec::new(),
            estimate,
        };
        let mut remaining = budget.max_new_calls.min(self.config.max_new_calls);
        let initial = remaining;
        if let (Some(client), Some(config)) = (&self.embedding, &self.config.embedding) {
            let journal = Journal::new(&index.root)?;
            let unresolved =
                recover_attempts(index, &journal, &space, config, &mut report.warnings)?;
            let mut blocked = 0;
            let mut seen = BTreeSet::new();
            let mut missing = Vec::new();
            for document in &documents.documents {
                if seen.insert(&document.id)
                    && index
                        .vector(&space, &document.id, config.dimensions)?
                        .is_none()
                {
                    if unresolved.contains_key(&document.id) {
                        blocked += 1;
                    } else {
                        missing.push(document);
                    }
                }
            }
            if blocked > 0 {
                report.warnings.push(format!("{blocked} embedding fragments have unresolved prior requests; no automatic resubmission"));
            }
            let mut position = 0;
            while position < missing.len() {
                if cancel.is_cancelled() {
                    report.outcome = "cancelled".into();
                    break;
                }
                let mut count = (missing.len() - position).min(config.max_batch);
                let (inputs, request) = loop {
                    let inputs: Vec<_> = missing[position..position + count]
                        .iter()
                        .map(|d| d.text.clone())
                        .collect();
                    match client.request_documents(&inputs) {
                        Ok(request) => break (inputs, request),
                        Err(_) if count > 1 => count /= 2,
                        Err(error) => return Err(io::Error::other(error)),
                    }
                };
                let bytes: usize = inputs.iter().map(String::len).sum();
                let replay = match journal.cached(&space, &request) {
                    Ok(value) => value.is_some(),
                    Err(error) => {
                        report.outcome = "failed".into();
                        report.warnings.push(error.to_string());
                        break;
                    }
                };
                if !replay
                    && (remaining == 0
                        || bytes > budget.max_input_bytes.saturating_sub(report.input_bytes))
                {
                    report.budget_exhausted = true;
                    report.outcome = "budget_exhausted".into();
                    break;
                }
                if !replay {
                    claim_attempts(
                        index,
                        &space,
                        &journal.group_key(&space, &request),
                        &missing[position..position + count],
                    )?;
                    report.input_bytes += bytes;
                }
                match self
                    .embedding_call(
                        client,
                        config,
                        &journal,
                        &space,
                        &request,
                        &inputs,
                        false,
                        &mut remaining,
                        &mut report.calls,
                        cancel,
                    )
                    .await
                {
                    Ok(batch) => {
                        for (document, vector) in missing[position..position + count]
                            .iter()
                            .zip(batch.vectors)
                        {
                            index.store_vector(&space, &document.id, &vector)?;
                            index
                                .connection
                                .execute(
                                    "DELETE FROM embedding_attempts WHERE space=?1 AND document=?2",
                                    params![space, document.id],
                                )
                                .map_err(io::Error::other)?;
                        }
                        report.fragments_embedded += count;
                        position += count;
                    }
                    Err(error) => {
                        report.outcome = if cancel.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        }
                        .into();
                        report.warnings.push(error.to_string());
                        break;
                    }
                }
            }
            report.fragments_pending = missing.len() - position + blocked;
            if blocked > 0 && report.outcome == "ok" {
                report.outcome = "partial".into();
            }
        }
        report.new_calls = initial - remaining;
        report.input_tokens =
            report
                .calls
                .iter()
                .filter(|c| !c.replayed)
                .try_fold(0u64, |sum, c| {
                    c.usage
                        .as_ref()?
                        .get("input_tokens")?
                        .as_u64()?
                        .checked_add(sum)
                });
        if report.new_calls > report.calls.iter().filter(|c| !c.replayed).count() {
            report.input_tokens = None;
        }
        let mut files: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for doc in &documents.documents {
            files.entry(doc.path.clone()).or_default().push(doc.clone());
        }
        let records: BTreeMap<_, _> = index
            .records()?
            .into_iter()
            .map(|r| (r.path.clone(), r))
            .collect();
        for (path, docs) in files {
            if cancel.is_cancelled() {
                report.outcome = "cancelled".into();
                break;
            }
            let complete = if let Some(config) = &self.config.embedding {
                docs.iter().try_fold(true, |ok, d| {
                    index
                        .vector(&space, &d.id, config.dimensions)
                        .map(|v| ok && v.is_some())
                })?
            } else {
                true
            };
            if !complete {
                continue;
            }
            let mut read = 0;
            if let Some(file) = records.get(&path) {
                match super::packet::read_current(
                    &index.root,
                    file,
                    options.max_file_bytes,
                    &mut read,
                ) {
                    Ok(_) => index.publish_file(&space, &docs)?,
                    Err(_) => {
                        report.outcome = "stale".into();
                        report.warnings.push(format!(
                            "{}: changed before index publication",
                            path.display()
                        ));
                    }
                }
            }
        }
        if !report.estimate.gaps.is_empty() && report.outcome == "ok" {
            report.outcome = "partial".into();
        }
        report.elapsed_ms = report.estimate.elapsed_ms + began.elapsed().as_secs_f64() * 1000.0;
        index.save_index_report(
            &space,
            &serde_json::to_value(&report).map_err(io::Error::other)?,
        )?;
        Ok(report)
    }
}

/// Claims payload identities before sending, so regrouping after restart cannot repeat an unknown call.
fn claim_attempts(
    index: &ContextIndex,
    space: &str,
    group: &str,
    documents: &[&super::IndexedDocument],
) -> io::Result<()> {
    let tx = index
        .connection
        .unchecked_transaction()
        .map_err(io::Error::other)?;
    for document in documents {
        tx.execute(
            "INSERT INTO embedding_attempts VALUES (?1,?2,?3)",
            params![space, document.id, group],
        )
        .map_err(io::Error::other)?;
    }
    tx.commit().map_err(io::Error::other)
}

/// Restores vectors from confirmed replies only. Missing/pending/failed groups remain reserved;
/// those documents are skipped even if edits or batching changes would produce a different request.
fn recover_attempts(
    index: &ContextIndex,
    journal: &Journal,
    space: &str,
    config: &EmbeddingConfig,
    warnings: &mut Vec<String>,
) -> io::Result<BTreeMap<String, String>> {
    index.connection.execute_batch("CREATE TABLE IF NOT EXISTS embedding_attempts(space TEXT NOT NULL,document TEXT NOT NULL,request_group TEXT NOT NULL,PRIMARY KEY(space,document));").map_err(io::Error::other)?;
    let mut statement = index
        .connection
        .prepare("SELECT document,request_group FROM embedding_attempts WHERE space=?1")
        .map_err(io::Error::other)?;
    let rows = statement
        .query_map([space], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(io::Error::other)?;
    let mut pending: BTreeMap<String, String> =
        rows.collect::<Result<_, _>>().map_err(io::Error::other)?;
    drop(statement);
    let groups: BTreeSet<_> = pending.values().cloned().collect();
    for group in groups {
        if !journal.group_exists(&group)? {
            // A reservation preceding the durable before-send callback is safe to release.
            index
                .connection
                .execute(
                    "DELETE FROM embedding_attempts WHERE space=?1 AND request_group=?2",
                    params![space, group],
                )
                .map_err(io::Error::other)?;
            pending.retain(|_, key| key != &group);
            continue;
        }
        let recovered = match journal.completed_group(space, &group) {
            Ok(value) => value,
            Err(_) => {
                warnings.push(
                    "prior embedding response cannot be validated; its fragments remain reserved"
                        .into(),
                );
                continue;
            }
        };
        let Some((request, batch)) = recovered else {
            continue;
        };
        let inputs = request["input"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 128)
            .ok_or_else(|| io::Error::other("invalid prior embedding inputs"))?;
        if batch.model != config.response_model.as_deref().unwrap_or(&config.model)
            || request["model"].as_str() != Some(config.model.as_str())
            || request["input_type"].as_str() != config.document_input_type.as_deref()
            || batch.vectors.len() != inputs.len()
            || batch.vectors.iter().any(|v| v.len() != config.dimensions)
        {
            return Err(io::Error::other("prior embedding identity mismatch"));
        }
        for (input, vector) in inputs.iter().zip(batch.vectors) {
            let text = input
                .as_str()
                .ok_or_else(|| io::Error::other("invalid prior embedding text"))?;
            let id = blake3::hash(text.as_bytes()).to_hex().to_string();
            if pending.get(&id) != Some(&group) {
                continue;
            }
            index.store_vector(space, &id, &vector)?;
            index
                .connection
                .execute(
                    "DELETE FROM embedding_attempts WHERE space=?1 AND document=?2",
                    params![space, id],
                )
                .map_err(io::Error::other)?;
            pending.remove(&id);
        }
    }
    Ok(pending)
}

/// Operator-only release after explicit acceptance of possible duplicate provider charges.
/// Makes no HTTP calls, requires the writer lease, and archives rather than deletes every old journal.
pub fn release_unresolved(
    index: &mut ContextIndex,
    config: &EmbeddingConfig,
    accept_possible_duplicate_charge: bool,
) -> io::Result<serde_json::Value> {
    if !accept_possible_duplicate_charge {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "explicit duplicate-charge acceptance required",
        ));
    }
    index.require_writer()?;
    let space = config.space_id().map_err(io::Error::other)?;
    let journal = Journal::new(&index.root)?;
    let pending = recover_attempts(index, &journal, &space, config, &mut Vec::new())?;
    let groups: BTreeSet<_> = pending.values().cloned().collect();
    let mut archives = Vec::new();
    for group in groups {
        let path = journal.archive_unresolved(&space, &group)?;
        index
            .connection
            .execute(
                "DELETE FROM embedding_attempts WHERE space=?1 AND request_group=?2",
                params![space, group],
            )
            .map_err(io::Error::other)?;
        archives.push(path);
    }
    Ok(
        serde_json::json!({"released_fragments":pending.len(),"archived_requests":archives,"network_requests":0,"possible_duplicate_charge_accepted":true}),
    )
}

#[cfg(test)]
mod recovery_tests {
    use super::super::{embedding::Backend, service::ModelConfig};
    use super::*;
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    /// Cancelling before the durable before-send callback leaves no billable attempt or permanent reservation.
    #[test]
    fn reservation_before_send_is_released_but_pending_send_is_not() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("index-before-send");
        std::fs::write(root.join("context.alpha"), "task source\n").unwrap();
        let config = EmbeddingConfig {
            backend: Backend::Local,
            endpoint: "http://127.0.0.1:9/embeddings".into(),
            model: "fixture".into(),
            response_model: None,
            revision: "v1".into(),
            dimensions: 2,
            credential_file: None,
            egress_allowed: false,
            timeout_ms: 100,
            max_batch: 128,
            max_input_bytes: 4096,
            query_input_type: None,
            document_input_type: None,
        };
        let space = config.space_id().unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&StructureOptions {
                respect_gitignore: false,
                ..Default::default()
            })
            .unwrap();
        let documents = index.documents(&DocumentOptions::default()).unwrap();
        let document = &documents.documents[0];
        let journal = Journal::new(&root).unwrap();
        let request = serde_json::json!({"model":"fixture","input":[document.text]});
        let key = journal.group_key(&space, &request);
        recover_attempts(&index, &journal, &space, &config, &mut Vec::new()).unwrap();
        claim_attempts(&index, &space, &key, &[document]).unwrap();
        assert!(
            recover_attempts(&index, &journal, &space, &config, &mut Vec::new())
                .unwrap()
                .is_empty()
        );
        claim_attempts(&index, &space, &key, &[document]).unwrap();
        let path = root.join(format!(".agent-ide/requests/{key}.json"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(
            serde_json::json!({"state":"pending","request":request})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            recover_attempts(&index, &journal, &space, &config, &mut Vec::new())
                .unwrap()
                .len(),
            1
        );
    }

    /// A confirmed response persisted before vector publication is restored without another request.
    #[tokio::test]
    async fn completed_response_recovers_vectors_with_zero_call_budget() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("index-response-recovery");
        std::fs::write(root.join("context.alpha"), "task source\n").unwrap();
        let config = EmbeddingConfig {
            backend: Backend::Local,
            endpoint: "http://127.0.0.1:9/embeddings".into(),
            model: "fixture".into(),
            response_model: None,
            revision: "v1".into(),
            dimensions: 2,
            credential_file: None,
            egress_allowed: false,
            timeout_ms: 100,
            max_batch: 128,
            max_input_bytes: 4096,
            query_input_type: None,
            document_input_type: None,
        };
        let space = config.space_id().unwrap();
        let service = ContextService::new(ModelConfig {
            embedding: Some(config.clone()),
            jev: None,
            max_new_calls: 0,
            include_threshold: 0.5,
        })
        .unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        let scope = StructureOptions {
            respect_gitignore: false,
            ..Default::default()
        };
        index.refresh(&scope).unwrap();
        let docs = index.documents(&DocumentOptions::default()).unwrap();
        let unique: BTreeMap<_, _> = docs.documents.iter().map(|d| (&d.id, d)).collect();
        let docs: Vec<_> = unique.into_values().collect();
        let inputs: Vec<_> = docs.iter().map(|d| d.text.clone()).collect();
        let request = service
            .embedding
            .as_ref()
            .unwrap()
            .request_documents(&inputs)
            .unwrap();
        let journal = Journal::new(&root).unwrap();
        let group = journal.group_key(&space, &request);
        recover_attempts(&index, &journal, &space, &config, &mut Vec::new()).unwrap();
        claim_attempts(&index, &space, &group, &docs).unwrap();
        let entry = serde_json::json!({"state":"complete","request":request,"response":{"model":"fixture","vectors":vec![vec![1.0,0.0];inputs.len()],"usage":{"input_tokens":10,"cost_usd":0.01},"elapsed_ms":1.0}});
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(format!(".agent-ide/requests/{group}.json")))
            .unwrap();
        file.write_all(entry.to_string().as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        drop(index);
        let mut index = ContextIndex::open(&root).unwrap();
        let report = service
            .index_project(
                &mut index,
                &scope,
                &DocumentOptions::default(),
                &IndexBudget {
                    max_new_calls: 0,
                    max_input_bytes: 0,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(report.outcome, "ok", "{report:?}");
        assert_eq!(report.new_calls, 0);
        assert_eq!(report.input_bytes, 0);
        assert_eq!(report.fragments_pending, 0);
        assert!(
            !index
                .published_documents(&space)
                .unwrap()
                .documents
                .is_empty()
        );
    }
}
