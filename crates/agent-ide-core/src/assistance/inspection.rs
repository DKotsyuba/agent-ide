//! `ide.inspect` consolidation: retained references and a bounded `wait_s` completion wait.
//!
//! One `ide.inspect` call reads one reference (as before) or up to eight references in order.
//! Waiting runs outside the worker's inspection loop and never blocks other sessions. Expiry
//! cancels no job. The unadvertised `wait_ms` remains compatible for existing clients.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use super::super::{
    content::{self, Envelope},
    host_binding::BindingRef,
    outbox::{MAX_DETAIL_REFS, MAX_WAIT_MS},
    reply::{FailureCode, PeerReply, ResultKind},
};
use super::WorkerHandle;

/// Pause between re-reads of references that are still pending during a wait.
const REREAD_PAUSE: Duration = Duration::from_millis(250);
/// Byte budget of the whole consolidated reply text, below the 64 KiB reply envelope.
const REPLY_TEXT_BUDGET: usize = 48 * 1024;

/// Cuts `text` to at most `limit` bytes on a character boundary.
fn cut(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Renders one reply's compact model text, or a fixed line when it cannot be projected.
fn compact(reply: &PeerReply) -> String {
    content::render_with_status(reply.clone(), None, Envelope::TextOnly)
        .and_then(|rendered| {
            rendered
                .content
                .first()
                .and_then(|block| block.as_text())
                .map(|text| text.text.clone())
        })
        .unwrap_or_else(|| "result could not be rendered".to_owned())
}

/// Wraps consolidated text as one complete `inspect` reply.
fn inspect_reply(text: String) -> PeerReply {
    PeerReply::Complete {
        kind: ResultKind::Inspect,
        text,
        detail_ref: None,
        truncated: false,
        continuation: false,
    }
}

impl WorkerHandle {
    /// Retrieves one reference under its ordinary delivery context, bounding the retrieval
    /// itself when a deadline exists. Expiry preserves the job and refuses disclosure because
    /// the inspection has not completed its authority/source checks.
    async fn inspect_until(
        &self,
        binding: BindingRef,
        reference: String,
        attachment: &str,
        delivery: Arc<std::sync::atomic::AtomicBool>,
        deadline: Option<tokio::time::Instant>,
    ) -> PeerReply {
        let inspect = self.with_delivery(
            delivery,
            self.inspect(binding, reference.clone(), attachment, None),
        );
        match deadline {
            Some(deadline) => {
                tokio::time::timeout_at(deadline, inspect)
                    .await
                    .unwrap_or(PeerReply::Error {
                        code: FailureCode::Deadline,
                        detail: Some("inspect:deadline".to_owned()),
                    })
            }
            None => inspect.await,
        }
    }

    /// Serves one `ide.inspect` call in any of its forms (OUTBOX §8.5).
    ///
    /// `parameters` are the validated call arguments: `detail_ref` (one reference, the original
    /// form) or `detail_refs` (1-8 references answered in order), with optional `wait_s` (1–30
    /// seconds) or legacy `wait_ms` (clamped to [`MAX_WAIT_MS`]), never both. `delivery` is the
    /// call's QW-4 delivery flag. A wait returns as
    /// soon as a named reference settles; at its deadline it answers with what is there and cancels
    /// nothing. Arguments naming neither form are refused; the facade validator never admits them.
    pub async fn inspect_call(
        &self,
        binding: BindingRef,
        parameters: &Value,
        attachment: &str,
        delivery: Arc<std::sync::atomic::AtomicBool>,
    ) -> PeerReply {
        let references = parameters
            .get("detail_ref")
            .and_then(Value::as_str)
            .into_iter()
            .chain(
                parameters
                    .get("detail_refs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        for reference in references.collect::<std::collections::BTreeSet<_>>() {
            let progress = self.shared.ledger.lock().ok().and_then(|ledger| {
                ledger
                    .details
                    .get(reference)
                    .filter(|detail| detail.binding == binding)
                    .map(|detail| detail.progress.clone())
            });
            if let Some(progress) = progress
                && let Ok(mut progress) = progress.lock()
                && progress.check_pending
            {
                progress.recovery(reference, false);
            }
        }
        let wait = parameters
            .get("wait_s")
            .and_then(Value::as_u64)
            .map(|seconds| Duration::from_secs(seconds.min(30)))
            .or_else(|| {
                parameters
                    .get("wait_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| Duration::from_millis(ms.min(MAX_WAIT_MS)))
            })
            .unwrap_or_default();
        let deadline = tokio::time::Instant::now() + wait;
        if let Some(reference) = parameters.get("detail_ref").and_then(Value::as_str) {
            loop {
                let reply = self
                    .inspect_until(
                        binding.clone(),
                        reference.to_owned(),
                        attachment,
                        delivery.clone(),
                        (!wait.is_zero()).then_some(deadline),
                    )
                    .await;
                let pending = matches!(reply, PeerReply::Pending { .. })
                    || self.shared.edit_check_pending(reference);
                if pending && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(
                        REREAD_PAUSE
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await;
                }
                if !pending || tokio::time::Instant::now() >= deadline {
                    return self.progress_reply(&binding, reference, reply);
                }
            }
        }
        if let Some(references) = parameters.get("detail_refs").and_then(Value::as_array) {
            let references = references
                .iter()
                .filter_map(Value::as_str)
                .take(MAX_DETAIL_REFS)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            return self
                .inspect_many(
                    &binding,
                    &references,
                    attachment,
                    delivery,
                    deadline,
                    !wait.is_zero(),
                )
                .await;
        }
        PeerReply::InvalidParameters {
            message: "invalid bounded parameters: detail_ref or detail_refs is required".to_owned(),
        }
    }

    /// Answers several references in order within the reply budget; with a wait, re-reads until
    /// at least one of them settles or the deadline passes.
    async fn inspect_many(
        &self,
        binding: &BindingRef,
        references: &[String],
        attachment: &str,
        delivery: Arc<std::sync::atomic::AtomicBool>,
        deadline: tokio::time::Instant,
        bounded: bool,
    ) -> PeerReply {
        loop {
            let mut replies = Vec::with_capacity(references.len());
            for reference in references {
                let reply = self
                    .inspect_until(
                        binding.clone(),
                        reference.clone(),
                        attachment,
                        delivery.clone(),
                        bounded.then_some(deadline),
                    )
                    .await;
                replies.push(reply);
            }
            let settled = references.iter().zip(&replies).any(|(reference, reply)| {
                !matches!(reply, PeerReply::Pending { .. })
                    && !self.shared.edit_check_pending(reference)
            });
            if !settled && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(
                    REREAD_PAUSE
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .await;
            }
            if settled || tokio::time::Instant::now() >= deadline {
                let share = REPLY_TEXT_BUDGET / references.len().max(1);
                let sections = references
                    .iter()
                    .zip(&replies)
                    .map(|(reference, reply)| {
                        let reply = self.progress_reply(binding, reference, reply.clone());
                        format!("== {reference} ==\n{}", cut(&compact(&reply), share))
                    })
                    .collect::<Vec<_>>();
                return inspect_reply(sections.join("\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cutting never splits a multi-byte character.
    #[test]
    fn cut_stops_on_a_character_boundary() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(cut("héllo", 3), "hé");
    }
}
