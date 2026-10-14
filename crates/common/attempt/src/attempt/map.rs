//! Shared observation / auth-reservation error mapping for Responses dialects.

use super::core::FamilyCoreError;
use super::ledger::{AttemptObservationError, AuthReservationError};

/// Dialect auth errors that map from shared auth-reservation failures.
pub trait FamilyAuthError: Sized {
    /// Service admission has not been granted.
    fn attempt_not_admitted() -> Self;
    /// No physical request generation is active.
    fn no_active_physical_request() -> Self;
    /// Authorization already resolved for this generation.
    fn authorization_already_resolved() -> Self;
    /// Generation or slot reservation does not match.
    fn binding_mismatch() -> Self;
}

/// Maps a shared auth-reservation failure into a dialect auth error.
#[must_use]
pub fn map_auth_reservation<E: FamilyAuthError>(error: AuthReservationError) -> E {
    match error {
        AuthReservationError::AttemptNotAdmitted => E::attempt_not_admitted(),
        AuthReservationError::NoActivePhysicalRequest => E::no_active_physical_request(),
        AuthReservationError::AlreadyResolved => E::authorization_already_resolved(),
        AuthReservationError::BindingMismatch | AuthReservationError::SlotNotEnabled => {
            E::binding_mismatch()
        }
    }
}

/// Dialect attempt errors that map from shared observation / core failures.
pub trait FamilyObservationError: Sized {
    /// Auth error embedded in attempt errors.
    type Auth: FamilyAuthError;

    /// A second physical request was started before the active one finished.
    fn physical_request_already_active() -> Self;
    /// A physical request operation was attempted without an active request.
    fn no_active_physical_request() -> Self;
    /// A lifecycle observation arrived after the logical operation was sealed.
    fn observation_after_logical_terminal() -> Self;
    /// A request finish contradicted acceptance already proved.
    fn finish_contradicts_active_acceptance() -> Self;
    /// Hosted-effect capability was declared only after dispatch began.
    fn hosted_effect_capability_after_dispatch() -> Self;
    /// The physical-request generation counter overflowed.
    fn physical_request_count_overflow() -> Self;
    /// Service admission attempted to inherit a pre-existing physical request.
    fn service_admission_after_dispatch() -> Self;
    /// One logical operation cannot enter simultaneous service calls.
    fn service_already_admitted() -> Self;
    /// Physical dispatch was attempted without service admission.
    fn authentication_not_admitted() -> Self;
    /// Invalid lifecycle / terminal phase.
    fn invalid_terminal_state() -> Self;
    /// Invalid admission transition.
    fn invalid_admission() -> Self;
    /// Not admitted for the required boundary.
    fn not_admitted() -> Self;
}

/// Maps a shared observation failure into a dialect attempt error.
#[must_use]
pub fn map_observation<E: FamilyObservationError>(error: AttemptObservationError) -> E {
    match error {
        AttemptObservationError::PhysicalRequestAlreadyActive => {
            E::physical_request_already_active()
        }
        AttemptObservationError::NoActivePhysicalRequest => E::no_active_physical_request(),
        AttemptObservationError::ObservationAfterLogicalTerminal => {
            E::observation_after_logical_terminal()
        }
        AttemptObservationError::FinishContradictsActiveAcceptance => {
            E::finish_contradicts_active_acceptance()
        }
        AttemptObservationError::HostedEffectCapabilityAfterDispatch => {
            E::hosted_effect_capability_after_dispatch()
        }
        AttemptObservationError::PhysicalRequestCountOverflow => {
            E::physical_request_count_overflow()
        }
        AttemptObservationError::ServiceAdmissionAfterDispatch => {
            E::service_admission_after_dispatch()
        }
        AttemptObservationError::ServiceAlreadyAdmitted => E::service_already_admitted(),
        AttemptObservationError::ServiceNotAdmitted => E::authentication_not_admitted(),
    }
}

/// Maps a family-core failure into a dialect attempt error.
#[must_use]
pub fn map_core<E: FamilyObservationError>(error: FamilyCoreError) -> E {
    match error {
        FamilyCoreError::InvalidAdmission => E::invalid_admission(),
        FamilyCoreError::NotAdmitted => E::not_admitted(),
        FamilyCoreError::InvalidTerminalState | FamilyCoreError::Lifecycle(_) => {
            E::invalid_terminal_state()
        }
        FamilyCoreError::Observation(inner) => map_observation(inner),
        FamilyCoreError::AuthReservation(inner) => {
            // Auth failures during core methods surface as attempt auth edges
            // only through reserve/validate helpers; treat as binding mismatch.
            let _ = inner;
            E::invalid_terminal_state()
        }
    }
}
