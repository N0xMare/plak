//! Pre-dispatch checkpoints: finish the current receipt, never start a turn after stop.
//! The checkpoint is the fence; watching only drains the mailbox for latency.

use super::{CliResult, ContentEvent, InterruptWatch, NodeActor};
use crate::interrupt::watched;
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

pub(super) async fn run_step<F: Future, P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    hooks: &mut Hooks,
    stage: Stage,
    future: F,
) -> F::Output {
    #[cfg(test)]
    let future = {
        let hook = hooks
            .hook
            .take_if(|(selected, _)| *selected == stage)
            .map(|(_, hook)| hook);
        hooks.called.push(stage);
        async {
            if let Some(hook) = hook {
                hook.await;
            }
            future.await
        }
    };
    #[cfg(not(test))]
    let _ = (hooks, stage);
    watched(watch, future).await
}

/// Production preparation has no test seams.
#[cfg(not(test))]
pub(in crate::runtime::host_loop) struct Hooks;

#[cfg(not(test))]
impl Hooks {
    pub(in crate::runtime::host_loop) const fn new() -> Self {
        Self
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> NodeActor<'_, P> {
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
mod hooks;
#[cfg(test)]
pub(in crate::runtime::host_loop) use hooks::Hooks;
