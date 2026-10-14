//! Seal-before-drop wrapper for nested `Service::call` futures.
//!
//! OpenAI's standard HTTP service admits inside `Service::call` when the outer
//! owner is not always `call_operation`. This future seals observation
//! authority before the inner future is dropped (including poll panic).

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::admitted_await::ServiceAdmissionSeal;

/// Seals service admission before the in-flight service future is dropped.
pub struct AdmissionFirstFuture<F, S: ServiceAdmissionSeal + Unpin> {
    admission: Option<S>,
    future: Option<Pin<Box<F>>>,
}

impl<F, S: ServiceAdmissionSeal + Unpin> AdmissionFirstFuture<F, S> {
    /// Wraps `future` with an optional open admission seal.
    #[must_use]
    pub fn new(future: F, admission: Option<S>) -> Self {
        Self {
            admission,
            future: Some(Box::pin(future)),
        }
    }

    fn seal(&mut self) {
        if let Some(admission) = &mut self.admission {
            ServiceAdmissionSeal::seal(admission);
        }
    }

    fn seal_before_inner_drop(&mut self) {
        self.seal();
        let future = self.future.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(future)));
        drop(self.admission.take());
    }
}

impl<F, S: ServiceAdmissionSeal + Unpin> Future for AdmissionFirstFuture<F, S>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // Fields are unpin (`Pin<Box<F>>`, `Option<S>`); project via get_mut.
        let this = self.get_mut();
        let result = catch_unwind(AssertUnwindSafe(|| {
            this.future
                .as_mut()
                .unwrap_or_else(|| {
                    unreachable!("completed admission-first future was polled again")
                })
                .as_mut()
                .poll(context)
        }));
        match result {
            Ok(result) => {
                if result.is_ready() {
                    this.seal();
                }
                result
            }
            Err(panic) => {
                this.seal();
                std::panic::resume_unwind(panic)
            }
        }
    }
}

impl<F, S: ServiceAdmissionSeal + Unpin> Drop for AdmissionFirstFuture<F, S> {
    fn drop(&mut self) {
        self.seal_before_inner_drop();
    }
}
