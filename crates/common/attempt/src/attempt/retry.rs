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
    /// The provider provably refused the request before accepting it.
    Refused,
    /// The transport proves the peer never received the complete request.
    RequestIncomplete,
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

/// Per-call allowances for unreachable and provably incomplete requests,
/// without spending ordinary retries.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnreachableRideOut {
    physical: u32,
    waited: Duration,
    incomplete_physical: u32,
    incomplete_waited: Duration,
}

impl UnreachableRideOut {
    /// Returns failed physical requests proven unreachable or incomplete.
    #[must_use]
    pub const fn physical(self) -> u32 {
        self.physical.saturating_add(self.incomplete_physical)
    }

    /// Returns the physical count charged to the ordinary four-request allowance.
    #[must_use]
    pub const fn ordinary_physical(self, facts: TerminalFacts) -> u32 {
        facts.physical_attempts().saturating_sub(self.physical())
    }

    /// Returns the current retry row's physical upper bound.
    #[must_use]
    pub fn max_physical(self, facts: TerminalFacts, point: FailurePoint) -> u32 {
        match point {
            FailurePoint::Unreachable => 12_u32
                .saturating_add(self.incomplete_physical)
                .saturating_add(self.ordinary_physical(facts)),
            FailurePoint::RequestIncomplete | FailurePoint::RecordRejectedInTransit => 16_u32
                .saturating_add(self.physical)
                .saturating_add(self.ordinary_physical(facts)),
            _ => RetryPolicy::TURN
                .max_physical()
                .saturating_add(self.physical()),
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
        self.next_wait(facts, point, retry_after, waited, jitter)
            .delay()
    }

    /// [`Self::next_delay`] with the reason no wait is permitted. The outage
    /// ride-out takes no provider advice.
    pub fn next_wait(
        &mut self,
        facts: TerminalFacts,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> RetryWait {
        if matches!(
            point,
            FailurePoint::RequestIncomplete | FailurePoint::RecordRejectedInTransit
        ) {
            return self.incomplete_wait(facts, point, jitter);
        }
        if point != FailurePoint::Unreachable {
            return RetryPolicy::TURN.next_wait_for_physical(
                facts,
                self.ordinary_physical(facts),
                point,
                retry_after,
                waited.saturating_sub(self.waited.saturating_add(self.incomplete_waited)),
                jitter,
            );
        }
        self.physical = self.physical.saturating_add(1);
        if self.physical >= 12 || facts.hosted_effects_possible() {
            return RetryWait::Exhausted;
        }
        let base = (2_u64 << self.physical.saturating_sub(1).min(4)).min(30);
        let delay = jitter.spread(Duration::from_secs(base));
        let total = self.waited.saturating_add(delay);
        if total > Duration::from_secs(240) {
            return RetryWait::Exhausted;
        }
        self.waited = total;
        RetryWait::After(delay)
    }

    fn incomplete_wait(
        &mut self,
        facts: TerminalFacts,
        point: FailurePoint,
        jitter: Jitter,
    ) -> RetryWait {
        self.incomplete_physical = self.incomplete_physical.saturating_add(1);
        if self.incomplete_physical >= 16 || facts.hosted_effects_possible() {
            return RetryWait::Exhausted;
        }
        // One immediate fresh-connection resend preserves the isolated TLS
        // rejection fast path; persistent failures must ride out an outage.
        let delay =
            if point == FailurePoint::RecordRejectedInTransit && facts.physical_attempts() == 1 {
                Duration::ZERO
            } else {
                let exponent = self.incomplete_physical.saturating_sub(1).min(3);
                jitter.spread(Duration::from_secs((10_u64 << exponent).min(60)))
            };
        let total = self.incomplete_waited.saturating_add(delay);
        if total > Duration::from_secs(900) {
            return RetryWait::Exhausted;
        }
        self.incomplete_waited = total;
        RetryWait::After(delay)
    }
}

/// The retry policy's answer for one failed physical request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryWait {
    /// Wait this long, then send the next physical request.
    After(Duration),
    /// The provider's retry advice exceeds what remains of the wait budget
    /// while physical requests remain: waiting less would only fail again.
    AdviceBeyondBudget,
    /// No physical request or wait remains, or the failure is not replayable.
    Exhausted,
}

impl RetryWait {
    /// The wait, when one is permitted.
    #[must_use]
    pub const fn delay(self) -> Option<Duration> {
        match self {
            Self::After(delay) => Some(delay),
            Self::AdviceBeyondBudget | Self::Exhausted => None,
        }
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
    /// `Retry-After` is honored as given up to sixty seconds and is never
    /// jittered; longer advice is clamped to sixty seconds while it still fits
    /// the remaining wait budget, and refused when it does not. Without advice
    /// the wait doubles from one second, with equal jitter. A peer-rejected
    /// TLS record on the first physical request has zero first backoff. At
    /// most two minutes of waiting in total.
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
        self.next_wait(facts, point, retry_after, waited, jitter)
            .delay()
    }

    /// [`Self::next_delay`] with the reason no wait is permitted.
    #[must_use]
    pub fn next_wait(
        self,
        facts: TerminalFacts,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> RetryWait {
        self.next_wait_for_physical(
            facts,
            facts.physical_attempts(),
            point,
            retry_after,
            waited,
            jitter,
        )
    }

    fn next_wait_for_physical(
        self,
        facts: TerminalFacts,
        physical: u32,
        point: FailurePoint,
        retry_after: Option<Duration>,
        waited: Duration,
        jitter: Jitter,
    ) -> RetryWait {
        if facts.hosted_effects_possible() || physical >= self.max_physical {
            return RetryWait::Exhausted;
        }
        let cost_only = facts.replay_safety() == ReplaySafety::CostOnly;
        let replayable = match point {
            FailurePoint::Unreachable
            | FailurePoint::Refused
            | FailurePoint::RequestIncomplete
            | FailurePoint::RecordRejectedInTransit => true,
            FailurePoint::AfterSend | FailurePoint::MidStream => cost_only,
        };
        if !replayable {
            return RetryWait::Exhausted;
        }
        let delay = match retry_after {
            _ if point == FailurePoint::RecordRejectedInTransit && physical == 1 => Duration::ZERO,
            Some(advice) if advice > self.max_total_delay.saturating_sub(waited) => {
                return RetryWait::AdviceBeyondBudget;
            }
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
        if waited.saturating_add(delay) <= self.max_total_delay {
            RetryWait::After(delay)
        } else {
            RetryWait::Exhausted
        }
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
            | "usage_limit_reached"
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
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests;
