//! Rejoin an admitted turn after attachment, without admitting another command.
use super::RootRecovery;

use super::super::usage::last_enrolled_source_bytes;
use super::{
    CliFailure, CliResult, ContextQuery, HostStep, Instant, RootActor, TerminalPhase,
    emit_terminal_status, host_outcome_blocks_continuation, query_context,
};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    #[allow(
        clippy::too_many_lines,
        reason = "A restored turn retains its original admission and terminal fence while reconnecting host events and pending cancel settlement."
    )]
    pub(super) async fn drive_restored_turn(&mut self) -> CliResult<HostStep> {
        let Some(recovered) = self.opened.take_recovered_turn() else {
            return Ok(HostStep::Continue);
        };
        let _ = self.role.events.send(ContentEvent::SetBusy(true));
        let pause = super::queue_pause(&self.role.config.session);
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::RestoringAdmittedTurn { pause }),
        });
        let _ = self.role.events.send(ContentEvent::Activity {
            label: "agent restoring".into(),
            active: false,
        });
        let turn = recovered
            .take_turn()
            .ok_or_else(|| CliFailure::new("restored turn was already taken"))?;
        let context_projection = query_context(&self.opened, ContextQuery::Current)
            .await
            .unwrap_or_else(|_| crate::context::ContextProjection::unavailable());
        let enrolled_source_bytes = last_enrolled_source_bytes(&self.opened);
        self.project().await;
        let live_target = self.live_target();
        let agent = self
            .opened
            .agent()
            .ok_or_else(|| CliFailure::new("restored session source is detached"))?;
        let started = Instant::now();
        let driven = Box::pin(super::super::turn_drive::drive_recovered_turn(
            turn,
            self.opened.sessions(),
            super::super::TurnHost {
                job_agent: agent,
                host_wakes: self.role.config.prompt_host_facts().host_wakes
                    && !self.role.config.session.child_wake_suppressed,
                quit_started_at: &mut self.quit_started_at,
                quit_job_stops: &mut self.quit_job_stops,
                actions: self.role.actions,
                deferred: &self.role.interrupt_state,
                events: &self.role.events,
                rows: &super::super::HostRows::new(
                    &self.role.config.state_roots,
                    self.role.config.store.as_ref(),
                ),
                roots: &self.role.config.state_roots,
                store: self.role.config.store.as_ref(),
                projection_store: &self.role.config.store,
                live_projector: self.role.live.as_mut(),
                live_target,
                home: &self.role.config.home,
                session: &mut self.role.config.session,
                context_projection,
                enrolled_source_bytes,
                root_template: &mut self.role.config.root_template,
                attached_one_shot: self.role.config.attached_one_shot,
                ready_catalog: &self.role.config.ready_catalog.accounts,
                surface: &self.role.config.surface,
                log: self.role.log.as_ref(),
                subscription_service: self.role.config.target.native_subscriptions().cloned(),
                subscription_usage_refresh: &self.role.subscription_usage_refresh,
                held_usage: &self.role.held_usage,
                ledger: &self.role.session_ledger,
                last_tree_revision: &mut self.role.last_tree_revision,
                job_events: &mut self.role.job_events,
                runtime_factory: self.role.config.runtime_factory.as_ref(),
            },
        ))
        .await;
        if self.role.actions.abandon_and_release_requested() && driven.is_err() {
            return Ok(HostStep::Quit);
        }
        if let Err(error) = &driven
            && let Some(failure) = error.refused_failure()
        {
            return Err(failure);
        }
        let quit = match &driven {
            Ok(driven) => driven.quit_after_terminal,
            Err(error) => error.quit_after_terminal,
        };
        let outcome = driven
            .map(|driven| driven.outcome)
            .map_err(|error| error.message);
        let interrupted = matches!(&outcome, Ok(super::TurnHostOutcome::Interrupted { .. }));
        if interrupted {
            self.suppress_child_wake(None)?;
        }
        if host_outcome_blocks_continuation(&outcome) {
            self.role.recovery = RootRecovery::Due;
        }
        self.publish_terminal(
            outcome,
            started,
            super::publication::TerminalErrorPresentation::Events,
        )
        .await;
        self.publish_projected_transcript(self.role.config.attached_one_shot)
            .await;
        self.update_background_process_count();
        self.publish_pending_continuation();
        let _ = self.role.events.send(ContentEvent::SetBusy(false));
        emit_terminal_status(
            &self.role.events,
            if self.role.recovery.blocked() {
                TerminalPhase::RecoveryPending
            } else if interrupted {
                TerminalPhase::Interrupted
            } else {
                TerminalPhase::Idle
            },
            self.role.config.session.pending_queue.len(),
        );
        Ok(if quit {
            HostStep::Quit
        } else {
            HostStep::Continue
        })
    }
}
