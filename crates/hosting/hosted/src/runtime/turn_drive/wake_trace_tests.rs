//! Which select arm ended the pass that took each queued follow-up, so a
//! test can tell an intent wake from a coalescing deadline. Test builds only.

use std::sync::Mutex;
use std::{cell::RefCell, rc::Rc};

static TAKEN: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

tokio::task_local! {
    pub(in crate::runtime) static PASSES: Rc<std::cell::Cell<usize>>;
    pub(in crate::runtime) static WAITING_ENGINE: Rc<RefCell<WaitingEngine>>;
    pub(in crate::runtime) static MAIL_REPLIES: Rc<std::cell::Cell<usize>>;
}

/// Hold back engine observations once its real wait is running, so none of
/// the tested wakes can ride on a runtime event or terminal result.
pub(in crate::runtime) struct WaitingEngine {
    isolate_next_wait: bool,
    cancel_pending: bool,
    pub(in crate::runtime) control: Option<plakous::agent::TurnControl>,
}

impl WaitingEngine {
    pub(in crate::runtime) fn new(cancel_pending: bool) -> Self {
        Self {
            isolate_next_wait: true,
            cancel_pending,
            control: None,
        }
    }
}

pub(super) fn isolate_wait<P: plakous_host_protocol::ProtocolExtension>(
    turn: &mut super::RootTurn<'_, P>,
    event: &plakous::agent::AgentEvent,
) {
    if matches!(event.kind(), super::AgentEventKind::ToolStarted { identity, .. } if identity.canonical_name().as_str() == "wait_agent")
    {
        let _ = WAITING_ENGINE.try_with(|probe| {
            let mut probe = probe.borrow_mut();
            if std::mem::take(&mut probe.isolate_next_wait) {
                probe.control = Some(turn.control.clone());
                turn.cancel_pending = probe.cancel_pending;
            }
        });
    }
}

pub(super) fn engine_observations_enabled() -> bool {
    WAITING_ENGINE
        .try_with(|probe| probe.borrow().control.is_none())
        .unwrap_or(true)
}

/// The pass after the drive's last wake, on `arm`, admitted the follow-up
/// `text`. The drive itself carries the arm, whichever thread polls it.
pub(super) fn took(text: &str, arm: &'static str) {
    TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((text.to_owned(), arm));
}

/// The wake that preceded the admission of `text`, if it was taken.
pub(in crate::runtime) fn wake_before(text: &str) -> Option<&'static str> {
    TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(taken, _)| taken == text)
        .map(|(_, arm)| *arm)
}
