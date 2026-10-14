use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::Notify;

/// A clonable, race-free cancellation signal for one or more exchanges.
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

#[derive(Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    /// Creates a token in the non-cancelled state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels this token and wakes all current waiters.
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Cancellation and an absolute deadline for one physical exchange.
///
/// A [`Self::new`] or [`Self::with_cancellation`] deadline bounds the whole
/// exchange, body included. A [`Self::streaming`] deadline bounds the exchange
/// only up to its response head; after that each body pull is bounded by the
/// client's `response_body_timeout` alone, which restarts on every received
/// frame, so a stream that keeps making progress has no absolute cap.
#[derive(Clone, Debug)]
pub struct ExchangeControl {
    cancellation: CancellationToken,
    deadline: Instant,
    body_deadline: Option<Instant>,
    fresh_connection: bool,
}

impl ExchangeControl {
    /// Creates control state with a new cancellation token.
    #[must_use]
    pub fn new(deadline: Instant) -> Self {
        Self::with_cancellation(deadline, CancellationToken::new())
    }

    /// Creates control state using a caller-owned cancellation token.
    #[must_use]
    pub const fn with_cancellation(deadline: Instant, cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline,
            body_deadline: Some(deadline),
            fresh_connection: false,
        }
    }

    /// Creates streaming control: `deadline` bounds connection setup, request
    /// write and the response head; the response body is bounded only by the
    /// per-pull idle timeout and cancellation.
    #[must_use]
    pub const fn streaming(deadline: Instant, cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline,
            body_deadline: None,
            fresh_connection: false,
        }
    }

    /// Adds a genuine caller hard deadline without extending the head budget.
    #[must_use]
    pub const fn with_body_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.body_deadline = deadline;
        self
    }

    /// Bypasses idle pool checkout for this exchange without changing the
    /// client's identity or preventing clean check-in of the new connection.
    #[must_use]
    pub const fn with_fresh_connection(mut self, fresh: bool) -> Self {
        self.fresh_connection = fresh;
        self
    }

    pub(crate) const fn fresh_connection(&self) -> bool {
        self.fresh_connection
    }

    /// Returns the absolute deadline that also bounds the response body, if any.
    #[must_use]
    pub const fn body_deadline(&self) -> Option<Instant> {
        self.body_deadline
    }

    /// Returns the cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the absolute deadline for the exchange up to its response head.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
}
