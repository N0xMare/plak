use super::{Driver, PromptCommand};
use crate::AgentError;
use crate::runtime::terminal::cancelled_error;

impl Driver {
    pub(super) async fn next_command(&mut self) -> Result<Option<PromptCommand>, AgentError> {
        if !self.roots.is_empty() {
            self.active_root = self.roots.pop_front();
            return Ok(self.active_root.clone());
        }
        loop {
            tokio::select! {
                biased;
                () = self.authority.lease_lost() => {
                    return Err(self.authority.heartbeat_failure());
                }
                () = self.shutdown.cancelled() => return Ok(None),
                control = self.control_receiver.recv() => {
                    if let Some(control) = control {
                        Box::pin(self.handle_control(None, control)).await;
                    } else {
                        return Ok(None);
                    }
                }
                () = std::future::ready(()), if self.retention_due.is_some() => {
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

    pub(super) fn check_dispatch_allowed(&self, command: &PromptCommand) -> Result<(), AgentError> {
        if command.cancellation.is_cancelled() || self.shutdown.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }
}
