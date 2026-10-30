use super::*;
use futures_util::StreamExt as _;
use plakous_model::{
    AttemptId, CancellationToken, Digest, DigestDomain, FixedClock, ModelCallContext, Timestamp,
};
use std::{error::Error, num::NonZeroUsize};

#[derive(Clone)]
struct UnusedHooks;

impl MessagesHttpHooks for UnusedHooks {
    type ErrorMetadata = ();

    fn build_request(
        &self,
        _: &Client,
        _: Origin,
        _: &MessagesAttempt,
        _: &PreparedMessagesHttpOperation,
    ) -> Result<HttpRequest, MessagesAttemptProtocolError> {
        unreachable!("declined retry does not dispatch")
    }

    fn classify_response_head(
        &self,
        _: &Response,
    ) -> Result<MessagesHttpResponseHead<()>, MessagesAttemptProtocolError> {
        unreachable!("declined retry does not exchange")
    }

    fn classify_error_response(
        &self,
        (): (),
        _: &[u8],
        _: MessagesHttpSafety,
    ) -> MessagesAttemptProtocolError {
        unreachable!("declined retry does not exchange")
    }

    fn classify_transport_failure(
        &self,
        _: HttpFailure,
        _: Option<&str>,
    ) -> MessagesAttemptProtocolError {
        unreachable!("declined retry does not exchange")
    }

    fn cancellation_error(&self) -> MessagesAttemptProtocolError {
        unreachable!("fixture is not cancelled")
    }

    fn classify_stream_error(&self, _: MessagesStreamError) -> MessagesAttemptProtocolError {
        unreachable!("declined retry does not exchange")
    }
}

async fn assert_decline(
    hard_deadline: Option<Instant>,
    reason: Decline,
) -> Result<(), Box<dyn Error>> {
    let digest = |domain| Digest::sha256(domain, 1, &[b"messages-decline"]);
    let operation = MessagesOperationBinding::new(
        digest(DigestDomain::ModelBinding)?,
        digest(DigestDomain::RequestPlan)?,
        digest(DigestDomain::ProviderOperation)?,
        plakous_attempt::ReplaySafety::CostOnly,
    );
    let context = ModelCallContext::new(
        AttemptId::try_from(uuid::Uuid::from_u128(100))?,
        CancellationToken::new(),
        None,
    );
    let (mut attempt, mut receiver) = MessagesAttempt::with_event_channel(
        operation,
        context,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(1_700_000_000, 0)?));
    let mut failure = Retryable::new(
        FailurePoint::MidStream,
        ModelRetryCause::StreamInterrupted {
            fault: ModelTransportFault::EndOfStream,
        },
    );
    failure.output_discarded = true;
    failure.loss.assistant_text_bytes = 7;
    let mut waited = Duration::ZERO;
    let step = retry_step(
        &mut attempt,
        RetryContext {
            hooks: &UnusedHooks,
            redispatch: None,
            turn_rooted: false,
            clock: &clock,
            hard_deadline,
            hook_budget: Duration::from_secs(5),
            request: &("fixture.invalid".into(), "/messages".into(), 2),
        },
        failure,
        RetryProgress {
            waited: &mut waited,
            long_stream_replays: &mut plakous_attempt::LongStreamReplayBudget::default(),
            unreachable: &mut plakous_attempt::UnreachableRideOut::default(),
        },
        Duration::from_secs(1),
    )
    .await;
    assert!(matches!(step, RetryStep::End));
    let event = receiver.next().await.ok_or("decline event")??;
    let ModelEvent::Retry(retry) = event else {
        panic!("expected retry observation")
    };
    assert_eq!(retry.loss().declined_reason, Some(reason));
    assert_eq!(retry.loss().assistant_text_bytes, 7);
    assert_eq!(retry.delay(), Duration::ZERO);
    assert!(!retry.output_discarded());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_expired_hard_deadline_declines_as_deadline() -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_decline(Some(deadline), Decline::Deadline).await
}

#[tokio::test(start_paused = true)]
async fn a_missing_redispatch_hook_declines_as_retry_disabled() -> Result<(), Box<dyn Error>> {
    assert_decline(None, Decline::RetryDisabled).await
}
