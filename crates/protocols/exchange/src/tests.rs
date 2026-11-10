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
        advice_beyond_budget: false,
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
    // Advice beyond the wait budget names itself, ahead of the budget.
    let advice = RetryDeclineInput {
        delay: None,
        advice_beyond_budget: true,
        ..input()
    };
    assert_eq!(decline(&advice), Some(Decline::AdviceBeyondBudget));
    let advice_without_redispatch = RetryDeclineInput {
        redispatch_available: false,
        advice_beyond_budget: true,
        ..input()
    };
    assert_eq!(
        decline(&advice_without_redispatch),
        Some(Decline::RetryDisabled)
    );
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
