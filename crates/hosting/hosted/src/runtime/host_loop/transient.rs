//! Root transient continuation owns no turn while waiting.

use super::{CliFailure, CliResult, HostLoop, HostStep, InterruptWatch};
use crate::driver::transient::{self, ResumeGate, ResumeOwner, ResumePlan};
use crate::runtime::TransientFailure;
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous::model::{CancellationToken, ReachabilityProbe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct PendingResume {
    session_id: String,
    plan: ResumePlan,
    ready: Arc<AtomicBool>,
    cancellation: CancellationToken,
    task: crate::TaskHandle<()>,
}

impl PendingResume {
    fn spawn(
        tasks: &crate::TaskSet,
        session_id: String,
        plan: ResumePlan,
        probe: Option<ReachabilityProbe>,
    ) -> Self {
        let ready = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new();
        let signal = ready.clone();
        let cancel = cancellation.clone();
        let gate = plan.gate;
        let task = tasks.spawn(async move {
            let reached = transient::wait_gate(
                gate,
                probe,
                &cancel,
                None,
                || Some(std::future::pending()),
                || std::future::ready(true),
            )
            .await;
            signal.store(reached, Ordering::Release);
        });
        Self {
            session_id,
            plan,
            ready,
            cancellation,
            task,
        }
    }
}

impl Drop for PendingResume {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn schedule_transient_resume(
        &mut self,
        failure: TransientFailure,
        error: &str,
    ) -> bool {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.defer_input();
        if self.recovery_blocked
            || self.quit_started_at.is_some()
            || watch.interrupted()
            || self.interrupt_state.borrow().has_deferred()
            || self.pending_start.is_some()
            || !self.config.session.pending_queue.is_empty()
            || self.config.session.queue_commands.has_pending()
        {
            return false;
        }
        let budget = self
            .config
            .state_roots
            .roster_source()
            .roster(
                self.config.session.account.as_deref().unwrap_or_default(),
                &self.config.model_id,
                self.config.session.effort.as_deref(),
            )
            .map_or(0, |roster| roster.transient_resumes.min(2));
        let Some(plan) = transient::plan(
            failure.gate,
            ResumeOwner::Root {
                failed_turn: failure.failed_turn,
            },
            error,
            self.transient_resumes_used,
            budget,
        ) else {
            return false;
        };
        let round = plan.round;
        let human = error.to_owned();
        let wait = match failure.gate {
            ResumeGate::Network => "when the network returns".to_owned(),
            ResumeGate::Delay(delay) => format!("after {}s", delay.as_secs()),
        };
        let session_id = self.config.session.id.clone();
        let probe = self.opened.reachability_probe();
        let Ok(pending) = self.executor.block_on(async {
            PendingResume::spawn(self.executor.tasks(), session_id, plan, probe)
        }) else {
            return false;
        };
        self.pending_resume = Some(pending);
        self.transient_resumes_used = round;
        let _ = self.events.notice(TranscriptRow::info(
            NoticeScope::Turn,
            format!(
                "{human}; will continue automatically {wait} ({}/{budget}); continuation may be cancelled",
                self.transient_resumes_used,
            ),
        ).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::CancelContinuation {}));
        true
    }

    pub(super) fn cancel_transient_resume(&mut self) {
        if self.pending_resume.take().is_some() {
            let _ = self.events.notice(TranscriptRow::info(
                NoticeScope::Turn,
                "automatic continuation cancelled",
            ));
        }
    }

    pub(super) fn transient_resume_prompt(&mut self) -> CliResult<HostStep> {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.defer_input();
        if watch.interrupted() || self.interrupt_state.borrow().has_deferred() {
            self.cancel_transient_resume();
            return Ok(HostStep::Continue);
        }
        let Some(pending) = self.pending_resume.as_ref() else {
            return Ok(HostStep::Continue);
        };
        if pending.session_id != self.config.session.id
            || self.recovery_blocked
            || self.pending_start.is_some()
            || !self.config.session.pending_queue.is_empty()
            || self.config.session.queue_commands.has_pending()
        {
            self.cancel_transient_resume();
            return Ok(HostStep::Continue);
        }
        if !pending.ready.load(Ordering::Acquire) {
            return Ok(HostStep::Continue);
        }
        let command = pending.plan.command_id.clone();
        let text = pending.plan.text.clone();
        let mut candidate = self.config.session.clone();
        let item = candidate
            .enqueue_live(text.clone())
            .map_err(|_| CliFailure::new("transient continuation queue admission was refused"))?;
        self.config.session = self
            .executor
            .block_on(super::super::result_mail::prepare_with_command(
                &self.config.topology_rows(),
                self.config.store.as_ref(),
                &candidate,
                &item,
                &text,
                None,
                command.as_str(),
                crate::driver::ledger::CommandKind::Resume,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::from)
            .inspect_err(|error| {
                if error.store_refusal() == Some(crate::error::StoreRefusal::StorageFull) {
                    self.cancel_transient_resume();
                }
            })?;
        self.pending_resume = None;
        Ok(HostStep::Prompt {
            text,
            queued_delivery: Some(item),
            user_echoed: true,
        })
    }
}

#[cfg(test)]
mod tests;
