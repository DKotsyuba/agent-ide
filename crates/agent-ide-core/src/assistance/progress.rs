//! Exclusive per-request stage timing and truthful edit progress, retained with each job.

use super::{AssistanceTool, INLINE_REPLY_WAIT, PeerReply, ResultKind};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Bounded request progress; paths come only from admitted request/commit results.
#[derive(Default)]
pub(super) struct Progress {
    /// Caller-supplied edit id used to join only identical unsettled requests before a receipt exists.
    pub(super) operation_id: Option<String>,
    /// A durable receipt is available while its independently accepted check still owes settlement.
    pub(super) check_pending: bool,
    /// Monotonic request admission, absent only in manually constructed test jobs.
    pub(super) requested_at: Option<Instant>,
    /// Whether the first usable result has already been journaled independently of diagnostics.
    pub(super) usable_reported: bool,
    /// Number of truthful progress envelopes returned before useful completion.
    pub(super) progress_replies: u32,
    /// Explicit recovery/retry requests, excluding ordinary diagnostic settlement.
    pub(super) recovery_requests: u32,
    /// Active stage and its monotonic start, absent before execution or after settlement.
    pub(super) active: Option<(&'static str, Instant)>,
    /// Exclusive accumulated time by the closed stage names used by this module's callers.
    pub(super) durations: BTreeMap<&'static str, Duration>,
    /// Paths whose write outcome is confirmed; no speculative candidate is listed here.
    pub(super) committed: Vec<String>,
    /// Requested file paths or addresses, preserved without interpreting them as write outcomes.
    pub(super) requested: Vec<String>,
}

impl Progress {
    /// Accounts for elapsed time in the previous stage and starts `stage`, or stops timing.
    pub(super) fn switch(&mut self, stage: Option<&'static str>) -> Option<&'static str> {
        let previous = self.active.take().map(|(name, started)| {
            *self.durations.entry(name).or_default() += started.elapsed();
            name
        });
        self.active = stage.map(|name| (name, Instant::now()));
        previous
    }

    /// Starts a nested stage, restoring its parent when the returned guard is dropped.
    pub(super) fn enter(progress: &Arc<Mutex<Self>>, stage: &'static str) -> StageGuard {
        let previous = progress
            .lock()
            .ok()
            .and_then(|mut state| state.switch(Some(stage)));
        StageGuard {
            progress: progress.clone(),
            previous,
        }
    }

    /// Current queued/running stage for a nonterminal reply; never implies a committed write.
    pub(super) fn stage(&self) -> &'static str {
        self.active.map_or("queued", |(stage, _)| stage)
    }

    /// Journals the first usable source/write result once, without waiting for background checks.
    /// The duration includes the admission queue; only counts and opaque correlation are recorded.
    pub(super) fn usable(&mut self, tool: AssistanceTool, reference: &str, request: Option<&str>) {
        if self.usable_reported || !matches!(tool, AssistanceTool::Read | AssistanceTool::Edit) {
            return;
        }
        self.usable_reported = true;
        let elapsed = self
            .requested_at
            .map(|start| start.elapsed())
            .unwrap_or_default();
        let detail = format!(
            "usable_result; request_to_usable_ms: {}; progress_replies: {}; recovery_requests: {}",
            elapsed.as_millis(),
            self.progress_replies,
            self.recovery_requests,
        );
        crate::errorlog::record(
            super::errorlog_method(tool),
            crate::errorlog::Outcome::Completed,
            crate::errorlog::Fields {
                correlation: Some(reference),
                request,
                detail: Some(&detail),
                duration_ms: Some(u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX)),
                ..Default::default()
            },
        );
    }

    /// Records first-useful latency only for delivered source or a confirmed write receipt.
    /// Terminal errors/refusals/progress envelopes never become usable results; stage timing is
    /// journaled independently. `source` means an actual retained read observation, not a handle.
    pub(super) fn usable_reply(
        &mut self,
        tool: AssistanceTool,
        reference: &str,
        request: Option<&str>,
        reply: &PeerReply,
        source: bool,
    ) {
        use crate::assistance::reply::EditBatchFileState;
        let useful = match reply {
            PeerReply::Complete {
                kind: ResultKind::Read,
                ..
            } if tool == AssistanceTool::Read => source,
            PeerReply::Edit { result, .. } if tool == AssistanceTool::Edit => {
                result.outcome.has_post_source() && !self.committed.is_empty()
            }
            PeerReply::EditBatch { files, .. } if tool == AssistanceTool::Edit => {
                !self.committed.is_empty()
                    && files.iter().any(|file| {
                        matches!(
                            file.state,
                            EditBatchFileState::Created
                                | EditBatchFileState::Replaced
                                | EditBatchFileState::Unchanged
                        )
                    })
            }
            _ => false,
        };
        if useful {
            self.usable(tool, reference, request);
        }
    }

    /// Records one progress response or explicit recovery request without logging requested paths.
    pub(super) fn recovery(&mut self, reference: &str, progress_reply: bool) {
        if progress_reply {
            self.progress_replies = self.progress_replies.saturating_add(1);
        } else {
            self.recovery_requests = self.recovery_requests.saturating_add(1);
        }
        let detail = format!(
            "progress_replies: {}; recovery_requests: {}; stage: {}",
            self.progress_replies,
            self.recovery_requests,
            self.stage()
        );
        crate::errorlog::record(
            crate::errorlog::Method::Daemon,
            crate::errorlog::Outcome::Incomplete,
            crate::errorlog::Fields {
                correlation: Some(reference),
                detail: Some(&detail),
                ..Default::default()
            },
        );
    }

    /// Stops timing and journals all applicable stage totals under the job's opaque reference.
    /// Long stages are warnings visible in `agent-ide errors`; no source or paths are logged.
    pub(super) fn journal(&mut self, tool: AssistanceTool, reference: &str, request: Option<&str>) {
        self.switch(None);
        let stages: &[&str] = match tool {
            AssistanceTool::Edit => &[
                "queue",
                "plan",
                "format",
                "gate",
                "commit",
                "check",
                "provider_wait",
            ],
            AssistanceTool::Read => &["queue", "plan", "provider_wait"],
            _ => return,
        };
        for stage in stages {
            let duration = self.durations.get(stage).copied().unwrap_or_default();
            let detail = format!(
                "stage: {stage}; duration_ms: {}; inline_wait_exceeded: {}",
                duration.as_millis(),
                duration > INLINE_REPLY_WAIT
            );
            crate::errorlog::record(
                super::errorlog_method(tool),
                if duration > INLINE_REPLY_WAIT {
                    crate::errorlog::Outcome::Incomplete
                } else {
                    crate::errorlog::Outcome::Completed
                },
                crate::errorlog::Fields {
                    correlation: Some(reference),
                    request,
                    detail: Some(&detail),
                    duration_ms: Some(u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)),
                    ..Default::default()
                },
            );
        }
    }
}

/// Owns a nested timing interval without borrowing the job across asynchronous work.
pub(super) struct StageGuard {
    /// Shared state retained by the job and its result slot.
    progress: Arc<Mutex<Progress>>,
    /// Parent stage restored on drop, including error and cancellation paths.
    previous: Option<&'static str>,
}

impl Drop for StageGuard {
    /// Accounts for this interval and resumes its parent; poisoned advisory state is ignored.
    fn drop(&mut self) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.switch(self.previous);
        }
    }
}

/// Stage-journal contract checks using captured events and a controlled monotonic interval.
#[cfg(test)]
mod tests {
    use super::*;

    /// Stage totals include the active interval, emit zeros for unused stages and expose slow
    /// work to the default warning reader without source paths or a new logging backend.
    #[test]
    fn slow_stages_are_journaled_as_warnings() {
        let mut progress = Progress {
            active: Some(("check", Instant::now() - Duration::from_secs(9))),
            requested: vec!["private/source.rs".into()],
            ..Default::default()
        };
        crate::errorlog::capture_start();
        progress.journal(
            AssistanceTool::Edit,
            "opaque-reference",
            Some("opaque-request"),
        );
        let mut events = crate::errorlog::capture_take();
        assert_eq!(events.len(), 7);
        assert!(events.iter().all(|event| event.correlation.as_deref()
            == Some("opaque-reference")
            && !event.detail.as_deref().unwrap().contains("private")));
        crate::errorlog::retain_warn_and_error(&mut events);
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("stage: check")
        );
        assert!(events[0].duration_ms.unwrap() >= 9000);
        assert!(progress.active.is_none());
    }
}
