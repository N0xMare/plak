//! Panic-contained sink delivery futures shared by Responses-family attempts.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Error types that can represent an independent delivery/extension panic.
pub trait PanicAsError {
    /// Constructs the typed panic failure for a sink delivery.
    fn panicked() -> Self;
}

/// Wraps a sink delivery future so poll and drop panics cannot unwind.
pub struct ContainedSinkDelivery<'a, E> {
    delivery: Option<Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>>,
}

impl<'a, E> ContainedSinkDelivery<'a, E>
where
    E: PanicAsError,
{
    /// Contains an owned delivery future.
    #[must_use]
    pub fn new(delivery: Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>) -> Self {
        Self {
            delivery: Some(delivery),
        }
    }

    /// Drops any remaining delivery future, returning whether drop panicked.
    pub fn dispose(&mut self) -> bool {
        let delivery = self.delivery.take();
        catch_unwind(AssertUnwindSafe(|| drop(delivery))).is_err()
    }
}

impl<E> Future for ContainedSinkDelivery<'_, E>
where
    E: PanicAsError,
{
    type Output = Result<(), E>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(delivery) = self.delivery.as_mut() else {
            return Poll::Ready(Err(E::panicked()));
        };
        match catch_unwind(AssertUnwindSafe(|| delivery.as_mut().poll(context))) {
            Ok(delivered) => delivered,
            Err(_) => {
                // Take the panicked future before returning so dispose/Drop cannot
                // re-poll or drop it (second unwind / double panic). Drop of the
                // taken future is also contained — Drop may panic independently.
                let taken = self.delivery.take();
                let _ = catch_unwind(AssertUnwindSafe(|| drop(taken)));
                Poll::Ready(Err(E::panicked()))
            }
        }
    }
}

impl<E> Drop for ContainedSinkDelivery<'_, E> {
    fn drop(&mut self) {
        // Panic-safe dispose even when the caller does not call `dispose`.
        let delivery = self.delivery.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(delivery)));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use std::task::Waker;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TestSinkError {
        Panicked,
    }

    impl PanicAsError for TestSinkError {
        fn panicked() -> Self {
            Self::Panicked
        }
    }

    struct PendingDropPanic;

    impl Future for PendingDropPanic {
        type Output = Result<(), TestSinkError>;

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingDropPanic {
        fn drop(&mut self) {
            panic!("synthetic independently dropped sink delivery panic");
        }
    }

    #[test]
    fn independently_dropped_pending_sink_delivery_cannot_unwind() {
        let mut delivery: ContainedSinkDelivery<'_, TestSinkError> =
            ContainedSinkDelivery::new(Box::pin(PendingDropPanic));
        let mut context = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut delivery).poll(&mut context).is_pending());
        assert!(catch_unwind(AssertUnwindSafe(|| drop(delivery))).is_ok());
    }

    /// Future whose poll panics; Drop also panics so a second drop would unwind.
    struct PollPanicDropPanic;

    impl Future for PollPanicDropPanic {
        type Output = Result<(), TestSinkError>;

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            panic!("synthetic sink delivery poll panic");
        }
    }

    impl Drop for PollPanicDropPanic {
        fn drop(&mut self) {
            panic!("synthetic sink delivery drop after poll panic");
        }
    }

    #[test]
    fn poll_panic_then_drop_cannot_unwind_twice() {
        let mut delivery: ContainedSinkDelivery<'_, TestSinkError> =
            ContainedSinkDelivery::new(Box::pin(PollPanicDropPanic));
        let mut context = Context::from_waker(Waker::noop());
        // First poll panics inside catch_unwind; future is taken so Drop cannot
        // re-enter a panicked future.
        let polled = catch_unwind(AssertUnwindSafe(|| {
            Pin::new(&mut delivery).poll(&mut context)
        }));
        assert!(polled.is_ok());
        match polled.unwrap() {
            Poll::Ready(Err(TestSinkError::Panicked)) => {}
            other => panic!("expected Ready(Panicked), got {other:?}"),
        }
        // Drop must not unwind (would be a second panic if the future remained).
        assert!(catch_unwind(AssertUnwindSafe(|| drop(delivery))).is_ok());
    }
}
