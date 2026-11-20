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
