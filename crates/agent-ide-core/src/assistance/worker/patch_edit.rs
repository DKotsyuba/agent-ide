//! `ide.edit {patch}` (symbol-read-edit-v1 section 5): a Codex `apply_patch` body applied
//! through the multi-file pipeline.
//!
//! The patch is parsed and every `Update File` hunk is located against the file's current bytes
//! before anything is written; any parse error or miss refuses the whole call with nothing
//! written. The resulting whole-file contents (`Add File` as creations, `Update File` as
//! replacements) then run through [`Worker::edit_files`], so the patch inherits that
//! pipeline's atomicity, receipts, immediate reply and single `edit-checked` fact.

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
    /// delete/move refusal). A missing file or an unmatched hunk is refused as `edit_refused`
    /// with `patch refused, nothing written: …` plus the nearest text and one repair line. On
    /// success the call is rewritten to the equivalent `files` request and delegated to
    /// [`Self::edit_files`], so its reply, receipt and project check are that pipeline's.
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
        let files = self.patch_files(job, &binding, parsed).await?;
        job.parameters = json!({"operation_id": operation_id, "files": files});
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
    /// does not match, quoting the nearest text; observation failures propagate. Nothing is
    /// written either way.
    pub(super) async fn patch_files(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        parsed: Vec<PatchFile>,
    ) -> Result<Vec<serde_json::Value>, FailureCode> {
        let mut files = Vec::with_capacity(parsed.len());
        for file in parsed {
            match file {
                PatchFile::Add { path, content } => {
                    files.push(json!({"path": path, "create": true, "content": content}));
                }
                PatchFile::Update { path, hunks } => {
                    let (observed, bytes) = self
                        .observe(binding, std::path::PathBuf::from(&path))
                        .await?;
                    if no_such_file(observed.state(), &path).is_some() {
                        return Err(refuse(job, format!("{path} does not exist")));
                    }
                    let source = observed_text(&observed, &bytes)?.to_owned();
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
