use std::future::Future;
use std::task::{Context, Poll};

use futures::executor::block_on;
use plakous_model::{DigestDomain, ResponseStarted};
use tower_service::Service;
use uuid::Uuid;

use super::*;

fn operation(label: &[u8]) -> Result<MessagesOperationBinding, Box<dyn std::error::Error>> {
    Ok(MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
        ReplaySafety::CostOnly,
    ))
}

fn hosted_operation(label: &[u8]) -> Result<MessagesOperationBinding, Box<dyn std::error::Error>> {
    Ok(MessagesOperationBinding::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
        ReplaySafety::ServerEffect,
    ))
}

fn context(
    id: u128,
    cancellation: CancellationToken,
) -> Result<ModelCallContext, Box<dyn std::error::Error>> {
    Ok(ModelCallContext::new(
        AttemptId::try_from(Uuid::from_u128(id))?,
        cancellation,
        None,
    ))
}

fn started(plan: Digest) -> ModelEvent {
    ModelEvent::ResponseStarted(ResponseStarted::new(None, plan))
}

#[test]
fn cancellation_wakes_backpressured_observation_delivery() -> Result<(), Box<dyn std::error::Error>>
{
    let cancellation = CancellationToken::new();
    let binding = operation(b"backpressure")?;
    let plan = binding.plan();
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        binding,
        context(1, cancellation.clone())?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let mut sink = attempt.events.clone();
    block_on(sink.send(started(plan)))?;
    let mut blocked = Box::pin(sink.send(started(plan)));
    let waker = futures::task::noop_waker();
    let mut task = Context::from_waker(&waker);
    assert!(blocked.as_mut().poll(&mut task).is_pending());
    cancellation.cancel();
    assert!(matches!(
        blocked.as_mut().poll(&mut task),
        Poll::Ready(Err(MessagesAttemptError::Cancelled { .. }))
    ));
    Ok(())
}

#[test]
fn terminal_observation_is_emitted_at_most_once() -> Result<(), Box<dyn std::error::Error>> {
    let (attempt, mut receiver) = MessagesAttempt::with_event_channel(
        operation(b"terminal")?,
        context(2, CancellationToken::new())?,
        NonZeroUsize::new(2).ok_or("nonzero")?,
    );
    let mut first = attempt.events.clone();
    let mut retained = attempt.events.clone();
    let terminal = MessagesAttemptProtocolError::new("terminal fixture");
    block_on(first.send_error(terminal.clone()))?;
    assert_eq!(
        block_on(futures_util::StreamExt::next(&mut receiver)),
        Some(Err(terminal))
    );
    assert!(matches!(
        block_on(retained.send_error(MessagesAttemptProtocolError::new("second"))),
        Err(MessagesAttemptError::Delivery { .. })
    ));
    assert_eq!(block_on(futures_util::StreamExt::next(&mut receiver)), None);
    Ok(())
}

#[test]
fn protocol_metadata_survives_attempt_error() -> Result<(), Box<dyn std::error::Error>> {
    let status = ModelHttpStatus::new(429)?;
    let retry_after = ModelRetryDelay::from_millis(2_000)?;
    let error = MessagesAttemptError::protocol(
        MessagesAttemptProtocolError::new("opaque rejection")
            .with_http_status(status)
            .with_retry_after(retry_after),
        TerminalFacts::preacceptance_test(),
    );
    assert_eq!(error.http_status(), Some(status));
    assert_eq!(error.retry_after(), Some(retry_after));
    Ok(())
}

struct AmbiguousService;
impl Service<MessagesAttempt> for AmbiguousService {
    type Response = MessagesAttemptResponse;
    type Error = MessagesAttemptError;
    type Future = futures_util::future::Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, attempt: MessagesAttempt) -> Self::Future {
        let result = (|| {
            attempt.ensure_service_admitted()?;
            attempt.observe_physical_request_started()?;
            attempt.observe_physical_request_finished(PhysicalRequestOutcome::AmbiguousPostSend)?;
            Err(MessagesAttemptError::protocol(
                MessagesAttemptProtocolError::new("ambiguous fixture"),
                attempt.terminal_facts(),
            ))
        })();
        futures_util::future::ready(result)
    }
}

#[test]
fn admitted_ambiguous_send_never_claims_safe_rejection() -> Result<(), Box<dyn std::error::Error>> {
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        operation(b"ambiguous")?,
        context(3, CancellationToken::new())?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
        .expect_err("ambiguous attempt succeeded");
    let facts = error.facts().ok_or("admitted facts")?;
    assert_eq!(facts.physical_attempts(), 1);
    assert_eq!(
        facts.provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    assert_ne!(
        facts.retry_class(),
        plakous_attempt::RetryClass::BeforeAcceptance
    );
    Ok(())
}

#[test]
fn hosted_capability_records_possible_effect_after_ambiguous_send()
-> Result<(), Box<dyn std::error::Error>> {
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        hosted_operation(b"hosted-ambiguous")?,
        context(6, CancellationToken::new())?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
        .expect_err("ambiguous hosted attempt succeeded");
    let facts = error.facts().ok_or("admitted facts")?;
    assert_eq!(facts.replay_safety(), ReplaySafety::ServerEffect);
    assert!(facts.hosted_effects_possible());
    assert_eq!(facts.retry_class(), plakous_attempt::RetryClass::Never);
    Ok(())
}

struct CancelAfterAcceptedService;
impl Service<MessagesAttempt> for CancelAfterAcceptedService {
    type Response = MessagesAttemptResponse;
    type Error = MessagesAttemptError;
    type Future = futures_util::future::Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, attempt: MessagesAttempt) -> Self::Future {
        let result = (|| {
            attempt.ensure_service_admitted()?;
            attempt.observe_physical_request_started()?;
            attempt.observe_provider_accepted()?;
            attempt.context().cancellation().cancel();
            Err(MessagesAttemptError::Cancelled {
                facts: Some(attempt.terminal_facts()),
            })
        })();
        futures_util::future::ready(result)
    }
}

#[test]
fn hosted_capability_survives_cancellation_after_acceptance()
-> Result<(), Box<dyn std::error::Error>> {
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        hosted_operation(b"hosted-cancel")?,
        context(7, CancellationToken::new())?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let error = block_on(call_messages_operation(
        &mut CancelAfterAcceptedService,
        attempt,
    ))
    .expect_err("cancelled hosted attempt succeeded");
    let facts = error.facts().ok_or("admitted facts")?;
    assert_eq!(facts.provider_acceptance(), ProviderAcceptance::Accepted);
    assert_eq!(facts.replay_safety(), ReplaySafety::ServerEffect);
    assert!(facts.hosted_effects_possible());
    assert_eq!(facts.retry_class(), plakous_attempt::RetryClass::Never);
    Ok(())
}

#[test]
fn admitted_exact_acceptance_is_preserved_in_terminal_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        operation(b"accepted")?,
        context(5, CancellationToken::new())?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    attempt.core.mark_service_admitted()?;
    attempt.observe_physical_request_started()?;
    attempt.observe_provider_accepted()?;
    let facts = attempt.terminal_facts();
    assert_eq!(facts.physical_attempts(), 1);
    assert_eq!(facts.provider_acceptance(), ProviderAcceptance::Accepted);
    assert_ne!(
        facts.retry_class(),
        plakous_attempt::RetryClass::BeforeAcceptance
    );
    Ok(())
}

#[test]
fn cancellation_before_admission_has_no_physical_facts() -> Result<(), Box<dyn std::error::Error>> {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let (attempt, _receiver) = MessagesAttempt::with_event_channel(
        operation(b"cancel")?,
        context(4, cancellation)?,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
        .expect_err("cancelled attempt succeeded");
    assert!(matches!(
        error,
        MessagesAttemptError::Cancelled { facts: None }
    ));
    Ok(())
}
