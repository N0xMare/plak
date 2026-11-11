//! Cooperative placement-hold parking before new paid model work.

use super::Driver;
use crate::runtime::terminal::cancelled_error;
use crate::{AgentError, AgentErrorKind};

impl Driver {
    /// Waits while this node's runner placement is held, before any new
    /// model or compaction dispatch (spec 01 §5.4): no request slot is kept,
    /// controls are answered, and cancellation, shutdown or a lost lease end
    /// the wait. Release is the placement owner's, after its reconnect
    /// barrier; a refused placement ends the wait without paid dispatch.
    /// The wait counts toward no model operation budget.
    pub(super) async fn wait_for_placement(
        &mut self,
        cancellation: plakous_model::CancellationToken,
    ) -> Result<(), AgentError> {
        let Some(mut gate) = self.tools.environment().placement_gate() else {
            return Ok(());
        };
        if !gate.is_held() {
            return placement_admission(&gate);
        }
        // A held node must not keep another agent's inference waiting.
        self.request_slot.take();
        let active = self.active_root.clone();
        let interruption = self.authority.reader();
        let started = crate::executor::now();
        let result = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(cancelled_error()),
                () = self.shutdown.cancelled() => break Err(cancelled_error()),
                () = self.authority.lease_lost() => break Err(self.authority.heartbeat_failure()),
                () = gate.released() => break placement_admission(&gate),
                // A session workflow (an idle compaction) has no turn to
                // cancel: its owner's cooperative interruption stops it.
                () = crate::executor::sleep(std::time::Duration::from_millis(100)), if active.is_none() => {
                    if interruption.cooperative_wait_interrupted() { break Err(cancelled_error()); }
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else { break Err(cancelled_error()); };
                    Box::pin(self.handle_control(active.as_ref(), control)).await;
                }
            }
        };
        self.placement_wait_elapsed = self
            .placement_wait_elapsed
            .saturating_add(started.elapsed());
        result
    }

    /// Whether this node's placement excludes new work, held or refused.
    pub(super) fn placement_unavailable(&self) -> bool {
        self.tools
            .environment()
            .placement_gate()
            .is_some_and(|gate| !gate.is_ready())
    }
}

fn placement_admission(
    gate: &plakous_tools::internal::runner::PlacementGate,
) -> Result<(), AgentError> {
    if gate.is_ready() {
        Ok(())
    } else {
        Err(AgentError::new(
            AgentErrorKind::ExternalBindingUnavailable,
            "execution placement was refused",
        ))
    }
}
