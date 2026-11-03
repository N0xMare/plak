use super::{
    Anchored, Applier, FeedCancelCause, FeedFact, FeedTurnOutcome, NoticeScope, PauseCause,
    ProjectError, ProjectedAbandonment, ReadRequest, ReadValue, RowKey, TerminalRead,
    TranscriptRow, abandoned_turn_operator_detail, abandoned_turn_operator_line,
    completed_tool_state, record_checkpoint,
};

impl Applier<'_> {
    pub(super) fn turn_terminal(
        &mut self,
        turn_id: plakous::model::TurnId,
        outcome: FeedTurnOutcome,
        steering_unapplied: bool,
    ) -> Result<(), ProjectError> {
        let turn = turn_id.to_string();
        let open_turn = self.views.state.open_turns.remove(&turn);
        // The operator's interrupt: a requested cancel, or any cancel of a
        // turn an accepted operator cancel named (its in-flight work may be
        // settled by an abandonment closure; the live row is the marker).
        let interrupted = match outcome {
            FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Requested,
            } => true,
            FeedTurnOutcome::Cancelled { .. } => {
                open_turn.as_ref().is_some_and(|turn| turn.cancel_requested)
            }
            _ => false,
        };
        let completed = matches!(outcome, FeedTurnOutcome::Completed { .. });
        self.settle_turn_calls(&turn, interrupted, completed);
        let anchor = self.seq();
        match outcome {
            FeedTurnOutcome::Completed {
                finish: plakous::model::FinishReason::Length,
            } => {
                let bound = match self.terminal_read(turn_id)? {
                    TerminalRead::Truncated(bound) => bound,
                    _ => None,
                };
                self.push(TranscriptRow::warning(
                    NoticeScope::Turn,
                    crate::runtime::truncation_note(bound),
                ));
            }
            FeedTurnOutcome::Failed { context, .. } => {
                let text = match self.terminal_read(turn_id)? {
                    TerminalRead::Failed(error) => error,
                    _ => "agent turn failed".to_owned(),
                };
                self.push(TranscriptRow::error(NoticeScope::Turn, text));
                if context.is_some() {
                    self.views.state.pause = Some(Anchored {
                        anchor,
                        value: Some(PauseCause::ContextCapacity {
                            turn_id: turn.clone(),
                        }),
                    });
                    if open_turn.as_ref().is_some_and(|turn| turn.host_wake) {
                        self.views.state.wake = Some(Anchored {
                            anchor,
                            value: true,
                        });
                    }
                }
            }
            FeedTurnOutcome::Cancelled { .. } if interrupted => {
                self.push(TranscriptRow::Interrupted);
            }
            FeedTurnOutcome::Cancelled {
                cause:
                    FeedCancelCause::Abandoned {
                        claim,
                        cause,
                        model_round_completed,
                        tool_round_completed,
                    },
            } => {
                let mut line = abandoned_turn_operator_detail(
                    claim,
                    cause,
                    tool_round_completed,
                    model_round_completed,
                )
                .to_owned();
                if steering_unapplied {
                    line.push_str(crate::runtime::STEERING_NOT_APPLIED_CLAUSE);
                }
                self.push(TranscriptRow::warning(NoticeScope::Recovery, line));
            }
            FeedTurnOutcome::RecoveryRequired => {
                self.push(
                    TranscriptRow::error(
                        NoticeScope::Recovery,
                        crate::runtime::RECOVERY_PENDING_NOTE,
                    )
                    .with_remedy(
                        plakous_host_protocol::notice_remedy::NoticeRemedy::RecoverSession {},
                    ),
                );
                self.views.state.pause = Some(Anchored {
                    anchor,
                    value: Some(PauseCause::RecoveryRequired { turn_id: turn }),
                });
            }
            _ => {}
        }
        // Spec 02 §4.2: a completed turn says so in one recovery notice, an
        // abandonment in its own line; a recorded failure or cancel says
        // nothing more.
        if steering_unapplied && matches!(outcome, FeedTurnOutcome::Completed { .. }) {
            self.push(TranscriptRow::info(
                NoticeScope::Recovery,
                crate::runtime::RECOVERED_COMPLETED_STEERING_NOTICE,
            ));
        }
        Ok(())
    }

    /// Spec 02 §4.2: the commit that closes a fenced root turn carries no
    /// fact of its own. The turn's recovery-required pause names it, and the
    /// closure's note on that turn's history marks the record; the notice is
    /// derived from the abandonment its terminal record now carries. The
    /// runtime appends that note once per turn, so the notice is derived once.
    pub(super) fn fenced_abandonment(&mut self) -> Result<(), ProjectError> {
        let Some(Anchored {
            value: Some(PauseCause::RecoveryRequired { turn_id: fenced }),
            ..
        }) = &self.views.state.pause
        else {
            return Ok(());
        };
        let facts = self.record.facts();
        if facts
            .iter()
            .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
        {
            return Ok(());
        }
        let Some(turn_id) = facts.iter().find_map(|fact| match fact {
            FeedFact::HistoryAppended {
                turn_id: Some(turn),
                ..
            } if turn.to_string() == *fenced => Some(*turn),
            _ => None,
        }) else {
            return Ok(());
        };
        let Some(checkpoint) = record_checkpoint(self.record) else {
            return Ok(());
        };
        let header = *self.record.header();
        let ReadValue::Abandoned(Some(report)) = self.read(ReadRequest::Abandoned {
            branch: header.branch_id,
            checkpoint,
            turn_id,
        })?
        else {
            return Ok(());
        };
        let report = report.clone();
        self.push(TranscriptRow::warning(
            NoticeScope::Recovery,
            abandoned_turn_operator_line(&report),
        ));
        if self.views.state.branch() == Some(header.branch_id) {
            self.abandonments.push(ProjectedAbandonment {
                session_id: header.session_id,
                turn_id,
                report,
            });
        }
        Ok(())
    }

    fn settle_turn_calls(&mut self, turn: &str, interrupted: bool, completed: bool) {
        let open: Vec<(String, RowKey, bool)> = self
            .views
            .state
            .calls
            .iter()
            .filter(|(_, call)| call.turn_id.as_deref() == Some(turn) && !call.settled)
            .map(|(id, call)| (id.clone(), call.key, call.operated))
            .collect();
        for (call_id, key, operated) in open {
            if let Some(call) = self.views.state.calls.get_mut(&call_id) {
                call.settled = true;
            }
            if let Some(TranscriptRow::Tool(mut tool)) = self.views.rows.get(&key).cloned() {
                tool.state = if interrupted {
                    "cancelled".to_owned()
                } else if completed && !operated {
                    completed_tool_state(&tool.name, Some((true, "")))
                } else {
                    "interrupted".to_owned()
                };
                self.replace(key, TranscriptRow::Tool(tool));
            }
        }
        // Provisional terminal presentation must retain both the row key and
        // counted bit, including a tombstone for an already truncated row.
        self.views.state.calls.retain(|_, call| {
            !call.counted
                || call.turn_id.as_deref() != Some(turn)
                || self.views.rows.get(&call.key).is_none_or(|row| {
                    row.as_tool().is_some_and(|tool| {
                        matches!(tool.state.as_str(), "interrupted" | "cancelled")
                    })
                })
        });
    }

    fn terminal_read(&self, turn_id: plakous::model::TurnId) -> Result<TerminalRead, ProjectError> {
        let Some(checkpoint) = record_checkpoint(self.record) else {
            return Ok(TerminalRead::NotRetained);
        };
        match self.read(ReadRequest::Terminal {
            branch: self.record.header().branch_id,
            checkpoint,
            turn_id,
        })? {
            ReadValue::Terminal(terminal) => Ok(terminal.clone()),
            _ => Ok(TerminalRead::NotRetained),
        }
    }
}
