//! Commit the terminal product projection before advertising completion.

use super::{
    ExceptionalTerminalProjection, HostEvent, HostLoop, Instant, InterruptWatch, SessionEvent,
    SessionQueueDrainState, TurnHostOutcome, commit_definite_failure_projection,
    commit_exceptional_terminal_projection, commit_session_update,
    context_capacity_recovery_guidance, elapsed_ms_since, emit_unavailable_context, handle_compact,
    log_ev, native_compact_facet, report_context, sessions,
};
use super::{HostBoundary, HostWorkflow, observe_boundary};
use crate::sessions::{NoticeScope, TranscriptRow};

/// One-shot callers return terminal failures; interactive hosts publish them.
#[derive(Clone, Copy)]
pub(super) enum TerminalErrorPresentation {
    Events,
    Return,
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn publish_terminal(
        &mut self,
        drive: Result<TurnHostOutcome, String>,
        turn_started: Instant,
        error_presentation: TerminalErrorPresentation,
    ) -> bool {
        let mut completed = matches!(&drive, Ok(TurnHostOutcome::Completed { .. }));
        let exchange = match &drive {
            Ok(TurnHostOutcome::Completed { .. } | TurnHostOutcome::ContextCapacity { .. }) => {
                Some(crate::agent::TerminalExchange::Closed)
            }
            Ok(
                TurnHostOutcome::Failed { exchange, .. }
                | TurnHostOutcome::Interrupted { exchange, .. },
            ) => Some(*exchange),
            _ => None,
        };
        if let Some(exchange) = exchange {
            self.settle_terminal_waits(exchange);
        }
        match drive {
            Ok(TurnHostOutcome::ConsumedDelivery) => completed = true,
            Ok(TurnHostOutcome::Completed {
                assistant,
                finish_reason,
                finish_detail,
                wire_output_limit,
            }) => {
                let transcript_line = super::super::turn::completed_turn_row(&assistant);
                match commit_session_update(
                    &*self.config.store,
                    &self.config.home,
                    &mut self.config.session,
                    |candidate| {
                        candidate.append_transcript(transcript_line);
                        if finish_reason == plakous::model::FinishReason::Refusal {
                            candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                        }
                        true
                    },
                ) {
                    Ok(true) => {
                        {
                            observe_boundary(
                                self.config.runtime_factory.as_ref(),
                                HostWorkflow::SendNow,
                                HostBoundary::ProductCommitted,
                            );
                        }
                        if finish_reason == plakous::model::FinishReason::Length {
                            let _ = self.events.send(HostEvent::Row(TranscriptRow::warning(
                                NoticeScope::Turn,
                                super::super::turn::truncation_note(wire_output_limit),
                            )));
                        }
                        if assistant.is_empty()
                            && finish_reason != plakous::model::FinishReason::Refusal
                        {
                            let _ = self.events.notice(TranscriptRow::warning(NoticeScope::Turn,
                                "turn finished with no visible text (empty stream — try again, or choose another profile with /agents)",
                            ));
                        }
                        if assistant.is_empty() {
                            log_ev(
                                self.log.as_ref(),
                                &self.config,
                                SessionEvent::new("turn.assistant_empty").u64("chars", 0),
                            );
                        } else {
                            log_ev(
                                self.log.as_ref(),
                                &self.config,
                                SessionEvent::new("turn.assistant")
                                    .str("text", &assistant)
                                    .u64("chars", assistant.len() as u64),
                            );
                            let _ = self.events.send(HostEvent::AssistantDone);
                        }
                        if finish_reason == plakous::model::FinishReason::Refusal {
                            let _ = self.events.send(HostEvent::ModelRefusal {
                                detail: finish_detail,
                            });
                        }
                        self.events
                            .resnapshot_transcript_if_required(&self.config.session);
                        log_ev(
                            self.log.as_ref(),
                            &self.config,
                            SessionEvent::new("turn.completed")
                                .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                        );
                        if let Err(error) = report_context(
                            &self.executor,
                            &self.opened,
                            &self.events,
                            "after turn publication",
                            self.config
                                .target
                                .native_provider()
                                .is_some_and(native_compact_facet),
                            self.config.session.context_counters(),
                        ) {
                            emit_unavailable_context(&self.events);
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Turn,
                                error.message().to_owned(),
                            ));
                        }
                    }
                    Ok(false) => unreachable!("completion update always changes the candidate"),
                    Err(_) => {
                        completed = false;
                        self.recovery_blocked = true;
                        let _ = commit_session_update(
                            &*self.config.store,
                            &self.config.home,
                            &mut self.config.session,
                            |candidate| {
                                candidate.queue_drain_state =
                                    SessionQueueDrainState::RestoredPaused;
                                true
                            },
                        );
                        let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                            "turn completed but its product transcript could not be committed; the next prompt (or /recover) reopens the session and continues"
                                ,
                        ));
                    }
                }
            }
            Ok(TurnHostOutcome::ContextCapacity {
                source_checkpoint,
                recovery,
                error,
            }) => {
                let exact_source = self.opened.locator().checkpoint_id() == source_checkpoint;
                if !exact_source {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Compaction,
                        "context-capacity result did not match the exact attached checkpoint; compact recovery was refused"
                            ,
                    ));
                }
                let recovery = if exact_source {
                    recovery
                } else {
                    plakous_host_protocol::ContextCapacityRecovery::Protected
                };
                let native = self
                    .config
                    .target
                    .native_provider()
                    .is_some_and(native_compact_facet);
                let segment_limit = self.opened.segment_limit_reached();
                // A fold that failed after dispatch, at the turn's fence or in
                // an earlier capacity recovery, would fail the same way on the
                // same source: the operator gets the guidance instead.
                let auto_compact = matches!(
                    recovery,
                    plakous_host_protocol::ContextCapacityRecovery::Compactable
                ) && exact_source
                    && self.config.compaction_enabled
                    && !self.opened.paid_fold_failed_since_last_round();
                let saved = commit_session_update(
                    &*self.config.store,
                    &self.config.home,
                    &mut self.config.session,
                    |candidate| {
                        candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                        candidate.append_transcript(TranscriptRow::error(
                            NoticeScope::Turn,
                            error.clone(),
                        ));
                        true
                    },
                );
                if saved.is_err() {
                    self.recovery_blocked = true;
                    self.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "context-capacity stop could not be committed; queue drain remains fail-closed",
                    ));
                    return false;
                }
                if auto_compact {
                    let _ = self.events.notice(TranscriptRow::warning(NoticeScope::Turn,
                        "this turn stopped at the context limit; compacting once for subsequent prompts. The failed turn will not be retried automatically",
                    ));
                    match handle_compact(
                        &self.executor,
                        InterruptWatch::new(self.actions, &self.interrupt_state),
                        &mut self.config,
                        &self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        self.log.as_ref(),
                        sessions::ProductCompactReason::CapacityRecovery,
                        None,
                    ) {
                        Ok(_) => {
                            completed = true;
                            if let Err(error) = report_context(
                                &self.executor,
                                &self.opened,
                                &self.events,
                                "after compaction",
                                native,
                                self.config.session.context_counters(),
                            ) {
                                emit_unavailable_context(&self.events);
                                let _ = self.events.notice(TranscriptRow::error(
                                    NoticeScope::Turn,
                                    error.message().to_owned(),
                                ));
                            }
                        }
                        Err(compact_error) => {
                            let guidance = context_capacity_recovery_guidance(
                                recovery,
                                exact_source,
                                native,
                                segment_limit,
                            );
                            if matches!(error_presentation, TerminalErrorPresentation::Events) {
                                let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                                    NoticeScope::Turn,
                                    error,
                                )));
                            }
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Compaction,
                                format!("compact: {}", compact_error.message()),
                            ));
                            let _ = self
                                .events
                                .notice(TranscriptRow::info(NoticeScope::Turn, guidance));
                            let _ = self
                                .events
                                .send(HostEvent::ContextCapacityRecovery(Some(recovery)));
                        }
                    }
                } else {
                    let guidance = context_capacity_recovery_guidance(
                        recovery,
                        exact_source,
                        native,
                        segment_limit,
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            error,
                        )));
                    }
                    let _ = self
                        .events
                        .notice(TranscriptRow::info(NoticeScope::Turn, guidance));
                    let _ = self
                        .events
                        .send(HostEvent::ContextCapacityRecovery(Some(recovery)));
                }
            }
            Ok(TurnHostOutcome::AdmissionRejected { error, .. }) => {
                if matches!(error_presentation, TerminalErrorPresentation::Events) {
                    let _ = self
                        .events
                        .notice(TranscriptRow::error(NoticeScope::Turn, error));
                }
            }
            Ok(TurnHostOutcome::Failed {
                error, transient, ..
            }) => {
                if let Ok(()) = commit_definite_failure_projection(
                    &mut self.config.session,
                    &error,
                    |candidate| self.config.store.save(candidate),
                ) {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("turn.failed")
                            .str("disposition", "definite")
                            .str("msg", &error)
                            .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            error.clone(),
                        )));
                    }
                    if !transient
                        .is_some_and(|failure| self.schedule_transient_resume(failure, &error))
                    {
                        let _ = self.events.notice(TranscriptRow::info(NoticeScope::Turn,
                            "turn failed definitively; the durable session remains available for a new prompt",
                        ));
                    }
                } else {
                    self.recovery_blocked = true;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "definite turn failure could not be saved; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Ok(TurnHostOutcome::RecoveryRequired { error }) => {
                self.recovery_blocked = true;
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::RecoveryRequired(&error),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("turn.recovery_required")
                            .str("disposition", "recovery_required")
                            .str("msg", &error)
                            .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            error,
                        )));
                    }
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Recovery,
                        RECOVERY_PENDING_NOTE,
                    ));
                } else {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "recovery-required terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Ok(TurnHostOutcome::Interrupted { assistant, .. }) => {
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::Interrupted(&assistant),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("control.interrupt"),
                    );
                    let _ = self.events.send(HostEvent::Row(TranscriptRow::Interrupted));
                } else {
                    self.recovery_blocked = true;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "interrupted terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Err(msg) => {
                self.recovery_blocked = true;
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::UnresolvedError(&msg),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    // drive_one_turn already applies operator_facing_agent_error.
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("error")
                            .str("code", "agent")
                            .str("msg", &msg),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self
                            .events
                            .send(HostEvent::Row(TranscriptRow::error(NoticeScope::Turn, msg)));
                    }
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Recovery,
                        RECOVERY_PENDING_NOTE,
                    ));
                } else {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "failed terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
        }

        completed
    }

    /// Settles the root's await receipts for the terminal being published,
    /// before anything below can compact (spec 03 section 4.4). A failure
    /// leaves them for the settlement at the next admission or open.
    fn settle_terminal_waits(&self, exchange: crate::agent::TerminalExchange) {
        let settled = self
            .executor
            .block_on(super::super::result_mail::settle_terminal_waits(
                &self.config.topology_rows(),
                &self.config.session,
                &self.opened,
                exchange,
            ))
            .unwrap_or_else(|_| Err("application execution failed".to_owned()));
        if let Err(error) = settled {
            let _ = self.events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("wait receipt settlement failed: {error}"),
            ));
        }
    }
}

/// What the operator is told when a turn's outcome could not be observed. The
/// session is not lost: the next action reopens it and records the uncertainty
/// once, and nothing the turn started is retried.
pub(crate) const RECOVERY_PENDING_NOTE: &str = "the turn's outcome is uncertain; the \
     next prompt (or /recover) records that and continues";

#[cfg(test)]
mod tests;

#[cfg(test)]
mod running_tree_tests;
#[cfg(test)]
mod wait_settlement_tests;
