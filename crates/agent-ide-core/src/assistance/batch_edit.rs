//! Multi-file `ide.edit {files}` (batch-api-v1 §7) on the worker.
//!
//! Phase 1 observes and validates every file and builds every candidate without writing; any
//! refusal answers `edit_refused` naming each failed file and change, with nothing written and no
//! receipt kept. Phase 2 records a durable digest receipt ([`crate::changes::batch`]) and writes
//! the files in request order through Workspace's stale-safe write, which re-checks each file's
//! bytes against its base immediately before installing it; the first file that changed meanwhile
//! or fails stops the commit, and the reply says exactly which files were written. After the last
//! write of the call one project check runs per affected checked language, and the reply groups
//! its diagnostics per file. A retry with the same `operation_id` and request resumes from the
//! receipt: written files stay, unwritten files are written only while their bytes still equal the
//! validated base and their recomputed candidate equals the recorded one, so nothing is applied
//! twice and no newer bytes are overwritten, even after a daemon crash.

use super::*;
use crate::assistance::batch::{self, EditBody, EditFile, FileReport, FileState, NotWritten};
use crate::changes::batch::{BatchAdmission, BatchFile, BatchReceipt};
use symbols::{no_such_file, observed_text};

/// Bytes of the combined `edit:refused:` detail of a refused multi-file edit (about eight file
/// sentences); a longer detail is cut on a character boundary.
const MAX_BATCH_REFUSAL_BYTES: usize = 2048;

/// One file whose candidate phase 1 (or a resume) has built and that the commit may write.
pub(super) struct Planned {
    /// Worktree-relative path as requested.
    path: String,
    /// The observation the write is checked against: the admitted `source_ref` observation, the
    /// fresh observation for a symbol-only change or a resume, or the observed absence for a
    /// file to create.
    base: SourceObservation,
    /// The exact bytes the commit writes.
    candidate: String,
    /// The first line of the landing note (where the file's changes landed), when there is one.
    note: Option<String>,
}

/// A multi-file edit parked after its writes, waiting for its one project check.
pub(super) struct BatchCheck {
    /// Every requested file in request order with its write state and landing note.
    reports: Vec<FileReport>,
    /// Post-write observations of the files written in this call or earlier, request order;
    /// they become the edit bases the reply's `source_ref` covers.
    sources: Vec<SourceObservation>,
    /// One single-file check stage per written file of a checked language, sharing one check
    /// generation per language.
    stages: Vec<JobStage>,
    /// Written files no IDE language checks: their diagnostics are `not_analysed` at once.
    unchecked: Vec<(String, EditDiagnostics)>,
    /// Authority under which the files were written.
    authority: AuthorityStamp,
    /// Last instant a check result may still change the reply.
    pub(super) deadline: tokio::time::Instant,
}

impl BatchCheck {
    /// The reply when the check cannot be awaited any longer: every written file's diagnostics
    /// are unknown, the write states are exactly as recorded.
    pub(super) fn unknown_reply(&self, reference: &str) -> PeerReply {
        let mut files: Vec<(String, EditDiagnostics)> = self
            .stages
            .iter()
            .filter_map(|stage| match stage {
                JobStage::EditAwaitingCheck { path, .. } => {
                    Some((path.clone(), EditDiagnostics::Unknown {}))
                }
                JobStage::EditBatchAwaitingCheck(_) => None,
            })
            .collect();
        files.extend(self.unchecked.iter().cloned());
        batch_reply(reference, &self.reports, &files)
    }
}

/// Builds the complete multi-file edit reply: per-file diagnostics joined to their report lines,
/// the summary first, and the job's own reference as the `source_ref` when any file is written.
fn batch_reply(
    reference: &str,
    reports: &[FileReport],
    diagnostics: &[(String, EditDiagnostics)],
) -> PeerReply {
    let (summary, words, new_lines) = batch::summarize_diagnostics(diagnostics);
    let mut reports = reports.to_vec();
    for ((path, _), word) in diagnostics.iter().zip(words) {
        if let Some(report) = reports.iter_mut().find(|report| report.path == *path) {
            report.diagnostics = Some(word);
        }
    }
    let written = reports.iter().any(|report| report.state.written());
    let text =
        batch::render_edit_reply(&reports, written.then_some(reference), &summary, &new_lines);
    PeerReply::Complete {
        kind: ResultKind::EditBatch,
        text,
        detail_ref: Some(reference.to_owned()),
        truncated: false,
        continuation: false,
    }
}

impl Worker<'_> {
    /// `ide.edit {operation_id, source_ref?, files}`: one logical change across several files.
    ///
    /// A new operation validates every file first and writes nothing on any refusal
    /// (`edit_refused`, every failed file and change named, the `operation_id` left unused). It
    /// then records the durable receipt and commits file by file; see the module documentation
    /// for the stop, resume and check rules. The same `operation_id` with the same request
    /// resumes (or, when settled, answers with every file written earlier); with a different
    /// request it answers `edit: conflicting_duplicate` and writes nothing. The reply is a
    /// complete `edit_batch` result whose text opens with `edit: applied …` or `edit: partial …`;
    /// its `detail_ref` is a valid edit base for every file written. The job may park while its
    /// project check runs, resuming through [`Self::finish_batch_check`].
    pub(super) async fn edit_files(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let (operation_id, files) =
            batch::edit_files_request(&job.parameters).ok_or(FailureCode::Internal)?;
        let digest = batch::edit_request_digest(&job.parameters);
        let receipts = self.edits.batches();
        if !matches!(
            receipts.install_schema().await,
            Ok(crate::app::store::MigrationAdmission::Applied { .. }
                | crate::app::store::MigrationAdmission::AlreadyApplied { .. })
        ) {
            job.failure_detail = Some("edit:batch_receipts_unavailable".to_owned());
            return Err(FailureCode::Internal);
        }
        let existing = receipts.load(&operation_id).await.map_err(|_| {
            job.failure_detail = Some("edit:batch_receipt_unreadable".to_owned());
            FailureCode::Internal
        })?;
        if let Some(existing) = existing {
            return self
                .resume_files(job, &binding, &operation_id, &files, digest, existing)
                .await;
        }
        let mut planned = Vec::with_capacity(files.len());
        let mut refusals = Vec::new();
        for (index, file) in files.iter().enumerate() {
            match self.plan_file(job, &binding, index, file, None).await? {
                Ok(plan) => planned.push(plan),
                Err(sentence) => refusals.push(sentence),
            }
        }
        if !refusals.is_empty() {
            job.failure_detail = Some(format!(
                "edit:refused: {} of {} files refused, nothing written — {}",
                refusals.len(),
                files.len(),
                crate::assistance::reply::bounded_utf8_prefix(
                    &refusals.join("; "),
                    MAX_BATCH_REFUSAL_BYTES
                )
            ));
            return Err(FailureCode::EditRefused);
        }
        let rows: Vec<BatchFile> = planned
            .iter()
            .map(|plan| BatchFile {
                path: plan.path.clone(),
                base: plan.base.bytes().map(|bytes| *bytes.digest()),
                candidate: *blake3::hash(plan.candidate.as_bytes()).as_bytes(),
                written: false,
            })
            .collect();
        match receipts.prepare(&operation_id, digest, &rows).await {
            Ok(BatchAdmission::Prepared) => {}
            // Another call recorded the same operation between our load and prepare: answer as
            // its retry does, from the receipt it left.
            Ok(BatchAdmission::Existing(existing)) => {
                return self
                    .resume_files(job, &binding, &operation_id, &files, digest, existing)
                    .await;
            }
            Err(_) => {
                job.failure_detail = Some("edit:batch_receipt_unavailable".to_owned());
                return Err(FailureCode::Internal);
            }
        }
        let receipt = BatchReceipt {
            request_digest: digest,
            settled: false,
            files: rows,
        };
        self.commit_files(
            job,
            &binding,
            &operation_id,
            &files,
            &receipt,
            planned.into_iter().map(Some).collect(),
        )
        .await
    }

    /// Answers a call whose `operation_id` already has a receipt: a different request is a
    /// conflicting duplicate (nothing written), a settled operation answers every file as
    /// written earlier, and a prepared one resumes the commit from its digests.
    async fn resume_files(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        operation_id: &str,
        files: &[EditFile],
        digest: [u8; 32],
        existing: BatchReceipt,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        if existing.request_digest != digest || existing.files.len() != files.len() {
            let authority = self.authority(binding).await.ok();
            return Ok((
                PeerReply::Complete {
                    kind: ResultKind::EditBatch,
                    text: format!(
                        "edit: conflicting_duplicate; operation_id {operation_id} already names a \
                         different multi-file edit; nothing written\n"
                    ),
                    detail_ref: None,
                    truncated: false,
                    continuation: false,
                },
                authority,
                None,
            ));
        }
        let planned = files.iter().map(|_| None).collect();
        self.commit_files(job, binding, operation_id, files, &existing, planned)
            .await
    }

    /// Phase 1 for one file: observes it, admits its base and builds its candidate, writing
    /// nothing.
    ///
    /// `resume_base` is `Some` when a resume already observed the file and proved its bytes equal
    /// the recorded base: that observation is then the base and no `source_ref` is consulted (a
    /// retry after a daemon restart has none left). Returns `Ok(Err(sentence))` for a refusal
    /// about this file (`file <k> <path>: …`), and `Err` for a failure that is not about it.
    async fn plan_file(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        index: usize,
        file: &EditFile,
        resume_base: Option<(SourceObservation, Vec<u8>)>,
    ) -> Result<Result<Planned, String>, FailureCode> {
        let label = format!("file {} {}", index + 1, file.path);
        let resuming = resume_base.is_some();
        let (observed, bytes) = match resume_base {
            Some(observation) => observation,
            None => self.observe(binding, file.path.clone().into()).await?,
        };
        let path = file.path.clone();
        if let EditBody::Create(content) = &file.body {
            if observed.state() != crate::workspace::observation::ObservedState::Missing {
                return Ok(Err(format!(
                    "{label}: already exists; \"create\" needs a missing path"
                )));
            }
            let candidate = self
                .format_candidate(&observed, Path::new(&path), content.clone())
                .await;
            return Ok(Ok(Planned {
                path,
                base: observed,
                candidate,
                note: None,
            }));
        }
        if no_such_file(observed.state(), &path).is_some() {
            return Ok(Err(format!("{label}: no such file")));
        }
        let base = match (&file.source_ref, resuming) {
            (Some(reference), false) => {
                let retained = self.shared.ledger.lock().ok().and_then(|ledger| {
                    ledger.details.get(reference).and_then(|detail| {
                        let source = admitted_edit_source(detail, binding, reference, &path)?;
                        let movement =
                            admitted_edit_line_movement(detail, binding, reference, &path);
                        Some((source, movement))
                    })
                });
                let lines = matches!(&file.body, EditBody::Changes(entries)
                    if entries.iter().any(|entry| entry.get("lines").is_some()));
                match retained {
                    Some((_, Some((line, delta)))) if lines => {
                        return Ok(Err(format!(
                            "{label}: stale_source (edit:lines_moved; the edit that produced \
                             {reference} moved lines after line {line} by {delta:+})"
                        )));
                    }
                    Some((retained, _)) if retained.bytes() == observed.bytes() => retained,
                    _ => {
                        return Ok(Err(format!(
                            "{label}: stale_source (changed since {reference})"
                        )));
                    }
                }
            }
            // A resume's base is the fresh observation the caller matched to the recorded base;
            // a symbol-only change without `source_ref` uses the fresh observation as the
            // one-file form does (the facade refused the rest).
            _ => observed.clone(),
        };
        match &file.body {
            EditBody::Replace(content) => {
                observed_text(&observed, &bytes)?;
                Ok(Ok(Planned {
                    path,
                    base,
                    candidate: content.clone(),
                    note: None,
                }))
            }
            EditBody::Changes(entries) => {
                match self
                    .changes_candidate(job, &observed, &bytes, &path, entries)
                    .await?
                {
                    Ok((candidate, note)) => Ok(Ok(Planned {
                        path,
                        base,
                        candidate,
                        note: note.lines().next().map(str::to_owned),
                    })),
                    Err(refusal) => Ok(Err(format!("{label}: {}", refusal.detail()))),
                }
            }
            EditBody::Create(_) => Err(FailureCode::Internal),
        }
    }

    /// Phase 2: writes the files in request order against `receipt`, then schedules the one
    /// project check and answers.
    ///
    /// `planned[k]` is the phase 1 plan of file `k` in a new operation and `None` on a resume.
    /// A file the receipt marks written, or whose bytes equal its recorded candidate, counts as
    /// written earlier. On a resume an unwritten file is written only when its bytes equal its
    /// recorded base and its recomputed candidate equals the recorded one; otherwise the commit
    /// stops there as `changed after validation`. The first file that changed, failed or has an
    /// unknown result stops the commit and every later file is `not reached`. No error after
    /// the first write leaves this function: each becomes the file's state, so the reply always
    /// says exactly what was written.
    async fn commit_files(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        operation_id: &str,
        files: &[EditFile],
        receipt: &BatchReceipt,
        mut planned: Vec<Option<Planned>>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let authority = self.authority(binding).await?;
        let receipts = self.edits.batches();
        let mut reports: Vec<FileReport> = Vec::with_capacity(files.len());
        let mut sources: Vec<SourceObservation> = Vec::new();
        let mut preexisting: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        let mut stopped = false;
        let mut unknown = false;
        for (index, (file, row)) in files.iter().zip(&receipt.files).enumerate() {
            let report = |state: FileState, note: Option<String>| FileReport {
                path: file.path.clone(),
                state,
                note,
                diagnostics: None,
            };
            if stopped {
                reports.push(report(FileState::NotWritten(NotWritten::NotReached), None));
                continue;
            }
            let plan = match planned[index].take() {
                Some(plan) => plan,
                None => {
                    let Ok((observed, bytes)) =
                        self.observe(binding, file.path.clone().into()).await
                    else {
                        stopped = true;
                        reports.push(report(FileState::NotWritten(NotWritten::IoError), None));
                        continue;
                    };
                    let current = observed.bytes().map(|bytes| *bytes.digest());
                    if row.written || current == Some(row.candidate) {
                        if current == Some(row.candidate) {
                            sources.push(observed);
                        }
                        if !row.written {
                            let _ = receipts.mark_written(operation_id, index).await;
                        }
                        reports.push(report(FileState::WrittenEarlier, None));
                        continue;
                    }
                    if current != row.base {
                        stopped = true;
                        reports.push(report(
                            FileState::NotWritten(NotWritten::ChangedAfterValidation),
                            None,
                        ));
                        continue;
                    }
                    match self
                        .plan_file(job, binding, index, file, Some((observed, bytes)))
                        .await
                    {
                        Ok(Ok(plan))
                            if *blake3::hash(plan.candidate.as_bytes()).as_bytes()
                                == row.candidate =>
                        {
                            plan
                        }
                        _ => {
                            stopped = true;
                            reports.push(report(
                                FileState::NotWritten(NotWritten::ChangedAfterValidation),
                                None,
                            ));
                            continue;
                        }
                    }
                }
            };
            preexisting.insert(
                plan.path.clone(),
                self.pre_edit_problem_keys(&authority, &plan.path),
            );
            if fault_seam(&format!("batch_edit_before_write_{}", index + 1)) {
                // Test seam only (never true in a release build): a native write lands between
                // this file's validation and its write.
                let target = authority.worktree().worktree_path().join(&plan.path);
                let _ = std::fs::OpenOptions::new()
                    .append(true)
                    .open(target)
                    .and_then(|mut file| std::io::Write::write_all(&mut file, b"native\n"));
            }
            let outcome = self
                .write_batch_file(job, binding, operation_id, &plan)
                .await;
            if fault_seam(&format!("batch_edit_after_write_{}", index + 1)) {
                panic!("agent-ide test seam: deliberate panic after multi-file edit write");
            }
            use crate::workspace::edit::EditOutcome as Written;
            let (state, read) = match outcome {
                Written::Created(read) => (FileState::Created, Some(read)),
                Written::Replaced(read) => (FileState::Replaced, Some(read)),
                Written::Unchanged(read) => (FileState::Unchanged, Some(read)),
                Written::StaleSource => (
                    FileState::NotWritten(NotWritten::ChangedAfterValidation),
                    None,
                ),
                Written::OutcomeUnknown { .. } => (FileState::Unknown, None),
                Written::UnsafeTarget
                | Written::CancelledNoEffect
                | Written::DeadlineNoEffect
                | Written::CapacityNoEffect => (FileState::NotWritten(NotWritten::IoError), None),
            };
            if let Some(read) = read {
                let _ = receipts.mark_written(operation_id, index).await;
                if let Some(observed) = self
                    .post_write_source(job, binding, &plan.path, &read)
                    .await
                {
                    sources.push(observed);
                }
                reports.push(report(state, plan.note));
            } else {
                stopped = true;
                unknown |= state == FileState::Unknown;
                reports.push(report(state, None));
            }
        }
        if reports.iter().all(|report| report.state.written()) {
            let _ = receipts.settle(operation_id).await;
        }
        if unknown {
            self.unsettled_edits
                .entry(binding.clone())
                .or_default()
                .insert(operation_id.to_owned());
        }
        self.schedule_batch_check(job, binding, authority, reports, sources, preexisting)
            .await
    }

    /// Writes one planned file through Workspace's stale-safe path: authority, the base's edit
    /// reference, the permit and the descriptor-held write, which refuses `stale_source` when the
    /// file's bytes no longer equal the base. Every failure is a Workspace outcome, never an error.
    async fn write_batch_file(
        &mut self,
        job: &Job,
        binding: &BindingRef,
        operation_id: &str,
        plan: &Planned,
    ) -> crate::workspace::edit::EditOutcome {
        use crate::workspace::edit::{EditOutcome, EditSourceRef};
        let Ok(authority) = self.authority(binding).await else {
            return EditOutcome::CancelledNoEffect;
        };
        let edit_source = match EditSourceRef::from_observation(&plan.base) {
            Ok(source) => source,
            Err(outcome) => return outcome,
        };
        let Ok(active) = self.shared.active(binding) else {
            return EditOutcome::CancelledNoEffect;
        };
        let (permit, target) = match self
            .workspace
            .prepare_edit(
                &authority,
                &active,
                operation_id.to_owned(),
                plan.path.clone().into(),
                edit_source.clone(),
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        if *job.cancel.borrow() {
            return EditOutcome::CancelledNoEffect;
        }
        if tokio::time::Instant::now() >= job.deadline {
            return EditOutcome::DeadlineNoEffect;
        }
        let Ok(active) = self.shared.active(binding) else {
            return EditOutcome::CancelledNoEffect;
        };
        self.workspace
            .replace_edit(
                &authority,
                &active,
                permit,
                target,
                &edit_source,
                plan.candidate.as_bytes(),
                || !*job.cancel.borrow() && tokio::time::Instant::now() < job.deadline,
            )
            .await
    }

    /// Observes a just-written file and returns the observation as an edit base only when it is
    /// exactly the Workspace post-read and no native write intervened (the one-file rule).
    async fn post_write_source(
        &mut self,
        job: &Job,
        binding: &BindingRef,
        path: &str,
        read: &crate::workspace::edit::EditPostRead,
    ) -> Option<SourceObservation> {
        let (observed, bytes) = self.observe(binding, path.into()).await.ok()?;
        let current_epoch = self
            .shared
            .ledger
            .lock()
            .ok()
            .and_then(|ledger| ledger.native_epoch.get(binding).copied())
            .unwrap_or(0);
        exact_post_read_observation(read, &observed, &bytes, job.native_epoch, current_epoch)
            .then_some(observed)
    }

    /// Triggers the one project check of the written files — one check generation per affected
    /// checked language — and parks the job until it lands or its deadline passes; without a
    /// project feed, without written files of a checked language, or without time left, it
    /// answers at once.
    async fn schedule_batch_check(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        authority: AuthorityStamp,
        reports: Vec<FileReport>,
        sources: Vec<SourceObservation>,
        mut preexisting: BTreeMap<String, BTreeMap<String, u32>>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let worktree = authority.worktree().worktree_path().to_path_buf();
        let deadline = job
            .deadline
            .checked_sub(EDIT_SETTLEMENT_RESERVE)
            .map(|deadline| deadline.min(tokio::time::Instant::now() + EDIT_CHECK_WAIT));
        let mut stages = Vec::new();
        let mut unchecked = Vec::new();
        let mut generations: BTreeMap<String, Option<u64>> = BTreeMap::new();
        for report in reports.iter().filter(|report| {
            matches!(
                report.state,
                FileState::Created | FileState::Replaced | FileState::Unchanged
            )
        }) {
            let checked = Language::for_path(Path::new(&report.path))
                .filter(|language| language.checks().is_some());
            let (Some(language), Some(feed), Some(deadline)) =
                (checked, self.shared.project_feed.as_ref(), deadline)
            else {
                if Language::for_path(Path::new(&report.path)).is_none() {
                    unchecked.push((
                        report.path.clone(),
                        EditDiagnostics::NotAnalysed {
                            reason: "no IDE language checks this file type".to_owned(),
                        },
                    ));
                }
                continue;
            };
            // One generation per language: the first written file of the language triggers it,
            // the rest of that language read the same check.
            let generation = *generations
                .entry(language.name().to_owned())
                .or_insert_with(|| {
                    feed.changed_generation(&binding.fingerprint(), Some(&report.path))
                });
            let Some(generation) = generation else {
                continue;
            };
            job.check_scheduled = true;
            stages.push(JobStage::EditAwaitingCheck {
                result: EditResult {
                    operation_id: String::new(),
                    path: report.path.clone(),
                    outcome: ChangesEditOutcome::Replaced,
                    source_ref: None,
                },
                refreshed: None,
                authority: authority.clone(),
                path: report.path.clone(),
                fallback: EditDiagnostics::Unknown {},
                generation,
                worktree: worktree.clone(),
                wanted: report.path.trim_start_matches("./").to_owned(),
                preexisting: preexisting.remove(&report.path).unwrap_or_default(),
                language,
                deadline,
            });
        }
        let check = BatchCheck {
            reports,
            sources,
            stages,
            unchecked,
            authority,
            deadline: deadline.unwrap_or_else(tokio::time::Instant::now),
        };
        self.finish_batch_check(job, check).await
    }

    /// Answers a parked multi-file edit once every per-file check result is known, or with
    /// `unknown` diagnostics at its deadline or on cancellation; otherwise parks it again for a
    /// quarter second. The answer retains every written file's post-write observation, so the
    /// reply's `source_ref` is an edit base for each.
    pub(super) async fn finish_batch_check(
        &mut self,
        job: &mut Job,
        check: BatchCheck,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let expired = *job.cancel.borrow() || tokio::time::Instant::now() >= check.deadline;
        let mut diagnostics = Vec::with_capacity(check.stages.len() + check.unchecked.len());
        for stage in &check.stages {
            let JobStage::EditAwaitingCheck { path, .. } = stage else {
                continue;
            };
            match self.check_diagnostics(stage) {
                Some(found) => diagnostics.push((path.clone(), found)),
                None if expired => diagnostics.push((path.clone(), EditDiagnostics::Unknown {})),
                None => {
                    job.stage = Some(JobStage::EditBatchAwaitingCheck(Box::new(check)));
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                    return Err(FailureCode::ProviderLoading);
                }
            }
        }
        diagnostics.extend(check.unchecked.iter().cloned());
        let reply = batch_reply(&job.reference, &check.reports, &diagnostics);
        let mut sources = check.sources;
        let first = (!sources.is_empty()).then(|| sources.remove(0));
        self.shared.add_edit_sources(&job.reference, sources);
        Ok((reply, Some(check.authority), first))
    }
}
