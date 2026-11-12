use super::{Driver, PromptCommand};
use crate::runtime::terminal::cancelled_error;
use crate::{ActorControl, AgentError, AgentSessionEndpointError};
use std::sync::Arc;

impl Driver {
    pub(super) async fn next_command(&mut self) -> Result<Option<PromptCommand>, AgentError> {
        if !self.roots.is_empty() {
            self.active_root = self.roots.pop_front();
            return Ok(self.active_root.clone());
        }
        loop {
            let boundary = Arc::clone(&self.boundary.0);
            let changed = boundary.changed.notified();
            let held = boundary.boundary(None, self.authority.checkpoint().id());
            tokio::select! {
                biased;
                () = self.authority.lease_lost() => {
                    return Err(self.authority.heartbeat_failure());
                }
                () = self.shutdown.cancelled() => return Ok(None),
                () = changed => {},
                control = self.control_receiver.recv() => {
                    if let Some(control) = control {
                        Box::pin(self.handle_control(None, control)).await;
                    } else {
                        return Ok(None);
                    }
                }
                () = std::future::ready(()), if self.retention_due.is_some() && !held => {
                    self.retain_due_history_when_idle().await;
                }
            }
            if self.recovery_gated {
                return Ok(None);
            }
            if !self.roots.is_empty() {
                self.active_root = self.roots.pop_front();
                return Ok(self.active_root.clone());
            }
        }
    }

    pub(super) async fn hold_step_boundary(
        &mut self,
        command: &PromptCommand,
    ) -> Result<std::time::Duration, AgentError> {
        let boundary = Arc::clone(&self.boundary.0);
        let mut parked = None;
        loop {
            if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                return Err(cancelled_error());
            }
            let changed = boundary.changed.notified();
            if !boundary.boundary(Some(command.turn_id), self.authority.checkpoint().id()) {
                return Ok(parked.map_or(
                    std::time::Duration::ZERO,
                    |start: crate::executor::Instant| start.elapsed(),
                ));
            }
            parked.get_or_insert_with(crate::executor::now);
            tokio::select! {
                biased;
                () = self.authority.lease_lost() => return Err(self.authority.heartbeat_failure()),
                () = self.shutdown.cancelled() => return Err(cancelled_error()),
                () = command.cancellation.cancelled() => return Err(cancelled_error()),
                () = changed => {},
                control = self.control_receiver.recv() => {
                    let Some(control) = control else { return Err(cancelled_error()) };
                    Box::pin(self.handle_control(Some(command), control)).await;
                }
            }
        }
    }

    pub(super) fn refuse_held_compaction(&self, control: ActorControl) -> Option<ActorControl> {
        if self.boundary.0.requested()
            && let ActorControl::CompactSession { reply, .. } = control
        {
            let _ = reply.send(Err(AgentSessionEndpointError::Busy));
            return None;
        }
        Some(control)
    }
}
