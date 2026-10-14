//! Neutral lease authority time, deliberately separate from UTC observations.

use std::cmp::Ordering;

use crate::session::{SessionStoreError, StoreInvalidReason};

/// Identity of one coherent lease authority timeline.
///
/// The store defines the epoch and scope encodings. Distinct domains have no
/// ordering, and changing either identity never renews an existing grant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeaseDomain {
    epoch: [u8; 16],
    scope: [u8; 32],
}

impl LeaseDomain {
    /// Creates an explicitly identified authority domain.
    #[must_use]
    pub const fn new(epoch: [u8; 16], scope: [u8; 32]) -> Self {
        Self { epoch, scope }
    }

    /// Returns the store-defined epoch identity.
    #[must_use]
    pub const fn epoch(self) -> [u8; 16] {
        self.epoch
    }

    /// Returns the store-defined authority scope.
    #[must_use]
    pub const fn scope(self) -> [u8; 32] {
        self.scope
    }
}

/// A nanosecond-resolution reading in one lease authority domain.
///
/// This is not a Unix timestamp. Comparison is explicitly checked so a reboot
/// or unrelated clock cannot make an old grant appear live.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeaseInstant {
    domain: LeaseDomain,
    seconds: u64,
    nanoseconds: u32,
}

impl LeaseInstant {
    /// Creates a reading with validated fractional seconds.
    ///
    /// # Errors
    /// Returns invalid lease expiry for nanoseconds outside one second.
    pub const fn new(
        domain: LeaseDomain,
        seconds: u64,
        nanoseconds: u32,
    ) -> Result<Self, SessionStoreError> {
        if nanoseconds >= 1_000_000_000 {
            return Err(SessionStoreError::Invalid(StoreInvalidReason::LeaseExpiry));
        }
        Ok(Self {
            domain,
            seconds,
            nanoseconds,
        })
    }

    /// Returns this reading's authority domain.
    #[must_use]
    pub const fn domain(self) -> LeaseDomain {
        self.domain
    }

    /// Returns whole seconds in the authority domain.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        self.seconds
    }

    /// Returns the fractional second in nanoseconds.
    #[must_use]
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    /// Compares readings only when their authority domains agree.
    ///
    /// # Errors
    /// Returns a domain mismatch rather than ordering unrelated clocks.
    pub fn same_domain_cmp(self, other: Self) -> Result<Ordering, SessionStoreError> {
        if self.domain != other.domain {
            return Err(SessionStoreError::LeaseDomainMismatch);
        }
        Ok((self.seconds, self.nanoseconds).cmp(&(other.seconds, other.nanoseconds)))
    }

    /// Adds a lease duration without changing the authority domain.
    ///
    /// # Errors
    /// Returns lease expiry overflow when the deadline cannot be represented.
    pub fn checked_add_seconds(self, seconds: u64) -> Result<Self, SessionStoreError> {
        let seconds = self
            .seconds
            .checked_add(seconds)
            .ok_or(SessionStoreError::Invalid(
                StoreInvalidReason::LeaseExpiryOverflow,
            ))?;
        Ok(Self { seconds, ..self })
    }
}

/// A store-owned coherent non-regressing lease authority clock.
///
/// Every client of one backing store must agree on the domain and readings.
/// An epoch may be replaced only after all prior-epoch owners are incapable of
/// executing; a new random epoch per connection does not satisfy this contract.
/// Identity discovery belongs at construction; reads must perform no file I/O,
/// actor rendezvous, or asynchronous wait. Observation clocks are independent.
pub trait LeaseClock: Send + Sync + 'static {
    /// Reads current authority time, refusing when the clock is unavailable.
    ///
    /// # Errors
    /// Returns a typed store error rather than substituting a different clock.
    fn now(&self) -> Result<LeaseInstant, SessionStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_epochs_and_scopes_cannot_order_grants() -> Result<(), SessionStoreError> {
        let domain = LeaseDomain::new([1; 16], [2; 32]);
        let now = LeaseInstant::new(domain, 80, 999_999_999)?;
        let deadline = now.checked_add_seconds(20)?;
        assert_eq!(now.same_domain_cmp(deadline)?, Ordering::Less);
        assert_eq!(deadline.same_domain_cmp(deadline)?, Ordering::Equal);
        for foreign in [
            LeaseDomain::new([3; 16], [2; 32]),
            LeaseDomain::new([1; 16], [3; 32]),
        ] {
            assert_eq!(
                now.same_domain_cmp(LeaseInstant::new(foreign, 0, 0)?),
                Err(SessionStoreError::LeaseDomainMismatch)
            );
        }
        assert_eq!(deadline.nanoseconds(), 999_999_999);
        assert!(LeaseInstant::new(domain, 0, 1_000_000_000).is_err());
        assert_eq!(
            LeaseInstant::new(domain, u64::MAX, 0)?.checked_add_seconds(1),
            Err(SessionStoreError::Invalid(
                StoreInvalidReason::LeaseExpiryOverflow
            ))
        );
        Ok(())
    }
}
