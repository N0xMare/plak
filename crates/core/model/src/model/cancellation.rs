//! Cooperative cancellation shared by model and tool calls.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

/// A cloneable cooperative cancellation signal.
#[derive(Clone, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    /// Creates an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes registered operations.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            let wakers = {
                let mut registered = match self.state.wakers.lock() {
                    Ok(registered) => registered,
                    Err(poisoned) => poisoned.into_inner(),
                };
                std::mem::take(&mut *registered)
            };
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Reports whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Returns a future that completes when cancellation is requested.
    #[must_use]
    pub fn cancelled(&self) -> CancellationFuture {
        CancellationFuture {
            token: self.clone(),
        }
    }

    pub(crate) fn register(&self, waker: &Waker) {
        if self.is_cancelled() {
            waker.wake_by_ref();
            return;
        }
        let mut registered = match self.state.wakers.lock() {
            Ok(registered) => registered,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !registered.iter().any(|known| known.will_wake(waker)) {
            registered.push(waker.clone());
        }
        if self.is_cancelled() {
            waker.wake_by_ref();
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

/// A future resolved by a [`CancellationToken`].
#[derive(Debug)]
pub struct CancellationFuture {
    token: CancellationToken,
}

impl Future for CancellationFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            Poll::Ready(())
        } else {
            self.token.register(context.waker());
            Poll::Pending
        }
    }
}
