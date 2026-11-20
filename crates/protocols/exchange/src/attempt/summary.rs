//! Transient, content-free accounting for a logical model attempt.

use std::fmt;

use super::retry::FailurePoint;
use super::terminal::ProviderRefusal;

/// Closed content-free transport phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptPhase {
    /// resolution classification.
    Resolution,
    /// connect classification.
    Connect,
    /// tls classification.
    Tls,
    /// request write classification.
    RequestWrite,
    /// request write or response head classification.
    RequestWriteOrResponseHead,
    /// response head classification.
    ResponseHead,
    /// response body classification.
    ResponseBody,
    /// cleanup classification.
    Cleanup,
}

impl AttemptPhase {
    /// Converts the owning kernel's closed label; unknown labels are not retained.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "resolution" => Some(Self::Resolution),
            "connect" => Some(Self::Connect),
            "tls" => Some(Self::Tls),
            "request_write" => Some(Self::RequestWrite),
            "request_write_or_response_head" => Some(Self::RequestWriteOrResponseHead),
            "response_head" => Some(Self::ResponseHead),
            "response_body" => Some(Self::ResponseBody),
            "cleanup" => Some(Self::Cleanup),
            _ => None,
        }
    }
}

/// Closed content-free retry-decline reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptRetryDecline {
    /// deadline classification.
    Deadline,
    /// budget classification.
    Budget,
    /// The provider's retry advice exceeds the remaining wait budget.
    AdviceBeyondBudget,
    /// server effect classification.
    ServerEffect,
    /// retry disabled classification.
    RetryDisabled,
    /// workflow classification.
    Workflow,
}

impl AttemptRetryDecline {
    /// Converts the owning kernel's closed label; unknown labels are not retained.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "deadline" => Some(Self::Deadline),
            "budget" => Some(Self::Budget),
            "advice_beyond_budget" => Some(Self::AdviceBeyondBudget),
            "server_effect" => Some(Self::ServerEffect),
            "retry_disabled" => Some(Self::RetryDisabled),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

impl fmt::Display for AttemptRetryDecline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Deadline => "deadline",
            Self::Budget => "budget",
            Self::AdviceBeyondBudget => "advice_beyond_budget",
            Self::ServerEffect => "server_effect",
            Self::RetryDisabled => "retry_disabled",
            Self::Workflow => "workflow",
        })
    }
}

/// Accounting rendered into terminal diagnostic text, never persisted as a field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttemptSummary {
    /// Physical requests started.
    pub physical: u32,
    /// Physical requests spent on the unreachable or incomplete-write ride-out.
    pub ride_out_physical: u32,
    /// Last classified failure point.
    pub last_point: Option<FailurePoint>,
    /// Closed physical phase label supplied by the owning kernel.
    pub last_phase: Option<AttemptPhase>,
    /// The last failure proved that no request bytes were sent.
    pub proven_unsent: bool,
    /// Closed retry-decline label supplied by the owning kernel.
    pub decline: Option<AttemptRetryDecline>,
    /// The refusal the provider named for the last failure, before the stream
    /// or inside it.
    pub last_refusal: Option<ProviderRefusal>,
}

impl fmt::Display for AttemptSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.physical == 0 {
            return Ok(());
        }
        write!(formatter, " [{} requests", self.physical)?;
        if self.ride_out_physical > 0 {
            write!(formatter, "; {} unsent", self.ride_out_physical)?;
        }
        // A refusal the provider named says more than where the exchange
        // stopped: it is the class the terminal failure is typed as.
        let last = match (self.last_refusal, self.last_phase) {
            (None, _) if self.last_point == Some(FailurePoint::RequestIncomplete) => {
                "request write incomplete"
            }
            (None, _) if self.last_point == Some(FailurePoint::RecordRejectedInTransit) => {
                "TLS record rejected"
            }
            (Some(refusal), _) => refusal.label(),
            (None, Some(AttemptPhase::Resolution)) => "DNS lookup failed",
            (None, Some(AttemptPhase::Connect)) => "could not connect",
            (None, Some(AttemptPhase::Tls)) => "TLS handshake failed",
            (None, Some(AttemptPhase::RequestWrite)) => "while sending",
            (None, Some(AttemptPhase::RequestWriteOrResponseHead | AttemptPhase::ResponseHead)) => {
                "after send"
            }
            (None, Some(AttemptPhase::ResponseBody)) => "response stream interrupted",
            (None, _) => match self.last_point {
                Some(FailurePoint::Unreachable) => "provider unreachable",
                Some(FailurePoint::Refused) => "request refused",
                Some(FailurePoint::RequestIncomplete) => "request write incomplete",
                Some(FailurePoint::RecordRejectedInTransit) => "TLS record rejected",
                Some(FailurePoint::AfterSend) => "after send",
                Some(FailurePoint::MidStream) => "response stream interrupted",
                None => "unclassified",
            },
        };
        write!(formatter, "; last: {last}")?;
        if self.proven_unsent {
            formatter.write_str(if self.last_point == Some(FailurePoint::Unreachable) {
                ", nothing sent"
            } else {
                ", no complete request sent"
            })?;
        }
        if let Some(decline) = self.decline {
            write!(formatter, "; retry declined: {decline}")?;
        }
        formatter.write_str("]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_one_canonical_content_free_suffix() {
        assert_eq!(AttemptSummary::default().to_string(), "");
        let summary = AttemptSummary {
            physical: 4,
            last_point: Some(FailurePoint::Unreachable),
            last_phase: Some(AttemptPhase::Resolution),
            proven_unsent: true,
            decline: Some(AttemptRetryDecline::Deadline),
            ..AttemptSummary::default()
        };
        assert_eq!(
            summary.to_string(),
            " [4 requests; last: DNS lookup failed, nothing sent; retry declined: deadline]"
        );
    }

    #[test]
    fn incomplete_request_summary_never_implies_a_complete_send() {
        let summary = AttemptSummary {
            physical: 16,
            ride_out_physical: 16,
            last_point: Some(FailurePoint::RequestIncomplete),
            last_phase: Some(AttemptPhase::RequestWriteOrResponseHead),
            proven_unsent: true,
            decline: Some(AttemptRetryDecline::Budget),
            ..AttemptSummary::default()
        };
        assert_eq!(
            summary.to_string(),
            " [16 requests; 16 unsent; last: request write incomplete, no complete request sent; retry declined: budget]"
        );
    }

    /// A refusal the provider named is the class, wherever it arrived.
    #[test]
    fn a_named_refusal_is_the_last_class() {
        for (refusal, label) in [
            (ProviderRefusal::RateLimit, "rate limited"),
            (ProviderRefusal::Overloaded, "unavailable"),
            (ProviderRefusal::Deadline, "timeout"),
        ] {
            let summary = AttemptSummary {
                physical: 1,
                last_point: Some(FailurePoint::MidStream),
                last_phase: Some(AttemptPhase::ResponseBody),
                last_refusal: Some(refusal),
                ..AttemptSummary::default()
            };
            assert_eq!(summary.to_string(), format!(" [1 requests; last: {label}]"));
        }
    }
}
