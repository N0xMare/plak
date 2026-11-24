//! The runtime opts an exact, undispatched authority check into a bounded
//! wait. Adapters can wait at the failed acquisition, without running the
//! rest of revalidation a second time.
//! The thread-local scope exists only while polling the authority future:
//! it is restored between polls and is not inherited by spawned tasks.

use std::cell::Cell;
use std::future::{pending, poll_fn};
use std::task::Poll;
use std::time::{Duration, Instant};

use crate::{RuntimeModelAuthority, RuntimeModelAuthorityError, RuntimeModelAuthorityOperation};

#[derive(Clone, Copy)]
struct Acquisition {
    window: Duration,
    deadline: Option<Instant>,
    failure: Option<RuntimeModelAuthorityError>,
    backoff: Option<Backoff>,
}

thread_local! {
    static CURRENT: Cell<Option<Acquisition>> = const { Cell::new(None) };
}

struct Restore(Option<Acquisition>);

impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.set(self.0);
    }
}

/// The point where the caller can release its resources before retrying.
#[derive(Clone, Copy)]
pub struct Backoff {
    /// Delay until the next attempt, capped by the remaining window.
    pub delay: Duration,
    /// Total time left, including this backoff and reacquiring resources.
    pub remaining: Duration,
}

/// Continue after backing off; break with the completed check and whether
/// it entered backoff.
pub type Step = std::ops::ControlFlow<
    (
        Result<RuntimeModelAuthority, RuntimeModelAuthorityError>,
        bool,
    ),
    Backoff,
>;

/// One authority check, including any caller-owned backoff steps.
pub struct Check {
    operation: RuntimeModelAuthorityOperation,
    state: Acquisition,
}

impl Check {
    /// Grants this check a waiting allowance, without running it yet.
    #[must_use]
    pub const fn new(operation: RuntimeModelAuthorityOperation, window: Duration) -> Self {
        Self {
            operation,
            state: Acquisition {
                window,
                deadline: None,
                failure: None,
                backoff: None,
            },
        }
    }

    /// Drives the check until completion or until the caller must back off.
    pub async fn step(&mut self) -> Step {
        poll_fn(|cx| {
            let _restore = Restore(CURRENT.replace(Some(self.state)));
            let result = self.operation.as_mut().poll(cx);
            if let Some(state) = CURRENT.take() {
                self.state = state;
            }
            let waited = self.state.deadline.is_some();
            if let Some(failure) = self.state.failure {
                return Poll::Ready(Step::Break((Err(failure), waited)));
            }
            if let Some(backoff) = self.state.backoff.take() {
                return Poll::Ready(Step::Continue(backoff));
            }
            result.map(|result| Step::Break((result, waited)))
        })
        .await
    }
}

/// Yields the retry's delay to the caller before sending anything else.
pub async fn backoff(delay: Duration, remaining: Duration) {
    let mut yielded = false;
    poll_fn(|_| {
        if !yielded && let Some(mut state) = CURRENT.get() {
            state.backoff = Some(Backoff { delay, remaining });
            CURRENT.set(Some(state));
            yielded = true;
            return Poll::Pending;
        }
        Poll::Ready(())
    })
    .await;
}

/// Returns the current check's deadline, starting the wait on the first
/// proven transient failure. Returns none outside an opted-in check.
#[must_use]
pub fn wait_deadline(now: Instant) -> Option<Instant> {
    let mut state = CURRENT.get()?;
    let deadline = *state.deadline.get_or_insert_with(|| now + state.window);
    CURRENT.set(Some(state));
    Some(deadline)
}

/// Ends an opted-in check at the failing acquisition. Suspending here lets
/// the scope surface the typed failure before an adapter can downgrade it
/// to a credential park or perform a follow-up call. Unscoped callers keep
/// their existing error mapping.
pub async fn refuse(error: RuntimeModelAuthorityError) {
    if let Some(mut state) = CURRENT.get() {
        state.failure = Some(error);
        CURRENT.set(Some(state));
        pending::<()>().await;
    }
}
