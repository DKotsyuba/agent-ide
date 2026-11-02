//! `ide.edit {patch}` (symbol-read-edit-v1 section 5): a Codex `apply_patch` body applied
//! through the multi-file pipeline.
//!
//! The patch is parsed and every `Update File` hunk is located against the file's current bytes
//! before anything is written; any parse error or miss refuses the whole call with nothing
//! written. The resulting whole-file contents (`Add File` as creations, `Update File` as
//! replacements) then run through [`Worker::edit_files`], so the patch inherits that
//! pipeline's batch preflight, partial recovery, receipts, immediate reply and single `edit-checked` fact.

use super::*;
use crate::assistance::patch::{PatchFile, apply_hunks, parse_patch};
use serde_json::json;
use symbols::{no_such_file, observed_text};

/// Most source lines quoted back in a "nearest text" refusal.
const MAX_NEAREST_LINES: usize = 12;

impl Worker<'_> {
    /// Applies one `ide.edit {operation_id, patch}` call.
    ///
    /// Parse errors answer as invalid parameters with the parser's own sentence (including the
    /// delete/move refusal). More than the batch file limit refuses before source observation
    /// or writes, with a split hint. Absolute file headers are normalized beneath the active worktree
    /// before an `Update File` is observed; an outside path refuses with a worktree-relative
    /// repair hint. A missing file or unmatched hunk is refused as `edit_refused` with
    /// `patch refused, nothing written: …` plus the nearest text and one repair line. Normalized
    /// admitted paths enter progress before formatting; they never imply committed writes. On success
    /// the call is rewritten to the equivalent `files` request and delegated to
    /// [`Self::edit_files`], so its reply, receipt and project check are that pipeline's. Existing
    /// receipts compare the original patch first; written/candidate-equal files are not re-hunked,
    /// while unresolved files still require the receipt's base/candidate digest proof.
    ///
    /// The file text is read in [`Self::patch_files`] and written by `edit_files`; every
    /// replacement carries the digest of the bytes its hunks were applied to, so a file that
    /// changed in between is refused as changed while the edit was applied, never overwritten
    /// (symbol-read-edit-v1 section 3.2 rule 6).
    pub(super) async fn edit_patch(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let patch = job.parameters["patch"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let operation_id = job.parameters["operation_id"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let parsed = match parse_patch(&patch) {
            Ok(parsed) => parsed,
            Err(message) => {
                let authority = self.authority(&binding).await.ok();
                return Ok((PeerReply::InvalidParameters { message }, authority, None));
            }
        };
        if parsed.len() > crate::changes::batch::MAX_BATCH_FILES {
            return Err(refuse(
                job,
                format!(
                    "at most {} files per edit; split this patch into smaller edits",
                    crate::changes::batch::MAX_BATCH_FILES
                ),
            ));
        }
        let existing = self.batch_receipt(job, &operation_id).await?;
        if existing.as_ref().is_some_and(|receipt| {
            receipt.request_digest != crate::assistance::batch::edit_request_digest(&job.parameters)
        }) {
            return Ok((
                crate::assistance::batch::conflicting_duplicate_reply(&operation_id),
                self.authority(&binding).await.ok(),
                None,
            ));
        }
        let authority = self.authority(&binding).await?;
        let root = authority.worktree().worktree_path();
        let normalized = parsed
            .into_iter()
            .map(|file| {
                let (path, file) = match file {
                    PatchFile::Add { path, content } => {
                        let path = Self::relativize_edit_path(root, &path)?;
                        (path.clone(), PatchFile::Add { path, content })
                    }
                    PatchFile::Update { path, hunks } => {
                        let path = Self::relativize_edit_path(root, &path)?;
                        (path.clone(), PatchFile::Update { path, hunks })
                    }
                };
                crate::workspace::edit::preflight_edit_path(authority.worktree(), Path::new(&path))
                    .map_err(|reason| format!("{path}: {reason}"))?;
                Ok(file)
            })
            .collect::<Result<Vec<_>, String>>();
        let parsed = normalized.map_err(|detail| refuse(job, detail))?;
        let files = self
            .patch_files(job, &binding, parsed, existing.as_ref())
            .await?;
        if let Ok(mut progress) = job.progress.lock() {
            progress.requested = files
                .iter()
                .filter_map(|file| file.get("path").and_then(Value::as_str).map(str::to_owned))
                .collect();
        }
        job.parameters = json!({"operation_id": operation_id, "patch": patch, "files": files});
        self.edit_files(job).await
    }

    /// Turns the parsed patch operations into the `files` entries of the equivalent multi-file
    /// edit: an `Add File` is a creation; an `Update File` is a whole-content replacement
    /// computed from the file's current bytes, carrying `expect`, the blake3 hex digest of those
    /// bytes, so the write is refused if the file no longer has them.
    ///
    /// # Errors
    ///
    /// [`FailureCode::EditRefused`] (detail recorded on `job`) for a missing file or a hunk that
    /// does not match, quoting the nearest text; observation failures propagate. `existing` lets
    /// a retry retain candidate-equal files without applying their hunks twice. Nothing is
    /// written either way.
    pub(super) async fn patch_files(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        parsed: Vec<PatchFile>,
        existing: Option<&crate::changes::batch::BatchReceipt>,
    ) -> Result<Vec<serde_json::Value>, FailureCode> {
        let mut files = Vec::with_capacity(parsed.len());
        for file in parsed {
            match file {
                PatchFile::Add { path, content } => {
                    files.push(json!({"path": path, "create": true, "content": content}));
                }
                PatchFile::Update { path, hunks } => {
                    if existing.is_some_and(|receipt| {
                        receipt
                            .files
                            .iter()
                            .any(|row| row.path == path && row.written)
                    }) {
                        files.push(json!({"path": path, "content": ""}));
                        continue;
                    }
                    let (observed, bytes) = self
                        .observe(binding, std::path::PathBuf::from(&path))
                        .await?;
                    if no_such_file(observed.state(), &path).is_some() {
                        return Err(refuse(job, format!("{path} does not exist")));
                    }
                    let source = observed_text(&observed, &bytes)?.to_owned();
                    if existing.is_some_and(|receipt| {
                        receipt.files.iter().any(|row| {
                            row.path == path && row.candidate == *blake3::hash(&bytes).as_bytes()
                        })
                    }) {
                        files.push(json!({"path": path, "content": source}));
                        continue;
                    }
                    match apply_hunks(&source, &hunks) {
                        Ok(applied) => {
                            // The write is refused if the file no longer has the bytes the hunks
                            // were applied to (`expect` is the digest of those bytes).
                            files.push(json!({
                                "path": path,
                                "content": applied.text,
                                "expect": blake3::hash(&bytes).to_hex().to_string(),
                            }));
                        }
                        Err(miss) => {
                            let lines: Vec<&str> = source.lines().collect();
                            let start = (miss.nearest_start as usize).max(1);
                            let end = (miss.nearest_end as usize)
                                .min(lines.len())
                                .min(start + MAX_NEAREST_LINES - 1);
                            let mut text = format!(
                                "hunk {} of {path} does not match. Nearest text (lines {start}-{end} of {path}):\n",
                                miss.hunk
                            );
                            for number in start..=end.max(start) {
                                if let Some(line) = lines.get(number - 1) {
                                    text.push_str(&format!("{number}\t{line}\n"));
                                }
                            }
                            text.push_str("Repair: resend the hunk with these lines as context.");
                            return Err(refuse(job, text));
                        }
                    }
                }
            }
        }
        Ok(files)
    }
}

/// Records a `patch refused, nothing written: <detail>` refusal on the job and returns its code.
fn refuse(job: &mut Job, detail: String) -> FailureCode {
    job.failure_detail = Some(format!(
        "edit:refused: patch refused, nothing written: {detail}"
    ));
    FailureCode::EditRefused
}
