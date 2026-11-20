//! Shared admitted-operation await core with cancellation cleanup polls.
//!
//! Dialects supply admission seals, cancellation futures, and error-mapping
//! hooks. This module owns only the fail-closed poll/cleanup/seal ordering.

use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::lifecycle::MAX_ADMITTED_CLEANUP_POLLS;

/// Cleanup budget and await knobs for one admitted operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmittedAwaitPolicy {
    /// Cooperative polls granted after cancellation is observed.
    pub max_cleanup_polls: usize,
}

impl AdmittedAwaitPolicy {
    /// Default cleanup budget shared by Responses-family dialects.
    pub const DEFAULT: Self = Self {
        max_cleanup_polls: MAX_ADMITTED_CLEANUP_POLLS,
    };
}

/// Dialect-owned error mapping for the shared admitted await loop.
pub trait AdmittedAwaitHooks {
    /// Successful terminal response type.
    type Ok;
    /// Typed terminal failure type.
    type Err;

    /// Cleanup budget exhausted while the service future remained pending.
    fn on_cleanup_exhausted(&self) -> Self::Err;

    /// Service future poll panicked.
    fn on_poll_panic(&self) -> Self::Err;

    /// Service future destructor panicked after admission was sealed.
    fn on_drop_future_panic(&self) -> Self::Err;

    /// Optional fail-closed check while the service future remains pending.
    ///
    /// Used by Messages to fence terminal event delivery that must seal the
    /// service immediately. Responses-family dialects leave the default.
    fn check_while_pending(&self) -> Option<Self::Err> {
        None
    }

    /// Normalizes the raw service result after seal, including cancellation.
    fn finalize(
        &self,
        result: Result<Self::Ok, Self::Err>,
        cancelled: bool,
    ) -> Result<Self::Ok, Self::Err>;
}

/// Seals service admission before the in-flight service future is dropped.
pub trait ServiceAdmissionSeal: Send {
    /// Idempotently seals observation authority and lifecycle fences.
    fn seal(&mut self);
}

struct AdmittedOperation<F> {
    admission: Box<dyn ServiceAdmissionSeal>,
    future: Option<Pin<Box<F>>>,
}

impl<F> AdmittedOperation<F> {
    fn new(future: F, admission: Box<dyn ServiceAdmissionSeal>) -> Self {
        Self {
            admission,
            future: Some(Box::pin(future)),
        }
    }

    fn seal(&mut self) {
        self.admission.seal();
    }

    /// Seals admission then drops the future; returns whether drop panicked.
    fn seal_and_drop_future(&mut self) -> bool {
        self.seal();
        let future = self.future.take();
        catch_unwind(AssertUnwindSafe(|| drop(future))).is_err()
    }

    fn poll_future(&mut self, context: &mut Context<'_>) -> Poll<F::Output>
    where
        F: Future,
    {
        self.future
            .as_mut()
            .map_or(Poll::Pending, |future| future.as_mut().poll(context))
    }
}

impl<F> Drop for AdmittedOperation<F> {
    fn drop(&mut self) {
        // Revoke authorization before the service future's destructor runs.
        // This ordering also holds during unwinding from outer drop paths.
        let _ = self.seal_and_drop_future();
    }
}

/// Awaits one admitted service future with cancellation cleanup polls.
///
/// Mid-call cancellation grants `policy.max_cleanup_polls` cooperative polls.
/// Exhaustion, poll panic, and drop panic are mapped through `hooks`. Admission
/// is sealed before the service future is dropped.
pub async fn await_admitted_operation<F, C, S, H>(
    future: F,
    mut cancelled: C,
    admission: S,
    hooks: H,
    policy: AdmittedAwaitPolicy,
) -> Result<H::Ok, H::Err>
where
    F: Future<Output = Result<H::Ok, H::Err>> + Send,
    C: Future<Output = ()> + Unpin,
    S: ServiceAdmissionSeal + 'static,
    H: AdmittedAwaitHooks,
{
    let mut operation = AdmittedOperation::new(future, Box::new(admission));
    let mut cleanup_polls_remaining = None;
    let mut saw_cancel = false;
    let result = poll_fn(|context| {
        match catch_unwind(AssertUnwindSafe(|| operation.poll_future(context))) {
            Ok(Poll::Ready(result)) => return Poll::Ready(result),
            Ok(Poll::Pending) => {}
            Err(_) => return Poll::Ready(Err(hooks.on_poll_panic())),
        }
        if let Some(error) = hooks.check_while_pending() {
            return Poll::Ready(Err(error));
        }
        if cleanup_polls_remaining.is_none() && Pin::new(&mut cancelled).poll(context).is_ready() {
            saw_cancel = true;
            cleanup_polls_remaining = Some(policy.max_cleanup_polls);
        }
        let Some(remaining) = cleanup_polls_remaining.as_mut() else {
            return Poll::Pending;
        };
        if *remaining == 0 {
            return Poll::Ready(Err(hooks.on_cleanup_exhausted()));
        }
        *remaining -= 1;
        context.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
    if operation.seal_and_drop_future() {
        return Err(hooks.on_drop_future_panic());
    }
    hooks.finalize(result, saw_cancel)
}
