use super::*;

#[test]
fn ctrl_c_clears_draft_before_anything_else() {
    let now = Instant::now();
    assert_eq!(
        classify_ctrl_c(false, true, false, None, now),
        CtrlCOutcome::ClearDraft
    );
    assert_eq!(
        classify_ctrl_c(false, false, false, None, now),
        CtrlCOutcome::ClearDraft
    );
    // Even with a cancel in flight, a draft still wins.
    assert_eq!(
        classify_ctrl_c(false, true, true, None, now),
        CtrlCOutcome::ClearDraft
    );
}

#[test]
fn ctrl_c_empty_and_busy_cancels_the_turn() {
    let now = Instant::now();
    assert_eq!(
        classify_ctrl_c(true, true, false, None, now),
        CtrlCOutcome::Interrupt
    );
}

#[test]
fn ctrl_c_escalates_to_quit_only_on_a_stuck_cancel() {
    let now = Instant::now();
    assert_eq!(
        classify_ctrl_c(true, true, true, None, now),
        CtrlCOutcome::ArmQuit
    );
    let arm = QuitArm::arm(now);
    assert_eq!(
        classify_ctrl_c(
            true,
            true,
            true,
            Some(&arm),
            now + Duration::from_millis(500)
        ),
        CtrlCOutcome::Quit
    );
    assert_eq!(
        classify_ctrl_c(
            true,
            true,
            true,
            Some(&arm),
            now + Duration::from_millis(1500)
        ),
        CtrlCOutcome::ArmQuit
    );
}

#[test]
fn ctrl_c_idle_and_empty_double_tap_quits() {
    let now = Instant::now();
    assert_eq!(
        classify_ctrl_c(true, false, false, None, now),
        CtrlCOutcome::ArmQuit
    );
    let arm = QuitArm::arm(now);
    assert_eq!(
        classify_ctrl_c(
            true,
            false,
            false,
            Some(&arm),
            now + Duration::from_millis(500)
        ),
        CtrlCOutcome::Quit
    );
    assert_eq!(
        classify_ctrl_c(
            true,
            false,
            false,
            Some(&arm),
            now + Duration::from_millis(1500)
        ),
        CtrlCOutcome::ArmQuit
    );
}

#[test]
fn esc_cancels_mid_turn_once_then_ignores() {
    let now = Instant::now();
    assert_eq!(
        classify_esc(true, true, false, None, now),
        EscOutcome::Cancel
    );
    assert_eq!(
        classify_esc(false, true, false, None, now),
        EscOutcome::Cancel
    );
    assert_eq!(
        classify_esc(true, true, true, None, now),
        EscOutcome::Ignore
    );
}

#[test]
fn esc_esc_clears_an_idle_draft_inside_the_window() {
    let now = Instant::now();
    assert_eq!(
        classify_esc(false, false, false, None, now),
        EscOutcome::ArmClearDraft
    );
    let arm = QuitArm::arm_for(now, ESC_CLEAR_WINDOW);
    assert_eq!(
        classify_esc(
            false,
            false,
            false,
            Some(&arm),
            now + Duration::from_millis(400)
        ),
        EscOutcome::ClearDraft
    );
    assert_eq!(
        classify_esc(
            false,
            false,
            false,
            Some(&arm),
            now + Duration::from_millis(900)
        ),
        EscOutcome::ArmClearDraft
    );
}

#[test]
fn esc_never_quits() {
    let now = Instant::now();
    let arm = QuitArm::arm(now);
    // Idle, empty draft, armed or not: nothing happens.
    assert_eq!(
        classify_esc(true, false, false, None, now),
        EscOutcome::Ignore
    );
    assert_eq!(
        classify_esc(true, false, false, Some(&arm), now),
        EscOutcome::Ignore
    );
}
