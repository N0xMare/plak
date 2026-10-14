//! Reconcile canonical completion or settled cancellation before publication.

use super::{
    AgentErrorKind, DriveTurnError, DrivenTurn, HostEvent, TurnDrive, TurnHostOutcome,
    TurnResultOutcome, emit_turn_lifecycle, flush_assistant_delta,
    flush_deferred_interrupted_status, observe_cancelled_tool_activity,
    observe_terminal_tool_activity, operator_facing_agent_error, reconcile_authoritative_assistant,
    settle_late_cancel, settle_live_thinking, terminal_turn_outcome, turn_lifecycle,
    unresolved_turn_lifecycle,
};

impl<P: plakous_host_protocol::ProtocolExtension> TurnDrive<'_, P> {
    pub(super) async fn finish(mut self) -> Result<DrivenTurn, DriveTurnError> {
        if self.abandoned_by_quit
            && self.turn_result.is_none()
            && self.host.quit_started_at.is_some()
        {
            *self.host.quit_abandoned_turn = true;
            self.host.begin_quit_job_stops();
        }
        // Final flush of any remaining coalesced assistant text.
        flush_assistant_delta(
            &mut self.delta_buf,
            &mut self.last_flush,
            self.host.events,
            true,
        );
        // Every exit settles the turn's reasoning, including one that never
        // observed a terminal event: an abandoned, lagged or closed stream
        // still showed it.
        settle_live_thinking(&mut self.projection, self.host.session, self.host.events);

        // an escalated quit over an unsettled cancel abandons the turn
        // without observing its terminal. Nothing here synthesises one; the
        // checkpoint remains the recovery authority.
        if self.abandoned_by_quit && self.turn_result.is_none() {
            emit_turn_lifecycle(
                self.host.events,
                self.host.log,
                self.host.session,
                unresolved_turn_lifecycle(self.control.turn_id(), None),
            );
            return Err(DriveTurnError {
                message: self.cancel_unsettled.map_or_else(
                    || "quit abandoned the turn before its terminal result".to_owned(),
                    |detail| {
                        format!("quit abandoned the turn before its terminal result: {detail}")
                    },
                ),
                quit_after_terminal: true,
            });
        }

        match self.turn_result {
            Some(Ok(result)) => {
                let lifecycle = turn_lifecycle(&result);
                let outcome = match result.outcome() {
                    TurnResultOutcome::Completed(response) => {
                        observe_terminal_tool_activity(
                            self.host.session,
                            self.host.events,
                            "failed:observation-gap",
                            Some(&self.projection),
                        );
                        if let Some(delta) = reconcile_authoritative_assistant(
                            &mut self.assistant,
                            response.output_text(),
                        ) {
                            let _ = self.host.events.send(HostEvent::AssistantDelta(delta));
                        }
                        Ok(terminal_turn_outcome(&result))
                    }
                    TurnResultOutcome::Cancelled(_) => {
                        observe_cancelled_tool_activity(
                            self.host.session,
                            self.host.events,
                            Some(&self.projection),
                        );
                        // Every row the cancel was holding open is terminal now,
                        // either observed or swept just above.
                        flush_deferred_interrupted_status(
                            &mut self.interrupted_status_pending,
                            self.host.session,
                            self.host.events,
                            &mut self.projection,
                        );
                        settle_late_cancel(
                            self.host.rows,
                            self.host.store,
                            self.host.home,
                            self.host.session,
                            self.host.events,
                            self.cancel_unsettled.as_deref(),
                        )
                        .await;
                        Ok(TurnHostOutcome::Interrupted {
                            assistant: self.assistant,
                            exchange: crate::agent::TerminalExchange::Closed,
                        })
                    }
                    TurnResultOutcome::Failed(_) | TurnResultOutcome::RecoveryRequired(_) => {
                        observe_terminal_tool_activity(
                            self.host.session,
                            self.host.events,
                            "failed:observation-gap",
                            Some(&self.projection),
                        );
                        Ok(terminal_turn_outcome(&result))
                    }
                };
                // The terminal marker follows the outcome's own settle
                // publications — cancelled tool rows and the interrupted
                // status — so its position in the observation stream does not
                // race whichever side of the result commit they landed on.
                emit_turn_lifecycle(
                    self.host.events,
                    self.host.log,
                    self.host.session,
                    lifecycle,
                );
                outcome.map(|outcome| DrivenTurn {
                    outcome,
                    quit_after_terminal: self.quit_after_terminal,
                })
            }
            Some(Err(err)) => {
                let lifecycle = unresolved_turn_lifecycle(self.control.turn_id(), Some(&err));
                if matches!(err.kind(), AgentErrorKind::Cancelled) {
                    observe_cancelled_tool_activity(
                        self.host.session,
                        self.host.events,
                        Some(&self.projection),
                    );
                    settle_late_cancel(
                        self.host.rows,
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        self.host.events,
                        self.cancel_unsettled.as_deref(),
                    )
                    .await;
                    emit_turn_lifecycle(
                        self.host.events,
                        self.host.log,
                        self.host.session,
                        lifecycle,
                    );
                    Ok(DrivenTurn {
                        outcome: TurnHostOutcome::Interrupted {
                            assistant: self.assistant,
                            exchange: crate::agent::TerminalExchange::Unresolved,
                        },
                        quit_after_terminal: self.quit_after_terminal,
                    })
                } else if let Some(error) = self.event_failure {
                    emit_turn_lifecycle(
                        self.host.events,
                        self.host.log,
                        self.host.session,
                        lifecycle,
                    );
                    Err(DriveTurnError {
                        message: error,
                        quit_after_terminal: self.quit_after_terminal,
                    })
                } else {
                    emit_turn_lifecycle(
                        self.host.events,
                        self.host.log,
                        self.host.session,
                        lifecycle,
                    );
                    // Always run operator framing (rate limit, capacity, capability).
                    Err(DriveTurnError {
                        message: operator_facing_agent_error("agent turn failed", &err),
                        quit_after_terminal: self.quit_after_terminal,
                    })
                }
            }
            None => {
                emit_turn_lifecycle(
                    self.host.events,
                    self.host.log,
                    self.host.session,
                    unresolved_turn_lifecycle(self.control.turn_id(), None),
                );
                Err(DriveTurnError {
                    message: "turn ended without result".into(),
                    quit_after_terminal: self.quit_after_terminal,
                })
            }
        }
    }
}
