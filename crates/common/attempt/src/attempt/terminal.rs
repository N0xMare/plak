//! Shared terminal facts, retry class, and provider failure for Responses attempts.

use super::facts::ProviderAcceptance;
use super::ledger::ObservationSnapshot;
use super::retry::ReplaySafety;

/// Conservative retry classification for one logical Responses-family operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryClass {
    /// Retrying is forbidden because acceptance, visibility, or effects are possible.
    Never,
    /// The request was definitely rejected before acceptance, so retry is safe.
    BeforeAcceptance,
    /// Retrying may duplicate provider inference or billing.
    OperationOwnedDuplicateRisk,
}

/// A refusal the provider itself named, as against a failure the client had to
/// infer.
///
/// Naming the refusal is what lets a caller tell a transient provider condition
/// from an outcome it cannot determine. Each of these is something the provider
/// reports instead of doing the work, so an exchange carrying one and nothing
/// else produced nothing at all.
///
/// It lives here, beside [`TerminalFacts`], because every wire needs the same
/// vocabulary: one classifier per protocol reading its own spelling is how the
/// same condition ends up definite on one route and unknown on another.
// Deliberately exhaustive: every consumer classifies on this, so a new refusal
// must break each classifier rather than fall into a wildcard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRefusal {
    /// The provider refused because a rate limit applied.
    RateLimit,
    /// The provider refused because it was overloaded, unavailable, or its edge
    /// could not reach the upstream it fronts.
    Overloaded,
    /// The exchange did not complete inside its own deadline.
    Deadline,
}

impl ProviderRefusal {
    /// Classifies one provider HTTP status, when it names a refusal.
    ///
    /// `429` is a rate limit. Every `5xx` is an overload, which includes the
    /// edge-origin statuses a gateway invents for an upstream it could not
    /// reach — Cloudflare's `520` through `527` — because those say exactly the
    /// same thing: the provider did not do the work and said why.
    #[must_use]
    pub const fn from_http_status(status: u16) -> Option<Self> {
        match status {
            429 => Some(Self::RateLimit),
            408 | 504 => Some(Self::Deadline),
            500..=599 => Some(Self::Overloaded),
            _ => None,
        }
    }

    /// The closed class this refusal names in operator text.
    ///
    /// These are the classes a terminal failure is typed as (`RateLimited`,
    /// `Unavailable`, `Timeout`), so a diagnostic and its typed kind agree.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RateLimit => "rate limited",
            Self::Overloaded => "unavailable",
            Self::Deadline => "timeout",
        }
    }
}

/// How an exchange broke when the provider named no refusal.
///
/// The kernel that owns the exchange stamps this; a classifier reads it
/// instead of parsing diagnostic text. It names the break, not its outcome:
/// whether a break is definite is decided by [`TerminalFacts::failure_risk`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExchangeBreak {
    /// The connection failed, reset, stalled, or ended before the terminal.
    Transport,
    /// A frame, event, or header violated the protocol.
    #[default]
    Protocol,
}

/// The strongest risk a failed logical operation leaves behind.
///
/// This is the one rule every dialect applies to a failure it had to infer:
/// only a possible provider-hosted effect leaves an outcome nobody can
/// determine. A break after acceptance with no such effect is a definite
/// failure; nothing may replay it automatically because the provider may have
/// run and billed the inference, but there is nothing to reconcile either.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureRisk {
    /// The provider never accepted the request.
    None,
    /// The provider may have run (and billed) the inference; no hosted effect.
    DuplicateInferenceOrBilling,
    /// A provider-hosted effect may have started.
    HostedEffect,
}

/// Whether the logical operation completed successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalOutcome {
    /// The logical operation did not complete successfully.
    NotSuccessful,
    /// Terminal success was sealed for this logical operation.
    Success,
}

/// Retry- and effect-relevant terminal facts for one Responses-family operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFacts {
    summary: super::summary::AttemptSummary,
    physical_attempts: u32,
    provider_acceptance: ProviderAcceptance,
    visible_output: bool,
    replay_safety: ReplaySafety,
    hosted_effects_possible: bool,
    logical_outcome: LogicalOutcome,
}

impl TerminalFacts {
    /// Attaches transient diagnostic accounting without changing retry authority.
    #[must_use]
    pub const fn with_summary(mut self, summary: super::summary::AttemptSummary) -> Self {
        self.summary = summary;
        self
    }

    /// Returns transient accounting for terminal diagnostic presentation.
    #[must_use]
    pub const fn summary(self) -> super::summary::AttemptSummary {
        self.summary
    }

    /// Builds facts from a ledger observation snapshot.
    #[must_use]
    pub const fn from_snapshot(snapshot: ObservationSnapshot) -> Self {
        Self {
            summary: super::summary::AttemptSummary {
                physical: snapshot.physical_attempts,
                ride_out_physical: 0,
                last_point: None,
                last_phase: None,
                proven_unsent: false,
                decline: None,
                last_refusal: None,
            },
            physical_attempts: snapshot.physical_attempts,
            provider_acceptance: snapshot.provider_acceptance,
            visible_output: snapshot.visible_output,
            replay_safety: snapshot.replay_safety,
            hosted_effects_possible: snapshot.hosted_effects_possible,
            logical_outcome: if snapshot.logical_success {
                LogicalOutcome::Success
            } else {
                LogicalOutcome::NotSuccessful
            },
        }
    }

    /// Zeroed pre-acceptance facts for deterministic tests.
    #[must_use]
    pub const fn preacceptance_test() -> Self {
        Self {
            summary: super::summary::AttemptSummary {
                physical: 0,
                ride_out_physical: 0,
                last_point: None,
                last_phase: None,
                proven_unsent: false,
                decline: None,
                last_refusal: None,
            },
            physical_attempts: 0,
            provider_acceptance: ProviderAcceptance::NotAccepted,
            visible_output: false,
            replay_safety: ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_outcome: LogicalOutcome::NotSuccessful,
        }
    }

    /// Returns the number of physical requests owned by this logical call.
    #[must_use]
    pub const fn physical_attempts(self) -> u32 {
        self.physical_attempts
    }

    /// Returns the conservative provider-acceptance state.
    #[must_use]
    pub const fn provider_acceptance(self) -> ProviderAcceptance {
        self.provider_acceptance
    }

    /// Reports whether any physical request was accepted by the provider.
    #[must_use]
    pub const fn provider_accepted(self) -> bool {
        matches!(self.provider_acceptance, ProviderAcceptance::Accepted)
    }

    /// Reports whether provider output became visible to the caller.
    #[must_use]
    pub const fn visible_output(self) -> bool {
        self.visible_output
    }

    /// Returns what replaying the immutable request can cost.
    #[must_use]
    pub const fn replay_safety(self) -> ReplaySafety {
        self.replay_safety
    }

    /// Reports whether an opaque provider-hosted effect may have started.
    #[must_use]
    pub const fn hosted_effects_possible(self) -> bool {
        self.hosted_effects_possible
    }

    /// Returns the risk a failure with these facts leaves behind.
    #[must_use]
    pub const fn failure_risk(self) -> FailureRisk {
        if self.hosted_effects_possible {
            FailureRisk::HostedEffect
        } else if matches!(self.provider_acceptance, ProviderAcceptance::NotAccepted) {
            FailureRisk::None
        } else {
            FailureRisk::DuplicateInferenceOrBilling
        }
    }

    /// Returns the safest retry classification supported by these facts.
    #[must_use]
    pub const fn retry_class(self) -> RetryClass {
        if matches!(self.logical_outcome, LogicalOutcome::Success) {
            return RetryClass::Never;
        }
        match self.provider_acceptance {
            ProviderAcceptance::NotAccepted => RetryClass::BeforeAcceptance,
            ProviderAcceptance::PossiblyAccepted | ProviderAcceptance::Accepted
                if self.visible_output
                    || matches!(self.replay_safety, ReplaySafety::ServerEffect)
                    || self.hosted_effects_possible =>
            {
                RetryClass::Never
            }
            ProviderAcceptance::PossiblyAccepted | ProviderAcceptance::Accepted => {
                RetryClass::OperationOwnedDuplicateRisk
            }
        }
    }
}

/// Acknowledged cooperative cancellation with admission-sensitive facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancellation {
    facts: Option<TerminalFacts>,
}

impl Cancellation {
    /// Builds a cancellation outcome with optional post-admission facts.
    #[must_use]
    pub const fn new(facts: Option<TerminalFacts>) -> Self {
        Self { facts }
    }

    /// Returns terminal facts when cancellation followed service admission.
    #[must_use]
    pub const fn facts(self) -> Option<TerminalFacts> {
        self.facts
    }

    /// Cancellation is never independently retryable.
    #[must_use]
    pub const fn retry_class(self) -> RetryClass {
        RetryClass::Never
    }
}

/// A safe classification for a terminal provider outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProviderFailure {
    /// Provider authentication failed.
    Authentication,
    /// Provider quota was exhausted.
    Quota,
    /// Provider rate limiting was reported.
    RateLimited,
    /// Provider service failure was reported.
    Server,
    /// The response ended explicitly incomplete.
    Incomplete,
    /// An unclassified terminal provider failure occurred.
    Unknown,
}
