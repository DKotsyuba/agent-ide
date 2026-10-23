//! Owns public preparation checks outside the sequential worker and gives main tests priority.
//!
//! A prepare takes a [`Ticket`] before its job is spawned. A main-test interrupt advances the
//! fence of its binding and cancels the registered check without waiting on any planning;
//! admission rechecks the ticket and registers the started check in one synchronous step under
//! the same lock, so a check is either cancelled by the interrupt or never started after it.
use super::preparation_evidence::{
    self as evidence, CheckStart, EvidenceConfig, PublicCheck, PublicCheckReport,
};
use super::*;

/// Preemption fence and live checks behind one plain lock, never held across an await.
#[derive(Default)]
pub(super) struct State(Mutex<Registry>);

/// Admission state shared by prepares and main-test interrupts.
#[derive(Default)]
struct Registry {
    /// Advanced by every all-bindings interrupt (stop of every binding, shutdown).
    all: u64,
    /// Advanced by every interrupt of one binding.
    bindings: BTreeMap<BindingRef, u64>,
    /// The started check of each binding until its prepare collects it.
    live: BTreeMap<BindingRef, Arc<PublicCheck>>,
}

/// The fence a prepare observed before spawning; admission requires it unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    all: u64,
    binding: u64,
}

impl Registry {
    /// The current fence of `binding`.
    fn ticket(&self, binding: &BindingRef) -> Ticket {
        Ticket {
            all: self.all,
            binding: self.bindings.get(binding).copied().unwrap_or(0),
        }
    }

    /// Advances the fence of `binding` (or of every binding) and takes its live checks.
    fn preempt(&mut self, binding: Option<&BindingRef>) -> Vec<Arc<PublicCheck>> {
        match binding {
            Some(binding) => {
                let generation = self.bindings.entry(binding.clone()).or_insert(0);
                *generation = generation.wrapping_add(1);
                self.live.remove(binding).into_iter().collect()
            }
            None => {
                self.all = self.all.wrapping_add(1);
                std::mem::take(&mut self.live).into_values().collect()
            }
        }
    }
}

impl State {
    /// Locks the registry; a poisoned lock keeps its data (it holds no partial invariant).
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Captures `binding`'s fence; call before the prepare job is spawned.
    pub(super) fn ticket(&self, binding: &BindingRef) -> Ticket {
        self.lock().ticket(binding)
    }

    /// Runs `admit` only while `ticket` is current and registers a started check under the
    /// same lock. `admit` must not await; its only blocking work is the synchronous spawn.
    fn admit_current(
        &self,
        binding: &BindingRef,
        ticket: Ticket,
        admit: impl FnOnce() -> CheckStart,
    ) -> Result<Arc<PublicCheck>, String> {
        let mut registry = self.lock();
        if registry.ticket(binding) != ticket {
            return Err("public check preempted by main test".into());
        }
        match admit() {
            CheckStart::Started(check) => {
                let check = Arc::new(check);
                registry.live.insert(binding.clone(), check.clone());
                Ok(check)
            }
            CheckStart::Busy { running } => Err(format!("public check busy: tests #{running}")),
            CheckStart::Unavailable(reason) => Err(reason.into()),
        }
    }

    /// Advances the fence and takes the affected live checks; never waits.
    fn preempt(&self, binding: Option<&BindingRef>) -> Vec<Arc<PublicCheck>> {
        self.lock().preempt(binding)
    }

    /// Unregisters `check` only if it is still the binding's registered one.
    fn finish(&self, binding: &BindingRef, check: &Arc<PublicCheck>) {
        let mut registry = self.lock();
        if registry
            .live
            .get(binding)
            .is_some_and(|live| Arc::ptr_eq(live, check))
        {
            registry.live.remove(binding);
        }
    }
}

/// Main-test or stop priority: fences `binding` (or every binding) so no later admission of an
/// earlier prepare starts, cancels only preparation-owned checks, and waits for their processes
/// to settle. Never waits on a prepare's planning.
pub(super) async fn interrupt(shared: &Shared, binding: Option<&BindingRef>) {
    for check in shared.preparation_checks.preempt(binding) {
        evidence::cancel_public_check(shared, &check);
        while evidence::poll_public_check(shared, &check).is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Plans outside any lock, admits only while `ticket` is current, then drains the check to its
/// settled report. Cancellation or the deadline cancels the own run once and still waits for it.
#[allow(
    clippy::too_many_arguments,
    reason = "explicit authority, fence, selection and cancellation contract"
)]
pub(super) async fn collect(
    shared: &Shared,
    authority: &AuthorityStamp,
    epoch: super::super::outbox::JobEpoch,
    ticket: Ticket,
    task: &str,
    config: &EvidenceConfig,
    target: &LaunchTarget,
    cancel: &tokio_util::sync::CancellationToken,
    deadline: tokio::time::Instant,
) -> std::io::Result<PublicCheckReport> {
    let planned = tokio::select! {
        () = cancel.cancelled() => return Err(std::io::Error::other("public check planning cancelled")),
        () = tokio::time::sleep_until(deadline) => return Err(std::io::Error::other("public check planning deadline")),
        planned = evidence::plan_public_check(shared, authority, epoch, task, config, target) =>
            planned.map_err(std::io::Error::other)?,
    };
    let binding = authority.binding();
    let check = shared
        .preparation_checks
        .admit_current(binding, ticket, || {
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                CheckStart::Unavailable("public check cancelled before admission")
            } else {
                evidence::admit_public_check(shared, planned)
            }
        })
        .map_err(std::io::Error::other)?;
    let mut cancel_sent = false;
    loop {
        if !cancel_sent && (cancel.is_cancelled() || tokio::time::Instant::now() >= deadline) {
            evidence::cancel_public_check(shared, &check);
            cancel_sent = true;
        }
        if let Some(report) = evidence::poll_public_check(shared, &check) {
            shared.preparation_checks.finish(binding, &check);
            return Ok(report);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn project() -> (crate::scratch::ScratchDir, PathBuf) {
        crate::lang::testing::install();
        let scratch = crate::scratch::ScratchDir::new("prepare-checks");
        let root = scratch.join("project");
        std::fs::create_dir_all(&root).unwrap();
        (scratch, root)
    }

    async fn settled(runs: &TestRuns, check: &PublicCheck) -> PublicCheckReport {
        for _ in 0..400 {
            if let Some(report) = evidence::fixture_settled(runs, check) {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("check {} did not settle", check.run_id);
    }

    /// An interrupt between a prepare's ticket and its admission refuses the start without
    /// running admission, so the main test never meets a later public check.
    #[test]
    fn interrupt_before_admission_refuses_start() {
        let state = State::default();
        let own = BindingRef::fixture("checks-actor", "checks-channel", 1);
        let ticket = state.ticket(&own);
        assert!(state.preempt(Some(&own)).is_empty());
        let admitted = AtomicBool::new(false);
        let refused = state.admit_current(&own, ticket, || {
            admitted.store(true, Ordering::SeqCst);
            CheckStart::Unavailable("must not run")
        });
        assert_eq!(
            refused.err().as_deref(),
            Some("public check preempted by main test")
        );
        assert!(!admitted.load(Ordering::SeqCst));
        let fresh = state.ticket(&own);
        assert_ne!(fresh, ticket);
        assert_eq!(
            state
                .admit_current(&own, fresh, || CheckStart::Unavailable("details_full"))
                .err()
                .as_deref(),
            Some("details_full")
        );
    }

    /// Fences are per binding; an all-bindings interrupt fences every outstanding ticket.
    #[test]
    fn fences_are_scoped_to_their_binding() {
        let state = State::default();
        let own = BindingRef::fixture("checks-a", "checks-channel", 1);
        let other = BindingRef::fixture("checks-b", "checks-channel", 1);
        let (own_ticket, other_ticket) = (state.ticket(&own), state.ticket(&other));
        state.preempt(Some(&own));
        assert_ne!(state.ticket(&own), own_ticket);
        assert_eq!(state.ticket(&other), other_ticket);
        state.preempt(None);
        assert_ne!(state.ticket(&other), other_ticket);
    }

    /// A check admitted before the interrupt is taken by it, cancelled by its exact origin and
    /// settled; the worktree is then free for the main test's ordinary admission.
    #[tokio::test]
    async fn interrupt_after_admission_cancels_registered_check() {
        let (_scratch, root) = project();
        let runs = TestRuns::default();
        let state = State::default();
        let own = BindingRef::fixture("checks-actor", "checks-channel", 1);
        let ticket = state.ticket(&own);
        let check = state
            .admit_current(&own, ticket, || {
                evidence::fixture_admit(&runs, &root, "sleep 10", &own, "task")
            })
            .unwrap();
        let taken = state.preempt(Some(&own));
        assert_eq!(taken.len(), 1);
        assert!(Arc::ptr_eq(&taken[0], &check));
        assert!(state.preempt(Some(&own)).is_empty());
        assert!(evidence::fixture_cancel(&runs, &taken[0]));
        let report = settled(&runs, &check).await;
        assert_eq!(report.outcome, evidence::CheckOutcome::Cancelled);
        let main = crate::checks::runner::RunSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: root.clone(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            read_denies: Vec::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        };
        assert!(matches!(
            runs.start_admitted(
                root.clone(),
                main,
                vec!["main".into()],
                &own,
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "main-detail".into(),
                None,
            ),
            StartResult::Started(_)
        ));
    }

    /// Collection unregisters only its own check, never a newer one of the same binding.
    #[tokio::test]
    async fn finish_keeps_a_newer_registration() {
        let (_scratch, root) = project();
        let runs = TestRuns::default();
        let state = State::default();
        let own = BindingRef::fixture("checks-actor", "checks-channel", 1);
        let first = state
            .admit_current(&own, state.ticket(&own), || {
                evidence::fixture_admit(&runs, &root, "exit 0", &own, "first")
            })
            .unwrap();
        settled(&runs, &first).await;
        let second = state
            .admit_current(&own, state.ticket(&own), || {
                evidence::fixture_admit(&runs, &root, "sleep 10", &own, "second")
            })
            .unwrap();
        state.finish(&own, &first);
        let taken = state.preempt(Some(&own));
        assert_eq!(taken.len(), 1);
        assert!(Arc::ptr_eq(&taken[0], &second));
        assert!(evidence::fixture_cancel(&runs, &second));
        settled(&runs, &second).await;
    }
}
