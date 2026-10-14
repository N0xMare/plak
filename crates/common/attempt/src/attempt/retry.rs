//! Bounded automatic retry of turn inference requests.
//!
//! The policy is pure: it reads the attempt ledger's terminal facts, where the
//! physical request failed, the provider's advice and a jitter sample, and
//! returns a delay. Each protocol kernel owns the loop, the sleep,
//! cancellation and its deadline; none may add a second loop.

use std::time::Duration;

use super::terminal::TerminalFacts;

/// What replaying one exact request body can cost.
///
/// Each protocol request builder computes this from the exact serialized body
/// by an allowlist: only known-pure tool types and fields are
/// [`Self::CostOnly`]; anything else, including a tool type nobody has
/// classified yet, is [`Self::ServerEffect`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReplaySafety {
    /// A replay can only cost duplicate inference and billing.
    #[default]
    CostOnly,
    /// The request can start provider-side work beyond inference (hosted code
    /// execution, unclassified hosted retrieval, programmatic tool calling, a mutable server
    /// object), so an ambiguous outcome is never replayed.
    ServerEffect,
}

/// Where one physical request failed, as the kernel that owns it observed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailurePoint {
    /// Resolution, connect or TLS failed with proof that no request bytes were sent.
    Unreachable,
    /// The provider provably refused the request before accepting it, or the
    /// request never left whole. Proven-unreachable connections are separate.
    Refused,
    /// The peer rejected a TLS record of the incomplete request and killed
    /// the connection. Its first resend needs no congestion backoff.
    RecordRejectedInTransit,
    /// The request may have reached the provider, but no response stream
    /// started: a transport failure before any response byte, or an ambiguous
    /// server status.
    AfterSend,
    /// The response stream broke after its head.
    MidStream,
}

/// Per-call allowance for riding out a short outage without spending ordinary retries.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnreachableRideOut {
    physical: u32,
    waited: Duration,
}

impl UnreachableRideOut {
    /// Returns failed physical requests proven unreachable.
    #[must_use]
    pub const fn physical(self) -> u32 {
        self.physical
    }

    /// Returns the physical count charged to the ordinary four-request allowance.
    #[must_use]
    pub const fn ordinary_physical(self, facts: TerminalFacts) -> u32 {
        facts.physical_attempts().saturating_sub(self.physical)
    }

    /// Returns the current retry row's physical upper bound.
    #[must_use]
    pub fn max_physical(self, facts: TerminalFacts, point: FailurePoint) -> u32 {
        if point == FailurePoint::Unreachable {
            12_u32.saturating_add(self.ordinary_physical(facts))
        } else {
            RetryPolicy::TURN
                .max_physical()
                .saturating_add(self.physical)
        }
    }

    /// Records one failed generation and returns its bounded, equal-jittered wait.
    /// Kernels still enforce cancellation, replay safety and the caller's hard deadline.
    pub fn next_delay(
        &mut self,
        facts: TerminalFacts,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> Option<Duration> {
        if point != FailurePoint::Unreachable {
            return RetryPolicy::TURN.next_delay_for_physical(
                facts,
                self.ordinary_physical(facts),
                point,
                retry_after,
                waited.saturating_sub(self.waited),
                jitter,
            );
        }
        self.physical = self.physical.saturating_add(1);
        if self.physical >= 12 || facts.hosted_effects_possible() {
            return None;
        }
        let base = (2_u64 << self.physical.saturating_sub(1).min(4)).min(30);
        let delay = jitter.spread(Duration::from_secs(base));
        let total = self.waited.saturating_add(delay);
        if total > Duration::from_secs(240) {
            return None;
        }
        self.waited = total;
        Some(delay)
    }
}

/// Restricts a logical attempt to one replay after its first long generation.
#[derive(Clone, Copy, Debug, Default)]
pub struct LongStreamReplayBudget {
    last_physical: Option<u32>,
}

impl LongStreamReplayBudget {
    /// Checks the cap independently of the ordinary safety and wait budgets.
    pub fn permits(&mut self, physical: u32, point: FailurePoint, elapsed: Duration) -> bool {
        if self.last_physical.is_some_and(|last| physical >= last) {
            return false;
        }
        if point == FailurePoint::MidStream && elapsed > Duration::from_secs(300) {
            self.last_physical.get_or_insert(physical.saturating_add(1));
        }
        true
    }
}

/// One uniform sample that spreads a computed backoff.
///
/// A kernel draws one per wait with [`Jitter::random`]; tests pass a fixed
/// sample so the policy stays deterministic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Jitter(u32);

impl Jitter {
    /// The sample that waits half the computed backoff.
    pub const SHORTEST: Self = Self(0);
    /// The sample that waits the whole computed backoff.
    pub const LONGEST: Self = Self(u32::MAX);

    /// A fixed sample.
    #[must_use]
    pub const fn new(sample: u32) -> Self {
        Self(sample)
    }

    /// A sample from the operating system's random source; the longest wait
    /// when that source is unavailable.
    #[must_use]
    pub fn random() -> Self {
        getrandom::u32().map_or(Self::LONGEST, Self)
    }

    /// Equal jitter: a wait in `[backoff / 2, backoff]`.
    fn spread(self, backoff: Duration) -> Duration {
        let half = backoff / 2;
        let span = backoff.saturating_sub(half);
        let nanos = span.as_nanos() * u128::from(self.0) / u128::from(u32::MAX);
        half.saturating_add(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }
}

/// The one retry policy every protocol kernel applies to a turn request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_physical: u32,
    base_delay: Duration,
    max_delay: Duration,
    max_total_delay: Duration,
}

impl RetryPolicy {
    /// Four physical requests per logical attempt. The provider's
    /// `Retry-After` is honored as given up to sixty seconds (longer advice is
    /// clamped, not refused) and is never jittered; without advice the wait
    /// doubles from one second, with equal jitter. A peer-rejected TLS record
    /// on the first physical request has zero first backoff. At most two
    /// minutes of waiting in total.
    pub const TURN: Self = Self {
        max_physical: 4,
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(60),
        max_total_delay: Duration::from_secs(120),
    };

    /// Returns the physical-request budget of one logical attempt.
    #[must_use]
    pub const fn max_physical(self) -> u32 {
        self.max_physical
    }

    /// Classifies a non-success HTTP status a kernel saw before any response
    /// stream started, or `None` when the status is never retried.
    ///
    /// A rate limit (`429`), service unavailable (`503`), or Anthropic's
    /// overloaded (`529`) prove the provider refused before acceptance. `500`,
    /// `502`, `504` and `408` may follow work the provider started, so they
    /// are as ambiguous as a dropped connection. Every other status, `401`
    /// included, is not this policy's to retry.
    #[must_use]
    pub const fn status_failure_point(status: u16) -> Option<FailurePoint> {
        match status {
            429 | 503 | 529 => Some(FailurePoint::Refused),
            408 | 500 | 502 | 504 => Some(FailurePoint::AfterSend),
            _ => None,
        }
    }

    /// Returns the wait before the next physical request, or `None` when the
    /// logical attempt must end with the failure it has.
    ///
    /// A provable refusal retries unless a hosted effect is possible. A
    /// failure after the request may have been sent retries only when the
    /// request is [`ReplaySafety::CostOnly`], even after a local tool call began
    /// streaming: no tool from an incomplete response ran. A possible hosted effect
    /// never retries. `waited` is the backoff already spent on this logical
    /// attempt.
    #[must_use]
    pub fn next_delay(
        self,
        facts: TerminalFacts,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> Option<Duration> {
        self.next_delay_for_physical(
            facts,
            facts.physical_attempts(),
            point,
            retry_after,
            waited,
            jitter,
        )
    }

    fn next_delay_for_physical(
        self,
        facts: TerminalFacts,
        physical: u32,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> Option<Duration> {
        if facts.hosted_effects_possible() || physical >= self.max_physical {
            return None;
        }
        let cost_only = facts.replay_safety() == ReplaySafety::CostOnly;
        let replayable = match point {
            FailurePoint::Unreachable
            | FailurePoint::Refused
            | FailurePoint::RecordRejectedInTransit => true,
            FailurePoint::AfterSend | FailurePoint::MidStream => cost_only,
        };
        if !replayable {
            return None;
        }
        let delay = match retry_after {
            _ if point == FailurePoint::RecordRejectedInTransit && physical == 1 => Duration::ZERO,
            Some(advice) => advice.min(self.max_delay),
            None => {
                let doublings = physical.saturating_sub(1).min(16);
                jitter.spread(
                    self.base_delay
                        .saturating_mul(1 << doublings)
                        .min(self.max_delay),
                )
            }
        };
        (waited.saturating_add(delay) <= self.max_total_delay).then_some(delay)
    }
}

/// Provider error labels that denote exhausted entitlement rather than a rate limit.
#[must_use]
pub fn terminal_usage_error(label: &str) -> bool {
    matches!(
        label,
        "insufficient_quota"
            | "quota_exceeded"
            | "payment_required"
            | "billing_error"
            | "credit_balance_exhausted"
            | "organization_spend_limit_exceeded"
            | "project_spend_limit_exceeded"
            | "usage_not_included"
    )
}

/// Checks a bounded error body for exhausted entitlement labels.
#[must_use]
pub fn body_reports_usage_limit(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    value_reports_usage_limit(&value)
}

/// Checks structured error labels without interpreting provider prose.
#[must_use]
pub fn value_reports_usage_limit(value: &serde_json::Value) -> bool {
    let error = value.get("error").unwrap_or(value);
    error.get("code").and_then(serde_json::Value::as_u64) == Some(402)
        || [error.get("code"), error.get("type"), value.get("type")]
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .any(terminal_usage_error)
}

/// Parses a `Retry-After` delay in whole seconds or HTTP-date form.
#[must_use]
pub fn retry_after_seconds(value: &str) -> Option<Duration> {
    retry_after_at(value, std::time::SystemTime::now())
}

fn retry_after_at(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn provider_advice_is_honored_and_clamped_not_refused() {
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
        assert_eq!(next(1, advice(3_600), 0), advice(60));
        // The total wait is bounded: two full minutes allow no third wait.
        assert_eq!(next(3, advice(60), 120), None);
        assert_eq!(next(2, advice(60), 60), advice(60));
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
}
