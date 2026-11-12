use super::*;
use crate::{ObservationSnapshot, ProviderAcceptance};

#[test]
fn terminal_usage_labels_do_not_include_ordinary_rate_limits() {
    assert!(body_reports_usage_limit(
        br#"{"error":{"code":"insufficient_quota"}}"#
    ));
    assert!(body_reports_usage_limit(
        br#"{"error":{"type":"payment_required"}}"#
    ));
    assert!(!body_reports_usage_limit(
        br#"{"error":{"code":"rate_limit_exceeded"}}"#
    ));
}

struct Facts {
    physical: u32,
    acceptance: ProviderAcceptance,
    safety: ReplaySafety,
    hosted_observed: bool,
}

impl Facts {
    const fn refused(physical: u32) -> Self {
        Self {
            physical,
            acceptance: ProviderAcceptance::NotAccepted,
            safety: ReplaySafety::CostOnly,
            hosted_observed: false,
        }
    }

    const fn accepted(physical: u32, safety: ReplaySafety) -> Self {
        Self {
            physical,
            acceptance: ProviderAcceptance::Accepted,
            safety,
            hosted_observed: false,
        }
    }

    fn build(self) -> TerminalFacts {
        let capable = self.safety == ReplaySafety::ServerEffect;
        TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: self.physical,
            provider_acceptance: self.acceptance,
            visible_output: false,
            replay_safety: self.safety,
            hosted_effects_possible: self.hosted_observed
                || (capable && self.acceptance != ProviderAcceptance::NotAccepted),
            logical_success: false,
        })
    }
}

const POLICY: RetryPolicy = RetryPolicy::TURN;

#[test]
fn incomplete_writes_ride_out_minutes_without_spending_ordinary_retries() {
    for point in [
        FailurePoint::RequestIncomplete,
        FailurePoint::RecordRejectedInTransit,
    ] {
        for jitter in [Jitter::SHORTEST, Jitter::LONGEST] {
            let mut budget = UnreachableRideOut::default();
            let mut waited = Duration::ZERO;
            for physical in 1..=16 {
                let delay = budget.next_delay(
                    Facts::refused(physical).build(),
                    point,
                    None,
                    waited,
                    jitter,
                );
                if physical == 16 {
                    assert_eq!(delay, None);
                } else {
                    let Some(delay) = delay else {
                        panic!("incomplete-write allowance exhausted early");
                    };
                    waited += delay;
                }
            }
            assert!((Duration::from_secs(360)..=Duration::from_secs(900)).contains(&waited));
            assert_eq!(budget.physical(), 16);
            for (physical, expected) in [(17, Some(1)), (18, Some(2)), (19, Some(4)), (20, None)] {
                let delay = budget.next_delay(
                    Facts::refused(physical).build(),
                    FailurePoint::Refused,
                    None,
                    waited,
                    Jitter::LONGEST,
                );
                assert_eq!(delay, expected.map(Duration::from_secs));
                waited += delay.unwrap_or_default();
            }
            // A possible earlier hosted effect still forbids even a proven
            // incomplete next request from buying another dispatch.
            assert_eq!(
                UnreachableRideOut::default().next_delay(
                    Facts::accepted(1, ReplaySafety::ServerEffect).build(),
                    point,
                    None,
                    Duration::ZERO,
                    jitter
                ),
                None
            );
        }
    }
}

#[test]
fn unreachable_ride_out_has_twelve_requests_and_exact_equal_jitter_bounds() {
    for (jitter, expected_total) in [(Jitter::LONGEST, 240), (Jitter::SHORTEST, 120)] {
        let mut budget = UnreachableRideOut::default();
        let mut waited = Duration::ZERO;
        for physical in 1..=12 {
            let delay = budget.next_delay(
                Facts::refused(physical).build(),
                FailurePoint::Unreachable,
                None,
                waited,
                jitter,
            );
            if physical == 12 {
                assert_eq!(delay, None);
            } else {
                let base = (2_u64 << (physical - 1).min(4)).min(30);
                let expected = Duration::from_secs(base);
                let expected = if jitter == Jitter::SHORTEST {
                    expected / 2
                } else {
                    expected
                };
                assert_eq!(delay, Some(expected));
                waited += expected;
            }
        }
        assert_eq!(waited, Duration::from_secs(expected_total));
        assert_eq!(budget.physical(), 12);
    }
}

#[test]
fn unreachable_ride_out_does_not_spend_ordinary_request_or_wait_budgets() {
    let mut budget = UnreachableRideOut::default();
    let mut waited = Duration::ZERO;
    for physical in 1..=10 {
        let delay = budget.next_delay(
            Facts::refused(physical).build(),
            FailurePoint::Unreachable,
            None,
            waited,
            Jitter::LONGEST,
        );
        assert!(delay.is_some(), "ride out exhausted early");
        waited += delay.unwrap_or_default();
    }
    assert!(waited > Duration::from_secs(120));
    for (physical, expected) in [(11, Some(1)), (12, Some(2)), (13, Some(4)), (14, None)] {
        let delay = budget.next_delay(
            Facts::refused(physical).build(),
            FailurePoint::Refused,
            None,
            waited,
            Jitter::LONGEST,
        );
        assert_eq!(delay, expected.map(Duration::from_secs));
        waited += delay.unwrap_or_default();
    }
    assert_eq!(budget.physical(), 10);
    let hosted = Facts {
        hosted_observed: true,
        ..Facts::refused(1)
    };
    assert_eq!(
        UnreachableRideOut::default().next_delay(
            hosted.build(),
            FailurePoint::Unreachable,
            None,
            Duration::ZERO,
            Jitter::LONGEST
        ),
        None
    );
}
const MID: FailurePoint = FailurePoint::MidStream;

fn delay(facts: Facts, point: FailurePoint) -> Option<Duration> {
    POLICY.next_delay(facts.build(), point, None, Duration::ZERO, Jitter::LONGEST)
}

#[test]
fn record_rejected_first_resend_is_immediate_then_uses_normal_jitter() {
    let point = FailurePoint::RecordRejectedInTransit;
    for jitter in [Jitter::SHORTEST, Jitter::LONGEST] {
        assert_eq!(
            POLICY.next_delay(
                Facts::refused(1).build(),
                point,
                None,
                Duration::ZERO,
                jitter
            ),
            Some(Duration::ZERO)
        );
    }
    assert_eq!(
        POLICY.next_delay(
            Facts::refused(2).build(),
            point,
            None,
            Duration::ZERO,
            Jitter::SHORTEST
        ),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        delay(Facts::refused(2), point),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        delay(Facts::refused(3), point),
        Some(Duration::from_secs(4))
    );
    assert_eq!(delay(Facts::refused(4), point), None);
    assert_eq!(
        POLICY.next_delay(
            Facts::refused(2).build(),
            point,
            None,
            Duration::from_secs(119),
            Jitter::LONGEST
        ),
        None
    );
    assert_eq!(
        POLICY.next_delay(
            Facts::refused(1).build(),
            point,
            None,
            Duration::from_secs(121),
            Jitter::LONGEST
        ),
        None
    );
    let mut server = Facts::refused(1);
    server.safety = ReplaySafety::ServerEffect;
    assert_eq!(delay(server, point), Some(Duration::ZERO));
    let mut hosted = Facts::refused(1);
    hosted.hosted_observed = true;
    assert_eq!(delay(hosted, point), None);
    assert_eq!(
        delay(Facts::refused(1), FailurePoint::Refused),
        Some(Duration::from_secs(1))
    );
}

#[test]
fn backoff_doubles_from_one_second_and_the_fourth_request_is_the_last() {
    let secs = |s| Some(Duration::from_secs(s));
    assert_eq!(delay(Facts::refused(1), FailurePoint::Refused), secs(1));
    assert_eq!(delay(Facts::refused(2), FailurePoint::Refused), secs(2));
    assert_eq!(delay(Facts::refused(3), FailurePoint::Refused), secs(4));
    assert_eq!(delay(Facts::refused(4), FailurePoint::Refused), None);
    let cost_only = ReplaySafety::CostOnly;
    assert_eq!(delay(Facts::accepted(4, cost_only), MID), None);
}

#[test]
fn a_long_generation_allows_only_one_replay_even_if_the_replay_is_short() {
    let mut budget = LongStreamReplayBudget::default();
    assert!(budget.permits(1, MID, Duration::from_secs(301)));
    assert!(!budget.permits(2, MID, Duration::from_secs(1)));
    assert!(!budget.permits(2, FailurePoint::Refused, Duration::ZERO));
}

#[test]
fn short_generations_and_the_exact_300_second_boundary_keep_the_normal_budget() {
    let mut budget = LongStreamReplayBudget::default();
    assert!(budget.permits(1, MID, Duration::from_secs(300)));
    assert!(budget.permits(2, MID, Duration::from_secs(300)));
    assert!(budget.permits(3, MID, Duration::from_secs(301)));
    assert!(!budget.permits(4, MID, Duration::ZERO));
}

#[test]
fn equal_jitter_stays_between_half_and_the_whole_backoff() {
    let facts = || Facts::refused(3).build();
    let wait = |jitter| {
        POLICY
            .next_delay(facts(), FailurePoint::Refused, None, Duration::ZERO, jitter)
            .unwrap_or_default()
    };
    assert_eq!(wait(Jitter::SHORTEST), Duration::from_secs(2));
    assert_eq!(wait(Jitter::LONGEST), Duration::from_secs(4));
    for sample in [1, 7, u32::MAX / 3, u32::MAX / 2, u32::MAX - 1] {
        let delay = wait(Jitter::new(sample));
        assert!(
            delay >= Duration::from_secs(2) && delay <= Duration::from_secs(4),
            "{delay:?}"
        );
    }
    // Provider advice is never jittered.
    assert_eq!(
        POLICY.next_delay(
            facts(),
            FailurePoint::Refused,
            Some(Duration::from_secs(7)),
            Duration::ZERO,
            Jitter::SHORTEST
        ),
        Some(Duration::from_secs(7))
    );
}

/// Advice that fits the remaining wait budget is honored, clamped to sixty
/// seconds; advice beyond it is refused at once rather than waited short.
#[test]
fn provider_advice_is_clamped_within_the_budget_and_refused_beyond_it() {
    let advice = |seconds| Some(Duration::from_secs(seconds));
    let next = |physical, retry_after, waited| {
        POLICY.next_delay(
            Facts::refused(physical).build(),
            FailurePoint::Refused,
            retry_after,
            Duration::from_secs(waited),
            Jitter::LONGEST,
        )
    };
    assert_eq!(next(1, advice(7), 0), advice(7));
    assert_eq!(next(3, advice(1), 0), advice(1));
    assert_eq!(next(1, advice(0), 0), advice(0));
    assert_eq!(next(1, advice(120), 0), advice(60));
    assert_eq!(next(1, advice(121), 0), None);
    assert_eq!(next(1, advice(3_600), 0), None);
    assert_eq!(next(1, advice(86_400), 0), None);
    assert_eq!(next(2, advice(90), 60), None);
    assert_eq!(next(2, advice(60), 60), advice(60));
    // The total wait is bounded: two full minutes allow no third wait.
    assert_eq!(next(3, advice(60), 120), None);
    let wait = |physical, retry_after, waited| {
        POLICY.next_wait(
            Facts::refused(physical).build(),
            FailurePoint::Refused,
            retry_after,
            Duration::from_secs(waited),
            Jitter::LONGEST,
        )
    };
    assert_eq!(wait(1, advice(121), 0), RetryWait::AdviceBeyondBudget);
    assert_eq!(wait(2, advice(61), 60), RetryWait::AdviceBeyondBudget);
    assert_eq!(
        wait(1, advice(120), 0),
        RetryWait::After(Duration::from_secs(60))
    );
    assert_eq!(wait(1, None, 0), RetryWait::After(Duration::from_secs(1)));
    // With no physical request left the budget decline is the honest one.
    assert_eq!(wait(4, advice(3_600), 0), RetryWait::Exhausted);
    // The outage ride-out takes no advice.
    let mut ride_out = UnreachableRideOut::default();
    assert_eq!(
        ride_out.next_wait(
            Facts::refused(1).build(),
            FailurePoint::Refused,
            advice(121),
            Duration::ZERO,
            Jitter::LONGEST,
        ),
        RetryWait::AdviceBeyondBudget
    );
    assert!(matches!(
        ride_out.next_wait(
            Facts::refused(1).build(),
            FailurePoint::Unreachable,
            advice(121),
            Duration::ZERO,
            Jitter::LONGEST,
        ),
        RetryWait::After(_)
    ));
}

#[test]
fn a_refusal_retries_for_any_request_without_a_possible_hosted_effect() {
    let server = Facts {
        safety: ReplaySafety::ServerEffect,
        ..Facts::refused(1)
    };
    assert!(delay(server, FailurePoint::Refused).is_some());
    // A server-effect request that was possibly accepted before may have
    // started its effect, so even a later refusal ends the attempt.
    let possibly = Facts {
        acceptance: ProviderAcceptance::PossiblyAccepted,
        ..Facts::accepted(1, ReplaySafety::ServerEffect)
    };
    assert_eq!(delay(possibly, FailurePoint::Refused), None);
}

#[test]
fn after_send_and_mid_stream_retry_only_cost_only_requests() {
    let possibly = |safety| Facts {
        acceptance: ProviderAcceptance::PossiblyAccepted,
        ..Facts::accepted(1, safety)
    };
    assert!(delay(possibly(ReplaySafety::CostOnly), FailurePoint::AfterSend).is_some());
    assert_eq!(
        delay(
            possibly(ReplaySafety::ServerEffect),
            FailurePoint::AfterSend
        ),
        None
    );
    assert!(delay(Facts::accepted(1, ReplaySafety::CostOnly), MID).is_some());
    assert_eq!(
        delay(Facts::accepted(1, ReplaySafety::ServerEffect), MID),
        None
    );
}

#[test]
fn cost_only_streams_retry_but_observed_hosted_activity_never_does() {
    let point = FailurePoint::MidStream;
    assert_eq!(
        delay(Facts::accepted(1, ReplaySafety::CostOnly), point),
        Some(Duration::from_secs(1))
    );
    let hosted = Facts {
        hosted_observed: true,
        ..Facts::accepted(1, ReplaySafety::CostOnly)
    };
    assert_eq!(delay(hosted, MID), None);
    let hosted = Facts {
        hosted_observed: true,
        ..Facts::refused(1)
    };
    assert_eq!(delay(hosted, FailurePoint::Refused), None);
}

#[test]
fn statuses_classify_as_refusals_ambiguities_or_never() {
    for status in [429, 503, 529] {
        assert_eq!(
            RetryPolicy::status_failure_point(status),
            Some(FailurePoint::Refused),
            "{status}"
        );
    }
    for status in [408, 500, 502, 504] {
        assert_eq!(
            RetryPolicy::status_failure_point(status),
            Some(FailurePoint::AfterSend),
            "{status}"
        );
    }
    for status in [400, 401, 402, 403, 404, 413, 422, 501, 505, 520] {
        assert_eq!(RetryPolicy::status_failure_point(status), None, "{status}");
    }
}

#[test]
fn exhausted_entitlement_is_terminal_but_ordinary_limits_are_not() {
    for label in [
        "insufficient_quota",
        "quota_exceeded",
        "payment_required",
        "billing_error",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "usage_not_included",
    ] {
        assert!(terminal_usage_error(label), "{label}");
    }
    for label in [
        "rate_limit_exceeded",
        "rate_limit_error",
        "overloaded_error",
    ] {
        assert!(!terminal_usage_error(label), "{label}");
    }
}

#[test]
fn retry_after_accepts_seconds_or_http_date() {
    assert_eq!(retry_after_seconds(" 12 "), Some(Duration::from_secs(12)));
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let later = now + Duration::from_secs(12);
    assert_eq!(
        retry_after_at(&httpdate::fmt_http_date(later), now),
        Some(Duration::from_secs(12))
    );
    assert_eq!(
        retry_after_at(&httpdate::fmt_http_date(now - Duration::from_secs(1)), now),
        Some(Duration::ZERO)
    );
    assert_eq!(retry_after_seconds("-1"), None);
    assert_eq!(retry_after_seconds("garbage"), None);
    assert_eq!(retry_after_seconds("+1"), None);
    assert_eq!(retry_after_seconds("1.5"), None);
    assert_eq!(retry_after_seconds("18446744073709551616"), None);
    assert_eq!(
        retry_after_seconds("18446744073709551615"),
        Some(Duration::from_secs(u64::MAX))
    );
}

#[test]
fn failure_risk_reads_acceptance_and_hosted_effects_only() {
    use crate::FailureRisk;
    assert_eq!(Facts::refused(1).build().failure_risk(), FailureRisk::None);
    for acceptance in [
        ProviderAcceptance::PossiblyAccepted,
        ProviderAcceptance::Accepted,
    ] {
        let cost = Facts {
            acceptance,
            ..Facts::accepted(1, ReplaySafety::CostOnly)
        };
        assert_eq!(
            cost.build().failure_risk(),
            FailureRisk::DuplicateInferenceOrBilling
        );
        let server = Facts {
            acceptance,
            ..Facts::accepted(1, ReplaySafety::ServerEffect)
        };
        assert_eq!(server.build().failure_risk(), FailureRisk::HostedEffect);
    }
}
