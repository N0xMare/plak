//! Pre-dispatch checkpoints: finish the current receipt, never start a turn after stop.

use super::{CliResult, ContentEvent, HostLoop, InterruptWatch};
use crate::interrupt::run_watched;
use plakous::application::{ApplicationExecutor, ApplicationExecutorError};
use std::future::Future;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Refresh,
    Settle,
    Tree,
}

pub(super) fn run_step<F: Future, P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    stage: Stage,
    future: F,
) -> Result<F::Output, ApplicationExecutorError> {
    let _ = stage;
    #[cfg(test)]
    let future = async {
        if let Some(hook) = hooks::HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|(selected, _)| *selected == stage)
            {
                slot.take().map(|(_, hook)| hook)
            } else {
                None
            }
        }) {
            hook.await;
        }
        future.await
    };
    run_watched(executor, watch, future)
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn stop_preparation(
        &mut self,
        one_shot: &mut Option<&mut crate::runtime::oneshot::OneShotState>,
    ) -> CliResult<bool> {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        if !watch.preparation_stopped() {
            return Ok(false);
        }
        self.pause_unrecorded("prompt preparation stopped; prompt remains queued".to_owned())?;
        if let Some(state) = one_shot.as_deref_mut() {
            state.result = Some(Ok(crate::runtime::oneshot::OneShotOutcome::Interrupted {
                assistant: String::new(),
            }));
        }
        let _ = self.events.send(ContentEvent::SetBusy(false));
        let _ = self.events.send(ContentEvent::Status {
            phase: "idle".into(),
            queue_len: self.config.session.pending_queue.len(),
            detail: Some("prompt preparation stopped; prompt remains queued".into()),
        });
        Ok(true)
    }
}

#[cfg(test)]
pub(super) mod hooks;
