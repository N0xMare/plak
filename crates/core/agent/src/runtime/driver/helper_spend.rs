//! The per-agent-turn helper budget of the nested-call choke point
//! (spec 01 §9.8.7).
//!
//! A composition that declares a helper budget reserves a callee's declared
//! most physical requests before the child is admitted, and settles the
//! reservation to the count the child reports when it settles. A child that
//! reports no count consumes its whole reservation, since a request that may
//! have reached the provider is never refunded. The count lives in memory for
//! the active agent turn only.

use crate::{AgentError, AgentErrorKind};

/// Helper requests reserved or spent in the active agent turn.
#[derive(Debug, Default)]
pub(super) struct HelperSpend {
    used: u32,
}

impl HelperSpend {
    /// Starts a fresh count for a new agent turn.
    pub(super) fn reset(&mut self) {
        self.used = 0;
    }

    /// Reserves `max` requests against `budget`. A reservation that does not
    /// fit is refused with the definite failure the cell raises.
    pub(super) fn reserve(&mut self, budget: u32, max: u32) -> Result<(), AgentError> {
        match self.used.checked_add(max).filter(|next| *next <= budget) {
            Some(next) => {
                self.used = next;
                Ok(())
            }
            None => Err(AgentError::new(
                AgentErrorKind::Backpressure,
                format!(
                    "helper spend limit reached ({} of {budget} requests this turn)",
                    self.used
                ),
            )),
        }
    }

    /// Returns a reservation whose child was never dispatched.
    pub(super) fn release(&mut self, reserved: u32) {
        self.used = self.used.saturating_sub(reserved);
    }

    /// Settles a dispatched child's reservation to the count it reported, or
    /// keeps all of it when the child reported none.
    pub(super) fn settle(&mut self, reserved: u32, reported: Option<u32>) {
        self.used = self
            .used
            .saturating_sub(reserved)
            .saturating_add(reported.unwrap_or(reserved));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_fit_refuse_and_settle_exactly() {
        let mut spend = HelperSpend::default();
        for _ in 0..8 {
            assert!(spend.reserve(32, 4).is_ok());
        }
        let refused = spend
            .reserve(32, 4)
            .err()
            .map(|error| error.message().to_owned());
        assert_eq!(
            refused.as_deref(),
            Some("helper spend limit reached (32 of 32 requests this turn)")
        );
        spend.settle(4, Some(1));
        spend.settle(4, None);
        spend.release(4);
        assert_eq!(spend.used, 25);
        spend.reset();
        assert_eq!(spend.used, 0);
    }
}
