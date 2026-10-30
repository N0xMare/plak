//! Interrupt ladder state machine (spec 02 §5.3).
//!
//! Esc is the interrupt key and **never quits**. Ctrl+C is a ladder that
//! clears a draft, then cancels a turn, then — when a cancel is already in
//! flight and has not settled, or when idle with an empty draft — escalates to
//! a confirmed quit. Quit is also immediate via `Ctrl+Q` or `/quit`.

use std::time::{Duration, Instant};

/// Confirmation window for the escalated Ctrl+C quit (spec 02 §5.3).
pub const QUIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1000);

/// Window in which a second idle Esc clears a non-empty draft (spec 02 §5.3).
pub const ESC_CLEAR_WINDOW: Duration = Duration::from_millis(800);

/// A pending double-tap confirmation with an exact deadline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuitArm {
    deadline: Instant,
}

impl QuitArm {
    /// Arm the escalated-quit confirmation at `now`.
    #[must_use]
    pub fn arm(now: Instant) -> Self {
        Self::arm_for(now, QUIT_CONFIRM_WINDOW)
    }

    /// Arm a confirmation with an explicit window.
    #[must_use]
    pub fn arm_for(now: Instant, window: Duration) -> Self {
        Self {
            deadline: now + window,
        }
    }

    /// Whether a second press at `now` confirms.
    #[must_use]
    pub fn confirms(&self, now: Instant) -> bool {
        now <= self.deadline
    }

    /// Whether the arm has expired.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now > self.deadline
    }
}

/// Outcome of Ctrl+C after modal owners have declined the key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtrlCOutcome {
    /// Non-empty draft → clear it; recoverable by one undo. This is the only effect.
    ClearDraft,
    /// Empty draft with a turn running → ask to stop this turn, in the same
    /// pop-up as a busy `Esc`.
    Interrupt,
    /// A cancel is in flight and unsettled, or idle with an empty draft → first
    /// press arms the quit escalation.
    ArmQuit,
    /// Second press inside the window quits.
    Quit,
}

/// Outcome of Esc after modal, dropdown, secret, search, history-shelf, and
/// selection owners have declined the key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscOutcome {
    /// Mid-turn: cancel this turn immediately — one press, no arming.
    Cancel,
    /// Idle with a non-empty draft: first press arms the clear.
    ArmClearDraft,
    /// Idle with a non-empty draft: second press inside the window clears it.
    ClearDraft,
    /// Nothing to do. Esc never quits.
    Ignore,
}

/// Pure Ctrl+C ladder (spec 02 §5.3).
///
/// 1. non-empty draft → clear (undoable)
/// 2. empty draft + turn running → cancel this turn
/// 3. cancel already in flight and unsettled → arm, then confirm, quit
/// 4. idle + empty draft → arm, then confirm, quit
#[must_use]
pub fn classify_ctrl_c(
    draft_empty: bool,
    busy: bool,
    cancel_pending: bool,
    quit_arm: Option<&QuitArm>,
    now: Instant,
) -> CtrlCOutcome {
    if !draft_empty {
        return CtrlCOutcome::ClearDraft;
    }
    if cancel_pending {
        return match quit_arm {
            Some(arm) if arm.confirms(now) => CtrlCOutcome::Quit,
            _ => CtrlCOutcome::ArmQuit,
        };
    }
    if busy {
        return CtrlCOutcome::Interrupt;
    }
    match quit_arm {
        Some(arm) if arm.confirms(now) => CtrlCOutcome::Quit,
        _ => CtrlCOutcome::ArmQuit,
    }
}

/// Pure Esc ladder (spec 02 §5.3). Esc never quits at any level.
#[must_use]
pub fn classify_esc(
    draft_empty: bool,
    busy: bool,
    cancel_pending: bool,
    clear_arm: Option<&QuitArm>,
    now: Instant,
) -> EscOutcome {
    if busy {
        // A second Esc while the cancel is in flight is a no-op, not an escalation.
        return if cancel_pending {
            EscOutcome::Ignore
        } else {
            EscOutcome::Cancel
        };
    }
    if draft_empty {
        return EscOutcome::Ignore;
    }
    match clear_arm {
        Some(arm) if arm.confirms(now) => EscOutcome::ClearDraft,
        _ => EscOutcome::ArmClearDraft,
    }
}

#[cfg(test)]
mod tests;
