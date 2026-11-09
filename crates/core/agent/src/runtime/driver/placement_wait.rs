//! Cooperative placement-hold parking before new paid model work.

use super::Driver;
use crate::AgentError;
use crate::runtime::terminal::cancelled_error;

impl Driver {
    /// Waits while this node's runner placement is held, before any new
    /// model or compaction dispatch (spec 01 §5.4): no request slot is kept,
    /// controls are answered, and cancellation, shutdown or a lost lease end
    /// the wait. Release is the placement owner's, after its reconnect
    /// barrier; a refused placement ends the wait too, and its calls refuse.
    /// The wait counts toward no model operation budget.
    pub(super) async fn wait_for_placement(
        &mut self,
        cancellation: plakous_model::CancellationToken,
    ) -> Result<(), AgentError> {
        let Some(mut gate) = self.tools.environment().placement_gate() else {
            return Ok(());
        };
        if !gate.is_held() {
            return Ok(());
        }
        // A held node must not keep another agent's inference waiting.
        self.request_slot.take();
        let active = self.active_root.clone();
        let started = crate::executor::now();
        let result = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(cancelled_error()),
                () = self.shutdown.cancelled() => break Err(cancelled_error()),
                () = self.authority.lease_lost() => break Err(self.authority.heartbeat_failure()),
                () = gate.released() => break Ok(()),
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
}
