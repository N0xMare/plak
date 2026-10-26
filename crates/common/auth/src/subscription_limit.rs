//! Classified native-request dispatch over the neutral request gate.

pub use plakous_model::subscription_limit::*;

/// Per-physical-attempt catalog authorization and trusted unsent refunds.
#[cfg(feature = "http-refresh")]
pub struct CatalogRequestDispatch {
    gate: Option<std::sync::Arc<dyn SubscriptionRequestGate>>,
    permit: Option<Box<dyn SubscriptionRequestPermit>>,
}

#[cfg(feature = "http-refresh")]
impl CatalogRequestDispatch {
    /// Uses the caller's existing catalog allowance; no gate means unmetered maintenance.
    #[must_use]
    pub fn new(gate: Option<std::sync::Arc<dyn SubscriptionRequestGate>>) -> Self {
        Self { gate, permit: None }
    }

    /// `None` authorizes the next dispatch; `Some` settles a transport failure.
    /// Only trusted `NotStarted` proof refunds a spent authorization.
    ///
    /// # Errors
    /// Propagates denied acquisition/authorization or unavailable refund accounting.
    pub fn observe(
        &mut self,
        failure: Option<plakous_http::TransmissionFact>,
    ) -> Result<(), SubscriptionLimitError> {
        if let Some(failure) = failure {
            if failure == plakous_http::TransmissionFact::NotStarted
                && let Some(permit) = &mut self.permit
            {
                permit.refund_not_started()?;
            }
        } else {
            self.permit = None;
            if let Some(gate) = &self.gate {
                let mut permit = gate.acquire(SubscriptionRequestClass::Catalog)?;
                permit.authorize_dispatch()?;
                self.permit = Some(permit);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "http-refresh"))]
#[allow(
    clippy::expect_used,
    reason = "test constants and accounting assertions"
)]
mod catalog_dispatch_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn catalog_attempts_keep_fresh_permits_and_only_refund_not_started() {
        let gate = Arc::new(InMemorySubscriptionRequestGate::new(
            SubscriptionRequestLimits::new(0, 3, 0, 0, None, 0).expect("limits"),
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let mut dispatch = CatalogRequestDispatch::new(Some(gate.clone()));
        dispatch.observe(None).expect("first permit");
        dispatch
            .observe(Some(plakous_http::TransmissionFact::NotStarted))
            .expect("refund");
        dispatch.observe(None).expect("fresh permit");
        dispatch
            .observe(Some(plakous_http::TransmissionFact::MayHaveTransmitted))
            .expect("sent");
        let state = gate
            .snapshot()
            .expect("snapshot")
            .class(SubscriptionRequestClass::Catalog);
        assert_eq!(state.reserved(), 1);
        assert_eq!(state.dispatched(), 1);
        assert_eq!(
            gate.dispatch_attempts(SubscriptionRequestClass::Catalog)
                .expect("attempts"),
            2
        );
    }
}
