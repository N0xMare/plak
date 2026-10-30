//! Rejoin an admitted turn after attachment, without admitting another command.

use super::super::usage::last_enrolled_source_bytes;
use super::{
    CliFailure, CliResult, ContextQuery, HostLoop, HostStep, Instant, TerminalPhase,
    emit_terminal_status, host_outcome_blocks_continuation, query_context,
};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    #[allow(
        clippy::too_many_lines,
        reason = "A restored turn retains its original admission and terminal fence while reconnecting host events and pending cancel settlement."
    )]
    pub(super) fn drive_restored_turn(&mut self) -> CliResult<HostStep> {
        let Some(recovered) = self.opened.take_recovered_turn() else {
            return Ok(HostStep::Continue);
        };
        let _ = self.events.send(ContentEvent::SetBusy(true));
        let pause = super::restored_paused_status_suffix(
            &self.config.topology_rows(),
            &self.config.session,
        );
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::RestoringAdmittedTurn { pause }),
        });
        let _ = self.events.send(ContentEvent::Activity {
            label: "agent restoring".into(),
            active: false,
        });
        let turn = recovered
            .take_turn()
            .ok_or_else(|| CliFailure::new("restored turn was already taken"))?;
        let context_projection = query_context(&self.executor, &self.opened, ContextQuery::Current)
            .unwrap_or_else(|_| crate::context::ContextProjection::unavailable());
        let enrolled_source_bytes = last_enrolled_source_bytes(&self.opened);
        self.project();
        let live_target = self.live_target();
        let agent = self
            .opened
            .agent()
            .ok_or_else(|| CliFailure::new("restored session source is detached"))?;
        let started = Instant::now();
        let driven = self
            .executor
            .block_on(super::super::turn_drive::drive_recovered_turn(
                turn,
                self.opened.sessions(),
                super::super::TurnHost {
                    job_agent: agent,
                    host_wakes: self.config.prompt_host_facts().host_wakes
                        && !self.config.session.child_wake_suppressed,
                    quit_started_at: &mut self.quit_started_at,
                    quit_job_stops: &mut self.quit_job_stops,
                    actions: self.actions,
                    deferred: &self.interrupt_state,
                    events: &self.events,
                    rows: &super::super::HostRows::new(
                        &self.config.state_roots,
                        self.config.store.as_ref(),
                    ),
                    roots: &self.config.state_roots,
                    store: self.config.store.as_ref(),
                    projection_store: &self.config.store,
                    live_projector: self.live.as_mut(),
                    live_target,
                    home: &self.config.home,
                    session: &mut self.config.session,
                    context_projection,
                    enrolled_source_bytes,
                    root_template: &mut self.config.root_template,
                    attached_one_shot: self.config.attached_one_shot,
                    ready_catalog: &self.config.ready_catalog.accounts,
                    surface: &self.config.surface,
                    log: self.log.as_ref(),
                    subscription_service: self.config.target.native_subscriptions().cloned(),
                    subscription_usage_refresh: &self.subscription_usage_refresh,
                    held_usage: &self.held_usage,
                    ledger: &self.session_ledger,
                    last_tree_revision: &mut self.last_tree_revision,
                    job_events: &mut self.job_events,
                    runtime_factory: self.config.runtime_factory.as_ref(),
                },
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?;
        if self.actions.abandon_and_release_requested() && driven.is_err() {
            return Ok(HostStep::Quit);
        }
        if let Err(error) = &driven
            && let Some(failure) = error.storage_failure()
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
        self.recovery_blocked |= host_outcome_blocks_continuation(&outcome);
        self.publish_terminal(
            outcome,
            started,
            super::publication::TerminalErrorPresentation::Events,
        );
        self.publish_projected_transcript(self.config.attached_one_shot);
        self.update_background_process_count();
        self.publish_pending_continuation();
        let _ = self.events.send(ContentEvent::SetBusy(false));
        emit_terminal_status(
            &self.events,
            if self.recovery_blocked {
                TerminalPhase::RecoveryPending
            } else if interrupted {
                TerminalPhase::Interrupted
            } else {
                TerminalPhase::Idle
            },
            self.config.session.pending_queue.len(),
        );
        Ok(if quit {
            HostStep::Quit
        } else {
            HostStep::Continue
        })
    }
}
