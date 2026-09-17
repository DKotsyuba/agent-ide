//! Long-lived client lease admission and idle-timeout daemon shutdown (EYES-r2 §2).
//!
//! Every managed MCP server that shares one repository's daemon holds one `ClientLease`
//! connection open for its whole lifetime. [`LeaseController`] counts those open connections,
//! independently of the transport's hook/assistance `max_connections` capacity, and drives the
//! daemon's [`LeaseController::idle_expired`] shutdown signal once no lease has been open, and no
//! daemon-owned work has been in flight, for the configured idle timeout.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// Bounded concurrent lease-connection admission, independent of the hook/assistance connection cap.
pub const LEASE_POOL_CAPACITY: usize = 32;

/// Idle-shutdown timeout used when no operator configuration overrides it (EYES-r2 §1 default).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared state behind every [`LeaseController`] clone and every outstanding [`LeaseGuard`].
struct Inner {
    /// Count of currently admitted, still-open lease connections.
    open: AtomicUsize,
    /// Duration the daemon must stay idle (zero leases, not busy) before it shuts down.
    idle_timeout: Duration,
    /// Reports whether daemon-owned work is in flight; always `false` until a future task supplies
    /// the real project-check signal (EYES-r2 §2).
    is_busy: Box<dyn Fn() -> bool + Send + Sync>,
    /// The instant the daemon most recently became idle (zero leases), or `None` while ineligible.
    became_idle_at: Mutex<Option<Instant>>,
    /// Signals every admission or release so [`LeaseController::idle_expired`] re-evaluates state.
    changed: Notify,
    /// Hooks run exactly once, in registration order, when this daemon shuts down for any reason.
    hooks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

/// Counts open lease connections and drives idle-timeout daemon shutdown.
///
/// Cheaply `Clone`d into the daemon's accept loop and every accepted connection task; every clone
/// shares one admission count, idle countdown, and shutdown hook list.
#[derive(Clone)]
pub struct LeaseController(Arc<Inner>);

impl LeaseController {
    /// Starts idle (zero open leases, countdown already running) from the moment of construction.
    ///
    /// `is_busy` reports whether daemon-owned work is in flight; a daemon with no such work supplies
    /// a fixed `|| false`, matching EYES-r2 §2 until a future task wires real check status through.
    pub fn new(idle_timeout: Duration, is_busy: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner {
            open: AtomicUsize::new(0),
            idle_timeout,
            is_busy: Box::new(is_busy),
            became_idle_at: Mutex::new(Some(Instant::now())),
            changed: Notify::new(),
            hooks: Mutex::new(Vec::new()),
        }))
    }

    /// Registers a hook run exactly once, in registration order, when this daemon shuts down for any
    /// reason (orderly idle expiry, SIGINT/SIGTERM, or a serving failure). Intended for a future
    /// check scheduler to cancel its own outstanding work (EYES-r2 §2).
    pub fn on_shutdown(&self, hook: impl FnOnce() + Send + 'static) {
        self.0.hooks.lock().unwrap().push(Box::new(hook));
    }

    /// Admits one open lease if the bounded pool has room, cancelling any pending idle countdown.
    ///
    /// Returns `None` when [`LEASE_POOL_CAPACITY`] is already reached; the caller must drop the
    /// connection exactly like any other admission refusal, never consuming the separate
    /// hook/assistance `max_connections` permits.
    pub fn try_admit(&self) -> Option<LeaseGuard> {
        let mut open = self.0.open.load(Ordering::SeqCst);
        loop {
            if open >= LEASE_POOL_CAPACITY {
                return None;
            }
            match self.0.open.compare_exchange_weak(
                open,
                open + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(observed) => open = observed,
            }
        }
        *self.0.became_idle_at.lock().unwrap() = None;
        self.0.changed.notify_waiters();
        Some(LeaseGuard(Arc::clone(&self.0)))
    }

    /// Resolves once the lease count has been zero, and the daemon has reported itself not busy, for
    /// one continuous idle timeout; never resolves while any lease is open or the daemon is busy.
    pub async fn idle_expired(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            match self.current_deadline() {
                Some(deadline) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {
                            if self.current_deadline().is_some() {
                                return;
                            }
                        }
                        () = &mut changed => {}
                    }
                }
                None => changed.await,
            }
        }
    }

    /// Runs every registered shutdown hook exactly once, in registration order, then clears them.
    pub async fn run_shutdown_hooks(&self) {
        let hooks = std::mem::take(&mut *self.0.hooks.lock().unwrap());
        for hook in hooks {
            hook();
        }
    }

    /// Returns the instant the idle countdown completes, or `None` while zero-lease-and-idle does
    /// not currently hold.
    fn current_deadline(&self) -> Option<Instant> {
        if self.0.open.load(Ordering::SeqCst) != 0 || (self.0.is_busy)() {
            return None;
        }
        self.0
            .became_idle_at
            .lock()
            .unwrap()
            .map(|since| since + self.0.idle_timeout)
    }
}

/// Releases one admitted lease connection on drop, arming the idle countdown when none remain.
pub struct LeaseGuard(Arc<Inner>);

impl Drop for LeaseGuard {
    /// Decrements the open lease count and, if this was the last open lease, starts the idle clock.
    fn drop(&mut self) {
        if self.0.open.fetch_sub(1, Ordering::SeqCst) == 1 {
            *self.0.became_idle_at.lock().unwrap() = Some(Instant::now());
        }
        self.0.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh controller with zero leases is immediately idle-eligible from its own construction.
    #[tokio::test(start_paused = true)]
    async fn fresh_controller_expires_after_its_configured_timeout() {
        let lease = LeaseController::new(Duration::from_millis(50), || false);
        tokio::time::timeout(Duration::from_millis(200), lease.idle_expired())
            .await
            .expect("idle controller must expire once its timeout elapses");
    }

    /// An open lease suppresses idle expiry indefinitely; releasing it restarts the countdown.
    #[tokio::test(start_paused = true)]
    async fn open_lease_suppresses_expiry_until_released() {
        let lease = LeaseController::new(Duration::from_millis(50), || false);
        let guard = lease.try_admit().expect("pool has room");
        assert!(
            tokio::time::timeout(Duration::from_millis(500), lease.idle_expired())
                .await
                .is_err(),
            "an open lease must never let idle expiry resolve"
        );
        drop(guard);
        tokio::time::timeout(Duration::from_millis(200), lease.idle_expired())
            .await
            .expect("idle countdown must restart once the last lease is released");
    }

    /// A lease opened during the countdown cancels the pending expiry.
    #[tokio::test(start_paused = true)]
    async fn a_new_lease_cancels_a_pending_countdown() {
        let lease = LeaseController::new(Duration::from_millis(100), || false);
        let expiry = tokio::spawn({
            let lease = lease.clone();
            async move { lease.idle_expired().await }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        let guard = lease.try_admit().expect("pool has room");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), expiry)
                .await
                .is_err(),
            "admitting a lease before expiry must cancel the countdown"
        );
        drop(guard);
    }

    /// Admission beyond the bounded pool capacity is refused without panicking or corrupting state.
    #[tokio::test]
    async fn admission_is_bounded_by_the_lease_pool_capacity() {
        let lease = LeaseController::new(Duration::from_secs(300), || false);
        let mut guards = Vec::new();
        for _ in 0..LEASE_POOL_CAPACITY {
            guards.push(lease.try_admit().expect("pool has room"));
        }
        assert!(
            lease.try_admit().is_none(),
            "admission beyond the bounded pool must be refused"
        );
        drop(guards.pop());
        assert!(
            lease.try_admit().is_some(),
            "releasing one lease must free exactly one admission slot"
        );
    }

    /// Every registered shutdown hook runs exactly once, in registration order.
    #[tokio::test]
    async fn shutdown_hooks_run_once_in_registration_order() {
        let lease = LeaseController::new(Duration::from_secs(300), || false);
        let order = Arc::new(Mutex::new(Vec::new()));
        for id in 0..3 {
            let order = Arc::clone(&order);
            lease.on_shutdown(move || order.lock().unwrap().push(id));
        }
        lease.run_shutdown_hooks().await;
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
        lease.run_shutdown_hooks().await;
        assert_eq!(
            *order.lock().unwrap(),
            vec![0, 1, 2],
            "a second run must not repeat already-run hooks"
        );
    }
}
