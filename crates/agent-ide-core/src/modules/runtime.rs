//! Supervision of one module instance slot `(module, scope, role)`: lazy start, `hello`, typed
//! failures, the restart budget, bounded stderr capture and orderly stop.
//!
//! A [`Supervisor`] never spawns by itself: its [`Launcher`] admits and starts the process
//! through Execution and later reaps it, so process ownership, admission accounting and group
//! cleanup stay in one place. Every fault settles the failing call with a typed
//! [`ModuleUnavailable`]; nothing is silently retried. The next demand may start a fresh instance
//! only as the [`RestartBudget`] permits.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    time::Instant,
};

use super::{
    contract::{Cause, HelloOffer, ModuleUnavailable, Stage},
    host::{Call, EffectRunner, HostChannel, Reply},
};

/// Automatic restarts permitted inside one [`RESTART_WINDOW`], with the delay before each.
pub const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(4),
];
/// The rolling window of the restart budget.
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Retained stderr bytes per instance (the most recent ones).
pub const STDERR_CAPTURE: usize = 64 * 1024;

/// What the restart budget permits for the next start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Permit {
    /// Start now.
    Now,
    /// Start no earlier than this instant (backoff).
    After(Instant),
    /// Exhausted until this instant.
    Exhausted(Instant),
}

/// Crash history of one slot: the initial start plus at most three automatic restarts in a
/// rolling 60 s, delayed 250 ms, 1 s and 4 s. A successful `hello` does not reset it; failures
/// only age out of the window.
#[derive(Clone, Debug, Default)]
pub struct RestartBudget {
    /// Failure instants inside the window, oldest first.
    failures: VecDeque<Instant>,
}

impl RestartBudget {
    /// Records one instance failure at `now`.
    pub fn record(&mut self, now: Instant) {
        self.failures.push_back(now);
    }

    /// What may happen at `now`.
    pub fn permit(&mut self, now: Instant) -> Permit {
        while self
            .failures
            .front()
            .is_some_and(|failed| now.saturating_duration_since(*failed) >= RESTART_WINDOW)
        {
            self.failures.pop_front();
        }
        let (Some(oldest), Some(latest)) = (self.failures.front(), self.failures.back()) else {
            return Permit::Now;
        };
        match RESTART_DELAYS.get(self.failures.len() - 1) {
            None => Permit::Exhausted(*oldest + RESTART_WINDOW),
            Some(delay) if *latest + *delay <= now => Permit::Now,
            Some(delay) => Permit::After(*latest + *delay),
        }
    }
}

/// The most recent [`STDERR_CAPTURE`] bytes of one instance's stderr, with the total count.
/// Raw bytes stay private to the runtime; only counts and sanitized causes leave it.
#[derive(Debug, Default)]
pub struct StderrTail {
    /// Retained tail.
    bytes: VecDeque<u8>,
    /// Every byte the instance wrote.
    total: u64,
}

impl StderrTail {
    /// Appends `chunk`, dropping the oldest bytes beyond the capture.
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(STDERR_CAPTURE);
        self.bytes.drain(..excess);
    }

    /// Bytes written in total.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Whether older bytes were dropped.
    pub fn truncated(&self) -> bool {
        self.total > self.bytes.len() as u64
    }

    /// The retained tail, for private diagnostics only.
    pub fn tail(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

/// The streams and handle of one started module process.
pub struct Spawned<P> {
    /// The module's stdout (protocol only).
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    /// The module's stdin.
    pub stdin: Box<dyn AsyncWrite + Send + Sync + Unpin>,
    /// The module's stderr, drained into the instance's [`StderrTail`].
    pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// The handle the launcher reaps.
    pub process: P,
}

/// Admits, starts and reaps module processes for one slot.
pub trait Launcher: Send {
    /// The launcher's handle of one started process.
    type Process: Send;

    /// Admits and starts instance `instance`. A refusal names its stage (`admission` waits do not
    /// count as crashes) and cause.
    fn launch(
        &mut self,
        instance: u64,
    ) -> impl Future<Output = Result<Spawned<Self::Process>, (Stage, Cause)>> + Send;

    /// Stops and reaps `process` (group TERM, grace, group KILL, direct wait); `Err` when the
    /// cleanup cannot be proven.
    fn reap(&mut self, process: Self::Process) -> impl Future<Output = Result<(), Cause>> + Send;

    /// Fingerprint of the accepted inputs (executable digest, configuration, environment). A
    /// deterministic refusal (`incompatible`, `tool_missing`, `policy_refused`) blocks the slot
    /// until it changes.
    fn inputs(&self) -> String;
}

/// One live instance.
struct Live<P> {
    /// Its channel.
    channel: HostChannel,
    /// Its process handle.
    process: P,
    /// Its captured stderr.
    stderr: Arc<Mutex<StderrTail>>,
    /// The linkage coverage its `hello` declared.
    linkage: Vec<crate::modules::payload::LinkageCoverage>,
}

/// Supervises one `(module, scope, role)` slot.
pub struct Supervisor<L: Launcher> {
    /// Starts and reaps processes.
    launcher: L,
    /// The offer every instance receives (its `instance` replaced).
    offer: HelloOffer,
    /// Spawn-to-`hello` ceiling.
    startup_budget: Duration,
    /// The live instance.
    live: Option<Live<L::Process>>,
    /// A launched process whose `hello` has not completed: kept here across the await so a
    /// cancelled start still reaps it (and releases its admission) on the next demand or stop.
    starting: Option<L::Process>,
    /// Crash history.
    budget: RestartBudget,
    /// A deterministic refusal and the inputs it was observed with.
    blocked: Option<(String, Stage, Cause)>,
    /// Last instance number used.
    instance: u64,
    /// Stderr of the latest instance, kept after it is gone.
    stderr: Arc<Mutex<StderrTail>>,
    /// Why an instance's cleanup could not be proven: its admission stays held and no
    /// replacement starts; every later demand is refused `drain` with this cause.
    unreaped: Option<Cause>,
}

/// Whether `cause` repeats until the accepted inputs change.
pub(crate) fn deterministic(cause: Cause) -> bool {
    matches!(
        cause,
        Cause::Incompatible | Cause::ToolMissing | Cause::PolicyRefused
    )
}

impl<L: Launcher> Supervisor<L> {
    /// A stopped slot that starts `offer`'s module with `launcher` on first demand.
    pub fn new(launcher: L, offer: HelloOffer, startup_budget: Duration) -> Self {
        Self {
            launcher,
            offer,
            startup_budget,
            live: None,
            starting: None,
            budget: RestartBudget::default(),
            blocked: None,
            instance: 0,
            stderr: Arc::default(),
            unreaped: None,
        }
    }

    /// The typed failure of this slot at `stage` with `cause`.
    fn unavailable(&self, stage: Stage, cause: Cause, retry: Option<Instant>) -> ModuleUnavailable {
        ModuleUnavailable {
            module_id: self.offer.module_id.clone(),
            module_version: self.offer.package_version.clone(),
            role: self.offer.role,
            stage,
            cause,
            instance: (self.instance > 0).then_some(self.instance),
            retry_after_ms: retry
                .map(|at| at.saturating_duration_since(Instant::now()).as_millis() as u64),
        }
    }

    /// Whether an instance is live (started and not yet known failed).
    pub fn is_live(&mut self) -> bool {
        self.live
            .as_mut()
            .is_some_and(|live| live.channel.idle_fault().is_none())
    }

    /// The linkage coverage the live instance declared in its `hello`.
    pub fn linkage(&self) -> Option<&[crate::modules::payload::LinkageCoverage]> {
        self.live.as_ref().map(|live| live.linkage.as_slice())
    }

    /// Stderr of the latest instance (private diagnostics).
    pub fn stderr(&self) -> Arc<Mutex<StderrTail>> {
        self.stderr.clone()
    }

    /// Sends `call` within `budget`, starting an instance first when none is live and the restart
    /// budget permits it (waiting out a backoff that fits the budget). A failure retires the
    /// instance and is returned as is; the call is never retried.
    pub async fn call(
        &mut self,
        call: Call,
        budget: Duration,
        effects: &mut dyn EffectRunner,
    ) -> Result<Reply, ModuleUnavailable> {
        let deadline = Instant::now() + budget;
        self.ready(deadline).await?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(live) = self.live.as_mut() else {
            return Err(self.unavailable(Stage::Request, Cause::Exited, None));
        };
        match live.channel.call(call, remaining, effects).await {
            Ok(reply) => Ok(reply),
            Err(failure) => {
                self.retire(true).await?;
                Err(failure)
            }
        }
    }

    /// Retires the live instance after a reply the core cannot use, counting it as a crash.
    pub async fn retire_failed(&mut self) -> Result<(), ModuleUnavailable> {
        self.retire(true).await
    }

    /// Reaps `process`; a cleanup that cannot be proven marks the slot unreaped and is returned
    /// as its `drain` failure.
    async fn reap(&mut self, process: L::Process) -> Result<(), ModuleUnavailable> {
        match self.launcher.reap(process).await {
            Ok(()) => Ok(()),
            Err(cause) => {
                self.unreaped = Some(cause);
                Err(self.unavailable(Stage::Drain, cause, None))
            }
        }
    }

    /// Makes sure a live instance exists before `deadline`.
    async fn ready(&mut self, deadline: Instant) -> Result<(), ModuleUnavailable> {
        if let Some(process) = self.starting.take() {
            self.reap(process).await?;
        }
        if let Some(cause) = self.unreaped {
            return Err(self.unavailable(Stage::Drain, cause, None));
        }
        if let Some(live) = self.live.as_mut() {
            if live.channel.idle_fault().is_none() {
                return Ok(());
            }
            self.retire(true).await?;
        }
        if let Some((inputs, stage, cause)) = &self.blocked {
            if *inputs == self.launcher.inputs() {
                return Err(self.unavailable(*stage, *cause, None));
            }
            self.blocked = None;
        }
        match self.budget.permit(Instant::now()) {
            Permit::Now => {}
            Permit::After(at) if at <= deadline => tokio::time::sleep_until(at).await,
            Permit::After(at) | Permit::Exhausted(at) => {
                return Err(self.unavailable(Stage::Spawn, Cause::RestartExhausted, Some(at)));
            }
        }
        self.instance += 1;
        let start_by = deadline.min(Instant::now() + self.startup_budget);
        let launched = tokio::time::timeout_at(start_by, self.launcher.launch(self.instance)).await;
        let spawned = match launched {
            Ok(Ok(spawned)) => spawned,
            Ok(Err((stage, cause))) => {
                if deterministic(cause) {
                    self.blocked = Some((self.launcher.inputs(), stage, cause));
                } else if stage != Stage::Admission {
                    self.budget.record(Instant::now());
                }
                return Err(self.unavailable(stage, cause, None));
            }
            // A queued admission past its deadline is not a crash.
            Err(_) => return Err(self.unavailable(Stage::Admission, Cause::Timeout, None)),
        };
        let stderr = Arc::new(Mutex::new(StderrTail::default()));
        self.stderr = stderr.clone();
        if let Some(mut stream) = spawned.stderr {
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stream.read(&mut buffer).await
                    && read > 0
                {
                    stderr
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(&buffer[..read]);
                }
            });
        }
        let mut offer = self.offer.clone();
        offer.instance = self.instance;
        let hello_budget = start_by.saturating_duration_since(Instant::now());
        self.starting = Some(spawned.process);
        let opened = HostChannel::open(spawned.stdout, spawned.stdin, offer, hello_budget).await;
        let process = self.starting.take().expect("kept across the hello");
        match opened {
            Ok((channel, reply)) => {
                self.live = Some(Live {
                    channel,
                    process,
                    stderr: self.stderr.clone(),
                    linkage: reply.linkage_kinds,
                });
                Ok(())
            }
            Err(failure) => {
                self.reap(process).await?;
                if deterministic(failure.cause) {
                    self.blocked = Some((self.launcher.inputs(), failure.stage, failure.cause));
                } else {
                    self.budget.record(Instant::now());
                }
                Err(failure)
            }
        }
    }

    /// Takes the live instance out, reaps it and, for a failure, records a crash; a cleanup
    /// that cannot be proven is the `drain` failure.
    async fn retire(&mut self, failed: bool) -> Result<(), ModuleUnavailable> {
        if let Some(live) = self.live.take() {
            drop(live.stderr);
            if failed {
                self.budget.record(Instant::now());
            }
            self.reap(live.process).await?;
        }
        Ok(())
    }

    /// Stops the live instance in order: `shutdown`, then the launcher's reap. Not a crash. A
    /// cleanup that cannot be proven, now or earlier, is returned (and every later stop and
    /// demand keeps refusing with it).
    pub async fn stop(&mut self) -> Result<(), Cause> {
        if let Some(process) = self.starting.take() {
            self.reap(process).await.map_err(|failure| failure.cause)?;
        }
        if let Some(mut live) = self.live.take() {
            live.channel.shutdown().await;
            self.reap(live.process)
                .await
                .map_err(|failure| failure.cause)?;
        }
        self.unreaped.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{
        contract::{Capability, ModuleId, Outcome, Role},
        fake::{FakeModule, Fault, offer, sample_call},
        host::NoEffects,
        serve::serve,
    };

    /// One scripted launch: `Ok` starts a fake with an optional fault, `Err` refuses.
    type Launch = Result<Option<(Capability, Fault)>, (Stage, Cause)>;

    /// Launches in-memory [`FakeModule`]s; each launch takes the next scripted outcome.
    struct FakeLauncher {
        /// Per launch: `Ok(fault)` starts a fake (with an optional fault), `Err` refuses.
        script: VecDeque<Launch>,
        /// Package version the fake reports.
        version: &'static str,
        /// Accepted-input fingerprint.
        inputs: String,
        /// Launches attempted.
        launches: u32,
        /// Reaps performed.
        reaps: u32,
        /// Started modules never answer `hello`.
        silent: bool,
        /// Every reap fails to prove its cleanup.
        unprovable: bool,
    }

    impl FakeLauncher {
        /// A launcher following `script`, then healthy fakes.
        fn new(script: Vec<Launch>) -> Self {
            Self {
                script: script.into(),
                version: "1.0",
                inputs: "inputs-1".into(),
                launches: 0,
                reaps: 0,
                silent: false,
                unprovable: false,
            }
        }
    }

    impl Launcher for FakeLauncher {
        type Process = tokio::task::JoinHandle<()>;

        /// Starts the next scripted fake on in-memory streams.
        async fn launch(
            &mut self,
            _instance: u64,
        ) -> Result<Spawned<Self::Process>, (Stage, Cause)> {
            self.launches += 1;
            let fault = self.script.pop_front().unwrap_or(Ok(None))?;
            let (core_out, module_in) = tokio::io::duplex(1 << 16);
            let (module_out, core_in) = tokio::io::duplex(1 << 16);
            let (stderr_out, stderr_in) = tokio::io::duplex(1 << 16);
            let mut module = FakeModule::new(ModuleId::bundled("alpha"), self.version);
            if let Some((capability, fault)) = fault {
                module = module.with_fault(capability, fault);
            }
            let silent = self.silent;
            let process = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                if silent {
                    let _keep = (&module_in, &module_out);
                    std::future::pending::<()>().await;
                }
                let mut stderr_out = stderr_out;
                let _ = stderr_out.write_all(&vec![b'e'; 100 * 1024]).await;
                let _ = serve(module, Role::Analyzer, module_in, module_out).await;
            });
            Ok(Spawned {
                stdout: Box::new(core_in),
                stdin: Box::new(core_out),
                stderr: Some(Box::new(stderr_in)),
                process,
            })
        }

        /// Aborts the fake's task.
        async fn reap(&mut self, process: Self::Process) -> Result<(), Cause> {
            self.reaps += 1;
            process.abort();
            if self.unprovable {
                return Err(Cause::ReapUnverified);
            }
            Ok(())
        }

        /// The configured fingerprint.
        fn inputs(&self) -> String {
            self.inputs.clone()
        }
    }

    /// A supervisor for `bundled.alpha` over `launcher`.
    fn slot(launcher: FakeLauncher) -> Supervisor<FakeLauncher> {
        crate::lang::testing::install();
        Supervisor::new(
            launcher,
            offer(ModuleId::bundled("alpha"), "1.0", Role::Analyzer, 0),
            Duration::from_secs(30),
        )
    }

    /// One call of `capability` with a 20 s budget.
    async fn call(
        supervisor: &mut Supervisor<FakeLauncher>,
        capability: Capability,
    ) -> Result<Reply, ModuleUnavailable> {
        supervisor
            .call(
                sample_call(capability),
                Duration::from_secs(20),
                &mut NoEffects,
            )
            .await
    }

    /// A start cancelled during `hello` keeps its process for the next demand or stop, which reaps
    /// it (releasing its admission) without counting a crash.
    #[tokio::test]
    async fn a_start_cancelled_during_hello_is_reaped_by_the_stop() {
        let mut launcher = FakeLauncher::new(vec![Ok(None)]);
        launcher.silent = true;
        let mut supervisor = slot(launcher);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                call(&mut supervisor, Capability::FileDoc)
            )
            .await
            .is_err(),
            "the caller gave up during hello"
        );
        assert!(supervisor.starting.is_some());
        assert_eq!(supervisor.stop().await, Ok(()));
        assert_eq!(supervisor.launcher.reaps, 1);
        assert!(supervisor.starting.is_none());
        assert_eq!(
            supervisor.budget.permit(Instant::now()),
            Permit::Now,
            "not a crash"
        );
    }

    /// The budget permits the initial start and three delayed restarts, then reports exhaustion
    /// until the oldest failure leaves the window.
    #[tokio::test(start_paused = true)]
    async fn restart_budget_follows_the_window() {
        let mut budget = RestartBudget::default();
        let start = Instant::now();
        assert_eq!(budget.permit(start), Permit::Now);
        budget.record(start);
        assert_eq!(
            budget.permit(start),
            Permit::After(start + Duration::from_millis(250))
        );
        budget.record(start + Duration::from_millis(250));
        budget.record(start + Duration::from_millis(1250));
        assert_eq!(
            budget.permit(start + Duration::from_millis(1250)),
            Permit::After(start + Duration::from_millis(5250))
        );
        budget.record(start + Duration::from_millis(5250));
        assert_eq!(
            budget.permit(start + Duration::from_secs(30)),
            Permit::Exhausted(start + RESTART_WINDOW)
        );
        assert_eq!(budget.permit(start + RESTART_WINDOW), Permit::Now);
    }

    /// A crash mid-request settles that call with `exited`; the next demand restarts after the
    /// backoff and answers. An idle exit is noticed before the next call, which restarts.
    #[tokio::test(start_paused = true)]
    async fn crashes_settle_the_call_and_restart_on_demand() {
        let mut supervisor = slot(FakeLauncher::new(vec![Ok(Some((
            Capability::Outline,
            Fault::Exit,
        )))]));
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!((error.stage, error.cause), (Stage::Request, Cause::Exited));
        let started = Instant::now();
        let reply = call(&mut supervisor, Capability::FileDoc).await.unwrap();
        assert!(matches!(reply.outcome, Outcome::Result(_)));
        assert!(
            Instant::now() >= started + RESTART_DELAYS[0],
            "backoff honoured"
        );
        assert_eq!(supervisor.instance, 2);
        // Kill while idle: abort the module task; the next call notices and restarts.
        let live = supervisor.live.take().unwrap();
        live.process.abort();
        supervisor.live = Some(live);
        tokio::task::yield_now().await;
        let reply = call(&mut supervisor, Capability::FileDoc).await.unwrap();
        assert!(matches!(reply.outcome, Outcome::Result(_)));
        assert_eq!(supervisor.instance, 3);
        assert_eq!(
            supervisor.launcher.reaps, 2,
            "every failed instance was reaped"
        );
    }

    /// A failed instance whose cleanup cannot be proven settles the call `drain`
    /// `reap_unverified`, and no replacement starts: every later demand is refused the same way.
    #[tokio::test(start_paused = true)]
    async fn an_unproven_reap_blocks_replacement() {
        let mut launcher = FakeLauncher::new(vec![Ok(Some((Capability::Outline, Fault::Exit)))]);
        launcher.unprovable = true;
        let mut supervisor = slot(launcher);
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Drain, Cause::ReapUnverified)
        );
        tokio::time::sleep(RESTART_DELAYS[0] * 2).await;
        let error = call(&mut supervisor, Capability::FileDoc)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Drain, Cause::ReapUnverified)
        );
        assert_eq!(supervisor.launcher.launches, 1, "no replacement started");
        assert_eq!(supervisor.stop().await, Err(Cause::ReapUnverified));

        // A stop whose own reap cannot be proven keeps the slot unreaped as well.
        let mut supervisor = slot(FakeLauncher::new(Vec::new()));
        call(&mut supervisor, Capability::FileDoc).await.unwrap();
        supervisor.launcher.unprovable = true;
        assert_eq!(supervisor.stop().await, Err(Cause::ReapUnverified));
        assert_eq!(supervisor.stop().await, Err(Cause::ReapUnverified));
        let error = call(&mut supervisor, Capability::FileDoc)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Drain, Cause::ReapUnverified)
        );
        assert_eq!(
            supervisor.launcher.launches, 1,
            "no replacement after the stop"
        );
    }

    /// A reply the core cannot use retires the live instance as a counted crash; the next demand
    /// starts a fresh one after the backoff.
    #[tokio::test(start_paused = true)]
    async fn an_unusable_reply_retires_the_instance() {
        let mut supervisor = slot(FakeLauncher::new(Vec::new()));
        call(&mut supervisor, Capability::FileDoc).await.unwrap();
        supervisor.retire_failed().await.unwrap();
        assert!(!supervisor.is_live());
        assert_eq!(supervisor.launcher.reaps, 1);
        let started = Instant::now();
        call(&mut supervisor, Capability::FileDoc).await.unwrap();
        assert!(
            Instant::now() >= started + RESTART_DELAYS[0],
            "counted as a crash"
        );
        assert_eq!(supervisor.instance, 2);
    }

    /// A crash loop exhausts the budget (`restart_exhausted` with a retry time, no further
    /// launch) and recovers once the window passes.
    #[tokio::test(start_paused = true)]
    async fn a_crash_loop_exhausts_and_recovers() {
        let exit = Ok(Some((Capability::Outline, Fault::Exit)));
        let mut supervisor = slot(FakeLauncher::new(vec![exit, exit, exit, exit]));
        for _ in 0..4 {
            let error = call(&mut supervisor, Capability::Outline)
                .await
                .unwrap_err();
            assert_eq!(error.cause, Cause::Exited);
        }
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Spawn, Cause::RestartExhausted)
        );
        assert!(error.retry_after_ms.is_some_and(|ms| ms > 0));
        assert_eq!(supervisor.launcher.launches, 4, "no launch while exhausted");
        tokio::time::advance(RESTART_WINDOW).await;
        assert!(call(&mut supervisor, Capability::Outline).await.is_ok());
    }

    /// A deterministic `hello` refusal blocks the slot until the inputs change; a queued
    /// admission timeout is not a crash; a stall times out and the slot recovers.
    #[tokio::test(start_paused = true)]
    async fn refusals_admission_and_stalls() {
        let mut launcher = FakeLauncher::new(vec![]);
        launcher.version = "0.9";
        let mut supervisor = slot(launcher);
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Hello, Cause::Incompatible)
        );
        let again = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!(again.cause, Cause::Incompatible);
        assert_eq!(
            supervisor.launcher.launches, 1,
            "blocked until inputs change"
        );
        supervisor.launcher.version = "1.0";
        supervisor.launcher.inputs = "inputs-2".into();
        assert!(call(&mut supervisor, Capability::Outline).await.is_ok());

        let mut supervisor = slot(FakeLauncher::new(vec![
            Err((Stage::Admission, Cause::Timeout)),
            Ok(Some((Capability::Outline, Fault::Stall))),
        ]));
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Admission, Cause::Timeout)
        );
        assert_eq!(
            supervisor.budget.permit(Instant::now()),
            Permit::Now,
            "not a crash"
        );
        let error = call(&mut supervisor, Capability::Outline)
            .await
            .unwrap_err();
        assert_eq!((error.stage, error.cause), (Stage::Request, Cause::Timeout));
        assert!(call(&mut supervisor, Capability::FileDoc).await.is_ok());
    }

    /// Stderr is drained continuously and only its most recent 64 KiB are retained; an orderly
    /// stop is not counted as a crash.
    #[tokio::test(start_paused = true)]
    async fn stderr_is_bounded_and_stop_is_orderly() {
        let mut supervisor = slot(FakeLauncher::new(vec![]));
        call(&mut supervisor, Capability::FileDoc).await.unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        {
            let stderr = supervisor.stderr();
            let tail = stderr.lock().unwrap();
            assert_eq!(tail.total(), 100 * 1024);
            assert!(tail.truncated());
            assert_eq!(tail.tail().len(), STDERR_CAPTURE);
        }
        supervisor.stop().await.unwrap();
        assert!(!supervisor.is_live());
        assert_eq!(supervisor.budget.permit(Instant::now()), Permit::Now);
    }
}
