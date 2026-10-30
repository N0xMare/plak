//! Pre-dispatch checkpoints: finish the current receipt, never start a turn after stop.
//! The checkpoint is the fence; watching only drains the mailbox for latency.

use super::{CliResult, ContentEvent, HostLoop, InterruptWatch};
use crate::interrupt::run_watched;
use plakous::application::{ApplicationExecutor, ApplicationExecutorError};
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};
use std::future::Future;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::host_loop) enum Stage {
    Refresh,
    Settle,
    Tree,
    #[cfg(test)]
    Query,
    #[cfg(test)]
    Compact,
    Context,
    Dispatch,
}

#[derive(Clone, Copy)]
pub(in crate::runtime::host_loop) struct Stopped;

pub(super) enum End {
    Stopped(Stopped),
    Refused,
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
        hooks::CALLED.with(|calls| calls.borrow_mut().push(stage));
        future.await
    };
    run_watched(executor, watch, future)
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(in crate::runtime::host_loop) fn stop_preparation(
        &mut self,
        one_shot: &mut Option<&mut crate::runtime::oneshot::OneShotState>,
    ) -> CliResult<Option<Stopped>> {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        if !watch.preparation_stopped() {
            return Ok(None);
        }
        let paused = self.pause_queue("preparation stopped; queue paused".to_owned(), true);
        if let Some(state) = one_shot.as_deref_mut() {
            state.result = Some(Ok(crate::runtime::oneshot::OneShotOutcome::Interrupted {
                assistant: String::new(),
            }));
        }
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: self.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::PromptPreparationStopped),
        });
        paused?;
        Ok(Some(Stopped))
    }
}

#[cfg(test)]
pub(in crate::runtime::host_loop) mod hooks;
