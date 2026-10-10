//! `ide.inspect` consolidation (OUTBOX-r3 §8.5): `{}`, `detail_refs` and `wait_ms`.
//!
//! One `ide.inspect` call reads one reference (as before), up to eight references in order, or
//! everything the session's outbox holds. `wait_ms` waits inside the daemon, outside the worker's
//! inspection loop, so a waiting agent spends one model turn instead of many polls and never
//! blocks other sessions' inspections. A wait that ends without a result cancels nothing.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use super::super::{
    content::{self, Envelope},
    host_binding::BindingRef,
    outbox::{Carrier, FactKind, HandoffRequest, MAX_DETAIL_REFS, MAX_WAIT_MS, SessionOutbox},
    reply::{PeerReply, ResultKind},
    session_outbox::render_fact,
};
use super::WorkerHandle;

/// Pause between re-reads of references that are still pending during a wait.
const REREAD_PAUSE: Duration = Duration::from_millis(250);
/// Byte budget of the facts an `ide.inspect {}` reply reserves before expanding bodies.
const FACT_BUDGET: usize = 16 * 1024;
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

/// Returns the discard line for a source-bearing answer whose re-proof found its file changed
/// (OUTBOX §7), or `None` for any other reply.
fn discarded(reply: &PeerReply, subject: &str) -> Option<String> {
    let PeerReply::Error {
        detail: Some(detail),
        ..
    } = reply
    else {
        return None;
    };
    let path = detail.strip_prefix("inspect:source_changed:")?;
    Some(format!(
        "discarded: {} changed; repeat {subject}",
        path.trim_matches('"')
    ))
}

impl WorkerHandle {
    /// Serves one `ide.inspect` call in any of its forms (OUTBOX §8.5).
    ///
    /// `parameters` are the validated call arguments: `detail_ref` (one reference, the original
    /// form), `detail_refs` (1-8 references answered in order) or neither (everything the session's
    /// outbox holds), each with an optional `wait_ms` (clamped to [`MAX_WAIT_MS`]). `delivery` is
    /// the call's QW-4 delivery flag. A wait returns as soon as a fact is ready (`{}`) or a named
    /// reference settles; at its deadline it answers with what is there and cancels nothing.
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
        if !wait.is_zero() {
            self.shared
                .outbox
                .wait_ready(&binding, false, deadline)
                .await;
        }
        self.session_facts(&binding, attachment).await
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

    /// Hands over everything the session's outbox holds (`ide.inspect {}`): each fact once, with a
    /// deferred source-bearing answer expanded from its retained result after re-proof, or its
    /// discard line when a fenced file changed. With nothing ready, lists the work still in flight
    /// and the last delivered facts.
    async fn session_facts(&self, binding: &BindingRef, attachment: &str) -> PeerReply {
        let Some(reservation) = self.shared.reserve(
            binding,
            HandoffRequest {
                carrier: Carrier::Inspect,
                budget: FACT_BUDGET,
                holding_only: false,
            },
        ) else {
            let mut text = "nothing ready yet".to_owned();
            let in_flight = self.shared.outbox.in_flight_subjects(binding);
            if !in_flight.is_empty() {
                text.push_str(&format!("\nin flight: {}", in_flight.join(", ")));
            }
            let history = self.shared.outbox.history(binding);
            if !history.is_empty() {
                text.push_str("\nrecently delivered:");
                for line in history {
                    text.push_str("\n  ");
                    text.push_str(&line);
                }
            }
            return inspect_reply(text);
        };
        let mut text = String::new();
        for fact in &reservation.facts {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&render_fact(fact));
            let expandable = fact.kind == FactKind::JobReady && fact.source_bearing;
            if let Some(reference) = fact.detail_ref.as_ref().filter(|_| expandable) {
                let reply = self
                    .inspect(binding.clone(), reference.clone(), attachment, None)
                    .await;
                let body = discarded(&reply, &fact.subject).unwrap_or_else(|| compact(&reply));
                let room = REPLY_TEXT_BUDGET.saturating_sub(text.len() + 1);
                text.push('\n');
                text.push_str(cut(&body, room));
            }
        }
        if reservation.remaining > 0 {
            text.push_str(&format!(
                "\n(+{} more facts; ide.inspect {{}} lists them)",
                reservation.remaining
            ));
        }
        self.shared.commit(binding, reservation.token);
        inspect_reply(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a source-changed re-proof failure becomes a discard line naming the path and the call.
    #[test]
    fn discard_lines_name_the_changed_path_and_the_call() {
        let changed = PeerReply::Error {
            code: super::super::super::reply::FailureCode::SourceUnavailable,
            detail: Some("inspect:source_changed:\"src/a.rs\"".to_owned()),
        };
        assert_eq!(
            discarded(&changed, "ide.symbol src/a.rs#Foo").as_deref(),
            Some("discarded: src/a.rs changed; repeat ide.symbol src/a.rs#Foo")
        );
        let other = PeerReply::Error {
            code: super::super::super::reply::FailureCode::Deadline,
            detail: Some("inspect:deadline".to_owned()),
        };
        assert_eq!(discarded(&other, "x"), None);
        assert_eq!(cut("héllo", 2), "h");
    }
}
