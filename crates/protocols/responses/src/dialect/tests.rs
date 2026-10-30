use super::*;

#[test]
fn openai_profile_matches_platform_defaults() {
    let profile = ResponsesDialectProfile::openai_platform();
    assert_eq!(profile.sequence(), SequencePolicy::RequiredStrict);
    assert_eq!(profile.terminal(), TerminalPolicy::TypedOptionalDone);
    assert_eq!(profile.finish(), FinishPolicy::AutoDispatchResidual);
    assert_eq!(profile.bom(), BomPolicy::Ignore);
}

#[test]
fn xai_profile_matches_typed_only_defaults() {
    let profile = ResponsesDialectProfile::xai();
    assert_eq!(
        profile.sequence(),
        SequencePolicy::OptionalMonotonic {
            fields: XAI_SEQUENCE_FIELDS,
        }
    );
    assert_eq!(profile.terminal(), TerminalPolicy::TypedOnly);
    assert_eq!(profile.finish(), FinishPolicy::BlankLineClosed);
    assert_eq!(profile.bom(), BomPolicy::Ignore);
    assert_eq!(
        decide_done_marker(profile.terminal(), true, false),
        DoneMarkerDecision::Forbidden
    );
    assert!(terminal_finish_ok(profile.terminal(), true, false));
    assert!(!terminal_finish_ok(profile.terminal(), false, false));
}

#[test]
fn openrouter_profile_requires_done_and_strips_bom() {
    let profile = ResponsesDialectProfile::openrouter();
    assert_eq!(
        profile.sequence(),
        SequencePolicy::OptionalMonotonic {
            fields: OPENROUTER_SEQUENCE_FIELDS,
        }
    );
    assert_eq!(profile.terminal(), TerminalPolicy::TypedThenDone);
    assert_eq!(profile.finish(), FinishPolicy::BlankLineClosed);
    assert_eq!(profile.bom(), BomPolicy::Strip);
    assert_eq!(
        decide_done_marker(profile.terminal(), true, false),
        DoneMarkerDecision::Accept
    );
    assert_eq!(
        decide_done_marker(profile.terminal(), true, true),
        DoneMarkerDecision::OutOfOrder
    );
    assert_eq!(
        decide_done_marker(profile.terminal(), false, false),
        DoneMarkerDecision::OutOfOrder
    );
    assert!(!terminal_finish_ok(profile.terminal(), true, false));
    assert!(terminal_finish_ok(profile.terminal(), true, true));
}

#[test]
fn required_strict_accepts_contiguous_from_zero() {
    let mut state = SequenceState::new();
    let policy = SequencePolicy::RequiredStrict;
    validate_next_sequence(policy, &mut state, Some(0)).expect("0");
    validate_next_sequence(policy, &mut state, Some(1)).expect("1");
    validate_next_sequence(policy, &mut state, Some(2)).expect("2");
    assert_eq!(state.expected(), 3);
    assert_eq!(state.last(), Some(2));
}

#[test]
fn required_strict_rejects_missing_gap_and_reorder() {
    let mut state = SequenceState::new();
    let policy = SequencePolicy::RequiredStrict;
    assert_eq!(
        validate_next_sequence(policy, &mut state, None),
        Err(SequenceValidationError::MissingRequired)
    );
    validate_next_sequence(policy, &mut state, Some(0)).expect("0");
    assert_eq!(
        validate_next_sequence(policy, &mut state, Some(2)),
        Err(SequenceValidationError::Gap {
            expected: 1,
            sequence: 2,
        })
    );
    assert_eq!(
        validate_next_sequence(policy, &mut state, Some(0)),
        Err(SequenceValidationError::DuplicateOrReordered {
            previous: 0,
            sequence: 0,
        })
    );
}

#[test]
fn optional_monotonic_allows_absent_or_contiguous() {
    let policy = SequencePolicy::OptionalMonotonic {
        fields: OPENROUTER_SEQUENCE_FIELDS,
    };
    let mut absent = SequenceState::new();
    validate_next_sequence(policy, &mut absent, None).expect("absent");
    validate_next_sequence(policy, &mut absent, None).expect("still absent");
    assert_eq!(absent.presence(), Some(false));

    let mut present = SequenceState::new();
    validate_next_sequence(policy, &mut present, Some(1)).expect("first any");
    validate_next_sequence(policy, &mut present, Some(2)).expect("next");
    assert_eq!(
        validate_next_sequence(policy, &mut present, Some(4)),
        Err(SequenceValidationError::Gap {
            expected: 3,
            sequence: 4,
        })
    );
    assert_eq!(
        validate_next_sequence(policy, &mut present, Some(2)),
        Err(SequenceValidationError::DuplicateOrReordered {
            previous: 2,
            sequence: 2,
        })
    );
}

#[test]
fn optional_monotonic_rejects_presence_flip() {
    let policy = SequencePolicy::OptionalMonotonic {
        fields: XAI_SEQUENCE_FIELDS,
    };
    let mut state = SequenceState::new();
    validate_next_sequence(policy, &mut state, Some(0)).expect("present");
    assert_eq!(
        validate_next_sequence(policy, &mut state, None),
        Err(SequenceValidationError::InconsistentPresence)
    );

    let mut other = SequenceState::new();
    validate_next_sequence(policy, &mut other, None).expect("absent");
    assert_eq!(
        validate_next_sequence(policy, &mut other, Some(1)),
        Err(SequenceValidationError::InconsistentPresence)
    );
}

#[test]
fn typed_optional_done_accepts_after_terminal_only() {
    let policy = TerminalPolicy::TypedOptionalDone;
    assert_eq!(
        decide_done_marker(policy, false, false),
        DoneMarkerDecision::OutOfOrder
    );
    assert_eq!(
        decide_done_marker(policy, true, false),
        DoneMarkerDecision::Accept
    );
    assert_eq!(
        decide_done_marker(policy, true, true),
        DoneMarkerDecision::Accept
    );
    assert!(terminal_finish_ok(policy, true, false));
    assert!(terminal_finish_ok(policy, true, true));
    assert!(!terminal_finish_ok(policy, false, true));
}
