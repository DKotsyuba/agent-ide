//! `ide.inspect` consolidation (OUTBOX-r3 §8.5): `detail_ref`, `detail_refs` and `wait_ms`.
//!
//! One `ide.inspect` call reads one reference (as before) or up to eight references in order.
//! `wait_ms` waits inside the daemon, outside the worker's inspection loop, so a waiting agent
//! spends one model turn instead of many polls and never blocks other sessions' inspections. A
//! wait that ends without a result cancels nothing.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use super::super::{
    content::{self, Envelope},
    host_binding::BindingRef,
    outbox::{MAX_DETAIL_REFS, MAX_WAIT_MS},
    reply::{PeerReply, ResultKind},
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
    /// Serves one `ide.inspect` call in any of its forms (OUTBOX §8.5).
    ///
    /// `parameters` are the validated call arguments: `detail_ref` (one reference, the original
    /// form) or `detail_refs` (1-8 references answered in order), each with an optional `wait_ms`
    /// (clamped to [`MAX_WAIT_MS`]). `delivery` is the call's QW-4 delivery flag. A wait returns as
    /// soon as a named reference settles; at its deadline it answers with what is there and cancels
    /// nothing. Arguments naming neither form are refused; the facade validator never admits them.
    pub async fn inspect_call(
        &self,
        binding: BindingRef,
        parameters: &Value,
        attachment: &str,
        delivery: Arc<std::sync::atomic::AtomicBool>,
    ) -> PeerReply {
        let wait = parameters
            .get("wait_ms")
            .and_then(Value::as_u64)
            .map(|ms| Duration::from_millis(ms.min(MAX_WAIT_MS)))
            .unwrap_or_default();
        let deadline = tokio::time::Instant::now() + wait;
        if let Some(reference) = parameters.get("detail_ref").and_then(Value::as_str) {
            loop {
                let reply = self
                    .with_delivery(
                        delivery.clone(),
                        self.inspect(binding.clone(), reference.to_owned(), attachment, None),
                    )
                    .await;
                if !matches!(reply, PeerReply::Pending { .. })
                    || tokio::time::Instant::now() >= deadline
                {
                    return reply;
                }
                tokio::time::sleep(REREAD_PAUSE.min(deadline - tokio::time::Instant::now())).await;
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
                .inspect_many(&binding, &references, attachment, delivery, deadline)
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
    ) -> PeerReply {
        loop {
            let mut replies = Vec::with_capacity(references.len());
            for reference in references {
                let reply = self
                    .with_delivery(
                        delivery.clone(),
                        self.inspect(binding.clone(), reference.clone(), attachment, None),
                    )
                    .await;
                replies.push(reply);
            }
            let settled = replies
                .iter()
                .any(|reply| !matches!(reply, PeerReply::Pending { .. }));
            if settled || tokio::time::Instant::now() >= deadline {
                let share = REPLY_TEXT_BUDGET / references.len().max(1);
                let sections = references
                    .iter()
                    .zip(&replies)
                    .map(|(reference, reply)| {
                        format!("== {reference} ==\n{}", cut(&compact(reply), share))
                    })
                    .collect::<Vec<_>>();
                return inspect_reply(sections.join("\n"));
            }
            tokio::time::sleep(REREAD_PAUSE.min(deadline - tokio::time::Instant::now())).await;
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
