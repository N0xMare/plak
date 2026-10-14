//! Shared lifecycle + observation core for Responses-family attempts.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use super::facts::PhysicalRequestOutcome;
use super::ledger::{
    AttemptObservationError, AuthReservationError, AuthReservationMask, LedgerPolicy,
    ObservationSnapshot, PhysicalRequestLedger,
};
use super::lifecycle::{
    ADMITTED, AttemptLifecycleError, DECODED, FAILED, PREPARED, TERMINAL, begin_decoding,
    claim_protocol_failure, is_active, mark_decoded, resume_after_interrupted_stream,
    seal_service_attempt,
};
use super::terminal::TerminalFacts;

/// Failure of a pure family-attempt core transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FamilyCoreError {
    /// Codec/service admission is invalid for the current lifecycle phase.
    #[error("attempt cannot enter admission from its current state")]
    InvalidAdmission,
    /// The attempt has not crossed the required admission boundary.
    #[error("terminal work requires an admitted attempt")]
    NotAdmitted,
    /// Lifecycle phase transition was invalid.
    #[error("attempt cannot commit a terminal response from its current state")]
    InvalidTerminalState,
    /// Physical-request observation failed.
    #[error(transparent)]
    Observation(#[from] AttemptObservationError),
    /// Auth-slot reservation or validation failed.
    #[error(transparent)]
    AuthReservation(#[from] AuthReservationError),
    /// Pure lifecycle transition failed.
    #[error(transparent)]
    Lifecycle(#[from] AttemptLifecycleError),
}

/// Shared lifecycle atom + physical-request ledger for one logical attempt.
pub struct FamilyAttemptCore {
    lifecycle: Arc<AtomicU8>,
    observations: Arc<PhysicalRequestLedger>,
}

#[allow(
    missing_docs,
    reason = "constructor/impl methods documented individually"
)]
impl FamilyAttemptCore {
    /// Creates a prepared core with Responses-family ledger policy.
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_policy(LedgerPolicy::RESPONSES_FAMILY)
    }

    /// Creates a prepared core with an explicit ledger policy.
    ///
    /// OpenAI/Codex must use [`LedgerPolicy::OPENAI_STRICT`] (dual-auth slots,
    /// require-active-request, no clear-service-on-success). Default family
    /// policy softens those fences — never construct OpenAI attempts via
    /// [`Self::new`] alone.
    #[must_use]
    pub fn new_with_policy(policy: LedgerPolicy) -> Self {
        Self {
            lifecycle: Arc::new(AtomicU8::new(PREPARED)),
            observations: Arc::new(PhysicalRequestLedger::new(policy)),
        }
    }

    #[must_use]
    pub fn lifecycle(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.lifecycle)
    }

    #[must_use]
    pub fn lifecycle_ref(&self) -> &Arc<AtomicU8> {
        &self.lifecycle
    }

    #[must_use]
    pub fn observations(&self) -> Arc<PhysicalRequestLedger> {
        Arc::clone(&self.observations)
    }

    #[must_use]
    pub fn observations_ref(&self) -> &PhysicalRequestLedger {
        &self.observations
    }

    /// Marks codec-level admission (`PREPARED → ADMITTED`, idempotent).
    pub fn mark_admitted(&self) -> Result<(), FamilyCoreError> {
        match self.lifecycle.compare_exchange(
            PREPARED,
            ADMITTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.observations.mark_admitted();
                Ok(())
            }
            Err(ADMITTED) => Ok(()),
            Err(_) => Err(FamilyCoreError::InvalidAdmission),
        }
    }

    pub fn ensure_admitted(&self) -> Result<(), FamilyCoreError> {
        if self.lifecycle.load(Ordering::Acquire) == ADMITTED {
            Ok(())
        } else {
            Err(FamilyCoreError::NotAdmitted)
        }
    }

    pub fn ensure_service_admitted(&self) -> Result<(), FamilyCoreError> {
        if self.observations.is_service_admitted() {
            Ok(())
        } else {
            Err(FamilyCoreError::NotAdmitted)
        }
    }

    pub fn mark_service_admitted(&self) -> Result<(), FamilyCoreError> {
        Ok(self.observations.admit_service()?)
    }

    pub fn reserve_primary_auth(&self) -> Result<u32, FamilyCoreError> {
        Ok(self
            .observations
            .reserve_auth(AuthReservationMask::PRIMARY)?)
    }

    pub fn validate_primary_auth(&self, generation: u32) -> Result<(), FamilyCoreError> {
        Ok(self
            .observations
            .validate_auth(generation, AuthReservationMask::PRIMARY)?)
    }

    pub fn declare_server_effect(&self) -> Result<(), FamilyCoreError> {
        Ok(self.observations.declare_server_effect()?)
    }

    pub fn physical_request_started(&self) -> Result<(), FamilyCoreError> {
        Ok(self.observations.physical_request_started()?)
    }

    pub fn physical_request_finished(
        &self,
        outcome: PhysicalRequestOutcome,
    ) -> Result<(), FamilyCoreError> {
        Ok(self.observations.physical_request_finished(outcome)?)
    }

    pub fn observe_provider_accepted(&self) -> Result<(), FamilyCoreError> {
        Ok(self.observations.provider_accepted()?)
    }

    pub fn ensure_active(&self) -> Result<(), FamilyCoreError> {
        if is_active(&self.lifecycle) {
            Ok(())
        } else {
            Err(FamilyCoreError::NotAdmitted)
        }
    }

    pub fn begin_decoding(&self) -> Result<(), FamilyCoreError> {
        Ok(begin_decoding(&self.lifecycle)?)
    }

    /// Closes an accepted generation whose stream broke and returns the
    /// logical attempt to admitted, so a retry may start the next one.
    pub fn interrupt_stream(&self) -> Result<(), FamilyCoreError> {
        self.observations
            .physical_request_finished(PhysicalRequestOutcome::AcceptedDisconnect)?;
        Ok(resume_after_interrupted_stream(&self.lifecycle)?)
    }

    pub fn mark_decoded(&self) -> Result<(), FamilyCoreError> {
        Ok(mark_decoded(&self.lifecycle)?)
    }

    pub fn ensure_decoded(&self) -> Result<(), FamilyCoreError> {
        if self.lifecycle.load(Ordering::Acquire) == DECODED {
            Ok(())
        } else {
            Err(FamilyCoreError::InvalidTerminalState)
        }
    }

    pub fn mark_terminal(&self) -> Result<(), FamilyCoreError> {
        self.lifecycle
            .compare_exchange(DECODED, TERMINAL, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| FamilyCoreError::InvalidTerminalState)
    }

    pub fn mark_failed(&self) {
        self.lifecycle.store(FAILED, Ordering::Release);
        self.observations.close_service();
    }

    #[must_use]
    pub fn claim_and_close_protocol_failure(&self) -> bool {
        if !claim_protocol_failure(&self.lifecycle) {
            return false;
        }
        self.observations.close_service();
        true
    }

    pub fn seal_service(&self) {
        self.observations.close_service();
        seal_service_attempt(&self.lifecycle);
    }

    pub fn abandon_active_request(&self) {
        self.observations.abandon_active_request();
    }

    #[must_use]
    pub fn terminal_facts(&self) -> TerminalFacts {
        TerminalFacts::from_snapshot(self.observations.snapshot())
            .with_summary(self.observations.attempt_summary())
    }

    #[must_use]
    pub fn observation_snapshot(&self) -> ObservationSnapshot {
        self.observations.snapshot()
    }

    pub fn observe_received_event(&self, has_hosted_effect: bool) -> Result<(), FamilyCoreError> {
        self.observations.mark_active_accepted()?;
        if has_hosted_effect {
            self.observations.observe_hosted_effect()?;
        }
        Ok(())
    }

    pub fn observe_terminal_response(
        &self,
        has_visible_output: bool,
        has_hosted_effect: bool,
    ) -> Result<(), FamilyCoreError> {
        self.observations.mark_active_accepted()?;
        self.observations.mark_admitted();
        if has_visible_output {
            self.observations.mark_visible_output();
        }
        if has_hosted_effect {
            self.observations.observe_hosted_effect()?;
        }
        Ok(())
    }

    pub fn observe_after_emit(&self, visible: bool) {
        self.observations.observe_after_emit(visible);
    }

    pub fn terminal_success(&self) -> Result<(), FamilyCoreError> {
        Ok(self.observations.terminal_success()?)
    }
}

impl Default for FamilyAttemptCore {
    fn default() -> Self {
        Self::new()
    }
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
    fn new_defaults_to_responses_family_policy() {
        let core = FamilyAttemptCore::new();
        // Family policy: service-gated dispatch, single auth.
        assert!(core.observations_ref().admit_service().is_ok());
    }

    #[test]
    fn new_with_policy_openai_strict_allows_dual_auth_slots() {
        let core = FamilyAttemptCore::new_with_policy(LedgerPolicy::OPENAI_STRICT);
        // OPENAI_STRICT: dual auth slots after service admit + active request.
        // (Dispatch does not require service admit; auth reservation still does.)
        core.observations_ref()
            .admit_service()
            .expect("service admit");
        core.observations_ref()
            .physical_request_started()
            .expect("physical start");
        let generation = core
            .observations_ref()
            .reserve_auth(AuthReservationMask::PRIMARY)
            .expect("primary auth");
        assert_eq!(
            core.observations_ref()
                .reserve_auth(AuthReservationMask::SECONDARY)
                .expect("secondary auth independent under OPENAI_STRICT"),
            generation
        );
    }
}
