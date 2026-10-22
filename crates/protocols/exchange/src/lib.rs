//! Shared physical-exchange plumbing for protocol-family kernels and providers.
//!
//! The Chat Completions, Messages and Responses kernels, and the provider
//! clients that issue their own credential exchanges, map `plakous-http`
//! failures to the same neutral `plakous-model` network facts, read error
//! bodies under the same bound and cancellation race, and decline a turn retry
//! with one precedence (spec 01 §13.1). This crate owns those pieces once.
//!
//! ## Not in this crate
//!
//! - Wire grammar, SSE framing, or family event machines
//! - Attempt lifecycle and observation ledgers (`plakous-attempt`)
//! - Provider credentials or route identities

#![forbid(unsafe_code)]

pub mod lookup;

use std::future::Future;
use std::time::Duration;

use futures_util::future::{Either, select};
use plakous_attempt::{
    FailurePoint, LongStreamReplayBudget, ProviderRefusal, ReplaySafety, TerminalFacts,
};
use plakous_http::{
    CancellationToken as HttpCancellationToken, ErrorKind, Failure, Phase, ResponseBody,
    TransmissionFact,
};
use plakous_model::{
    CancellationToken, ModelNetworkFailure, ModelNetworkTransmission, ModelRetryCause,
    ModelRetryDeclineReason, ModelRetryPhase, ModelTransportFault, Timestamp,
};

/// Maps a transport phase to its neutral retry phase; `None` for phases the
/// model vocabulary does not name.
#[must_use]
pub const fn retry_phase(phase: Phase) -> Option<ModelRetryPhase> {
    Some(match phase {
        Phase::Resolution => ModelRetryPhase::Resolution,
        Phase::Connect => ModelRetryPhase::Connect,
        Phase::Tls => ModelRetryPhase::Tls,
        Phase::RequestWrite => ModelRetryPhase::RequestWrite,
        Phase::RequestWriteOrResponseHead => ModelRetryPhase::RequestWriteOrResponseHead,
        Phase::ResponseHead => ModelRetryPhase::ResponseHead,
        Phase::ResponseBody => ModelRetryPhase::ResponseBody,
        Phase::Cleanup => ModelRetryPhase::Cleanup,
        _ => return None,
    })
}

/// Returns the content-free human network cause for a physical exchange failure.
#[must_use]
pub fn network_failure(failure: Failure) -> Option<ModelNetworkFailure> {
    Some(ModelNetworkFailure {
        phase: retry_phase(failure.phase())?,
        sent: match failure.transmission() {
            TransmissionFact::NotStarted => ModelNetworkTransmission::NotSent,
            TransmissionFact::RequestIncomplete => ModelNetworkTransmission::Incomplete,
            TransmissionFact::MayHaveTransmitted => ModelNetworkTransmission::MayHaveSent,
        },
        timed_out: failure.kind() == ErrorKind::DeadlineExceeded,
    })
}

/// Time left from `now` until `deadline`, zero once it has passed.
#[must_use]
pub fn time_until(deadline: Timestamp, now: Timestamp) -> Duration {
    let nanos =
        |at: Timestamp| u128::from(at.seconds()) * 1_000_000_000 + u128::from(at.nanoseconds());
    Duration::from_nanos(
        u64::try_from(nanos(deadline).saturating_sub(nanos(now))).unwrap_or(u64::MAX),
    )
}

/// A future's output and whether caller cancellation raced it.
pub struct Cancellable<T> {
    /// The future's output; after cancellation, the output of its aborted run.
    pub value: T,
    /// Whether the caller cancelled before the future finished.
    pub cancelled: bool,
}

/// Runs `future` until it finishes or `cancellation` fires. On cancellation
/// the HTTP exchange is cancelled and the future still runs to its own end,
/// so transport cleanup is never detached.
pub async fn cancellable<F: Future>(
    future: F,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
) -> Cancellable<F::Output> {
    match select(Box::pin(future), Box::pin(cancellation.clone().cancelled())).await {
        Either::Left((value, _)) => Cancellable {
            value,
            cancelled: false,
        },
        Either::Right(((), future)) => {
            http_cancellation.cancel();
            Cancellable {
                value: future.await,
                cancelled: true,
            }
        }
    }
}

/// Retains at most `limit + 1` bytes of an error body, so the caller can tell
/// a truncated body from a complete one.
///
/// # Errors
///
/// Returns only internal transport failures (a required capture failed); a
/// read or close failure of any other kind ends the body with what was read.
pub async fn read_bounded_body(
    mut body: ResponseBody,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let mut retained = Vec::new();
    loop {
        let next = cancellable(body.next_chunk(), cancellation, http_cancellation).await;
        // Error bodies are optional, but required capture failures must end the
        // attempt before a status retry or credential refresh can dispatch.
        if let Some(Err(failure)) = &next.value
            && failure.kind() == ErrorKind::Internal
        {
            return Err(*failure);
        }
        if next.cancelled {
            close_body(body).await?;
            return Ok(retained);
        }
        match next.value {
            Some(Ok(chunk)) => {
                let remaining = limit.saturating_add(1).saturating_sub(retained.len());
                retained.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if retained.len() > limit {
                    close_body(body).await?;
                    return Ok(retained);
                }
            }
            Some(Err(_)) | None => return Ok(retained),
        }
    }
}

async fn close_body(body: ResponseBody) -> Result<(), Failure> {
    match body.close().await {
        Err(failure) if failure.kind() == ErrorKind::Internal => Err(failure),
        _ => Ok(()),
    }
}

/// The refusal a failed generation's provider named, for the attempt summary.
///
/// A status before the stream and the same refusal inside it name one class,
/// so every kernel records the same one. A failure the client inferred names
/// none.
#[must_use]
pub const fn retry_cause_refusal(cause: ModelRetryCause) -> Option<ProviderRefusal> {
    match cause {
        ModelRetryCause::HttpStatus { status } => ProviderRefusal::from_http_status(status),
        ModelRetryCause::StreamInterrupted {
            fault: ModelTransportFault::Overloaded,
        } => Some(ProviderRefusal::Overloaded),
        ModelRetryCause::StreamInterrupted {
            fault: ModelTransportFault::RateLimited,
        } => Some(ProviderRefusal::RateLimit),
        _ => None,
    }
}

/// Everything the turn-retry decline ladder reads for one failed generation.
#[derive(Clone, Copy, Debug)]
pub struct RetryDeclineInput {
    /// Time left before the caller's hard deadline, if it set one.
    pub deadline_remaining: Option<Duration>,
    /// A decline the exchange already established, such as a provider's
    /// retry-disabled response header.
    pub established: Option<ModelRetryDeclineReason>,
    /// Whether a resend can be re-authorized; without it the retry is disabled.
    pub redispatch_available: bool,
    /// Whether this operation is a replayable turn workflow. Other workflows
    /// retry only on typed evidence that the request was not fully sent.
    pub turn_retry: bool,
    /// The failure's typed cause.
    pub cause: ModelRetryCause,
    /// Where the failure happened.
    pub point: FailurePoint,
    /// The logical attempt's facts so far.
    pub facts: TerminalFacts,
    /// Time since the failed physical request started.
    pub elapsed: Duration,
    /// The ride-out delay before the next request; `None` when the physical or
    /// wait budget is exhausted.
    pub delay: Option<Duration>,
}

/// Returns why a turn retry must not happen, or `None` to retry after
/// `input.delay`. Precedence: a passed deadline, an established decline, a
/// disabled retry, the workflow, a possible server effect, the budget, and a
/// deadline the delay would cross. The long-stream budget is consulted (and
/// charged) only when every earlier rung admits the retry.
#[must_use]
pub fn retry_decline(
    input: &RetryDeclineInput,
    long_stream_replays: &mut LongStreamReplayBudget,
) -> Option<ModelRetryDeclineReason> {
    use ModelRetryDeclineReason as Decline;
    let facts = input.facts;
    if input.deadline_remaining.is_some_and(|left| left.is_zero()) {
        Some(Decline::Deadline)
    } else if input.established.is_some() {
        input.established
    } else if !input.redispatch_available {
        Some(Decline::RetryDisabled)
    } else if !input.turn_retry
        && !matches!(
            input.cause,
            ModelRetryCause::Connect | ModelRetryCause::RecordRejectedInTransit
        )
    {
        Some(Decline::Workflow)
    } else if facts.hosted_effects_possible()
        || (facts.replay_safety() == ReplaySafety::ServerEffect
            && !matches!(
                input.point,
                FailurePoint::Unreachable
                    | FailurePoint::Refused
                    | FailurePoint::RecordRejectedInTransit
            ))
    {
        Some(Decline::ServerEffect)
    } else if !long_stream_replays.permits(facts.physical_attempts(), input.point, input.elapsed)
        || input.delay.is_none()
    {
        Some(Decline::Budget)
    } else if input
        .deadline_remaining
        .is_some_and(|left| input.delay.unwrap_or_default() >= left)
    {
        Some(Decline::Deadline)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests assert on infallible constants"
)]
mod tests {
    use super::*;
    use plakous_model::ModelRetryDeclineReason as Decline;

    fn decline(input: &RetryDeclineInput) -> Option<Decline> {
        retry_decline(input, &mut LongStreamReplayBudget::default())
    }

    fn input() -> RetryDeclineInput {
        RetryDeclineInput {
            deadline_remaining: Some(Duration::from_secs(10)),
            established: None,
            redispatch_available: true,
            turn_retry: true,
            cause: ModelRetryCause::HttpStatus { status: 503 },
            point: FailurePoint::Refused,
            facts: TerminalFacts::preacceptance_test(),
            elapsed: Duration::from_secs(1),
            delay: Some(Duration::from_secs(1)),
        }
    }

    #[test]
    fn a_provider_named_refusal_is_one_class_before_and_inside_the_stream() {
        use ModelRetryCause as Cause;
        for (cause, expected) in [
            (
                Cause::HttpStatus { status: 429 },
                Some(ProviderRefusal::RateLimit),
            ),
            (
                Cause::HttpStatus { status: 503 },
                Some(ProviderRefusal::Overloaded),
            ),
            (
                Cause::HttpStatus { status: 504 },
                Some(ProviderRefusal::Deadline),
            ),
            (Cause::HttpStatus { status: 400 }, None),
            (
                Cause::StreamInterrupted {
                    fault: ModelTransportFault::Overloaded,
                },
                Some(ProviderRefusal::Overloaded),
            ),
            (
                Cause::StreamInterrupted {
                    fault: ModelTransportFault::RateLimited,
                },
                Some(ProviderRefusal::RateLimit),
            ),
            (
                Cause::StreamInterrupted {
                    fault: ModelTransportFault::Timeout,
                },
                None,
            ),
            (
                Cause::TransportAfterSend {
                    fault: ModelTransportFault::Timeout,
                },
                None,
            ),
            (Cause::Connect, None),
        ] {
            assert_eq!(retry_cause_refusal(cause), expected, "{cause:?}");
        }
    }

    #[test]
    fn retries_when_nothing_declines() {
        assert_eq!(decline(&input()), None);
        let unbounded = RetryDeclineInput {
            deadline_remaining: None,
            ..input()
        };
        assert_eq!(decline(&unbounded), None);
    }

    #[test]
    fn a_passed_deadline_outranks_every_other_decline() {
        let passed = RetryDeclineInput {
            deadline_remaining: Some(Duration::ZERO),
            established: Some(Decline::RetryDisabled),
            redispatch_available: false,
            turn_retry: false,
            delay: None,
            ..input()
        };
        assert_eq!(decline(&passed), Some(Decline::Deadline));
    }

    #[test]
    fn declines_follow_the_specified_precedence() {
        let established = RetryDeclineInput {
            established: Some(Decline::RetryDisabled),
            turn_retry: false,
            ..input()
        };
        assert_eq!(decline(&established), Some(Decline::RetryDisabled));
        let no_redispatch = RetryDeclineInput {
            redispatch_available: false,
            turn_retry: false,
            ..input()
        };
        assert_eq!(decline(&no_redispatch), Some(Decline::RetryDisabled));
        let workflow = RetryDeclineInput {
            turn_retry: false,
            delay: None,
            ..input()
        };
        assert_eq!(decline(&workflow), Some(Decline::Workflow));
        let budget = RetryDeclineInput {
            delay: None,
            deadline_remaining: Some(Duration::from_millis(1)),
            ..input()
        };
        assert_eq!(decline(&budget), Some(Decline::Budget));
        // The first long-generation replay is admitted; it bounds later ones.
        let long_stream = RetryDeclineInput {
            point: FailurePoint::MidStream,
            elapsed: Duration::from_secs(301),
            ..input()
        };
        assert_eq!(decline(&long_stream), None);
        let crosses_deadline = RetryDeclineInput {
            deadline_remaining: Some(Duration::from_secs(1)),
            ..input()
        };
        assert_eq!(decline(&crosses_deadline), Some(Decline::Deadline));
    }

    #[test]
    fn a_workflow_still_retries_a_request_proven_unsent() {
        for cause in [
            ModelRetryCause::Connect,
            ModelRetryCause::RecordRejectedInTransit,
        ] {
            let unsent = RetryDeclineInput {
                turn_retry: false,
                cause,
                ..input()
            };
            assert_eq!(decline(&unsent), None);
        }
    }

    #[test]
    fn time_until_saturates_at_zero() {
        let at = |seconds, nanos| Timestamp::new(seconds, nanos).expect("timestamp");
        assert_eq!(
            time_until(at(10, 500), at(9, 400)),
            Duration::from_nanos(1_000_000_100)
        );
        assert_eq!(time_until(at(9, 0), at(10, 0)), Duration::ZERO);
    }

    #[test]
    fn maps_every_named_transport_phase() {
        for (phase, expected) in [
            (Phase::Resolution, ModelRetryPhase::Resolution),
            (Phase::Connect, ModelRetryPhase::Connect),
            (Phase::Tls, ModelRetryPhase::Tls),
            (Phase::RequestWrite, ModelRetryPhase::RequestWrite),
            (
                Phase::RequestWriteOrResponseHead,
                ModelRetryPhase::RequestWriteOrResponseHead,
            ),
            (Phase::ResponseHead, ModelRetryPhase::ResponseHead),
            (Phase::ResponseBody, ModelRetryPhase::ResponseBody),
            (Phase::Cleanup, ModelRetryPhase::Cleanup),
        ] {
            assert_eq!(retry_phase(phase), Some(expected));
        }
    }
}

#[cfg(test)]
mod lookup_tests;
