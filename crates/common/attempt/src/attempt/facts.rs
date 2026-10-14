//! Pure acceptance / physical-request outcome facts for Responses attempts.

/// Conservative provider acceptance across physical request generations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProviderAcceptance {
    /// Every finished physical request was definitely rejected before acceptance.
    #[default]
    NotAccepted,
    /// A transmitted request may have been accepted, but no proof arrived.
    PossiblyAccepted,
    /// Response headers or protocol events prove provider acceptance.
    Accepted,
}

impl ProviderAcceptance {
    /// Joins two acceptance observations conservatively.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Accepted, _) | (_, Self::Accepted) => Self::Accepted,
            (Self::PossiblyAccepted, _) | (_, Self::PossiblyAccepted) => Self::PossiblyAccepted,
            (Self::NotAccepted, Self::NotAccepted) => Self::NotAccepted,
        }
    }
}

/// How an active physical request finished without completing the operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PhysicalRequestOutcome {
    /// The request was definitely rejected before provider acceptance.
    DefiniteRejection,
    /// Transmission occurred, but acceptance could not be proved or disproved.
    AmbiguousPostSend,
    /// The provider accepted the request before the response disconnected.
    AcceptedDisconnect,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[test]
    fn provider_acceptance_join_is_conservative() {
        assert_eq!(
            ProviderAcceptance::NotAccepted.join(ProviderAcceptance::NotAccepted),
            ProviderAcceptance::NotAccepted
        );
        assert_eq!(
            ProviderAcceptance::NotAccepted.join(ProviderAcceptance::PossiblyAccepted),
            ProviderAcceptance::PossiblyAccepted
        );
        assert_eq!(
            ProviderAcceptance::PossiblyAccepted.join(ProviderAcceptance::Accepted),
            ProviderAcceptance::Accepted
        );
        assert_eq!(
            ProviderAcceptance::Accepted.join(ProviderAcceptance::NotAccepted),
            ProviderAcceptance::Accepted
        );
    }
}
