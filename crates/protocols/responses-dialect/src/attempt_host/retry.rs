//! Retry and decline classification and the bounded wait between attempts.

use super::{
    Arc, Decline, Duration, FailurePoint, FailureRisk, HttpErrorKind, HttpFailure, Jitter,
    ModelRetry, ModelRetryCause, ModelTransportFault, PhysicalRequestOutcome, ReplaySafety,
    ResponsesAttempt, ResponsesAttemptError, ResponsesCredentialRejectedFn,
    ResponsesFamilyStreamError, ResponsesHttpStatusClass, ResponsesObservation, RetryDeclineInput,
    RetryPolicy, StatusCode, StreamProgress, milliseconds, monotonic_now, retry_decline,
    retry_phase, status_outcome,
};

/// What a refused generation leads to.
pub(super) enum RetryStep {
    /// Send the next generation.
    Send,
    /// The caller cancelled while waiting.
    Cancelled,
    /// The operation deadline passed while waiting.
    Deadline,
    /// End the logical attempt with the refusal it has.
    End,
}

/// A connection that could not be established sent nothing, and one the peer
/// broke before the request's final byte was written delivered no request.
/// Any other local failure after final authorization stays may-have-transmitted.
pub(super) fn refused_connection(source: HttpFailure) -> bool {
    use plakous_http::{Phase, TransmissionFact};
    match source.transmission() {
        TransmissionFact::NotStarted => matches!(
            source.phase(),
            Phase::Resolution | Phase::Connect | Phase::Tls
        ),
        TransmissionFact::RequestIncomplete => true,
        TransmissionFact::MayHaveTransmitted => false,
    }
}

/// Whether a refused dispatch may be re-dispatched after a forced credential
/// refresh: only an HTTP 401 head, within the physical budget, while no
/// generation was possibly accepted and no hosted effect is possible.
pub(super) fn credential_rejection_retry_eligible(
    error: &ResponsesAttemptError,
    unreachable: &plakous_attempt::UnreachableRideOut,
) -> bool {
    matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            class: ResponsesHttpStatusClass::Authentication,
            facts,
            retry_disabled: false,
            ..
        } if unreachable.ordinary_physical(*facts) < RetryPolicy::TURN.max_physical()
            && facts.failure_risk() == FailureRisk::None
    )
}

/// How long a retry marker may wait for observation capacity beyond the
/// retry delay itself.
pub(super) const RETRY_MARKER_GRACE: Duration = Duration::from_secs(1);

/// One failed physical request the turn retry policy may replay.
pub(super) struct Retryable {
    point: FailurePoint,
    cause: ModelRetryCause,
    retry_after: Option<Duration>,
    output_discarded: bool,
    detail: String,
    phase: Option<plakous_model::ModelRetryPhase>,
}

/// Classifies a failed physical request for [`RetryPolicy::TURN`]. A failure
/// after the response head is mid-stream; its partial output is discarded.
pub(super) fn in_band_retryable(fault: ModelTransportFault, stream: StreamProgress) -> Retryable {
    Retryable {
        point: FailurePoint::MidStream,
        cause: ModelRetryCause::StreamInterrupted { fault },
        retry_after: None,
        output_discarded: stream.delivered,
        detail: format!("in-band refusal ({fault})"),
        phase: Some(plakous_model::ModelRetryPhase::ResponseBody),
    }
}

pub(super) fn retryable(
    error: &ResponsesAttemptError,
    stream: StreamProgress,
) -> Option<Retryable> {
    let interrupted = |fault: ModelTransportFault, detail: String| Retryable {
        point: FailurePoint::MidStream,
        cause: ModelRetryCause::StreamInterrupted { fault },
        retry_after: None,
        output_discarded: stream.delivered,
        phase: Some(plakous_model::ModelRetryPhase::ResponseBody),
        detail,
    };
    match error {
        ResponsesAttemptError::HttpStatus {
            status,
            retry_after,
            ..
        } => {
            let point = match RetryPolicy::status_failure_point(status.as_u16())? {
                // Only a definite rejection proves the request was refused.
                FailurePoint::Refused
                    if status_outcome(*status) != PhysicalRequestOutcome::DefiniteRejection =>
                {
                    FailurePoint::AfterSend
                }
                point => point,
            };
            Some(Retryable {
                point,
                cause: ModelRetryCause::HttpStatus {
                    status: status.as_u16(),
                },
                retry_after: *retry_after,
                output_discarded: false,
                detail: format!("HTTP {}", status.as_u16()),
                phase: Some(plakous_model::ModelRetryPhase::ResponseHead),
            })
        }
        ResponsesAttemptError::Transport { source, .. } if !stream.started => {
            if refused_connection(*source) {
                return Some(Retryable {
                    point: if source.cause() == Some("peer_rejected_record") {
                        FailurePoint::RecordRejectedInTransit
                    } else if source.transmission() == plakous_http::TransmissionFact::NotStarted {
                        FailurePoint::Unreachable
                    } else {
                        FailurePoint::Refused
                    },
                    cause: if source.cause() == Some("peer_rejected_record") {
                        ModelRetryCause::RecordRejectedInTransit
                    } else {
                        ModelRetryCause::Connect
                    },
                    retry_after: None,
                    output_discarded: false,
                    detail: format!("{source:?}"),
                    phase: retry_phase(source.phase()),
                });
            }
            Some(Retryable {
                point: FailurePoint::AfterSend,
                cause: ModelRetryCause::TransportAfterSend {
                    fault: transport_fault(*source)?,
                },
                retry_after: None,
                output_discarded: false,
                detail: format!("{source:?}"),
                phase: retry_phase(source.phase()),
            })
        }
        _ if !stream.interrupted => None,
        ResponsesAttemptError::Transport { source, .. } => Some(Retryable {
            phase: retry_phase(source.phase()),
            ..interrupted(transport_fault(*source)?, format!("{source:?}"))
        }),
        ResponsesAttemptError::Deadline { .. } => Some(interrupted(
            ModelTransportFault::Timeout,
            "stream idle timeout".to_owned(),
        )),
        ResponsesAttemptError::Stream {
            source:
                source @ (ResponsesFamilyStreamError::Truncated
                | ResponsesFamilyStreamError::MissingTerminal),
            ..
        } => Some(interrupted(
            ModelTransportFault::EndOfStream,
            format!("{source:?}"),
        )),
        _ => None,
    }
}

/// The transport faults a turn may replay; cancellation, local limits, an
/// invalid response and internal failures are not transient.
pub(super) const fn transport_fault(failure: HttpFailure) -> Option<ModelTransportFault> {
    match failure.kind() {
        HttpErrorKind::Io => Some(ModelTransportFault::Io),
        HttpErrorKind::Protocol => Some(ModelTransportFault::Protocol),
        HttpErrorKind::Tls => Some(ModelTransportFault::Tls),
        HttpErrorKind::DeadlineExceeded => Some(ModelTransportFault::Timeout),
        _ => None,
    }
}

/// Forces the credential refresh after a 401, bounded by the operation
/// deadline and cancellation.
pub(super) async fn refresh_credential<D: Send + Sync + 'static>(
    hook: ResponsesCredentialRejectedFn<D>,
    dialect: &Arc<D>,
    attempt: &mut ResponsesAttempt,
) -> RetryStep {
    let started = monotonic_now();
    attempt.next_dispatch_started = Some(started);
    attempt.deadline = started
        .checked_add(attempt.dispatch_timeout)
        .unwrap_or(started);
    if let Some(hard) = attempt.hard_deadline {
        attempt.deadline = attempt.deadline.min(hard);
    }
    tokio::select! {
        biased;
        () = attempt.cancellation.cancelled() => RetryStep::Cancelled,
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(attempt.deadline)) => {
            RetryStep::Deadline
        }
        refreshed = hook(dialect.as_ref()) => match refreshed {
            Ok(()) => RetryStep::Send,
            Err(_) => RetryStep::End,
        },
    }
}

/// Applies [`RetryPolicy::TURN`]: announces the retry on the observation
/// channel, ordered after the failed request's events, then waits its delay
/// when the wait fits before the operation deadline.
#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub(super) async fn wait_to_retry(
    attempt: &mut ResponsesAttempt,
    failure: Retryable,
    waited: &mut Duration,
) -> RetryStep {
    let facts = attempt.facts();
    let elapsed = attempt
        .stream
        .physical_started
        .map_or(Duration::ZERO, |started| {
            monotonic_now().saturating_duration_since(started)
        });
    let delay = attempt.unreachable.next_delay(
        facts,
        failure.point,
        failure.retry_after,
        *waited,
        Jitter::random(),
    );
    let reason = retry_decline(
        &RetryDeclineInput {
            deadline_remaining: attempt
                .hard_deadline
                .map(|hard| hard.saturating_duration_since(monotonic_now())),
            established: attempt
                .stream
                .retry_disabled
                .then_some(Decline::RetryDisabled),
            redispatch_available: true,
            turn_retry: attempt.retry_permitted,
            cause: failure.cause,
            point: failure.point,
            facts,
            elapsed,
            delay,
        },
        &mut attempt.long_stream_replays,
    );
    let unsent = failure.point == FailurePoint::Unreachable;
    attempt.core.observations_ref().record_failure(
        failure.point,
        failure.phase.map(plakous_model::ModelRetryPhase::as_str),
        unsent,
        reason.map(Decline::as_str),
    );
    let delay = delay.unwrap_or_default();
    let output_discarded = failure.output_discarded && reason.is_none();
    let mut retry = ModelRetry::new(
        facts.physical_attempts().saturating_add(1),
        attempt.unreachable.max_physical(facts, failure.point),
        if reason.is_some() {
            Duration::ZERO
        } else {
            delay
        },
        failure.cause,
        output_discarded,
        failure.detail,
    )
    .with_loss(plakous_model::ModelRetryLoss {
        elapsed_ms: milliseconds(elapsed),
        server_effect: facts.replay_safety() == ReplaySafety::ServerEffect,
        transport_phase: failure.phase,
        declined_reason: reason,
        ..attempt.stream.loss
    });
    if let Some((host, route, bytes)) = &attempt.request_log {
        retry = retry.with_request(host, route, *bytes);
    }
    if reason.is_some() {
        if let Some(observations) = &attempt.observations {
            let _ = observations.try_send(ResponsesObservation::Retry(retry));
        }
        return RetryStep::End;
    }
    attempt.fresh_connection = failure.cause == ModelRetryCause::RecordRejectedInTransit;
    let resume = tokio::time::Instant::now() + delay;
    let pause = async {
        let announced = match &attempt.observations {
            None => true,
            Some(observations) => {
                let send = observations.send(ResponsesObservation::Retry(retry));
                // A closed receiver only loses a notice nobody reads.
                tokio::time::timeout(delay.max(RETRY_MARKER_GRACE), send)
                    .await
                    .is_ok()
            }
        };
        tokio::time::sleep_until(resume).await;
        announced
    };
    let announced = tokio::select! {
        biased;
        () = attempt.cancellation.cancelled() => return RetryStep::Cancelled,
        () = async {
            match attempt.hard_deadline {
                Some(hard) => tokio::time::sleep_until(tokio::time::Instant::from_std(hard)).await,
                None => std::future::pending().await,
            }
        } => return RetryStep::Deadline,
        announced = pause => announced,
    };
    // A receiver that never learns to discard the failed request's output
    // would join it to the replay's, so that replay must not happen.
    if !announced && output_discarded {
        return RetryStep::End;
    }
    *waited = waited.saturating_add(delay);
    RetryStep::Send
}
