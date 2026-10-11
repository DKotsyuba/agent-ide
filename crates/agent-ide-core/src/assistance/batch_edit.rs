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

/// What one commit pass did, for the caller to answer from.
pub(super) struct Committed {
    /// Every requested file in request order with its state in this call.
    reports: Vec<FileReport>,
    /// Post-write observations of the files that now hold their candidate, request order.
    sources: Vec<SourceObservation>,
    /// Per written path, the problem identities the latest check reported before its write.
    preexisting: BTreeMap<String, BTreeMap<String, u32>>,
    /// Authority under which the files were written.
    authority: AuthorityStamp,
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
    /// The operation the `edit-checked` fact names.
    operation_id: String,
    /// The written files with their post-write digests, the fact's fence.
    fences: Vec<crate::assistance::outbox::FencedPath>,
    /// True once the caller got the reply with `diagnostics: pending`; the check's outcome is
    /// then pushed as one `edit-checked` fact instead (session-outbox-v1 §8.6).
    answered_early: bool,
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
        batch_reply(&self.operation_id, reference, &self.reports, &files)
    }
}

/// Builds the complete multi-file edit reply of `operation_id` ([`batch::edit_batch_reply`]):
/// per-file diagnostics joined to their report lines, the summary first, and the job's own
/// reference as the `source_ref` when any file is written.
fn batch_reply(
    operation_id: &str,
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
    batch::edit_batch_reply(
        operation_id,
        &reports,
        Some(reference),
        &summary,
        &new_lines,
    )
}

/// The diagnostics a settled operation's receipt keeps for its retries
/// ([`crate::changes::batch::BatchReceiptStore::record_diagnostics`]): the summary words, each
/// file's words by path and the new-problem lines, as JSON.
fn stored_diagnostics(diagnostics: &[(String, EditDiagnostics)]) -> String {
    let (summary, words, new_lines) = batch::summarize_diagnostics(diagnostics);
    let files = diagnostics
        .iter()
        .map(|(path, _)| path)
        .zip(words)
        .collect::<Vec<_>>();
    serde_json::json!({"summary": summary, "files": files, "new": new_lines}).to_string()
}

/// The reply of a retried, settled operation from the diagnostics its receipt kept
/// ([`stored_diagnostics`]): every file as written earlier with its settled words, the settled
/// summary and new-problem lines, and `reference` as the `source_ref`; `None` when the stored text
/// does not read back.
fn settled_reply(
    operation_id: &str,
    reference: &str,
    reports: &[FileReport],
    stored: &str,
) -> Option<PeerReply> {
    let value: Value = serde_json::from_str(stored).ok()?;
    let summary = value.get("summary")?.as_str()?;
    let mut reports = reports.to_vec();
    for entry in value.get("files")?.as_array()? {
        let (path, word) = (entry.get(0)?.as_str()?, entry.get(1)?.as_str()?);
        if let Some(report) = reports.iter_mut().find(|report| report.path == path) {
            report.diagnostics = Some(word.to_owned());
        }
    }
    let new_lines = value
        .get("new")?
        .as_array()?
        .iter()
        .filter_map(|line| line.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    Some(batch::edit_batch_reply(
        operation_id,
        &reports,
        Some(reference),
        summary,
        &new_lines,
    ))
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
            diagnostics: None,
        };
        let committed = self
            .commit_files(
                job,
                &binding,
                &operation_id,
                &files,
                &receipt,
                planned.into_iter().map(Some).collect(),
            )
            .await?;
        self.schedule_batch_check(job, &binding, &operation_id, committed)
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
                batch::conflicting_duplicate_reply(operation_id),
                authority,
                None,
            ));
        }
        let planned = files.iter().map(|_| None).collect();
        let committed = self
            .commit_files(job, binding, operation_id, files, &existing, planned)
            .await?;
        // A settled operation answers the diagnostics its settled check recorded.
        if let Some(stored) = existing.diagnostics.as_deref()
            && committed
                .reports
                .iter()
                .all(|report| report.state.written())
            && let Some(reply) =
                settled_reply(operation_id, &job.reference, &committed.reports, stored)
        {
            let Committed {
                mut sources,
                authority,
                ..
            } = committed;
            let first = (!sources.is_empty()).then(|| sources.remove(0));
            self.shared.add_edit_sources(&job.reference, sources);
            return Ok((reply, Some(authority), first));
        }
        self.schedule_batch_check(job, binding, operation_id, committed)
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

    /// Phase 2: writes the files in request order against `receipt` and reports each file.
    ///
    /// `planned[k]` is the plan of file `k`: from phase 1 in a new operation, recomputed by a
    /// caller that resumes (a rename), or `None` to recompute it here from `files[k]` on a
    /// resume. A file the receipt marks written counts as written earlier. A file without a plan
    /// whose bytes equal its recorded candidate counts as written earlier too; otherwise its plan
    /// is rebuilt with its current observation as the base. Every plan must match its receipt row
    /// (base digest and candidate digest), so nothing is written that the operation did not
    /// validate; a mismatch stops the commit there as `changed after validation`. The first file
    /// that changed, failed or has an unknown result stops the commit and every later file is
    /// `not reached`. No error after the first write leaves this function: each becomes the
    /// file's state, so the caller can always say exactly what was written. A fully written
    /// operation is settled; an unknown write keeps the binding unsettled for the handover guard.
    async fn commit_files(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        operation_id: &str,
        files: &[EditFile],
        receipt: &BatchReceipt,
        mut planned: Vec<Option<Planned>>,
    ) -> Result<Committed, FailureCode> {
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
            if row.written {
                if let Ok((observed, _)) = self.observe(binding, file.path.clone().into()).await
                    && observed.bytes().map(|bytes| *bytes.digest()) == Some(row.candidate)
                {
                    sources.push(observed);
                }
                reports.push(report(
                    FileState::WrittenEarlier {
                        created: row.base.is_none(),
                    },
                    None,
                ));
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
                    if observed.bytes().map(|bytes| *bytes.digest()) == Some(row.candidate) {
                        sources.push(observed);
                        let _ = receipts.mark_written(operation_id, index).await;
                        reports.push(report(
                            FileState::WrittenEarlier {
                                created: row.base.is_none(),
                            },
                            None,
                        ));
                        continue;
                    }
                    match self
                        .plan_file(job, binding, index, file, Some((observed, bytes)))
                        .await
                    {
                        Ok(Ok(plan)) => plan,
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
            if plan.base.bytes().map(|bytes| *bytes.digest()) != row.base
                || *blake3::hash(plan.candidate.as_bytes()).as_bytes() != row.candidate
            {
                stopped = true;
                reports.push(report(
                    FileState::NotWritten(NotWritten::ChangedAfterValidation),
                    None,
                ));
                continue;
            }
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
        // Count this call's writes for the outbox's staleness labels before any check fact is
        // registered, so the operation's own fact never labels its own writes.
        let written_now = reports
            .iter()
            .filter(|report| {
                matches!(
                    report.state,
                    FileState::Created | FileState::Replaced | FileState::Unchanged
                )
            })
            .map(|report| report.path.clone())
            .collect::<Vec<_>>();
        if !written_now.is_empty() {
            self.shared.outbox.note_edit(binding, &written_now);
        }
        if unknown {
            self.unsettled_edits
                .entry(binding.clone())
                .or_default()
                .insert(operation_id.to_owned());
        }
        Ok(Committed {
            reports,
            sources,
            preexisting,
            authority,
        })
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
        operation_id: &str,
        committed: Committed,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let Committed {
            reports,
            sources,
            mut preexisting,
            authority,
        } = committed;
        let worktree = authority.worktree().worktree_path().to_path_buf();
        let deadline = job
            .deadline
            .checked_sub(EDIT_SETTLEMENT_RESERVE)
            .map(|deadline| deadline.min(tokio::time::Instant::now() + EDIT_CHECK_WAIT));
        let mut stages = Vec::new();
        let mut unchecked = Vec::new();
        let mut generations: BTreeMap<String, Option<u64>> = BTreeMap::new();
        // Every file that holds its candidate is reported, one written by an earlier call of
        // the operation included; a file the check cannot cover is `not_analysed` with why.
        for report in reports.iter().filter(|report| report.state.written()) {
            let language = Language::for_path(Path::new(&report.path));
            let feed = self.shared.project_feed.as_ref();
            let not_analysed = match (language, feed, deadline) {
                (None, ..) => Some("no IDE language checks this file type"),
                (Some(language), ..) if language.checks().is_none() => {
                    Some("no project check covers this file type")
                }
                (_, None, _) => Some("no project check runs in this session"),
                (_, _, None) => Some("no time was left in this call for the project check"),
                _ => None,
            };
            let (None, Some(language), Some(feed), Some(deadline)) =
                (not_analysed, language, feed, deadline)
            else {
                unchecked.push((
                    report.path.clone(),
                    EditDiagnostics::NotAnalysed {
                        reason: not_analysed
                            .unwrap_or("no IDE language checks this file type")
                            .to_owned(),
                    },
                ));
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
                unchecked.push((
                    report.path.clone(),
                    EditDiagnostics::NotAnalysed {
                        reason: "the project check did not accept this change".to_owned(),
                    },
                ));
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
                // A file written by an earlier call has no pre-write record: the latest check
                // of it is its baseline, so only problems that appear now read as new.
                preexisting: preexisting
                    .remove(&report.path)
                    .unwrap_or_else(|| self.pre_edit_problem_keys(&authority, &report.path)),
                language,
                deadline,
            });
        }
        let fences = sources
            .iter()
            .map(|source| crate::assistance::outbox::FencedPath {
                path: source.path().display().to_string(),
                digest: source.bytes().map(|bytes| *bytes.digest()),
            })
            .collect();
        let check = BatchCheck {
            reports,
            sources,
            stages,
            unchecked,
            authority,
            deadline: deadline.unwrap_or_else(tokio::time::Instant::now),
            operation_id: operation_id.to_owned(),
            fences,
            answered_early: false,
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
        mut check: BatchCheck,
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
                    self.reply_batch_early(job, &mut check);
                    job.stage = Some(JobStage::EditBatchAwaitingCheck(Box::new(check)));
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                    return Err(FailureCode::ProviderLoading);
                }
            }
        }
        diagnostics.extend(check.unchecked.iter().cloned());
        if check.answered_early
            && let Some(work) = self.shared.outbox.take_holding(&job.reference)
        {
            let draft = batch::edit_checked_draft(
                &check.operation_id,
                &job.reference,
                &diagnostics,
                check.fences.clone(),
            );
            let mut recipients = vec![work.owner];
            recipients.extend(work.subscribers);
            self.shared
                .outbox
                .push_draft(&recipients, draft, work.epoch);
        }
        if check.reports.iter().all(|report| report.state.written()) {
            let _ = self
                .edits
                .batches()
                .record_diagnostics(&check.operation_id, &stored_diagnostics(&diagnostics))
                .await;
        }
        let reply = batch_reply(
            &check.operation_id,
            &job.reference,
            &check.reports,
            &diagnostics,
        );
        let mut sources = check.sources;
        let first = (!sources.is_empty()).then(|| sources.remove(0));
        self.shared.add_edit_sources(&job.reference, sources);
        Ok((reply, Some(check.authority), first))
    }

    /// Answers a multi-file edit whose project check is still running with `diagnostics:
    /// pending` at once, right after the writes (symbol-read-edit-v1 §3.3), as the one-file edit
    /// does.
    ///
    /// Only a caller still waiting is answered, only once, and only when the session's outbox
    /// can hold the check as in-flight work under the job's reference: the job stays parked on
    /// its check, and [`Self::finish_batch_check`] then pushes one `edit-checked` fact for the
    /// whole operation. Otherwise nothing changes and the final reply carries the diagnostics.
    fn reply_batch_early(&self, job: &mut Job, check: &mut BatchCheck) {
        if check.answered_early {
            return;
        }
        let waiting = job
            .stop_reply
            .as_ref()
            .is_some_and(|sender| !sender.is_closed());
        let binding = job.invocation.binding_ref().clone();
        if !waiting || !self.shared.outbox.is_open(&binding) {
            return;
        }
        let subject = format!("edit {}", check.operation_id);
        if !self
            .shared
            .outbox
            .begin_holding(&job.reference, &binding, &subject, check.deadline)
        {
            return;
        }
        let mut pending: Vec<(String, EditDiagnostics)> = check
            .stages
            .iter()
            .filter_map(|stage| match stage {
                JobStage::EditAwaitingCheck { path, .. } => Some((
                    path.clone(),
                    EditDiagnostics::Pending {
                        detail_ref: job.reference.clone(),
                    },
                )),
                JobStage::EditBatchAwaitingCheck(_) => None,
            })
            .collect();
        pending.extend(check.unchecked.iter().cloned());
        let reply = batch_reply(
            &check.operation_id,
            &job.reference,
            &check.reports,
            &pending,
        );
        if let Some(sender) = job.stop_reply.take() {
            if sender.send(reply).is_err() {
                let _ = self.shared.outbox.take_holding(&job.reference);
                return;
            }
            check.answered_early = true;
        }
    }

    /// Answers a rename whose `operation_id` already has a receipt without asking the language
    /// server: a different request is a conflicting duplicate, and a settled rename answers every
    /// file as written earlier. Returns `None` when there is no receipt or the rename is only
    /// partly written, so the caller recomputes the edit and resumes through
    /// [`Self::commit_rename`].
    pub(super) async fn settled_rename(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        operation_id: &str,
    ) -> Result<Option<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>)>, FailureCode>
    {
        let receipt = self.rename_receipt(job, operation_id).await?;
        let digest = batch::rename_request_digest(&job.parameters);
        match receipt {
            Some(existing) if existing.request_digest != digest => {
                let authority = self.authority(binding).await.ok();
                Ok(Some((
                    batch::conflicting_duplicate_reply(operation_id),
                    authority,
                    None,
                )))
            }
            Some(existing) if existing.settled => {
                let files = rename_files(&existing);
                let planned = existing.files.iter().map(|_| None).collect();
                let committed = self
                    .commit_files(job, binding, operation_id, &files, &existing, planned)
                    .await?;
                Ok(Some(self.partial_rename(job, operation_id, committed)))
            }
            _ => Ok(None),
        }
    }

    /// Writes one rename's files through the multi-file commit and answers it.
    ///
    /// `files` holds every file of the language server's edit with the observation its
    /// candidate was built from, in commit order (the caller puts the symbol's own file last, so
    /// a rename stopped midway still resolves its symbol on the retry); `note` is the rename's
    /// summary line. A new rename records its receipt first; a partly written one resumes from
    /// it, writing only files whose bytes still equal their recorded base and whose recomputed
    /// candidate equals the recorded one. When every file ends written and at least one was
    /// written now, the reply is the rename's usual `edit: renamed` reply (diagnostics of the
    /// last file, `source_ref` an edit base for every written file); otherwise it is the
    /// multi-file reply naming each file's state. A rename never waits for the project check.
    pub(super) async fn commit_rename(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        operation_id: &str,
        files: Vec<RenameFile>,
        note: String,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let digest = batch::rename_request_digest(&job.parameters);
        let receipt = match self.rename_receipt(job, operation_id).await? {
            Some(existing) if existing.request_digest != digest => {
                let authority = self.authority(binding).await.ok();
                return Ok((
                    batch::conflicting_duplicate_reply(operation_id),
                    authority,
                    None,
                ));
            }
            Some(existing) => existing,
            None => {
                let rows: Vec<BatchFile> = files
                    .iter()
                    .map(|file| BatchFile {
                        path: file.path.clone(),
                        base: file.base.bytes().map(|bytes| *bytes.digest()),
                        candidate: *blake3::hash(file.candidate.as_bytes()).as_bytes(),
                        written: false,
                    })
                    .collect();
                match self
                    .edits
                    .batches()
                    .prepare(operation_id, digest, &rows)
                    .await
                {
                    Ok(BatchAdmission::Prepared) => BatchReceipt {
                        request_digest: digest,
                        settled: false,
                        files: rows,
                        diagnostics: None,
                    },
                    Ok(BatchAdmission::Existing(existing)) if existing.request_digest == digest => {
                        existing
                    }
                    Ok(BatchAdmission::Existing(_)) => {
                        let authority = self.authority(binding).await.ok();
                        return Ok((
                            batch::conflicting_duplicate_reply(operation_id),
                            authority,
                            None,
                        ));
                    }
                    Err(_) => {
                        job.failure_detail = Some("edit:batch_receipt_unavailable".to_owned());
                        return Err(FailureCode::Internal);
                    }
                }
            }
        };
        let mut by_path: BTreeMap<String, RenameFile> = files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect();
        let planned = receipt
            .files
            .iter()
            .map(|row| {
                by_path.remove(&row.path).map(|file| Planned {
                    path: file.path,
                    base: file.base,
                    candidate: file.candidate,
                    note: None,
                })
            })
            .collect();
        let edit_files = rename_files(&receipt);
        let committed = self
            .commit_files(job, binding, operation_id, &edit_files, &receipt, planned)
            .await?;
        let complete = committed
            .reports
            .iter()
            .all(|report| report.state.written())
            && committed
                .reports
                .iter()
                .any(|report| !matches!(report.state, FileState::WrittenEarlier { .. }));
        if !complete {
            return Ok(self.partial_rename(job, operation_id, committed));
        }
        let Committed {
            reports,
            mut sources,
            authority,
            ..
        } = committed;
        let last = reports.last().ok_or(FailureCode::Internal)?;
        let outcome = match last.state {
            FileState::Created => ChangesEditOutcome::Created,
            FileState::Unchanged => ChangesEditOutcome::Unchanged,
            _ => ChangesEditOutcome::Replaced,
        };
        let diagnostics = self.file_diagnostics(job, binding, &last.path).await;
        let result = EditResult {
            operation_id: operation_id.to_owned(),
            path: last.path.clone(),
            outcome,
            source_ref: Some(job.reference.clone()),
        };
        let first = (!sources.is_empty()).then(|| sources.remove(0));
        self.shared.add_edit_sources(&job.reference, sources);
        Ok((
            PeerReply::Edit {
                result,
                diagnostics,
                note: Some(note),
                operation: Some("renamed".to_owned()),
            },
            Some(authority),
            first,
        ))
    }

    /// The multi-file reply of a rename that is not complete (or was written entirely by an
    /// earlier call): every file's state, unknown diagnostics for the written ones, and a
    /// `source_ref` covering the files that hold their candidate.
    fn partial_rename(
        &self,
        job: &Job,
        operation_id: &str,
        committed: Committed,
    ) -> (PeerReply, Option<AuthorityStamp>, Option<SourceObservation>) {
        let Committed {
            reports,
            mut sources,
            authority,
            ..
        } = committed;
        let diagnostics = reports
            .iter()
            .filter(|report| report.state.written())
            .map(|report| (report.path.clone(), EditDiagnostics::Unknown {}))
            .collect::<Vec<_>>();
        let reply = batch_reply(operation_id, &job.reference, &reports, &diagnostics);
        let first = (!sources.is_empty()).then(|| sources.remove(0));
        self.shared.add_edit_sources(&job.reference, sources);
        (reply, Some(authority), first)
    }

    /// Loads the receipt of a rename's operation id after making sure its schema exists.
    async fn rename_receipt(
        &mut self,
        job: &mut Job,
        operation_id: &str,
    ) -> Result<Option<BatchReceipt>, FailureCode> {
        let receipts = self.edits.batches();
        if !matches!(
            receipts.install_schema().await,
            Ok(crate::app::store::MigrationAdmission::Applied { .. }
                | crate::app::store::MigrationAdmission::AlreadyApplied { .. })
        ) {
            job.failure_detail = Some("edit:batch_receipts_unavailable".to_owned());
            return Err(FailureCode::Internal);
        }
        receipts.load(operation_id).await.map_err(|_| {
            job.failure_detail = Some("edit:batch_receipt_unreadable".to_owned());
            FailureCode::Internal
        })
    }

    /// The language server's diagnostics for one just-written file, as a one-file edit reports
    /// them without waiting for the project check: `unknown` when no exact provider answer
    /// arrives before the edit's diagnostic deadline.
    async fn file_diagnostics(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        path: &str,
    ) -> EditDiagnostics {
        let Ok((observed, bytes)) = self.observe(binding, path.into()).await else {
            return EditDiagnostics::Unknown {};
        };
        let Some(deadline) = edit_diagnostic_deadline(job.deadline) else {
            return EditDiagnostics::Unknown {};
        };
        let original = std::mem::replace(&mut job.deadline, deadline);
        let result = self
            .semantic_context(
                job,
                &observed,
                &bytes,
                crate::intelligence::context::ContextQuery::File,
            )
            .await;
        job.deadline = original;
        match result {
            Ok(Some(provider)) => EditDiagnostics::from_snapshot(
                path.trim_start_matches("./"),
                &provider.context,
                &provider.diagnostics,
            ),
            _ => EditDiagnostics::Unknown {},
        }
    }
}

/// One file of a rename's language-server edit, ready to commit.
pub(super) struct RenameFile {
    /// Worktree-relative path.
    pub(super) path: String,
    /// The observation the candidate was built from; the write is checked against it.
    pub(super) base: SourceObservation,
    /// The file's bytes after the rename.
    pub(super) candidate: String,
}

/// The commit's per-file requests of a rename receipt: whole-content replacements whose content
/// is never recomputed from the request (a rename's candidates come from the language server),
/// so a file without a fresh plan cannot be written and stops the commit honestly.
fn rename_files(receipt: &BatchReceipt) -> Vec<EditFile> {
    receipt
        .files
        .iter()
        .map(|row| EditFile {
            path: row.path.clone(),
            source_ref: None,
            body: EditBody::Replace(String::new()),
        })
        .collect()
}
