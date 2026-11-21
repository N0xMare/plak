//! The physical retry budget and observation shared by all wire families.

use std::time::Duration;

use plakous_http::{ErrorKind, Failure, Phase, Request, TransmissionFact};
use plakous_model::{
    ModelRetry, ModelRetryCause, ModelRetryDeclineReason, ModelRetryLoss, ModelRetryPhase,
    ModelTransportFault,
};

use crate::attempt::{
    FailurePoint, FailureRisk, Jitter, LongStreamReplayBudget, PhysicalRequestLedger,
    ProviderRefusal, ReplaySafety, RetryPolicy, RetryWait, TerminalFacts, UnreachableRideOut,
};
use crate::{RetryDeclineInput, retry_cause_refusal, retry_decline, retry_phase};

/// Typed proof that the provider cannot have received the entire request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRejection {
    /// No request bytes were sent while establishing the connection.
    Unreachable,
    /// The peer did not receive the final request byte.
    Incomplete,
    /// The peer rejected the TLS record, requiring a fresh connection.
    RecordRejected,
}

impl TransportRejection {
    /// Reads dispatch evidence, without treating arbitrary local failures as unsent.
    #[must_use]
    pub fn from_failure(failure: Failure) -> Option<Self> {
        let rejection = match failure.transmission() {
            TransmissionFact::NotStarted
                if matches!(
                    failure.phase(),
                    Phase::Resolution | Phase::Connect | Phase::Tls
                ) =>
            {
                Self::Unreachable
            }
            TransmissionFact::RequestIncomplete => Self::Incomplete,
            _ => return None,
        };
        Some(if failure.cause() == Some("peer_rejected_record") {
            Self::RecordRejected
        } else {
            rejection
        })
    }
}

/// The transport faults a turn may replay; local limits and internal failures
/// are not transient.
#[must_use]
pub const fn transport_fault(failure: Failure) -> Option<ModelTransportFault> {
    match failure.kind() {
        ErrorKind::Io => Some(ModelTransportFault::Io),
        ErrorKind::Protocol => Some(ModelTransportFault::Protocol),
        ErrorKind::Tls => Some(ModelTransportFault::Tls),
        ErrorKind::DeadlineExceeded => Some(ModelTransportFault::Timeout),
        _ => None,
    }
}

/// A physical failure for which the host must evaluate the retry policy.
pub struct RetryFailure {
    point: FailurePoint,
    cause: ModelRetryCause,
    retry_after: Option<Duration>,
    discarded: bool,
    detail: String,
    loss: ModelRetryLoss,
    provider_refusal: Option<ProviderRefusal>,
    request: Option<(String, String, usize)>,
}

impl RetryFailure {
    /// Classifies one failure before the response head.
    #[must_use]
    pub fn transport(failure: Failure, rejection: Option<TransportRejection>) -> Option<Self> {
        let (point, cause) = match rejection {
            Some(TransportRejection::Unreachable) => {
                (FailurePoint::Unreachable, ModelRetryCause::Connect)
            }
            Some(TransportRejection::Incomplete) => {
                (FailurePoint::RequestIncomplete, ModelRetryCause::Connect)
            }
            Some(TransportRejection::RecordRejected) => (
                FailurePoint::RecordRejectedInTransit,
                ModelRetryCause::RecordRejectedInTransit,
            ),
            None => (
                FailurePoint::AfterSend,
                ModelRetryCause::TransportAfterSend {
                    fault: transport_fault(failure)?,
                },
            ),
        };
        Some(
            Self::new(point, cause, format!("{failure:?}"))
                .with_phase(retry_phase(failure.phase())),
        )
    }

    /// Classifies a status, using the effective refusal status for a wrapped
    /// upstream error and the caller's proof of rejection.
    #[must_use]
    pub fn status(
        status: u16,
        refused: bool,
        retry_after: Option<Duration>,
        detail: String,
    ) -> Option<Self> {
        let point = RetryPolicy::status_failure_point(status)?;
        let mut failure = Self::new(
            if refused {
                point
            } else {
                FailurePoint::AfterSend
            },
            ModelRetryCause::HttpStatus { status },
            detail,
        )
        .with_phase(Some(ModelRetryPhase::ResponseHead));
        failure.retry_after = retry_after;
        Some(failure)
    }

    /// Describes a broken stream and the provisional output lost if replayed.
    #[must_use]
    pub fn interrupted(
        fault: ModelTransportFault,
        detail: String,
        discarded: bool,
        loss: ModelRetryLoss,
    ) -> Self {
        let mut failure = Self::new(
            FailurePoint::MidStream,
            ModelRetryCause::StreamInterrupted { fault },
            detail,
        );
        failure.discarded = discarded;
        failure.loss = loss;
        failure
    }

    fn new(point: FailurePoint, cause: ModelRetryCause, detail: String) -> Self {
        Self {
            point,
            cause,
            retry_after: None,
            discarded: false,
            detail,
            loss: ModelRetryLoss::default(),
            provider_refusal: None,
            request: None,
        }
    }

    #[cfg(test)]
    pub(super) fn record_rejection_fixture() -> Self {
        Self::new(
            FailurePoint::RecordRejectedInTransit,
            ModelRetryCause::RecordRejectedInTransit,
            String::new(),
        )
    }

    /// Retains the exact phase, including phases without neutral names.
    #[must_use]
    pub const fn with_phase(mut self, phase: Option<ModelRetryPhase>) -> Self {
        self.loss.transport_phase = phase;
        self
    }

    /// Keeps an already-determined refusal rather than inferring it from the cause.
    #[must_use]
    pub const fn with_refusal(mut self, refusal: ProviderRefusal) -> Self {
        self.provider_refusal = Some(refusal);
        self
    }

    /// Establishes a decline ahead of the common retry ladder.
    #[must_use]
    pub const fn with_decline(mut self, decline: ModelRetryDeclineReason) -> Self {
        self.loss.declined_reason = Some(decline);
        self
    }

    /// Whether the next generation must not reuse the rejected connection.
    #[must_use]
    pub fn needs_fresh_connection(&self) -> bool {
        self.cause == ModelRetryCause::RecordRejectedInTransit
    }

    /// Attaches only the content-free request diagnostics used by retry notices.
    #[must_use]
    pub fn with_request(self, request: (String, String, usize)) -> Self {
        Self {
            request: Some(request),
            ..self
        }
    }

    /// Extracts the content-free diagnostics before transport consumes the request.
    #[must_use]
    pub fn request_log(request: &Request) -> (String, String, usize) {
        (
            request.origin().host().to_owned(),
            request.path_and_query().path().to_owned(),
            request.body().len(),
        )
    }
}

/// Call policy and the time bounds at the point of a retry decision.
#[derive(Clone, Copy)]
pub(super) struct RetryWindow {
    /// Remaining time under the caller's hard deadline.
    pub remaining: Option<Duration>,
    /// Total elapsed time this generation, including any credential work.
    pub elapsed: Duration,
    /// Whether the next generation can be re-authorized by this adapter.
    pub redispatch_available: bool,
    /// Whether the logical operation is a turn rather than a session workflow.
    pub turn_rooted: bool,
}

#[derive(Default)]
pub(super) struct RetryBudget {
    waited: Duration,
    long_stream_replays: LongStreamReplayBudget,
    unreachable: UnreachableRideOut,
}

impl RetryBudget {
    pub(super) fn permits_renewal(&self, facts: TerminalFacts) -> bool {
        facts.failure_risk() == FailureRisk::None
            && self.unreachable.ordinary_physical(facts) < RetryPolicy::TURN.max_physical()
    }

    pub(super) fn waited(&mut self, duration: Duration) {
        self.waited = self.waited.saturating_add(duration);
    }

    pub(super) fn evaluate(
        &mut self,
        ledger: &PhysicalRequestLedger,
        window: RetryWindow,
        failure: RetryFailure,
    ) -> ModelRetry {
        let facts = TerminalFacts::from_snapshot(ledger.snapshot());
        let wait = self.unreachable.next_wait(
            facts,
            failure.point,
            failure.retry_after,
            self.waited,
            Jitter::random(),
        );
        let delay = wait.delay();
        let reason = retry_decline(
            &RetryDeclineInput {
                deadline_remaining: window.remaining,
                established: failure.loss.declined_reason,
                redispatch_available: window.redispatch_available,
                turn_retry: window.turn_rooted,
                cause: failure.cause,
                point: failure.point,
                facts,
                elapsed: window.elapsed,
                delay,
                advice_beyond_budget: wait == RetryWait::AdviceBeyondBudget,
            },
            &mut self.long_stream_replays,
        );
        ledger.record_failure(
            failure.point,
            failure.loss.transport_phase.map(ModelRetryPhase::as_str),
            failure
                .provider_refusal
                .or_else(|| retry_cause_refusal(failure.cause)),
            matches!(
                failure.point,
                FailurePoint::Unreachable
                    | FailurePoint::RequestIncomplete
                    | FailurePoint::RecordRejectedInTransit
            ),
            reason.map(ModelRetryDeclineReason::as_str),
        );
        let retry = ModelRetry::new(
            facts.physical_attempts().saturating_add(1),
            self.unreachable.max_physical(facts, failure.point),
            if reason.is_some() {
                Duration::ZERO
            } else {
                delay.unwrap_or_default()
            },
            failure.cause,
            failure.discarded && reason.is_none(),
            failure.detail,
        )
        .with_loss(ModelRetryLoss {
            elapsed_ms: u64::try_from(window.elapsed.as_millis()).unwrap_or(u64::MAX),
            server_effect: facts.replay_safety() == ReplaySafety::ServerEffect,
            declined_reason: reason,
            ..failure.loss
        });
        if let Some((host, route, bytes)) = failure.request {
            retry.with_request(&host, &route, bytes)
        } else {
            retry
        }
    }
}
