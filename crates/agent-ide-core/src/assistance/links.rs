//! Worker glue for the cross-language name index (the language bridge).
//!
//! The index work is blocking (listing, `lstat` sweeps, authorized reads), so it runs on the
//! blocking pool like the project card. A query that finds the index still building parks its
//! job and retries through the ordinary provider-loading path, so a build that finishes within the
//! inline reply wait is invisible to the agent.

use super::*;
use crate::intelligence::names::{IndexState, NameIndex};
use crate::workspace::authority::WorktreeRef;

/// Longest one query sweeps before it parks its job.
const NAMES_QUERY_WAIT: Duration = Duration::from_secs(1);
/// Delay before a parked query retries a building index.
const NAMES_PARK: Duration = Duration::from_millis(300);

// ponytail: no tool calls these yet; the bridge card integration (stage 3) is the first caller.
#[allow(dead_code)]
impl Worker<'_> {
    /// Refreshes `worktree`'s name index on the blocking pool for at most [`NAMES_QUERY_WAIT`]
    /// and returns it with its state.
    ///
    /// # Errors
    ///
    /// [`FailureCode::ProviderLoading`] (detail `names:building`) while the index is still
    /// building; the job is parked for [`NAMES_PARK`] when its deadline leaves room to retry.
    /// [`FailureCode::Internal`] when the blocking task or the index lock fails.
    pub(super) async fn name_index(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
    ) -> Result<(Arc<Mutex<NameIndex>>, IndexState), FailureCode> {
        let index = self.names.for_worktree(worktree);
        let deadline = std::time::Instant::now() + NAMES_QUERY_WAIT;
        let state = with_names(index.clone(), move |index| index.refresh(deadline)).await?;
        if state == IndexState::Building {
            let now = tokio::time::Instant::now();
            if job.deadline.saturating_duration_since(now) > NAMES_QUERY_WAIT {
                job.park_until = Some(now + NAMES_PARK);
            }
            job.failure_detail = Some("names:building".to_owned());
            return Err(FailureCode::ProviderLoading);
        }
        Ok((index, state))
    }
}

/// Runs `query` against the locked index on the blocking pool (sites, `verify`, `uncovered`).
///
/// # Errors
///
/// [`FailureCode::Internal`] when the blocking task panics or the index lock is poisoned.
#[allow(dead_code)]
pub(super) async fn with_names<T: Send + 'static>(
    index: Arc<Mutex<NameIndex>>,
    query: impl FnOnce(&mut NameIndex) -> T + Send + 'static,
) -> Result<T, FailureCode> {
    tokio::task::spawn_blocking(move || {
        index
            .lock()
            .map(|mut index| query(&mut index))
            .map_err(|_| FailureCode::Internal)
    })
    .await
    .map_err(|_| FailureCode::Internal)?
}
