//! Shared Tower `call_operation` skeleton for Responses-family dialects.
//!
//! Dialects supply admission marks, seals, await hooks, and panic/cancel
//! mapping. This module owns only cancel-aware readiness, admit-then-call
//! ordering, shared admitted await, and response-binding checks.

use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::Poll;

use tower::Service;

use super::admitted_await::{
    AdmittedAwaitHooks, AdmittedAwaitPolicy, ServiceAdmissionSeal, await_admitted_operation,
};

/// Dialect-owned inputs for one [`call_admitted_operation`] invocation.
pub trait AdmittedCallDriver {
    /// Moved Tower request type (the attempt).
    type Request: Send;
    /// Successful terminal response.
    type Response: Send;
    /// Typed terminal failure.
    type Error: Send;
    /// Admission seal closed before the service future drops.
    type Seal: ServiceAdmissionSeal + 'static;
    /// Frozen admission context (binding + facts handles).
    type Binding;
    /// Await-hook policy (paid recovery, cleanup, panics).
    type Hooks: AdmittedAwaitHooks<Ok = Self::Response, Err = Self::Error>;

    /// Context frozen before `Service::call` consumes the request (binding + facts).
    fn expected_context(request: &Self::Request) -> Self::Binding;

    /// True when the successful response matches the frozen admission context.
    fn response_matches(response: &Self::Response, expected: &Self::Binding) -> bool;

    /// True when the attempt's cancellation token is already cancelled.
    fn is_cancelled(request: &Self::Request) -> bool;

    /// `poll_ready` panicked.
    fn on_ready_panic(request: &Self::Request) -> Self::Error;

    /// Cancellation observed before or at admission.
    fn on_cancel(request: &Self::Request) -> Self::Error;

    /// Marks service admission and builds the seal + await hooks.
    ///
    /// Called only after readiness succeeded and cancel was rechecked.
    fn admit(request: &Self::Request) -> Result<(Self::Seal, Self::Hooks), Self::Error>;

    /// `Service::call` panicked after admission was opened.
    fn on_call_panic(seal: &mut Self::Seal, hooks: &Self::Hooks) -> Self::Error;

    /// Success response does not match the frozen admission context.
    ///
    /// Invoked after await (hooks already consumed). `Binding` should carry any
    /// ledger handles needed to build conservative facts.
    fn on_binding_mismatch(response: &Self::Response, expected: &Self::Binding) -> Self::Error;

    /// Cleanup poll budget for the shared admitted await loop.
    fn await_policy() -> AdmittedAwaitPolicy {
        AdmittedAwaitPolicy::DEFAULT
    }
}

/// Cancel-aware readiness, admit, call, await, and binding check for one attempt.
///
/// # Errors
///
/// Propagates readiness, cancel, admit, service, await, or binding failures.
pub async fn call_admitted_operation<S, D, C>(
    service: &mut S,
    request: D::Request,
    mut cancelled: C,
    _driver: D,
) -> Result<D::Response, D::Error>
where
    D: AdmittedCallDriver,
    S: Service<D::Request, Response = D::Response, Error = D::Error> + Send,
    S::Future: Future<Output = Result<D::Response, D::Error>> + Send,
    C: Future<Output = ()> + Unpin,
{
    let expected = D::expected_context(&request);
    let readiness = poll_fn(|context| {
        if Pin::new(&mut cancelled).poll(context).is_ready() {
            return Poll::Ready(Err(D::on_cancel(&request)));
        }
        match catch_unwind(AssertUnwindSafe(|| service.poll_ready(context))) {
            Ok(result) => result,
            Err(_) => Poll::Ready(Err(D::on_ready_panic(&request))),
        }
    })
    .await;
    readiness?;

    if D::is_cancelled(&request) {
        return Err(D::on_cancel(&request));
    }

    let (mut admission, hooks) = D::admit(&request)?;
    if D::is_cancelled(&request) {
        ServiceAdmissionSeal::seal(&mut admission);
        return Err(D::on_cancel(&request));
    }

    let future = match catch_unwind(AssertUnwindSafe(|| service.call(request))) {
        Ok(future) => future,
        Err(_) => {
            let error = D::on_call_panic(&mut admission, &hooks);
            ServiceAdmissionSeal::seal(&mut admission);
            return Err(error);
        }
    };

    let result =
        await_admitted_operation(future, cancelled, admission, hooks, D::await_policy()).await;

    match result {
        Ok(response) if !D::response_matches(&response, &expected) => {
            Err(D::on_binding_mismatch(&response, &expected))
        }
        other => other,
    }
}
