use super::super::test_wire::TestWire;
use super::*;
use futures_util::StreamExt as _;
use plakous_model::{
    AttemptId, CancellationToken, Digest, DigestDomain, ModelCallContext, ModelRetryLoss,
};
use std::{error::Error, num::NonZeroUsize};

struct DeclineHooks {
    attempt: FamilyAttempt<TestWire>,
    failure: Option<RetryFailure>,
}

impl FamilyExchangeHooks for DeclineHooks {
    type Result = bool;
    type Terminal = ();

    fn ledger(&self) -> &crate::attempt::PhysicalRequestLedger {
        self.attempt.ledger()
    }

    fn exchange(&mut self, _: Generation) -> impl Future<Output = GenerationEnd<()>> + Send {
        let Some(failure) = self.failure.take() else {
            panic!("declined retry does not dispatch")
        };
        std::future::ready(GenerationEnd::Retry {
            terminal: (),
            failure,
        })
    }

    async fn announce_retry(&mut self, retry: ModelRetry) -> bool {
        self.attempt.events_mut().send_retry(retry).await.is_ok()
    }

    fn reauthorize(&mut self, _: bool) -> impl Future<Output = Reauthorization> + Send {
        assert!(
            self.failure.is_some(),
            "declined retry does not reauthorize"
        );
        std::future::ready(Reauthorization::End)
    }

    fn finish(&mut self, (): (), deferred: bool) -> impl Future<Output = bool> + Send {
        std::future::ready(deferred)
    }

    fn cancelled(&mut self) -> bool {
        false
    }

    fn deadline(&mut self, (): ()) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }
}

async fn assert_decline(
    hard_deadline: Option<Instant>,
    reason: Decline,
) -> Result<(), Box<dyn Error>> {
    let digest = |domain| Digest::sha256(domain, 1, &[b"messages-decline"]);
    let operation = FamilyOperationBinding::new(
        digest(DigestDomain::ModelBinding)?,
        digest(DigestDomain::RequestPlan)?,
        digest(DigestDomain::ProviderOperation)?,
        crate::attempt::ReplaySafety::CostOnly,
    );
    let cancellation = CancellationToken::new();
    let context = ModelCallContext::new(
        AttemptId::try_from(uuid::Uuid::from_u128(100))?,
        cancellation.clone(),
        None,
    );
    let (attempt, mut receiver) = FamilyAttempt::<TestWire>::with_event_channel(
        operation,
        context,
        NonZeroUsize::new(1).ok_or("nonzero")?,
    );
    let failure = RetryFailure::interrupted(
        ModelTransportFault::EndOfStream,
        "short stream".into(),
        true,
        ModelRetryLoss {
            assistant_text_bytes: 7,
            ..ModelRetryLoss::default()
        },
    )
    .with_request(("fixture.invalid".into(), "/messages".into(), 2));
    let host = FamilyExchange {
        timeout: Duration::from_secs(5),
        hard_deadline,
        turn_rooted: false,
        redispatch_available: false,
        retry_delivery: RetryDelivery::BeforeDelay,
    };
    assert!(
        host.run(
            &mut DeclineHooks {
                attempt,
                failure: Some(failure)
            },
            &cancellation
        )
        .await
    );
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
