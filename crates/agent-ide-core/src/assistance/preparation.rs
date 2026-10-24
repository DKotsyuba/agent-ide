//! Explicit task-context jobs: bounded private indexing off the worker, current source proof on delivery.
use super::*;
#[cfg(feature = "jev-pilot")]
use crate::project::context::{
    ContextIndex, DocumentOptions, PacketOptions,
    service::{ContextService, ModelConfig, Preparation},
};

/// One owned computation. Dropping an abnormal worker cancels later network sends.
#[cfg(feature = "jev-pilot")]
pub(super) struct Pending {
    binding: BindingRef,
    authority: AuthorityStamp,
    epoch: u64,
    /// When this computation must have finished; past it the call answers a final deadline
    /// refusal instead of a pending state the agent would have to poll.
    deadline: tokio::time::Instant,
    /// Legacy `token_budget` argument when the caller sent one. Recorded in the trace and
    /// answered with a warning; it never limits the packet.
    budget: Option<u64>,
    /// Background indexing state of this worktree, for the not-ready answer.
    index_note: String,
    cancel: tokio_util::sync::CancellationToken,
    pub(super) handle: Option<tokio::task::JoinHandle<std::io::Result<Computed>>>,
}
#[cfg(feature = "jev-pilot")]
impl Drop for Pending {
    /// Abrupt teardown never authorizes further model calls.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Private computation handoff; public failure anchors never change the serialized model packet.
#[cfg(feature = "jev-pilot")]
pub(super) struct Computed {
    prepared: Preparation,
    anchors: Vec<TestAnchor>,
}

/// A validated public-check line tied to the exact bytes observed by that check.
#[cfg(feature = "jev-pilot")]
struct TestAnchor {
    path: std::path::PathBuf,
    hash: String,
    line: u32,
}

/// Trusted opt-in only; model parameters cannot choose root, provider or credentials.
#[cfg(feature = "jev-pilot")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    enabled: bool,
    models: ModelConfig,
    /// Optional operator-selected public check used to ground the task before retrieval.
    #[serde(default)]
    evidence: Option<Value>,
    /// Background indexing; disabled unless the operator enables it.
    #[serde(default)]
    indexing: super::indexing::IndexingConfig,
}

impl Worker<'_> {
    /// Localizes public tests and supported source members; refusal preserves the original selection.
    /// Provider startup is awaited normally, never cancelled while its backend is out of its slot.
    #[cfg(feature = "jev-pilot")]
    async fn localize_public_tests(
        &mut self,
        job: &mut Job,
        sources: &[SourceFile],
        units: &mut Vec<Unit>,
        anchors: &[TestAnchor],
        deadline: tokio::time::Instant,
        prepared: &mut Preparation,
    ) -> Result<(), FailureCode> {
        let until = deadline.min(tokio::time::Instant::now() + Duration::from_secs(2));
        for (file, source) in sources.iter().enumerate() {
            let Some(language) = crate::lang::Language::for_path(&source.path) else {
                continue;
            };
            let is_test = language.support().is_test_file(&source.path);
            let lines: Vec<u32> = anchors
                .iter()
                .filter(|anchor| anchor.path == source.path && anchor.hash == source.hash)
                .map(|anchor| anchor.line)
                .collect();
            let Some(mut original) = units
                .iter()
                .filter(|unit| unit.file == file)
                .min_by_key(|unit| unit.rank)
                .cloned()
            else {
                continue;
            };
            if lines.is_empty() {
                continue;
            }
            if !is_test {
                let budget = until.saturating_duration_since(tokio::time::Instant::now());
                let spans = self
                    .source_probe_context(job, &source.observed, &source.text, budget, &lines)
                    .await;
                if *job.cancel.borrow() {
                    return Err(FailureCode::Cancelled);
                }
                if let Some(trace) = &prepared.trace {
                    trace.record("localization",serde_json::json!({"path":source.path,"kind":"source_context",
                        "anchors":lines,"selected_lines":spans,"outcome":if spans.is_some(){"localized"}else{"original selection retained; probe unavailable or closure unproven"}}))
                        .map_err(|_|FailureCode::Internal)?;
                }
                if let Some(spans) = spans {
                    let offsets: Vec<_> = std::iter::once(0)
                        .chain(source.text.match_indices('\n').map(|(at, _)| at + 1))
                        .collect();
                    let ranges: Option<Vec<_>> = spans
                        .iter()
                        .map(|span| {
                            let start = *offsets.get(span.start.checked_sub(1)? as usize)?;
                            let end = offsets
                                .get(span.end as usize)
                                .copied()
                                .unwrap_or(source.text.len());
                            (start < end && end <= source.text.len()).then_some((start, end))
                        })
                        .collect();
                    if let Some(ranges) = ranges {
                        original.required =
                            units.iter().any(|unit| unit.file == file && unit.required);
                        units.retain(|unit| unit.file != file);
                        for range in ranges {
                            let mut unit = original.clone();
                            unit.range = range;
                            unit.complete = true;
                            unit.bundle = Some(file);
                            unit.symbol = None;
                            unit.reasons.push("source members with syntactic callers, dependencies and surrounding state".into());
                            units.push(unit);
                        }
                        prepared.warnings.push(format!(
                            "{}: public source context localized",
                            source.path.display()
                        ));
                    }
                }
                continue;
            }
            let whole = units
                .iter()
                .any(|unit| unit.file == file && unit.range == (0, source.text.len()));
            original.required = units.iter().any(|unit| unit.file == file && unit.required);
            original.reasons = dedup(
                units
                    .iter()
                    .filter(|unit| unit.file == file)
                    .flat_map(|unit| unit.reasons.iter().cloned())
                    .collect(),
            );
            if !whole {
                for unit in units.iter_mut().filter(|unit| unit.file == file) {
                    unit.complete = false;
                }
            }
            let Some(imports) = language
                .support()
                .source_import_spans(&source.path, &source.text)
            else {
                prepared.warnings.push(format!(
                    "{}: imports unavailable; original test context retained",
                    source.path.display()
                ));
                continue;
            };
            let mut localized = None;
            let began = tokio::time::Instant::now();
            let mut outline_reason = String::from("outline budget exhausted before request");
            let mut test_symbols = 0;
            let mut attempts = 0;
            let probe_budget = until.saturating_duration_since(tokio::time::Instant::now());
            let (syntax_outline, probe_note) = self
                .source_probe_outline(job, &source.observed, &source.text, probe_budget)
                .await;
            if *job.cancel.borrow() {
                return Err(FailureCode::Cancelled);
            }
            let syntax_available = syntax_outline.is_some();
            if let Some(outline) = syntax_outline {
                attempts = 1;
                outline_reason = "syntax-only source probe".into();
                for symbol in &outline.symbols {
                    symbol.walk(&mut |symbol| {
                        test_symbols += usize::from(symbol.kind == crate::lang::SymbolKind::Test);
                    });
                }
                localized =
                    preparation_test_units::localize(&outline, &source.text, &lines, &imports);
            }
            // At most two ordinary outline requests: the first can start a cold provider;
            // only an already-owned session is waited on before the second request.
            for attempt in 0..2 {
                if syntax_available {
                    break;
                }
                if *job.cancel.borrow() {
                    return Err(FailureCode::Cancelled);
                }
                if tokio::time::Instant::now() >= until {
                    break;
                }
                attempts = attempt + 1;
                let failure = job.failure_detail.take();
                let parked = job.park_until.take();
                let owner = job.session_binding.clone();
                let answer = self
                    .outline_of(job, &source.observed, source.text.as_bytes())
                    .await;
                job.failure_detail = failure;
                job.park_until = parked;
                job.session_binding = owner;
                if *job.cancel.borrow() || matches!(&answer, Err(FailureCode::Cancelled)) {
                    return Err(FailureCode::Cancelled);
                }
                let (outline, _, lexical) = match &answer {
                    Ok(outline) => outline,
                    Err(code) => {
                        outline_reason = format!("outline refused: {code:?}");
                        break;
                    }
                };
                outline_reason = match lexical {
                    None => "exact outline".into(),
                    Some(symbols::Lexical::Loading) => "source outline while provider loads".into(),
                    Some(symbols::Lexical::Unavailable { module }) => module
                        .clone()
                        .unwrap_or_else(|| "source outline; provider unavailable".into()),
                    Some(symbols::Lexical::Exchange { cause }) => {
                        format!("source outline; exchange: {cause}")
                    }
                    Some(symbols::Lexical::Unverified { cause }) => {
                        format!("source outline; resolution: {cause}")
                    }
                };
                test_symbols = 0;
                for symbol in &outline.symbols {
                    symbol.walk(&mut |symbol| {
                        test_symbols += usize::from(symbol.kind == crate::lang::SymbolKind::Test);
                    });
                }
                // Source-backed parsers can prove complete test units without a live server.
                // The localizer refuses outlines that do not contain the enclosing tests.
                localized =
                    preparation_test_units::localize(outline, &source.text, &lines, &imports);
                if localized.is_some() || !matches!(lexical, Some(symbols::Lexical::Loading)) {
                    break;
                }
                if attempt != 0 {
                    break;
                }
                let budget = until.saturating_duration_since(tokio::time::Instant::now());
                if budget.is_zero() {
                    break;
                }
                let binding = job.invocation.binding_ref().clone();
                let ready = tokio::select! {
                    ready = self.providers.wait_existing_session(&binding, &source.path, budget) => ready,
                    _ = job.cancel.changed() => return Err(FailureCode::Cancelled),
                };
                if !ready {
                    break;
                }
            }
            if let Some(trace) = &prepared.trace {
                trace.record("localization",serde_json::json!({"path":source.path,"kind":"test_context",
                    "anchors":lines,"selected_byte_spans":localized,"outline_reason":outline_reason,"probe_note":probe_note,
                    "outcome":if localized.is_some(){"localized"}else{"original selection retained; complete test context not proven"}}))
                    .map_err(|_|FailureCode::Internal)?;
            }
            if let Some(spans) = localized {
                let resolved: Vec<_> = units
                    .iter()
                    .filter(|unit| unit.file == file)
                    .map(|unit| {
                        format!(
                            "{}: bytes {}..{} are not a complete declaration",
                            source.path.display(),
                            unit.range.0,
                            unit.range.1
                        )
                    })
                    .collect();
                prepared.packet.gaps.retain(|gap| !resolved.contains(gap));
                units.retain(|unit| unit.file != file);
                for range in spans {
                    let mut unit = original.clone();
                    unit.range = range;
                    unit.complete = true; // Complete source-backed context only as one bundle.
                    unit.bundle = Some(file);
                    unit.role = "test".into();
                    unit.symbol = None;
                    unit.reasons
                        .push("public test block with surrounding setup".into());
                    units.push(unit);
                }
                prepared.warnings.push(format!(
                    "{}: public test context localized",
                    source.path.display()
                ));
            } else {
                prepared.warnings.push(format!(
                    "{}: test blocks not localized; {} retained ({outline_reason}; {probe_note}; {test_symbols} test symbols; {attempts} outline attempts; {}ms)",
                    source.path.display(),
                    if whole {
                        "whole-file candidate"
                    } else {
                        "incomplete selected fragments"
                    },
                    began.elapsed().as_millis()
                ));
            }
        }
        Ok(())
    }

    /// Cancels and joins owned computations before stop/shutdown releases their authority.
    pub(super) async fn cancel_preparations(&mut self, binding: Option<&BindingRef>) {
        #[cfg(feature = "jev-pilot")]
        {
            preparation_checks::interrupt(&self.shared, binding).await;
            let keys: Vec<_> = self
                .preparations
                .iter()
                .filter(|(_, job)| binding.is_none_or(|binding| binding == &job.binding))
                .map(|(id, _)| id.clone())
                .collect();
            for key in keys {
                if let Some(mut pending) = self.preparations.remove(&key) {
                    pending.cancel.cancel();
                    if let Some(handle) = pending.handle.take() {
                        let _ = handle.await;
                    }
                }
            }
        }
        #[cfg(not(feature = "jev-pilot"))]
        let _ = binding;
    }

    /// Stops background indexing of `binding`'s writer worktree (every worktree when `None`) and
    /// waits until its running job returned, before the authority it indexed under is released.
    pub(super) async fn stop_indexing(&mut self, binding: Option<&BindingRef>) {
        let root = match binding {
            Some(binding) => match self.grants.get(binding) {
                Some(receipt)
                    if receipt.role() == crate::workspace::authority::StartRole::Writer =>
                {
                    let root = receipt.worktree().worktree_path().to_path_buf();
                    self.indexing.deactivate(&root);
                    Some(root)
                }
                _ => return,
            },
            None => {
                self.indexing.cancel_all();
                None
            }
        };
        while self.indexing.running(root.as_deref()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Parks only this job while indexing and HTTP execute outside the sequential worker.
    pub(super) async fn prepare_context(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        #[cfg(not(feature = "jev-pilot"))]
        {
            job.failure_detail = Some("prepare:feature_disabled".into());
            Err(FailureCode::LauncherConfiguration)
        }
        #[cfg(feature = "jev-pilot")]
        {
            let binding = job.invocation.binding_ref().clone();
            if !self.preparations.contains_key(&job.reference) {
                // Readers may prepare too: the call only reads the published index.
                let authority = self.authority(&binding).await?;
                let config: Config = self
                    .shared
                    .launcher
                    .task_context()
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .filter(|config: &Config| config.enabled)
                    .ok_or_else(|| {
                        job.failure_detail = Some("prepare:disabled_or_invalid_config".into());
                        FailureCode::LauncherConfiguration
                    })?;
                if !self.preparations.is_empty() {
                    return Err(FailureCode::Capacity);
                }
                let root = authority.worktree().worktree_path().to_path_buf();
                let note = index_note(config.indexing.enabled, self.indexing.state(&root));
                let task = job.parameters["task"]
                    .as_str()
                    .ok_or(FailureCode::Internal)?
                    .to_owned();
                let budget = job.parameters.get("token_budget").and_then(Value::as_u64);
                // Test-only stall before any work, to prove a slow preparation still answers in
                // one call; default builds never read it.
                let stall = crate::test_seams::var("AGENT_IDE_TEST_PREPARE_DELAY_MS")
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_millis);
                let cancel = tokio_util::sync::CancellationToken::new();
                let cancelled = cancel.clone();
                let deadline = job
                    .deadline
                    .min(tokio::time::Instant::now() + PREPARE_COMPUTE_BUDGET);
                let check_config = preparation_evidence::EvidenceConfig::parse(
                    config.evidence.as_ref(),
                )
                .map_err(|reason| {
                    job.failure_detail = Some(reason.into());
                    FailureCode::LauncherConfiguration
                })?;
                let shared = self.shared.clone();
                let check_authority = authority.clone();
                let check_epoch = self.shared.outbox.current_epoch(&binding);
                let check_ticket = self.shared.preparation_checks.ticket(&binding);
                let target = job.target.clone();
                let daemon_runtime = tokio::runtime::Handle::current();
                let handle = tokio::task::spawn_blocking(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async move {
                        if cancelled.is_cancelled() || tokio::time::Instant::now() >= deadline {
                            return Err(std::io::Error::other("context cancelled"));
                        }
                        if let Some(stall) = stall {
                            tokio::select! {
                                () = cancelled.cancelled() => {
                                    return Err(std::io::Error::other("context cancelled"));
                                }
                                () = tokio::time::sleep(stall) => {}
                            }
                        }
                        // Only an index the background indexer already published is read: no
                        // creation, no refresh and no document embedding inside this call.
                        let index = ContextIndex::open_existing(&root)?.ok_or_else(|| {
                            std::io::Error::new(std::io::ErrorKind::NotFound, NOT_READY)
                        })?;
                        let space = config
                            .models
                            .embedding
                            .as_ref()
                            .map(|c| c.space_id().map_err(std::io::Error::other))
                            .transpose()?
                            .unwrap_or_else(|| "lexical".into());
                        if index.index_report(&space)?.is_none() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "context_index_not_ready",
                            ));
                        }
                        let mut check_notes = Vec::new();
                        let public_report = if let Some(mut check_config) = check_config {
                            // Keep time for retrieval and response construction even after a slow check.
                            let check_deadline = deadline - Duration::from_secs(10);
                            if tokio::time::Instant::now() >= check_deadline {
                                check_notes
                                    .push("public check skipped: retrieval time reserved".into());
                                None
                            } else {
                                check_config.budget = check_config.budget.min(
                                    check_deadline
                                        .saturating_duration_since(tokio::time::Instant::now()),
                                );
                                let check_task = task.clone();
                                let check_cancel = cancelled.clone();
                                let outcome = daemon_runtime
                                    .spawn(async move {
                                        preparation_checks::collect(
                                            &shared,
                                            &check_authority,
                                            check_epoch,
                                            check_ticket,
                                            &check_task,
                                            &check_config,
                                            &target,
                                            &check_cancel,
                                            check_deadline,
                                        )
                                        .await
                                    })
                                    .await;
                                match outcome {
                                    Ok(Ok(report)) => Some(report),
                                    Ok(Err(error)) => {
                                        check_notes
                                            .push(format!("public check unavailable: {error}"));
                                        None
                                    }
                                    Err(_) => {
                                        check_notes.push("public check task failed".into());
                                        None
                                    }
                                }
                            }
                        } else {
                            None
                        };
                        if cancelled.is_cancelled() || tokio::time::Instant::now() >= deadline {
                            return Err(std::io::Error::other("context cancelled"));
                        }
                        let service = ContextService::new(config.models)?;
                        // Delivery collects whole selected units; no output budget reaches selection.
                        let options = PacketOptions::default();
                        let docs = DocumentOptions::default();
                        let evidence = public_report
                            .as_ref()
                            .and_then(|report| report.retrieval_evidence())
                            .filter(|evidence| match evidence.retrieval_query(&index, &task) {
                                Ok(_) => true,
                                Err(error) => {
                                    check_notes
                                        .push(format!("public evidence unavailable: {error}"));
                                    false
                                }
                            });
                        let mut prepared = tokio::time::timeout_at(deadline, async {
                            if let Some(evidence) = &evidence {
                                service
                                    .prepare_with_evidence_for_delivery(
                                        &index, &task, evidence, &options, &docs, &cancelled,
                                    )
                                    .await
                            } else {
                                service
                                    .prepare_for_delivery(
                                        &index, &task, &options, &docs, &cancelled,
                                    )
                                    .await
                            }
                        })
                        .await
                        .map_err(std::io::Error::other)??;
                        let anchors = evidence
                            .as_ref()
                            .into_iter()
                            .flat_map(|evidence| {
                                evidence.failures.iter().filter_map(|failure| {
                                    failure.line.map(|line| TestAnchor {
                                        path: failure.path.clone(),
                                        hash: failure.content_hash.clone(),
                                        line,
                                    })
                                })
                            })
                            .collect();
                        prepared.warnings.extend(check_notes);
                        if let Some(report) = public_report {
                            prepared.warnings.push(format!(
                                "public check {}: {:?}; passed {}; failed {}; elapsed {}ms",
                                report.run_id,
                                report.outcome,
                                report.passed,
                                report.failed,
                                report.elapsed_ms
                            ));
                            prepared.warnings.extend(report.errors);
                        }
                        Ok(Computed { prepared, anchors })
                    })
                });
                self.preparations.insert(
                    job.reference.clone(),
                    Pending {
                        binding: binding.clone(),
                        authority,
                        epoch: job.native_epoch,
                        deadline,
                        budget,
                        index_note: note,
                        cancel,
                        handle: Some(handle),
                    },
                );
            }
            let now = tokio::time::Instant::now();
            if let Some(pending) = self.preparations.get(&job.reference)
                && !pending.handle.as_ref().unwrap().is_finished()
                && now >= pending.deadline
            {
                // Cancel now and keep the entry: the worker's failed-prepare cleanup
                // (`cancel_preparations`) removes it and joins the computation.
                pending.cancel.cancel();
                job.failure_detail = Some(DEADLINE_DETAIL.into());
                return Err(FailureCode::Deadline);
            }
            if self
                .preparations
                .get(&job.reference)
                .is_some_and(|pending| !pending.handle.as_ref().unwrap().is_finished())
            {
                job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(25));
                return Ok((
                    PeerReply::Pending {
                        detail_ref: job.reference.clone(),
                    },
                    None,
                    None,
                ));
            }
            let mut pending = self
                .preparations
                .remove(&job.reference)
                .ok_or(FailureCode::Internal)?;
            let computed = pending
                .handle
                .take()
                .unwrap()
                .await
                .map_err(|_| FailureCode::Internal)?;
            if let Err(error) = &computed
                && is_not_ready(error)
            {
                return Ok((not_ready_reply(&pending.index_note), None, None));
            }
            let Computed {
                mut prepared,
                anchors,
            } = computed.map_err(|_| {
                if tokio::time::Instant::now() >= pending.deadline {
                    job.failure_detail = Some(DEADLINE_DETAIL.into());
                    FailureCode::Deadline
                } else {
                    job.failure_detail = Some("prepare:computation_failed".into());
                    FailureCode::Internal
                }
            })?;
            let authority = self.authority(&binding).await?;
            if authority.epoch() != pending.authority.epoch() || job.native_epoch != pending.epoch {
                return Err(FailureCode::Cancelled);
            }
            // Every distinct file is re-observed once and must still match the selected version.
            let mut sources: Vec<SourceFile> = Vec::new();
            let mut units: Vec<Unit> = Vec::new();
            let mut unreadable: Vec<std::path::PathBuf> = Vec::new();
            for window in &prepared.packet.windows {
                if unreadable.contains(&window.path) {
                    continue;
                }
                let file = match sources.iter().position(|file| file.path == window.path) {
                    // Every window must name the version that was verified for its file.
                    Some(file) if sources[file].hash == window.content_hash => file,
                    Some(_) => return Err(FailureCode::SourceUnavailable),
                    None => {
                        let (observed, bytes) = self
                            .observe_as(&binding, window.path.clone(), false)
                            .await?;
                        if blake3::hash(&bytes).to_hex().as_str() != window.content_hash {
                            return Err(FailureCode::SourceUnavailable);
                        }
                        // Byte offsets and line numbers hold only for exact UTF-8 text.
                        let Ok(text) = String::from_utf8(bytes) else {
                            unreadable.push(window.path.clone());
                            continue;
                        };
                        sources.push(SourceFile {
                            path: window.path.clone(),
                            hash: window.content_hash.clone(),
                            observed,
                            text,
                        });
                        sources.len() - 1
                    }
                };
                units.push(Unit {
                    file,
                    range: (window.start_byte, window.end_byte),
                    role: window.role.clone(),
                    rank: window.rank,
                    required: window.required,
                    complete: window.unit_complete || window.complete_file,
                    bundle: None,
                    symbol: window.symbol.clone(),
                    reasons: window.reasons.clone(),
                });
            }
            self.localize_public_tests(
                job,
                &sources,
                &mut units,
                &anchors,
                pending.deadline,
                &mut prepared,
            )
            .await?;
            let root = authority.worktree().worktree_path().to_path_buf();
            let mut prelude = Vec::new();
            for path in prelude_paths(&root, sources.iter().map(|file| file.path.as_path())) {
                let (observed, bytes) = self.observe_as(&binding, path.clone(), false).await?;
                if std::str::from_utf8(&bytes).is_err() {
                    unreadable.push(path);
                    continue;
                }
                let mut file = PreludeFile::new(path, &bytes);
                file.observed = Some(observed);
                prelude.push(file);
            }
            let active = self.shared.active(&binding)?;
            self.workspace
                .authorize(&authority, &active)
                .await
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
            let mut omitted = dedup(prepared.packet.gaps.clone());
            // Index coverage, not a missing required source: a warning, never an omission.
            if prepared.embedding_used && !prepared.embedding_complete {
                prepared
                    .warnings
                    .push("embeddings: not every indexed unit was embedded".to_owned());
            }
            omitted.extend(
                unreadable
                    .iter()
                    .map(|path| format!("{}: not UTF-8 text", path.display())),
            );
            if let Some(reason) = prepared
                .trace
                .as_ref()
                .and_then(|trace| trace.degradation())
            {
                prepared
                    .warnings
                    .push(format!("prepare trace incomplete: {reason}"));
            }
            if pending.budget.is_some() {
                prepared.warnings.push(
                    "token_budget is ignored: the packet carries every selected unit whole"
                        .to_owned(),
                );
            }
            if let Some(trace) = &prepared.trace {
                trace.record("packing_input", serde_json::json!({"operation_ref":job.reference,
                    "requested_token_budget":pending.budget,
                    "prelude_policy":{"manifests":MANIFEST_FILES,"instructions":RULE_FILES},
                    "units":units.iter().map(|u| serde_json::json!({"path":sources[u.file].path,
                        "content_hash":sources[u.file].hash,"range":u.range,"rank":u.rank,"required":u.required,
                        "bundle":u.bundle,"role":u.role,"complete_unit":u.complete})).collect::<Vec<_>>(),
                    "prelude":prelude.iter().map(|f|serde_json::json!({"path":f.path,"bytes":f.text.len(),"included_bytes":f.text.len(),"total_bytes":f.text.len()})).collect::<Vec<_>>(),
                    "prior_omissions":omitted
                })).map_err(|_| FailureCode::Internal)?;
            }
            let reply = select_packet(&prepared, &sources, &mut units, &mut prelude, &omitted)?;
            // Read memory gets exactly the delivered rows, so unit and window edits see what the
            // agent saw.
            let files = delivered_files(&sources, &units);
            for file in &files {
                let source = &sources[file.source];
                for (first, last) in &file.spans {
                    self.remember_file_read(
                        job,
                        &source.observed,
                        &source.text,
                        crate::lang::LineRange::new(*first, *last),
                        &reply,
                        None,
                    )
                    .await;
                }
            }
            // Every delivered file keeps its observation as a freshness fence, fragments and
            // delivered rules/manifests included;
            // `ide.prepare` is not an admitted edit source, so no edit authority follows from it.
            let mut fences = files
                .iter()
                .map(|file| sources[file.source].observed.clone())
                .chain(prelude.iter().filter_map(|file| file.observed.clone()));
            let primary = fences.next();
            self.shared
                .add_edit_sources(&job.reference, fences.collect());
            if let Some(trace) = &prepared.trace {
                trace.finish(serde_json::json!({"operation_ref":job.reference,
                    "files":files.iter().map(|f|serde_json::json!({"path":sources[f.source].path,
                        "content_hash":sources[f.source].hash,"lines":f.spans,"whole_file":f.whole})).collect::<Vec<_>>(),
                    "prelude":prelude.iter().map(|f|serde_json::json!({"path":f.path,"bytes":f.text.len(),"included_bytes":f.text.len(),"total_bytes":f.text.len()})).collect::<Vec<_>>(),
                    "omissions":omitted,"carrier_bytes_without_status":content::prepare_carrier_bytes(&reply),
                    "host_visibility":"unverified; worker output is not external client receipt"
                })).map_err(|_| FailureCode::Internal)?;
                if let Some(reason) = trace.degradation() {
                    eprintln!("agent-ide: prepare trace incomplete: {reason}");
                }
            }
            Ok((reply, Some(authority), primary))
        }
    }
}

/// Instruction files a project addresses to agents, read from the worktree root and from every
/// directory above a delivered file.
#[cfg(feature = "jev-pilot")]
const RULE_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
/// Manifests that name a module's build, dependencies, features and test targets.
#[cfg(feature = "jev-pilot")]
const MANIFEST_FILES: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
/// Longest a preparation computes; kept below the call's inline wait so a slow preparation ends
/// in a final deadline refusal, never in a pending state.
#[cfg(feature = "jev-pilot")]
const PREPARE_COMPUTE_BUDGET: Duration = PREPARE_INLINE_WAIT.saturating_sub(Duration::from_secs(5));
/// Closed error message of an index that was never published (shared with the context index).
#[cfg(feature = "jev-pilot")]
const NOT_READY: &str = "context_index_not_ready";

/// Whether `error` says the task-context index is not built or holds no ready documents yet.
#[cfg(feature = "jev-pilot")]
fn is_not_ready(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound && error.to_string() == NOT_READY
}

/// The final answer while no ready index exists: never a success, never a pending state, and it
/// names how the index gets built. `ide.prepare` itself never builds it.
#[cfg(feature = "jev-pilot")]
fn not_ready_reply(note: &str) -> PeerReply {
    PeerReply::Complete {
        kind: ResultKind::Prepare,
        text: format!(
            "prepare: not_ready; 0 files\n\nThe task-context index of this worktree is not \
             ready; nothing was delivered.\nindex: {note}\nContinue with ide.read and \
             ide.outline for this task.\n"
        ),
        detail_ref: None,
        truncated: false,
        continuation: false,
    }
}

/// The not-ready explanation: disabled indexing, or the supervisor's state of this worktree.
#[cfg(feature = "jev-pilot")]
fn index_note(enabled: bool, state: Option<super::indexing::IndexState>) -> String {
    use super::indexing::IndexState;
    match (enabled, state) {
        (false, _) => {
            "background indexing is disabled in the launcher configuration (task_context.indexing)."
                .to_owned()
        }
        (true, None | Some(IndexState::Pending)) => {
            "background indexing is scheduled and has not published this index yet.".to_owned()
        }
        (true, Some(IndexState::Building)) => "background indexing is running.".to_owned(),
        (true, Some(IndexState::Done(report))) => format!("last indexing run: {}.", report.summary),
    }
}

/// The daemon's indexing supervisor from the trusted `task_context` configuration; disabled
/// unless both task context and its background indexing are enabled.
pub(super) fn indexing_supervisor(task_context: Option<&Value>) -> super::indexing::Supervisor {
    #[cfg(feature = "jev-pilot")]
    {
        let Some(config) = task_context
            .cloned()
            .and_then(|value| serde_json::from_value::<Config>(value).ok())
            .filter(|config| config.enabled && config.indexing.enabled)
        else {
            return super::indexing::Supervisor::disabled();
        };
        let (models, settings) = (config.models, config.indexing);
        super::indexing::Supervisor::start(
            settings,
            std::sync::Arc::new(move |root, budget, cancel| {
                index_once(&models, settings, root, budget, cancel)
            }),
        )
    }
    #[cfg(not(feature = "jev-pilot"))]
    {
        let _ = task_context;
        super::indexing::Supervisor::disabled()
    }
}

/// One background indexing run of `root` within `budget`, on the supervisor thread.
///
/// A free local estimate comes first. A first full model indexing (nothing published yet in the
/// configured embedding space) runs only when `initial_allowed`; otherwise, and whenever the
/// budget is spent, the pass runs model-free and prepares local facts only. A configured semantic
/// index then stays not ready (no lexical substitute); without an embedding model the published
/// index is the lexical one. Jev is never constructed here. An error is reported once by its
/// closed kind; the supervisor never retries it blindly.
#[cfg(feature = "jev-pilot")]
fn index_once(
    models: &ModelConfig,
    settings: super::indexing::IndexingConfig,
    root: &std::path::Path,
    budget: super::indexing::Budget,
    cancel: &tokio_util::sync::CancellationToken,
) -> super::indexing::RunReport {
    use super::indexing::{RunOutcome, RunReport};
    use crate::project::context::indexer::{IndexBudget, estimate_indexing};
    // Set once a pass is admitted to a model: from then on a failure may follow sent requests.
    let mut admitted: Option<(u64, u64)> = None;
    let mut run = || -> std::io::Result<RunReport> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let scope = crate::project::structure::StructureOptions {
            max_depth: 64,
            max_nodes: 100_000,
            ..Default::default()
        };
        let documents = DocumentOptions::default();
        let mut index = ContextIndex::open(root)?;
        let estimate = estimate_indexing(
            &mut index,
            &scope,
            &documents,
            models.embedding.as_ref(),
            cancel,
        )?;
        // A first full model indexing: nothing is published yet in this embedding space.
        let first_full = match &models.embedding {
            Some(config) => {
                let space = config.space_id().map_err(std::io::Error::other)?;
                match index.published_documents(&space) {
                    Ok(batch) => batch.documents.is_empty(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error),
                }
            }
            None => false,
        };
        let preflight = first_full && !settings.initial_allowed;
        let calls = usize::try_from(budget.calls).unwrap_or(usize::MAX);
        let bytes = usize::try_from(budget.input_bytes).unwrap_or(usize::MAX);
        let use_model = models.embedding.is_some() && !preflight && calls > 0 && bytes > 0;
        let service = ContextService::new(ModelConfig {
            embedding: use_model.then(|| models.embedding.clone()).flatten(),
            jev: None,
            max_new_calls: calls.min(512),
            include_threshold: models.include_threshold,
        })?;
        if use_model {
            admitted = Some((
                u64::try_from(calls).unwrap_or(u64::MAX),
                u64::try_from(bytes).unwrap_or(u64::MAX),
            ));
        }
        // The estimate's refresh is reused: no second project walk, no lost changed files.
        let report = runtime.block_on(service.index_prepared(
            &mut index,
            &documents,
            &IndexBudget {
                max_new_calls: calls,
                max_input_bytes: bytes,
            },
            cancel,
            estimate,
        ))?;
        let outcome = match report.outcome.as_str() {
            _ if preflight => RunOutcome::NeedsPreflight,
            "ok" | "partial" | "stale" => RunOutcome::Ready,
            "budget_exhausted" => RunOutcome::BudgetExhausted,
            "cancelled" => RunOutcome::Cancelled,
            other => RunOutcome::Failed(other.to_owned()),
        };
        let mut summary = format!(
            "generation {}; {} fragments embedded, {} pending; {} model calls; outcome {}",
            report.generation,
            report.fragments_embedded,
            report.fragments_pending,
            report.new_calls,
            report.outcome
        );
        if preflight {
            summary.push_str(&format!(
                "; first full model indexing awaits the operator's preflight: {} fragments, \
                 ~{} input tokens, ~{} requests (local facts prepared; semantic index not ready)",
                report.estimate.missing_fragments,
                report.estimate.estimated_input_tokens,
                report.estimate.estimated_requests
            ));
        }
        Ok(RunReport {
            outcome,
            new_calls: u64::try_from(report.new_calls).unwrap_or(u64::MAX),
            input_bytes: u64::try_from(report.input_bytes).unwrap_or(u64::MAX),
            summary,
        })
    };
    let result = run();
    result.unwrap_or_else(|error| failed_run(error.kind(), admitted))
}

/// The report of a failed run. Before model admission nothing was sent; after it the spend is
/// unknown, so the whole budget handed to the pass counts as used and the summary says so.
/// Either way the supervisor never retries it before the next `ide.start`.
#[cfg(feature = "jev-pilot")]
fn failed_run(
    kind: std::io::ErrorKind,
    admitted: Option<(u64, u64)>,
) -> super::indexing::RunReport {
    use super::indexing::{RunOutcome, RunReport};
    match admitted {
        None => RunReport {
            outcome: RunOutcome::Failed(format!("{kind:?}")),
            new_calls: 0,
            input_bytes: 0,
            summary: format!("indexing failed ({kind:?}); not retried until the next ide.start"),
        },
        Some((calls, bytes)) => RunReport {
            outcome: RunOutcome::Failed(format!("{kind:?}_after_model_admission")),
            new_calls: calls,
            input_bytes: bytes,
            summary: format!(
                "indexing failed ({kind:?}) after model requests may have been sent; spend \
                 unknown, up to {calls} calls and {bytes} input bytes counted as used; not \
                 retried until the next ide.start"
            ),
        },
    }
}

/// Failure detail of a preparation that missed its compute budget.
#[cfg(feature = "jev-pilot")]
const DEADLINE_DETAIL: &str =
    "prepare:deadline: the packet was not ready within the one-call wait; nothing was delivered";

/// One re-observed source file of the packet.
#[cfg(feature = "jev-pilot")]
struct SourceFile {
    path: std::path::PathBuf,
    hash: String,
    observed: SourceObservation,
    text: String,
}

/// One selected unit (a declaration, a range or a whole file) of a [`SourceFile`].
#[cfg(feature = "jev-pilot")]
#[derive(Clone)]
struct Unit {
    file: usize,
    range: (usize, usize),
    role: String,
    rank: u32,
    required: bool,
    complete: bool,
    /// Disjoint ranges of one complete test context must be admitted together.
    bundle: Option<usize>,
    symbol: Option<String>,
    reasons: Vec<String>,
}

/// One file as delivered: its source index, merged line spans, whether they cover the file,
/// and the best-ranked role and the reasons of its units.
#[cfg(feature = "jev-pilot")]
struct DeliveredFile {
    source: usize,
    spans: Vec<(u32, u32)>,
    whole: bool,
    role: String,
    reasons: Vec<String>,
}

/// Groups the remaining units by file in first-selection order with merged line spans.
#[cfg(feature = "jev-pilot")]
fn delivered_files(sources: &[SourceFile], units: &[Unit]) -> Vec<DeliveredFile> {
    let mut order: Vec<usize> = Vec::new();
    for unit in units {
        if !order.contains(&unit.file) {
            order.push(unit.file);
        }
    }
    order
        .into_iter()
        .map(|source| {
            let mine: Vec<&Unit> = units.iter().filter(|unit| unit.file == source).collect();
            let ranges: Vec<(usize, usize)> = mine.iter().map(|unit| unit.range).collect();
            let text = &sources[source].text;
            let mut spans: Vec<(u32, u32)> = Vec::new();
            for (start, end) in merged(&ranges) {
                let (first, last) = line_span(text, start, end);
                match spans.last_mut() {
                    Some(span) if first <= span.1 + 1 => span.1 = span.1.max(last),
                    _ => spans.push((first, last)),
                }
            }
            let role = mine
                .iter()
                .min_by_key(|unit| unit.rank)
                .map(|unit| unit.role.clone())
                .unwrap_or_default();
            DeliveredFile {
                source,
                whole: covers_every_line(text, &ranges),
                spans,
                role,
                reasons: dedup(mine.iter().flat_map(|unit| unit.reasons.clone()).collect()),
            }
        })
        .collect()
}

/// Recognizes an explicit relative path token, not a suffix inside an unrelated nested path.
#[cfg(feature = "jev-pilot")]
fn task_names_path(task: &str, path: &std::path::Path) -> bool {
    let path = path.to_string_lossy();
    let path_char = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\');
    [path.to_string(), format!("./{path}")].iter().any(|name| {
        task.match_indices(name.as_str()).any(|(at, _)| {
            let tail = &task[at + name.len()..];
            let end_boundary = !tail.chars().next().is_some_and(path_char)
                || tail
                    .strip_prefix('.')
                    .is_some_and(|rest| !rest.chars().next().is_some_and(path_char));
            !task[..at].chars().next_back().is_some_and(path_char) && end_boundary
        })
    })
}

/// Assembles the whole selected packet: every ranked unit (required first, then by rank), the
/// rules and task-named files, then each manifest not already delivered whole as ranked source.
/// Nothing is omitted for size; `omitted` carries only selection gaps. The serialized size is
/// measured for the trace and never limits delivery.
#[cfg(feature = "jev-pilot")]
fn select_packet(
    prepared: &Preparation,
    sources: &[SourceFile],
    units: &mut [Unit],
    prelude: &mut Vec<PreludeFile>,
    omitted: &[String],
) -> Result<PeerReply, FailureCode> {
    units.sort_by_key(|unit| (!unit.required, unit.rank));
    let (mut kept, manifests): (Vec<_>, Vec<_>) =
        std::mem::take(prelude).into_iter().partition(|file| {
            file.path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| RULE_FILES.contains(&name))
                || task_names_path(&prepared.packet.task, &file.path)
        });
    let files = delivered_files(sources, units);
    for file in manifests {
        // A manifest already delivered whole as ranked source needs no duplicate prelude.
        let already_delivered = files.iter().any(|delivered| {
            let source = &sources[delivered.source];
            delivered.whole && source.path == file.path && source.text == file.text
        });
        if !already_delivered {
            kept.push(file);
        }
    }
    *prelude = kept;
    let text = render_packet(prepared, sources, &files, prelude, units, omitted);
    let reply = PeerReply::Complete {
        kind: ResultKind::Prepare,
        text,
        detail_ref: None,
        truncated: false,
        continuation: false,
    };
    if let Some(trace) = &prepared.trace {
        let bytes = content::prepare_carrier_bytes(&reply);
        trace
            .record(
                "packing_all",
                serde_json::json!({"carrier_bytes":bytes,
                    "estimated_tokens":bytes.map(|bytes| bytes.div_ceil(4)),
                    "units":units.len(),"prelude_files":prelude.len()}),
            )
            .map_err(|_| FailureCode::Internal)?;
    }
    Ok(reply)
}

/// `path#symbol` for a declaration unit, else `path lines a-b`.
#[cfg(feature = "jev-pilot")]
fn unit_label(source: &SourceFile, unit: &Unit) -> String {
    match &unit.symbol {
        Some(symbol) => format!("{}#{symbol}", source.path.display()),
        None => {
            let (first, last) = line_span(&source.text, unit.range.0, unit.range.1);
            format!("{} lines {first}-{last}", source.path.display())
        }
    }
}

/// One rule or manifest file, carried whole and verbatim.
#[cfg(feature = "jev-pilot")]
#[derive(Clone)]
struct PreludeFile {
    path: std::path::PathBuf,
    text: String,
    /// Observation of the delivered bytes, kept as a freshness fence.
    observed: Option<SourceObservation>,
}

#[cfg(feature = "jev-pilot")]
impl PreludeFile {
    /// Keeps the whole file; the caller has already checked it is UTF-8.
    fn new(path: std::path::PathBuf, bytes: &[u8]) -> Self {
        Self {
            path,
            text: String::from_utf8_lossy(bytes).into_owned(),
            observed: None,
        }
    }
}

/// Worktree-relative rule files (root first, then nearer directories) and the nearest manifest
/// of every delivered file plus the root manifest, each a regular file and listed once. The
/// list is bounded by the delivered files' ancestors.
///
/// Only the ancestors of `files` are probed with `symlink_metadata`: no walk, no Git call and no
/// path outside `root`. A symlink or non-regular file declares nothing.
#[cfg(feature = "jev-pilot")]
fn prelude_paths<'a>(
    root: &std::path::Path,
    files: impl Iterator<Item = &'a std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let regular = |relative: &std::path::Path| {
        std::fs::symlink_metadata(root.join(relative)).is_ok_and(|metadata| metadata.is_file())
    };
    let mut rules = Vec::new();
    let mut manifests = Vec::new();
    for name in MANIFEST_FILES {
        if regular(std::path::Path::new(name)) {
            manifests.push(std::path::PathBuf::from(name));
            break;
        }
    }
    for file in files {
        let mut directories: Vec<&std::path::Path> = file.ancestors().skip(1).collect();
        directories.reverse();
        for directory in &directories {
            for name in RULE_FILES {
                let path = directory.join(name);
                if regular(&path) && !rules.contains(&path) {
                    rules.push(path);
                }
            }
        }
        let nearest = directories.iter().rev().find_map(|directory| {
            MANIFEST_FILES
                .iter()
                .map(|name| directory.join(name))
                .find(|path| regular(path))
        });
        if let Some(path) = nearest
            && !manifests.contains(&path)
        {
            manifests.push(path);
        }
    }
    rules.extend(manifests);
    rules
}

/// Whether rows for `ranges` cover every line of `text` (rows are whole lines, so a window cut
/// inside a line still delivers that whole line).
#[cfg(feature = "jev-pilot")]
fn covers_every_line(text: &str, ranges: &[(usize, usize)]) -> bool {
    let total = text.lines().count();
    let mut next = 1_usize;
    for (start, end) in merged(ranges) {
        let (first, last) = line_span(text, start, end);
        if first as usize > next {
            return false;
        }
        next = next.max(last as usize + 1);
    }
    next > total
}

/// Sorted, merged byte ranges: overlapping or touching windows become one range.
#[cfg(feature = "jev-pilot")]
fn merged(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (start, end) in sorted {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// One-based first and last line of the byte range `start..end` of `text`.
#[cfg(feature = "jev-pilot")]
fn line_span(text: &str, start: usize, end: usize) -> (u32, u32) {
    let start = start.min(text.len());
    let end = end.clamp(start, text.len());
    let first = text.as_bytes()[..start]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1;
    let inner = text.as_bytes()[start..end]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    let last = if end > start && text.as_bytes()[end - 1] == b'\n' {
        first + inner - 1
    } else {
        first + inner
    };
    (
        u32::try_from(first).unwrap_or(u32::MAX),
        u32::try_from(last.max(first)).unwrap_or(u32::MAX),
    )
}

/// Order-preserving deduplication.
#[cfg(feature = "jev-pilot")]
fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Whether one-based lines `first..=last` exist in `text`, so printing them cannot underflow or
/// run past the file.
#[cfg(feature = "jev-pilot")]
fn rows_exist(text: &str, first: u32, last: u32) -> bool {
    first >= 1 && first <= last && last as usize <= text.lines().count()
}

/// Longest signature printed in a declaration line; a longer one ends in `…`.
#[cfg(feature = "jev-pilot")]
const MAX_SIGNATURE_CHARS: usize = 160;

/// Whether a retriever warning is an unanswered request for more evidence (Jev asked for it, or
/// its evidence did not fit the request), which leaves the packet `partial`.
#[cfg(feature = "jev-pilot")]
fn is_open_evidence_request(warning: &str) -> bool {
    warning.contains("requests additional evidence")
        || warning.contains("evidence exceeds Jev request limit")
}

/// `start-end name: signature` with the signature on one line and bounded; `kind name` when the
/// fact carries no signature. The file prefix of the symbol address is dropped.
#[cfg(feature = "jev-pilot")]
fn declaration_line(fact: &crate::project::context::DeclarationFact) -> String {
    let name = fact
        .symbol
        .split_once('#')
        .map_or(fact.symbol.as_str(), |(_, name)| name);
    let signature = fact
        .signature
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if signature.is_empty() {
        return format!("{}-{} {} {name}", fact.start_line, fact.end_line, fact.kind);
    }
    let shown: String = signature.chars().take(MAX_SIGNATURE_CHARS).collect();
    let cut = if shown.len() < signature.len() {
        "…"
    } else {
        ""
    };
    format!("{}-{} {name}: {shown}{cut}", fact.start_line, fact.end_line)
}

/// The agent-facing packet: status line, rules and manifests, numbered source per file (with
/// the file's imports and declaration list when only part of it is delivered), explicit
/// omissions and one usage line. Contains no continuation reference.
///
/// `needs_evidence` when nothing is delivered; `complete` only when every remaining required
/// unit is complete and nothing was omitted or cut (retriever gaps, prelude, dropped units);
/// otherwise `partial`. File facts are printed only for a delivered file whose facts carry that
/// file's verified content hash.
#[cfg(feature = "jev-pilot")]
fn render_packet(
    prepared: &Preparation,
    sources: &[SourceFile],
    files: &[DeliveredFile],
    prelude: &[PreludeFile],
    units: &[Unit],
    omitted: &[String],
) -> String {
    use std::fmt::Write as _;
    let mut unfinished: Vec<String> = units
        .iter()
        .filter(|unit| unit.required && !unit.complete)
        .map(|unit| {
            format!(
                "{}: required unit not complete",
                unit_label(&sources[unit.file], unit)
            )
        })
        .collect();
    // An unanswered request for more evidence is an open gap, not a note.
    let (open_requests, notes): (Vec<&String>, Vec<&String>) = prepared
        .warnings
        .iter()
        .partition(|warning| is_open_evidence_request(warning));
    unfinished.extend(open_requests.into_iter().cloned());
    let mut body = String::new();
    if !prelude.is_empty() {
        body.push_str("rules and manifests:\n");
        for file in prelude {
            let _ = writeln!(body, "--- {}", file.path.display());
            body.push_str(&file.text);
            if !file.text.ends_with('\n') {
                body.push('\n');
            }
        }
        body.push('\n');
    }
    for file in files {
        let source = &sources[file.source];
        let total_lines = source.text.lines().count();
        let extent = if file.whole {
            format!("whole file, {total_lines} lines")
        } else {
            let listed: Vec<String> = file
                .spans
                .iter()
                .map(|(first, last)| format!("{first}-{last}"))
                .collect();
            format!("lines {} of {total_lines}", listed.join(", "))
        };
        let role = if file.role.is_empty() {
            String::new()
        } else {
            format!("; {}", file.role)
        };
        let _ = writeln!(body, "## {} — {extent}{role}", source.path.display());
        if !file.reasons.is_empty() {
            let shown: Vec<&str> = file.reasons.iter().take(3).map(String::as_str).collect();
            let _ = writeln!(body, "why: {}", shown.join("; "));
        }
        let facts =
            (!file.whole)
                .then(|| {
                    prepared.packet.file_facts.iter().find(|facts| {
                        facts.path == source.path && facts.content_hash == source.hash
                    })
                })
                .flatten();
        if let Some(facts) = facts {
            // The module header (file documentation) gives a partial file its meaning; it is
            // printed from the verified source rows, not from the normalized excerpt.
            if let Some(header) = &facts.header {
                let covered = file
                    .spans
                    .iter()
                    .any(|(first, last)| *first <= header.start_line && header.end_line <= *last);
                if !covered && rows_exist(&source.text, header.start_line, header.end_line) {
                    body.push_str("header:\n");
                    body.push_str(&super::target::numbered_rows(
                        &source.text,
                        header.start_line,
                        header.end_line,
                        usize::MAX,
                    ));
                }
            }
            let uncovered: Vec<_> = facts
                .imports
                .iter()
                .filter(|import| {
                    rows_exist(&source.text, import.start_line, import.end_line)
                        && !file.spans.iter().any(|(first, last)| {
                            *first <= import.start_line && import.end_line <= *last
                        })
                })
                .collect();
            if !uncovered.is_empty() {
                body.push_str("imports:\n");
            }
            for import in uncovered {
                body.push_str(&super::target::numbered_rows(
                    &source.text,
                    import.start_line,
                    import.end_line,
                    usize::MAX,
                ));
            }
            // Only declarations whose rows are not delivered: the rest is already readable.
            let unseen: Vec<_> = facts
                .declarations
                .iter()
                .filter(|fact| {
                    !file
                        .spans
                        .iter()
                        .any(|(first, last)| *first <= fact.start_line && fact.end_line <= *last)
                })
                .collect();
            if !unseen.is_empty() {
                body.push_str("other declarations:\n");
                for fact in unseen {
                    let _ = writeln!(body, "  {}", declaration_line(fact));
                }
            }
            if facts.truncated {
                unfinished.push(format!(
                    "{}: declaration list incomplete",
                    source.path.display()
                ));
            }
        }
        for (index, (first, last)) in file.spans.iter().enumerate() {
            if index > 0 {
                body.push_str("…\n");
            }
            body.push_str(&super::target::numbered_rows(
                &source.text,
                *first,
                *last,
                usize::MAX,
            ));
        }
        body.push('\n');
    }
    if !omitted.is_empty() || !unfinished.is_empty() {
        body.push_str("omitted:\n");
        for item in omitted.iter().chain(&unfinished) {
            let _ = writeln!(body, "- {item}");
        }
    }
    for warning in notes {
        let _ = writeln!(body, "note: {warning}");
    }
    let failed = prepared
        .calls
        .iter()
        .filter(|call| call.error.is_some())
        .count();
    let replayed = prepared.calls.iter().filter(|call| call.replayed).count();
    let _ = writeln!(
        body,
        "usage: embeddings {}, jev {}; model calls {} (replayed {replayed}, failed {failed}); {:.0} ms",
        if prepared.embedding_used {
            "used"
        } else {
            "off"
        },
        if prepared.jev_used { "used" } else { "off" },
        prepared.calls.len(),
        prepared.elapsed_ms
    );
    let status = if files.is_empty() {
        "needs_evidence"
    } else if omitted.is_empty() && unfinished.is_empty() {
        "complete"
    } else {
        "partial"
    };
    // The estimate covers the whole reply text, its own status line included.
    let head = |tokens: usize| {
        format!(
            "prepare: {status}; {} files; ~{tokens} tokens (estimated)\n\n",
            files.len()
        )
    };
    let tokens = (head(body.len()).len() + body.len()).div_ceil(4);
    format!("{}{body}", head(tokens))
}

#[cfg(all(test, feature = "jev-pilot"))]
mod tests {
    use super::*;

    /// Model-free retrieval and final carrier packing must keep a relevant dependency even when
    /// it was outside the initial shortlist and an earlier test imported a less relevant file.
    #[tokio::test]
    async fn relevant_source_dependency_survives_the_final_carrier() {
        dependency_carrier("needle", None).await;
    }

    /// A behavior named only by the public runner label must still rank its implementation.
    #[tokio::test]
    async fn failure_label_selects_dependency_in_final_carrier() {
        dependency_carrier("restore behavior", Some("needle")).await;
    }

    /// Runs the same competing-import carrier with either task or public-label relevance.
    async fn dependency_carrier(task: &str, failure_name: Option<&str>) {
        use crate::project::context::evidence::{FailureEvidence, PublicEvidence};
        use crate::project::context::service::{ContextService, ModelConfig};
        use crate::project::context::{ContextIndex, DocumentOptions, PacketOptions};
        use crate::workspace::observation::{
            ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceRevision,
        };
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("dependency-carrier");
        let imports = |start| {
            format!(
                "{}\nfail\n",
                (start..start + 8)
                    .map(|n| format!("ref:noise-{n:02}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        let mut files = vec![
            ("test_a.beta".to_owned(), imports(0)),
            ("test_b.beta".to_owned(), imports(8)),
            (
                "lib.beta".to_owned(),
                "ref:needle-worker\nfail\n".to_owned(),
            ),
            (
                "needle-worker.alpha".to_owned(),
                "needle implementation row\n".repeat(150),
            ),
            ("needle-00.alpha".to_owned(), "unrelated\n".to_owned()),
        ];
        files.extend((0..16).map(|n| (format!("noise-{n:02}.alpha"), "n".repeat(6000))));
        for (path, text) in &files {
            std::fs::write(root.join(path), text).unwrap();
        }
        let service = ContextService::new(ModelConfig {
            embedding: None,
            jev: None,
            max_new_calls: 0,
            include_threshold: 0.5,
        })
        .unwrap();
        let mut index = ContextIndex::open(&root).unwrap();
        service
            .index_project(
                &mut index,
                &crate::project::structure::StructureOptions {
                    respect_gitignore: false,
                    ..Default::default()
                },
                &DocumentOptions::default(),
                &crate::project::context::indexer::IndexBudget {
                    max_new_calls: 0,
                    max_input_bytes: 0,
                },
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            index.search("needle", 1).unwrap()[0].file.path,
            std::path::Path::new("needle-00.alpha")
        );
        let evidence = PublicEvidence {
            run_id: "public-1".into(),
            failures: ["test_a.beta", "test_b.beta", "lib.beta"]
                .into_iter()
                .map(|path| FailureEvidence {
                    failure_name: failure_name.map(str::to_owned),
                    path: path.into(),
                    content_hash: blake3::hash(&std::fs::read(root.join(path)).unwrap())
                        .to_hex()
                        .to_string(),
                    line: Some(2),
                    message: "observed failure".into(),
                })
                .collect(),
        };
        let prepared = service
            .prepare_with_evidence_for_delivery(
                &index,
                task,
                &evidence,
                &PacketOptions {
                    candidates: 1,
                    ..Default::default()
                },
                &DocumentOptions::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let worktree = crate::workspace::authority::WorktreeRef::from_discovery(
            root.to_path_buf(),
            root.to_path_buf(),
            ".git".into(),
            1,
        )
        .unwrap();
        let mut sources: Vec<SourceFile> = Vec::new();
        let mut units = Vec::new();
        for window in &prepared.packet.windows {
            let file =
                if let Some(file) = sources.iter().position(|source| source.path == window.path) {
                    file
                } else {
                    let text = std::fs::read_to_string(root.join(&window.path)).unwrap();
                    let observed = SourceObservation::new(
                        worktree.clone(),
                        1,
                        1,
                        ObservationRef::new(window.path.to_string_lossy()).unwrap(),
                        window.path.clone(),
                        Some(SourceBytes::from_bytes(text.as_bytes())),
                        SourceRevision::new("fixture").unwrap(),
                        SourceCoverage::Complete,
                        ObservedState::Present,
                    )
                    .unwrap();
                    sources.push(SourceFile {
                        path: window.path.clone(),
                        hash: window.content_hash.clone(),
                        observed,
                        text,
                    });
                    sources.len() - 1
                };
            units.push(Unit {
                file,
                range: (window.start_byte, window.end_byte),
                role: window.role.clone(),
                rank: window.rank,
                required: window.required,
                complete: window.unit_complete || window.complete_file,
                bundle: None,
                symbol: window.symbol.clone(),
                reasons: window.reasons.clone(),
            });
        }
        let gaps = prepared.packet.gaps.clone();
        let reply = select_packet(&prepared, &sources, &mut units, &mut Vec::new(), &gaps).unwrap();
        assert!(content::fits(&reply, content::reply_envelope()));
        let PeerReply::Complete { text, .. } = reply else {
            panic!("expected packet")
        };
        assert!(
            text.contains("## needle-worker.alpha") && text.contains("needle implementation row"),
            "{text}"
        );
        assert!(text.contains("## test_a.beta") && text.contains("## lib.beta"));
        assert!(
            !gaps.iter().any(|gap| gap.contains("needle-worker.alpha")),
            "{gaps:?}"
        );
        // The competing import is delivered too: nothing is omitted to fit one reply.
        assert!(text.contains("noise-"), "{text}");
        assert!(!text.contains("omitted to fit"), "{text}");
    }

    /// Every selected unit, every rule and every manifest reaches the packet whole, past the
    /// former 38 000-byte carrier, the former 32 KiB prelude head and any `token_budget`; bundles
    /// and overlapping selections are all kept, and the trace measures the size it never limits.
    #[test]
    fn packing_delivers_every_selected_unit_and_manifest_whole() {
        use crate::project::context::{ContextBundle, IndexSnapshot};
        use crate::workspace::observation::{
            ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceRevision,
        };
        let scratch = crate::scratch::ScratchDir::new("packing");
        let root = scratch.to_path_buf();
        let worktree = crate::workspace::authority::WorktreeRef::from_discovery(
            root.clone(),
            root.clone(),
            ".git".into(),
            1,
        )
        .unwrap();
        let mut sources = Vec::new();
        for (name, width, lines) in [
            ("large-test.alpha", 63, 260),
            ("implementation.alpha", 31, 400),
            ("small.alpha", 27, 70),
        ] {
            let text = (format!("{}\n", "x".repeat(width))).repeat(lines);
            let path: std::path::PathBuf = name.into();
            let observed = SourceObservation::new(
                worktree.clone(),
                1,
                1,
                ObservationRef::new(name).unwrap(),
                path.clone(),
                Some(SourceBytes::from_bytes(text.as_bytes())),
                SourceRevision::new(name).unwrap(),
                SourceCoverage::Complete,
                ObservedState::Present,
            )
            .unwrap();
            sources.push(SourceFile {
                path,
                hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
                observed,
                text,
            });
        }
        std::fs::create_dir(root.join(".agent-ide")).unwrap();
        let trace_directory = root.join(".agent-ide/prepare-traces");
        let prepared = Preparation {
            trace: Some(crate::project::context::trace::PrepareTrace::new(&root).unwrap()),
            packet: ContextBundle {
                task: "Repair observed failures".into(),
                snapshot: IndexSnapshot {
                    generation: 1,
                    root,
                    files: 3,
                    reused_files: 3,
                    scope: Default::default(),
                    discovery_complete: true,
                    details_complete: true,
                    warnings: vec![],
                },
                windows: vec![],
                file_facts: vec![],
                gaps: vec![],
                estimated_tokens: 0,
                accounting_is_estimated: true,
            },
            embedding_used: false,
            embedding_complete: true,
            jev_used: false,
            calls: vec![],
            warnings: vec![],
            elapsed_ms: 0.0,
        };
        let units: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(file, source)| Unit {
                file,
                range: (0, source.text.len()),
                role: "primary".into(),
                rank: file as u32,
                required: file < 2,
                complete: true,
                bundle: None,
                symbol: None,
                reasons: vec![],
            })
            .collect();
        // Longer than the former 32 KiB prelude head.
        let manifest = "m".repeat(40_000) + "\n";
        let mut prelude = vec![
            PreludeFile::new("AGENTS.md".into(), b"Keep the public behavior.\n"),
            PreludeFile::new("package.json".into(), manifest.as_bytes()),
        ];
        let mut all = units.clone();
        let reply = select_packet(&prepared, &sources, &mut all, &mut prelude, &[]).unwrap();
        let PeerReply::Complete { text, .. } = &reply else {
            panic!("expected packet")
        };
        assert!(text.starts_with("prepare: complete"), "{text}");
        for source in &sources {
            assert!(text.contains(&format!("## {}", source.path.display())));
        }
        assert!(text.contains("--- AGENTS.md") && text.contains(&manifest));
        assert!(!text.contains("omitted"), "nothing is omitted for size");
        // Each whole file arrives through its last numbered row.
        for source in &sources {
            let rows = source.text.lines().count();
            assert!(
                text.contains(&format!("\n{rows}\t")),
                "{}",
                source.path.display()
            );
        }
        let carrier = content::prepare_carrier_bytes(&reply).unwrap();
        assert!(carrier > 64 * 1024, "{carrier}");
        let trace_file = std::fs::read_dir(&trace_directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let events: Vec<serde_json::Value> = std::fs::read_to_string(trace_file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(events.iter().any(|event| event["stage"] == "packing_all"
            && event["data"]["carrier_bytes"] == carrier
            && event["data"]["units"] == 3
            && event["data"]["prelude_files"] == 2));
        assert!(
            !events
                .iter()
                .any(|event| event["stage"] == "worker_reply_ready")
        );

        // Test, setup and import context of one bundle all arrive with the other units.
        let bundle: Vec<_> = [(0, 3200), (6400, 9600), (12800, 16000)]
            .into_iter()
            .map(|range| {
                let mut unit = units[0].clone();
                unit.range = range;
                unit.bundle = Some(0);
                unit
            })
            .collect();
        let mut bundled = bundle.clone();
        bundled.push(units[2].clone());
        let bundle_reply =
            select_packet(&prepared, &sources, &mut bundled, &mut Vec::new(), &[]).unwrap();
        let PeerReply::Complete { text, .. } = bundle_reply else {
            panic!("expected packet")
        };
        assert!(
            text.contains("## large-test.alpha — lines 1-50, 101-150, 201-250 of 260"),
            "{text}"
        );
        assert!(text.contains("\n 50\t") || text.contains("\n50\t"));
        assert!(text.contains("\n150\t") && text.contains("\n250\t"));
        assert!(text.contains("## small.alpha — whole file"), "{text}");

        // Overlapping selections are both kept and render as merged rows.
        let mut costly = units[0].clone();
        costly.range = (0, 32 * 50);
        costly.reasons = vec!["explanation".repeat(6000)];
        let mut whole = units[0].clone();
        whole.rank = 1;
        let mut overlap = vec![costly, whole];
        let overlap_reply =
            select_packet(&prepared, &sources, &mut overlap, &mut Vec::new(), &[]).unwrap();
        let PeerReply::Complete { text, .. } = overlap_reply else {
            panic!("expected packet")
        };
        assert!(text.starts_with("prepare: complete"));
        assert!(text.contains("## large-test.alpha — whole file"), "{text}");
        assert_eq!(
            text.matches("\n260\t").count(),
            1,
            "merged rows render once"
        );

        // A manifest already delivered whole as ranked source is not repeated in the prelude.
        let mut as_source = units.clone();
        let mut duplicate = vec![PreludeFile::new(
            "small.alpha".into(),
            sources[2].text.as_bytes(),
        )];
        select_packet(&prepared, &sources, &mut as_source, &mut duplicate, &[]).unwrap();
        assert!(duplicate.is_empty());
    }

    /// Explicit manifest names do not accidentally protect a different root manifest.
    #[test]
    fn manifest_names_are_path_tokens() {
        let path = std::path::Path::new("package.json");
        assert!(task_names_path("Update `package.json`", path));
        assert!(task_names_path("Update ./package.json", path));
        assert!(task_names_path("Update package.json.", path));
        assert!(!task_names_path("Update packages/web/package.json", path));
        assert!(!task_names_path("Update package.json.backup", path));
    }

    /// Overlapping and touching windows merge; separate ones stay apart in byte order.
    #[test]
    fn windows_of_one_file_merge_into_ordered_ranges() {
        assert_eq!(
            merged(&[(50, 60), (0, 10), (10, 20), (15, 30)]),
            [(0, 30), (50, 60)]
        );
    }

    /// Line spans are one-based and a range ending on a newline stops on that line.
    #[test]
    fn byte_ranges_map_to_line_spans() {
        let text = "a\nbb\nccc\n";
        assert_eq!(line_span(text, 0, text.len()), (1, 3));
        assert_eq!(line_span(text, 2, 5), (2, 2));
        assert_eq!(line_span(text, 3, 7), (2, 3));
    }

    /// A long prelude file is carried whole, byte for byte.
    #[test]
    fn prelude_file_is_carried_whole() {
        let text = ("é".repeat(49) + "\n").repeat(1_000);
        let file = PreludeFile::new("AGENTS.md".into(), text.as_bytes());
        assert_eq!(file.text, text);
    }

    /// Declaration lines carry a bounded one-line signature, or kind and name without one.
    #[test]
    fn declaration_lines_show_signatures() {
        let fact = |signature: &str| crate::project::context::DeclarationFact {
            symbol: "src/scan.rs#Block/close".into(),
            kind: "field".into(),
            signature: signature.into(),
            start_line: 19,
            end_line: 20,
        };
        assert_eq!(
            declaration_line(&fact("close:\n    Option<usize>")),
            "19-20 Block/close: close: Option<usize>"
        );
        assert_eq!(declaration_line(&fact("")), "19-20 field Block/close");
        let long = declaration_line(&fact(&"x".repeat(400)));
        assert!(long.ends_with('…') && long.chars().count() < 200, "{long}");
    }

    /// Only unanswered evidence requests are open gaps; coverage notes stay notes.
    #[test]
    fn evidence_requests_are_open_gaps() {
        assert!(is_open_evidence_request(
            "src/scan.rs: Jev requests additional evidence; candidate retained, automatic expansion not performed"
        ));
        assert!(is_open_evidence_request(
            "src/a.rs: evidence exceeds Jev request limit"
        ));
        assert!(!is_open_evidence_request("Cargo.toml: path-only source"));
        assert!(!is_open_evidence_request(
            "semantic index coverage is partial; lexical candidates were retained"
        ));
    }

    /// Fact rows are printed only when they exist in the verified text.
    #[test]
    fn fact_rows_are_checked_before_printing() {
        let text = "a\nb\nc\n";
        assert!(rows_exist(text, 1, 3));
        assert!(!rows_exist(text, 0, 2), "zero line");
        assert!(!rows_exist(text, 3, 2), "reversed");
        assert!(!rows_exist(text, 2, 4), "past the end");
    }

    /// A failure after model admission never reads as free: the pass's whole budget counts as
    /// spent and the summary names the unknown spend; before admission nothing was sent.
    #[test]
    fn failure_after_admission_counts_the_budget_as_spent() {
        use super::super::indexing::RunOutcome;
        let before = failed_run(std::io::ErrorKind::Other, None);
        assert_eq!((before.new_calls, before.input_bytes), (0, 0));
        let after = failed_run(std::io::ErrorKind::Other, Some((8, 4096)));
        assert_eq!((after.new_calls, after.input_bytes), (8, 4096));
        assert!(
            matches!(after.outcome, RunOutcome::Failed(ref reason) if reason.ends_with("_after_model_admission"))
        );
        assert!(after.summary.contains("spend unknown"), "{}", after.summary);
    }

    /// A file counts as delivered whole only when its rows cover every line.
    #[test]
    fn only_full_line_coverage_is_whole() {
        let text = "a\nbb\nccc\n";
        assert!(covers_every_line(text, &[(0, 9)]));
        assert!(
            covers_every_line(text, &[(0, 2), (3, 9)]),
            "line-adjacent pieces"
        );
        assert!(
            !covers_every_line(text, &[(0, 2), (5, 9)]),
            "line 2 missing"
        );
        assert!(!covers_every_line(text, &[(2, 9)]), "line 1 missing");
    }

    /// Prelude paths: root rules and manifest, nested rules above a file, nearest manifest,
    /// each once; a symlinked rule file declares nothing.
    #[test]
    fn prelude_paths_follow_delivered_files_only() {
        let scratch = crate::scratch::ScratchDir::new("prelude");
        let root: &std::path::Path = &scratch;
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("crates/a/AGENTS.md"), "a\n").unwrap();
        std::fs::write(root.join("crates/a/Cargo.toml"), "[package]\n").unwrap();
        std::os::unix::fs::symlink(root.join("AGENTS.md"), root.join("crates/a/CLAUDE.md"))
            .unwrap();
        let files = [
            std::path::PathBuf::from("crates/a/src/lib.rs"),
            std::path::PathBuf::from("crates/a/src/scan.rs"),
        ];
        let paths = prelude_paths(root, files.iter().map(std::path::PathBuf::as_path));
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            paths,
            [
                "AGENTS.md",
                "crates/a/AGENTS.md",
                "Cargo.toml",
                "crates/a/Cargo.toml"
            ]
        );
    }
}
