//! A test hook on the actor's exit: a loop failure injected once a root is
//! active, and a hold between the loop's end and the ingress closing.

use crate::{AgentError, AgentErrorKind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::sync::{Mutex, MutexGuard};

/// How an armed probe ends the actor's loop.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LoopFault {
    Error = 1,
    Panic = 2,
}

#[derive(Default)]
pub(crate) struct ExitProbe {
    fault: AtomicU8,
    reached: AtomicBool,
    hold: Mutex<()>,
}

impl ExitProbe {
    /// Ends the loop with `fault` once the next root becomes active.
    pub(crate) fn arm(&self, fault: LoopFault) {
        self.fault.store(fault as u8, Ordering::Release);
    }

    /// Keeps the actor between its loop's end and its ingress closing for as
    /// long as the guard lives.
    pub(crate) async fn hold(&self) -> MutexGuard<'_, ()> {
        self.hold.lock().await
    }

    /// Whether the actor's loop has ended.
    pub(crate) fn reached(&self) -> bool {
        self.reached.load(Ordering::Acquire)
    }

    pub(super) fn strike(&self) -> Result<(), AgentError> {
        match self.fault.swap(0, Ordering::AcqRel) {
            1 => Err(AgentError::new(
                AgentErrorKind::Internal,
                "injected actor loop failure",
            )),
            2 => panic!("injected actor loop panic"),
            _ => Ok(()),
        }
    }

    pub(super) async fn at_exit(&self) {
        self.reached.store(true, Ordering::Release);
        drop(self.hold.lock().await);
    }
}
