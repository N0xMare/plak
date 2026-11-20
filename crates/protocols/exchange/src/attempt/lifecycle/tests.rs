use super::*;

#[test]
fn happy_path_admitted_decode_terminal_phases() {
    let lifecycle = AtomicU8::new(PREPARED);
    assert!(!is_active(&lifecycle));
    assert!(!is_decoding(&lifecycle));

    lifecycle
        .compare_exchange(PREPARED, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
        .expect("prepared → admitted");
    assert!(is_active(&lifecycle));
    assert!(!is_decoding(&lifecycle));

    begin_decoding(&lifecycle).expect("admitted → decoding");
    assert!(is_active(&lifecycle));
    assert!(is_decoding(&lifecycle));

    mark_decoded(&lifecycle).expect("decoding → decoded");
    assert!(!is_active(&lifecycle));
    assert!(!is_decoding(&lifecycle));
    assert_eq!(lifecycle.load(Ordering::Acquire), DECODED);

    lifecycle
        .compare_exchange(DECODED, TERMINAL, Ordering::AcqRel, Ordering::Acquire)
        .expect("decoded → terminal");
    seal_service_attempt(&lifecycle);
    assert_eq!(
        lifecycle.load(Ordering::Acquire),
        TERMINAL,
        "terminal must not be overwritten by seal"
    );
}

#[test]
fn begin_decoding_rejects_non_admitted() {
    let lifecycle = AtomicU8::new(PREPARED);
    assert_eq!(
        begin_decoding(&lifecycle),
        Err(AttemptLifecycleError::InvalidTerminalState)
    );
    lifecycle.store(DECODING, Ordering::Release);
    assert_eq!(
        begin_decoding(&lifecycle),
        Err(AttemptLifecycleError::InvalidTerminalState)
    );
    lifecycle.store(FAILED, Ordering::Release);
    assert_eq!(
        begin_decoding(&lifecycle),
        Err(AttemptLifecycleError::InvalidTerminalState)
    );
}

#[test]
fn mark_decoded_requires_decoding() {
    let lifecycle = AtomicU8::new(ADMITTED);
    assert_eq!(
        mark_decoded(&lifecycle),
        Err(AttemptLifecycleError::InvalidTerminalState)
    );
    lifecycle.store(DECODING, Ordering::Release);
    mark_decoded(&lifecycle).expect("decoding → decoded");
    assert_eq!(
        mark_decoded(&lifecycle),
        Err(AttemptLifecycleError::InvalidTerminalState)
    );
}

#[test]
fn mark_decode_failed_only_from_decoding() {
    let lifecycle = AtomicU8::new(ADMITTED);
    mark_decode_failed(&lifecycle);
    assert_eq!(lifecycle.load(Ordering::Acquire), ADMITTED);

    lifecycle.store(DECODING, Ordering::Release);
    mark_decode_failed(&lifecycle);
    assert_eq!(lifecycle.load(Ordering::Acquire), DECODE_FAILED);
}

#[test]
fn claim_protocol_failure_from_active_and_decode_failed() {
    for start in [ADMITTED, DECODING, DECODE_FAILED] {
        let lifecycle = AtomicU8::new(start);
        assert!(claim_protocol_failure(&lifecycle));
        assert_eq!(lifecycle.load(Ordering::Acquire), FAILED);
        assert!(!claim_protocol_failure(&lifecycle));
    }

    for start in [PREPARED, DECODED, FAILED, TERMINAL] {
        let lifecycle = AtomicU8::new(start);
        assert!(!claim_protocol_failure(&lifecycle));
        assert_eq!(lifecycle.load(Ordering::Acquire), start);
    }
}

#[test]
fn seal_service_attempt_forces_failed_except_terminal() {
    for start in [PREPARED, ADMITTED, DECODING, DECODED, DECODE_FAILED] {
        let lifecycle = AtomicU8::new(start);
        seal_service_attempt(&lifecycle);
        assert_eq!(lifecycle.load(Ordering::Acquire), FAILED);
    }

    let failed = AtomicU8::new(FAILED);
    seal_service_attempt(&failed);
    assert_eq!(failed.load(Ordering::Acquire), FAILED);

    let terminal = AtomicU8::new(TERMINAL);
    seal_service_attempt(&terminal);
    assert_eq!(terminal.load(Ordering::Acquire), TERMINAL);
}
