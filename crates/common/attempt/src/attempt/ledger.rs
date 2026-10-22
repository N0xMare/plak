//! Shared physical-request observation ledger for Responses-family attempts.
//!
//! Dialects own public error enums, event classifiers, paid recovery, and
//! Tower service orchestration. This module owns only the fail-closed
//! admission / acceptance / visibility / hosted-effect observation state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::facts::{PhysicalRequestOutcome, ProviderAcceptance};
use super::retry::ReplaySafety;

/// Contradictory or out-of-order physical-request lifecycle observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AttemptObservationError {
    /// A second physical request was started before the active request terminated.
    #[error("a physical request is already active")]
    PhysicalRequestAlreadyActive,
    /// A terminal request observation arrived without an active physical request.
    #[error("no physical request is active")]
    NoActivePhysicalRequest,
    /// A lifecycle observation arrived after the logical operation was sealed.
    #[error("the logical operation is already terminal")]
    ObservationAfterLogicalTerminal,
    /// A non-acceptance finish contradicted acceptance of the active request.
    #[error("the physical-request finish contradicts its accepted state")]
    FinishContradictsActiveAcceptance,
    /// A server-effect request was declared only after dispatch began.
    #[error("server-effect replay safety must be declared before dispatch")]
    HostedEffectCapabilityAfterDispatch,
    /// The physical-request counter could not be incremented.
    #[error("physical request count overflowed")]
    PhysicalRequestCountOverflow,
    /// Service admission was attempted after caller-visible dispatch observations.
    #[error("service admission cannot inherit a pre-existing physical request")]
    ServiceAdmissionAfterDispatch,
    /// One logical operation cannot enter two simultaneous service calls.
    #[error("operation is already service-admitted")]
    ServiceAlreadyAdmitted,
    /// Physical dispatch was attempted without service admission.
    #[error("physical dispatch requires service admission")]
    ServiceNotAdmitted,
}

/// Failure of auth-slot reservation or generation validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthReservationError {
    /// Service admission has not been granted.
    #[error("attempt is not service-admitted")]
    AttemptNotAdmitted,
    /// No physical request generation is active.
    #[error("no physical request is active")]
    NoActivePhysicalRequest,
    /// The selected auth slot was already reserved for this generation.
    #[error("authorization is already resolved for this generation")]
    AlreadyResolved,
    /// Generation or slot reservation does not match the active request.
    #[error("authorization binding mismatch")]
    BindingMismatch,
    /// The selected auth slot is not enabled by ledger policy.
    #[error("authorization slot is not enabled by ledger policy")]
    SlotNotEnabled,
}

/// Bitmask of authorization reservation slots on an active physical request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuthReservationMask(u8);

impl AuthReservationMask {
    /// Primary route authorization (platform / single-slot dialects).
    pub const PRIMARY: Self = Self(0b0000_0001);
    /// Secondary route authorization (subscription dual-auth dialects).
    pub const SECONDARY: Self = Self(0b0000_0010);

    /// Reports whether every bit of `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the union of two masks.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Conservative retry- and effect-relevant observation snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationSnapshot {
    /// Number of physical requests started by this logical operation.
    pub physical_attempts: u32,
    /// Aggregate provider acceptance including any unresolved active request.
    pub provider_acceptance: ProviderAcceptance,
    /// Whether caller-visible output was observed after successful emit.
    pub visible_output: bool,
    /// What replaying the immutable request can cost, as declared before dispatch.
    pub replay_safety: ReplaySafety,
    /// Whether a hosted effect may have started (observed or capable+accepted).
    pub hosted_effects_possible: bool,
    /// Whether terminal success sealed a successful logical completion.
    pub logical_success: bool,
}

/// Policy knobs for family-specific physical-attempt observation laws.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerPolicy {
    /// When true, physical dispatch requires prior [`PhysicalRequestLedger::admit_service`].
    pub require_service_admitted_for_dispatch: bool,
    /// When true, event/accept/terminal observations always require an active request.
    pub require_active_request_always: bool,
    /// When true, secondary auth slots may be reserved independently of primary.
    pub dual_auth_slots: bool,
    /// When true, [`PhysicalRequestLedger::terminal_success`] clears service admission.
    pub clear_service_on_terminal_success: bool,
}

impl LedgerPolicy {
    /// Messages-family: service-gated dispatch, one authorization, terminal close.
    pub const MESSAGES_FAMILY: Self = Self {
        require_service_admitted_for_dispatch: true,
        require_active_request_always: false,
        dual_auth_slots: false,
        clear_service_on_terminal_success: true,
    };

    /// xAI / OpenRouter style: service-gated dispatch, single auth slot.
    pub const RESPONSES_FAMILY: Self = Self {
        require_service_admitted_for_dispatch: true,
        require_active_request_always: false,
        dual_auth_slots: false,
        clear_service_on_terminal_success: true,
    };

    /// OpenAI style: dual auth, always-active-request observations.
    pub const OPENAI_STRICT: Self = Self {
        require_service_admitted_for_dispatch: false,
        require_active_request_always: true,
        dual_auth_slots: true,
        clear_service_on_terminal_success: false,
    };
}

/// Fail-closed observation ledger for one logical Responses attempt.
pub struct PhysicalRequestLedger {
    policy: LedgerPolicy,
    admitted: AtomicBool,
    service_admitted: AtomicBool,
    state: Mutex<LedgerState>,
}

impl PhysicalRequestLedger {
    /// Creates an empty ledger for `policy`.
    #[must_use]
    pub fn new(policy: LedgerPolicy) -> Self {
        Self {
            policy,
            admitted: AtomicBool::new(false),
            service_admitted: AtomicBool::new(false),
            state: Mutex::new(LedgerState::default()),
        }
    }

    /// Marks codec-level admission for cancellation-fact presence.
    pub fn mark_admitted(&self) {
        self.admitted.store(true, Ordering::Release);
    }

    /// Reports whether codec or service admission has been observed.
    #[must_use]
    pub fn is_admitted(&self) -> bool {
        self.admitted.load(Ordering::Acquire)
    }

    /// Reports whether the service-admission boundary is currently open.
    #[must_use]
    pub fn is_service_admitted(&self) -> bool {
        self.service_admitted.load(Ordering::Acquire)
    }

    /// Reports whether the logical operation has been sealed terminal.
    #[must_use]
    pub fn is_logically_terminal(&self) -> bool {
        self.lock_state().logical_terminal
    }

    /// Reports whether a physical request generation is currently open.
    #[must_use]
    pub fn has_active_physical_request(&self) -> bool {
        self.lock_state().active_request.is_some()
    }

    /// Opens the private service-admission boundary for one logical call.
    ///
    /// # Errors
    ///
    /// Rejects re-admission and admission after dispatch or terminal sealing.
    pub fn admit_service(&self) -> Result<(), AttemptObservationError> {
        let state = self.lock_state();
        if state.logical_terminal || state.physical_attempts != 0 || state.active_request.is_some()
        {
            return Err(AttemptObservationError::ServiceAdmissionAfterDispatch);
        }
        if self.service_admitted.swap(true, Ordering::AcqRel) {
            return Err(AttemptObservationError::ServiceAlreadyAdmitted);
        }
        self.admitted.store(true, Ordering::Release);
        drop(state);
        Ok(())
    }

    /// Seals service admission and joins any unresolved active request conservatively.
    pub fn close_service(&self) {
        let mut state = self.lock_state();
        self.service_admitted.store(false, Ordering::Release);
        if let Some(active) = state.active_request.take() {
            let unresolved = match active.acceptance {
                ActiveRequestAcceptance::AwaitingAcceptance => ProviderAcceptance::PossiblyAccepted,
                ActiveRequestAcceptance::Accepted => ProviderAcceptance::Accepted,
            };
            state.aggregate_acceptance = state.aggregate_acceptance.join(unresolved);
        }
        state.logical_terminal = true;
    }

    /// Declares that the immutable request can start provider-side effects
    /// ([`ReplaySafety::ServerEffect`]).
    ///
    /// # Errors
    ///
    /// Rejects declarations after dispatch or terminal sealing.
    pub fn declare_server_effect(&self) -> Result<(), AttemptObservationError> {
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        if state.physical_attempts != 0 {
            return Err(AttemptObservationError::HostedEffectCapabilityAfterDispatch);
        }
        if state.hosted_effect == HostedEffectObservation::NotCapable {
            state.hosted_effect = HostedEffectObservation::Capable;
        }
        Ok(())
    }

    /// Starts one physical request generation.
    ///
    /// # Errors
    ///
    /// Rejects pre-admission (when required), overlapping requests, terminal
    /// operations, and generation overflow.
    pub fn physical_request_started(&self) -> Result<(), AttemptObservationError> {
        if self.policy.require_service_admitted_for_dispatch
            && !self.service_admitted.load(Ordering::Acquire)
        {
            return Err(AttemptObservationError::ServiceNotAdmitted);
        }
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        if state.active_request.is_some() {
            return Err(AttemptObservationError::PhysicalRequestAlreadyActive);
        }
        state.physical_attempts = state
            .physical_attempts
            .checked_add(1)
            .ok_or(AttemptObservationError::PhysicalRequestCountOverflow)?;
        // The summary's last classification describes the latest physical
        // request; a new one starts unclassified until its own failure is
        // recorded, so an earlier request's refusal never labels a later end.
        state.summary = super::summary::AttemptSummary {
            ride_out_physical: state.summary.ride_out_physical,
            ..super::summary::AttemptSummary::default()
        };
        state.active_request = Some(ActivePhysicalRequest {
            generation: state.physical_attempts,
            acceptance: ActiveRequestAcceptance::AwaitingAcceptance,
            auth_reserved: AuthReservationMask::default(),
        });
        self.admitted.store(true, Ordering::Release);
        Ok(())
    }

    /// Finishes the active physical request without completing the logical operation.
    ///
    /// # Errors
    ///
    /// Rejects finishes without an active request, after terminal seal, or that
    /// contradict already-proved acceptance.
    pub fn physical_request_finished(
        &self,
        outcome: PhysicalRequestOutcome,
    ) -> Result<(), AttemptObservationError> {
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        let Some(active) = state.active_request else {
            return Err(AttemptObservationError::NoActivePhysicalRequest);
        };
        match (active.acceptance, outcome) {
            (
                ActiveRequestAcceptance::Accepted,
                PhysicalRequestOutcome::DefiniteRejection
                | PhysicalRequestOutcome::AmbiguousPostSend,
            ) => return Err(AttemptObservationError::FinishContradictsActiveAcceptance),
            (_, PhysicalRequestOutcome::DefiniteRejection) => {}
            (_, PhysicalRequestOutcome::AmbiguousPostSend) => {
                state.aggregate_acceptance = state
                    .aggregate_acceptance
                    .join(ProviderAcceptance::PossiblyAccepted);
            }
            (_, PhysicalRequestOutcome::AcceptedDisconnect) => {
                state.aggregate_acceptance = ProviderAcceptance::Accepted;
            }
        }
        state.active_request = None;
        Ok(())
    }

    /// Records response headers or protocol evidence proving provider acceptance.
    ///
    /// # Errors
    ///
    /// Rejects the observation unless a physical request is active (always under
    /// OpenAI strict; only while service-admitted under Responses-family).
    pub fn provider_accepted(&self) -> Result<(), AttemptObservationError> {
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        self.require_active_for_observation(&state)?;
        if let Some(active) = state.active_request.as_mut() {
            active.acceptance = ActiveRequestAcceptance::Accepted;
        }
        state.aggregate_acceptance = ProviderAcceptance::Accepted;
        Ok(())
    }

    /// Reserves an authorization slot for the active physical generation.
    ///
    /// # Errors
    ///
    /// Rejects when not service-admitted, no request is active, the slot is
    /// already reserved, or the slot is disabled by policy.
    pub fn reserve_auth(&self, slot: AuthReservationMask) -> Result<u32, AuthReservationError> {
        self.validate_slot_policy(slot)?;
        if !self.service_admitted.load(Ordering::Acquire) {
            return Err(AuthReservationError::AttemptNotAdmitted);
        }
        let mut state = self.lock_state();
        let Some(active) = state.active_request.as_mut() else {
            return Err(AuthReservationError::NoActivePhysicalRequest);
        };
        if active.auth_reserved.contains(slot) {
            return Err(AuthReservationError::AlreadyResolved);
        }
        active.auth_reserved = active.auth_reserved.union(slot);
        Ok(active.generation)
    }

    /// Validates a previously reserved authorization generation and slot.
    ///
    /// # Errors
    ///
    /// Rejects when not service-admitted, no request is active, or the
    /// generation/slot reservation does not match.
    pub fn validate_auth(
        &self,
        generation: u32,
        slot: AuthReservationMask,
    ) -> Result<(), AuthReservationError> {
        self.validate_slot_policy(slot)?;
        if !self.service_admitted.load(Ordering::Acquire) {
            return Err(AuthReservationError::AttemptNotAdmitted);
        }
        let state = self.lock_state();
        let Some(active) = state.active_request else {
            return Err(AuthReservationError::NoActivePhysicalRequest);
        };
        if active.generation != generation || !active.auth_reserved.contains(slot) {
            return Err(AuthReservationError::BindingMismatch);
        }
        Ok(())
    }

    /// Marks the active request accepted without changing hosted-effect state.
    ///
    /// Used by event/response classifiers after protocol evidence arrives.
    ///
    /// # Errors
    ///
    /// Same active-request rules as [`Self::provider_accepted`].
    pub fn mark_active_accepted(&self) -> Result<(), AttemptObservationError> {
        self.provider_accepted()
    }

    /// Records that a provider-hosted effect was observed on the wire.
    ///
    /// # Errors
    ///
    /// Rejects observations after terminal seal or without an active request
    /// when policy requires one.
    pub fn observe_hosted_effect(&self) -> Result<(), AttemptObservationError> {
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        self.require_active_for_observation(&state)?;
        state.hosted_effect = HostedEffectObservation::Observed;
        Ok(())
    }

    /// Records caller-visible output only after a successful sink emit.
    pub fn observe_after_emit(&self, visible_output: bool) {
        if visible_output {
            self.mark_visible_output();
        }
    }

    /// Marks that provider output became visible to the caller.
    pub fn mark_visible_output(&self) {
        self.lock_state().visible_output = true;
    }

    /// Clears the active physical request without joining an outcome.
    ///
    /// Used only for dialect-specific authorize-rejection paths that must drop
    /// the generation fence without treating the request as finished.
    pub fn abandon_active_request(&self) {
        self.lock_state().active_request = None;
    }

    /// Seals successful terminal completion of the logical operation.
    ///
    /// # Errors
    ///
    /// Rejects after terminal seal or without an active request when required.
    pub fn terminal_success(&self) -> Result<(), AttemptObservationError> {
        let mut state = self.lock_state();
        if state.logical_terminal {
            return Err(AttemptObservationError::ObservationAfterLogicalTerminal);
        }
        self.require_active_for_observation(&state)?;
        state.aggregate_acceptance = ProviderAcceptance::Accepted;
        state.active_request = None;
        state.logical_success = true;
        state.logical_terminal = true;
        if self.policy.clear_service_on_terminal_success {
            self.service_admitted.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Records the last closed failure, the refusal the provider named for
    /// it, and the owning kernel's retry decision.
    pub fn record_failure(
        &self,
        point: super::retry::FailurePoint,
        phase: Option<&'static str>,
        refusal: Option<super::terminal::ProviderRefusal>,
        proven_unsent: bool,
        decline: Option<&'static str>,
    ) {
        let mut state = self.lock_state();
        if point == super::retry::FailurePoint::Unreachable {
            state.summary.ride_out_physical = state.summary.ride_out_physical.saturating_add(1);
        }
        state.summary.last_point = Some(point);
        state.summary.last_phase = phase.and_then(super::summary::AttemptPhase::from_label);
        state.summary.last_refusal = refusal;
        state.summary.proven_unsent = proven_unsent;
        state.summary.decline = decline.and_then(super::summary::AttemptRetryDecline::from_label);
    }

    /// Snapshots transient attempt accounting without changing acceptance authority.
    #[must_use]
    pub fn attempt_summary(&self) -> super::summary::AttemptSummary {
        let state = self.lock_state();
        super::summary::AttemptSummary {
            physical: state.physical_attempts,
            ..state.summary
        }
    }

    /// Snapshots conservative observation facts.
    #[must_use]
    pub fn snapshot(&self) -> ObservationSnapshot {
        let state = self.lock_state();
        snapshot_from_state(&state)
    }

    /// Returns facts when codec/service admission has already occurred.
    #[must_use]
    pub fn cancellation_facts(&self) -> Option<ObservationSnapshot> {
        self.admitted
            .load(Ordering::Acquire)
            .then(|| self.snapshot())
    }

    fn validate_slot_policy(&self, slot: AuthReservationMask) -> Result<(), AuthReservationError> {
        if slot.contains(AuthReservationMask::SECONDARY) && !self.policy.dual_auth_slots {
            return Err(AuthReservationError::SlotNotEnabled);
        }
        if slot.0 == 0 {
            return Err(AuthReservationError::SlotNotEnabled);
        }
        Ok(())
    }

    fn require_active_for_observation(
        &self,
        state: &LedgerState,
    ) -> Result<(), AttemptObservationError> {
        let require = self.policy.require_active_request_always
            || self.service_admitted.load(Ordering::Acquire);
        if require && state.active_request.is_none() {
            return Err(AttemptObservationError::NoActivePhysicalRequest);
        }
        Ok(())
    }

    fn lock_state(&self) -> MutexGuard<'_, LedgerState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for PhysicalRequestLedger {
    fn default() -> Self {
        Self::new(LedgerPolicy::RESPONSES_FAMILY)
    }
}

#[derive(Debug, Default)]
struct LedgerState {
    summary: super::summary::AttemptSummary,
    physical_attempts: u32,
    active_request: Option<ActivePhysicalRequest>,
    aggregate_acceptance: ProviderAcceptance,
    logical_terminal: bool,
    logical_success: bool,
    visible_output: bool,
    hosted_effect: HostedEffectObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRequestAcceptance {
    AwaitingAcceptance,
    Accepted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActivePhysicalRequest {
    generation: u32,
    acceptance: ActiveRequestAcceptance,
    auth_reserved: AuthReservationMask,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HostedEffectObservation {
    #[default]
    NotCapable,
    Capable,
    Observed,
}

fn snapshot_from_state(state: &LedgerState) -> ObservationSnapshot {
    let active_acceptance = match state.active_request.map(|active| active.acceptance) {
        Some(ActiveRequestAcceptance::AwaitingAcceptance) => ProviderAcceptance::PossiblyAccepted,
        Some(ActiveRequestAcceptance::Accepted) => ProviderAcceptance::Accepted,
        None => ProviderAcceptance::NotAccepted,
    };
    let provider_acceptance = state.aggregate_acceptance.join(active_acceptance);
    let server_effect = state.hosted_effect != HostedEffectObservation::NotCapable;
    // Observed, or a server-effect request the provider possibly accepted.
    let hosted_effects_possible = state.hosted_effect == HostedEffectObservation::Observed
        || (server_effect && provider_acceptance != ProviderAcceptance::NotAccepted);
    ObservationSnapshot {
        physical_attempts: state.physical_attempts,
        provider_acceptance,
        visible_output: state.visible_output,
        replay_safety: if server_effect {
            ReplaySafety::ServerEffect
        } else {
            ReplaySafety::CostOnly
        },
        hosted_effects_possible,
        logical_success: state.logical_success,
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
    fn hosted_effects_possible_matches_existing_formula() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::RESPONSES_FAMILY);
        ledger.admit_service().expect("admit");
        ledger.declare_server_effect().expect("server effect");
        assert!(!ledger.snapshot().hosted_effects_possible);

        ledger.physical_request_started().expect("start");
        assert!(ledger.snapshot().hosted_effects_possible);

        ledger.provider_accepted().expect("accept");
        assert!(ledger.snapshot().hosted_effects_possible);
        assert_eq!(ledger.snapshot().replay_safety, ReplaySafety::ServerEffect);
    }

    #[test]
    fn openai_strict_requires_active_request_for_accept() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::OPENAI_STRICT);
        ledger.admit_service().expect("admit");
        assert_eq!(
            ledger.provider_accepted(),
            Err(AttemptObservationError::NoActivePhysicalRequest)
        );
    }

    #[test]
    fn dual_auth_slots_are_independent() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::OPENAI_STRICT);
        ledger.admit_service().expect("admit");
        ledger.physical_request_started().expect("start");
        let generation = ledger
            .reserve_auth(AuthReservationMask::PRIMARY)
            .expect("primary");
        assert_eq!(
            ledger
                .reserve_auth(AuthReservationMask::SECONDARY)
                .expect("secondary"),
            generation
        );
        ledger
            .validate_auth(generation, AuthReservationMask::PRIMARY)
            .expect("primary ok");
        ledger
            .validate_auth(generation, AuthReservationMask::SECONDARY)
            .expect("secondary ok");
    }

    #[test]
    fn auth_reservation_is_one_shot_per_generation_and_slot() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::OPENAI_STRICT);
        assert_eq!(
            ledger.reserve_auth(AuthReservationMask::PRIMARY),
            Err(AuthReservationError::AttemptNotAdmitted)
        );
        ledger.admit_service().expect("admit");
        assert_eq!(
            ledger.reserve_auth(AuthReservationMask::PRIMARY),
            Err(AuthReservationError::NoActivePhysicalRequest)
        );
        ledger.physical_request_started().expect("start");
        let first = ledger
            .reserve_auth(AuthReservationMask::PRIMARY)
            .expect("first primary");
        assert_eq!(first, 1);
        assert_eq!(
            ledger.reserve_auth(AuthReservationMask::PRIMARY),
            Err(AuthReservationError::AlreadyResolved)
        );
        ledger
            .validate_auth(first, AuthReservationMask::PRIMARY)
            .expect("validate first");
        ledger
            .physical_request_finished(PhysicalRequestOutcome::DefiniteRejection)
            .expect("finish");
        ledger.physical_request_started().expect("retry");
        let second = ledger
            .reserve_auth(AuthReservationMask::PRIMARY)
            .expect("second primary");
        assert_eq!(second, 2);
        assert_eq!(
            ledger.validate_auth(first, AuthReservationMask::PRIMARY),
            Err(AuthReservationError::BindingMismatch)
        );
        ledger
            .validate_auth(second, AuthReservationMask::PRIMARY)
            .expect("validate second");
        ledger.close_service();
        assert_eq!(
            ledger.validate_auth(second, AuthReservationMask::PRIMARY),
            Err(AuthReservationError::AttemptNotAdmitted)
        );
    }

    /// A refusal recorded for one physical request never labels a later one.
    #[test]
    fn a_new_physical_request_starts_its_summary_unclassified() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::OPENAI_STRICT);
        ledger.admit_service().expect("admit");
        ledger.physical_request_started().expect("start");
        ledger
            .physical_request_finished(PhysicalRequestOutcome::DefiniteRejection)
            .expect("finish");
        ledger.record_failure(
            super::super::retry::FailurePoint::Refused,
            Some("response_head"),
            Some(super::super::terminal::ProviderRefusal::RateLimit),
            false,
            None,
        );
        assert_eq!(
            ledger.attempt_summary().to_string(),
            " [1 requests; last: rate limited]"
        );
        ledger.physical_request_started().expect("replay");
        assert_eq!(
            ledger.attempt_summary().to_string(),
            " [2 requests; last: unclassified]"
        );
    }

    #[test]
    fn poisoned_mutex_into_inner_preserves_observations() {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::RESPONSES_FAMILY);
        ledger.admit_service().expect("admit");
        ledger.physical_request_started().expect("start");
        // Poison the mutex via a panicking lock holder.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ledger.lock_state();
            panic!("poison ledger mutex");
        }));
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.physical_attempts, 1);
        assert_eq!(
            snapshot.provider_acceptance,
            ProviderAcceptance::PossiblyAccepted
        );
    }
}
