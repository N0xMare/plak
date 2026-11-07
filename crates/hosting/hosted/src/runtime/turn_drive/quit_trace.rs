//! Which quit branch each session took and the order of its durable writes,
//! plus injected cancel refusals and join expiry. Test builds only.

use std::sync::{Mutex, PoisonError};

static STEPS: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());
static CANCEL_REFUSALS: Mutex<Vec<(String, plakous::agent::ControlError)>> = Mutex::new(Vec::new());
static EXPIRED_JOINS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Resume the next quit join with its result settled and its deadline expired.
pub(in crate::runtime) fn expire_next_join(session: &str) {
    EXPIRED_JOINS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(session.to_owned());
}

pub(super) fn take_expired_join(session: &str) -> bool {
    let mut joins = EXPIRED_JOINS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = joins.iter().position(|id| id == session) {
        joins.remove(index);
        return true;
    }
    false
}

/// The quit path of `session` reached `step`.
pub(super) fn step(session: &str, step: &'static str) {
    STEPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((session.to_owned(), step));
}

/// The quit steps `session` reached, in order.
pub(in crate::runtime) fn steps(session: &str) -> Vec<&'static str> {
    STEPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(stepped, _)| stepped == session)
        .map(|(_, step)| *step)
        .collect()
}

/// The next interrupt of `session` is refused at ingress with `error`.
pub(in crate::runtime) fn refuse_next_cancel(session: &str, error: plakous::agent::ControlError) {
    CANCEL_REFUSALS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((session.to_owned(), error));
}

pub(super) fn take_cancel_refusal(session: &str) -> Option<plakous::agent::ControlError> {
    let mut refusals = CANCEL_REFUSALS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let index = refusals
        .iter()
        .position(|(refused, _)| refused == session)?;
    Some(refusals.remove(index).1)
}
