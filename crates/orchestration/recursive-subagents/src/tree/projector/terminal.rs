use super::{
    AttemptStatus, BlockedReason, CommandFact, FAILED_TURN_NOTICE_KIND, FailedTurnDisposition,
    FailureRead, FeedCancelCause, FeedTurnOutcome, NodeStatus, ProductAgentTree, RecoveryNeed,
    RecoveryResolution, TURN_ABANDONED, TerminalRead, TopologyError, TreeFeedReads, TreeProjection,
    TurnRefusal, classify_provider_refusal, failed_turn_notice_id, host_recovery,
};

impl ProductAgentTree {
    #[allow(clippy::too_many_arguments, reason = "one terminal fact's fields")]
    pub(super) fn apply_terminal(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        command_id: &str,
        outcome: FeedTurnOutcome,
        closed_exchange: bool,
        committed_at_ms: u64,
        reads: &TreeFeedReads,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        // Wait settlement (spec 03 §4.4).
        match outcome {
            FeedTurnOutcome::RecoveryRequired => {}
            _ if closed_exchange => {
                self.retire_wait_receipts(agent_id);
            }
            _ => {
                let unclosed: Vec<(String, bool)> = self
                    .retained_wait_receipts(agent_id)
                    .into_iter()
                    .map(|id| {
                        // Retention can drop HistoryAppended but retain this
                        // terminal. Its quiescent head proves committed calls.
                        let committed = reads
                            .waits
                            .values()
                            .flatten()
                            .any(|(call, committed)| *call == id && *committed);
                        (id, committed)
                    })
                    .collect();
                self.settle_wait_receipts(agent_id, &unclosed)?;
            }
        }
        // The withheld-mail return (spec 03 §8).
        let withheld = matches!(
            outcome,
            FeedTurnOutcome::Failed {
                context: Some(context),
                ..
            } if context.withheld
        );
        if withheld
            && let Some(command) = self.commands.get(command_id).filter(|command| {
                command.recipient == agent_id && command.fact == Some(CommandFact::Accepted)
            })
        {
            let message_ids = command.message_ids.clone();
            self.return_admitted_mail(command_id, agent_id, &message_ids)?;
            if let Some(command) = self.commands.get_mut(command_id) {
                command.fact = Some(CommandFact::Returned);
            }
            self.bump();
        }
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        if let Some(attempt_id) = self
            .commands
            .get(command_id)
            .filter(|command| command.recipient == agent_id)
            .and_then(|command| command.attempt_id.clone())
            && self.node(agent_id)?.attempt_id.as_ref() != Some(&attempt_id)
        {
            if let FeedTurnOutcome::Failed {
                kind, model_kind, ..
            } = outcome
            {
                self.settle_failed(
                    agent_id,
                    &attempt_id,
                    turn_id,
                    (kind, model_kind),
                    committed_at_ms,
                    reads.failure(turn_id),
                )?;
            }
            if outcome != FeedTurnOutcome::RecoveryRequired
                && let Some(attempt) = self.attempts.get_mut(&attempt_id)
                && attempt.turn_id.as_deref() == Some(turn_id)
            {
                attempt.turn_id = None;
                self.bump();
            }
            return Ok(());
        }
        // The child's bound turn ended.
        let Some(attempt) = self.current_attempt(agent_id) else {
            return Ok(());
        };
        if attempt.turn_id.as_deref() != Some(turn_id) {
            return Ok(());
        }
        let attempt_id = attempt.attempt_id.clone();
        let stopped = attempt.status.is_stopped();
        if self.node(agent_id)?.active_turn {
            self.set_active_turn(agent_id, false)?;
        }
        if let FeedTurnOutcome::Failed {
            kind, model_kind, ..
        } = outcome
        {
            if !self.has_transient_resume(agent_id, &attempt_id, turn_id)? {
                self.settle_failed(
                    agent_id,
                    &attempt_id,
                    turn_id,
                    (kind, model_kind),
                    committed_at_ms,
                    reads.failure(turn_id),
                )?;
            }
            if let Some(attempt) = self.current_attempt_mut(agent_id) {
                attempt.turn_id = None;
            }
            self.bump();
            return Ok(());
        }
        // A lost owner's turn closed by a reopen settles `turn_abandoned`; so
        // does a resumable turn a recovery reopen cancelled, for a node
        // waiting on recovery. A live interrupt's cancel settles nothing here.
        let recovering = matches!(self.recovery_need(agent_id)?, RecoveryNeed::Session { .. });
        let fallback = match outcome {
            FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned { .. },
            } => format!("plak: {agent_id} recovered an interrupted turn"),
            FeedTurnOutcome::Cancelled { .. } if recovering => format!(
                "plak: {agent_id} recovered a resumable turn and cancelled it; nothing from it \
                 is replayed"
            ),
            FeedTurnOutcome::RecoveryRequired => return Ok(()),
            _ => {
                if let Some(attempt) = self.current_attempt_mut(agent_id) {
                    attempt.turn_id = None;
                }
                self.bump();
                return Ok(());
            }
        };
        self.settle_abandoned(
            agent_id,
            &attempt_id,
            stopped,
            recovering,
            fallback,
            reads.terminal(turn_id),
            committed_at_ms,
            projection,
        )
    }

    /// A failed terminal of a running (or submitted) attempt's bound turn ends
    /// the attempt (spec 03 §10, §14.3): the model error's kind and the read's
    /// retryability classify it, a block's `resets_at` counts from the
    /// record's commit, and the end stamp is that commit. The message is the
    /// read error's, joined with the host's failed-turn notice. A terminal
    /// whose read is gone still ends the attempt, on the fact's kinds alone.
    /// An attempt already settled (stopped, recovering, replayed) is left; a
    /// closed node's running attempt still records its failure.
    fn settle_failed(
        &mut self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        turn_id: &str,
        (kind, model_kind): (
            plakous::agent::AgentErrorKind,
            Option<plakous::model::ModelErrorKind>,
        ),
        committed_at_ms: u64,
        read: Option<&FailureRead>,
    ) -> Result<(), TopologyError> {
        let Some(attempt) = self.attempts.get(attempt_id).filter(|attempt| {
            match attempt.status {
                AttemptStatus::Running | AttemptStatus::Submitted => true,
                // A close fenced the node while the failed turn unwound.
                AttemptStatus::Closed => attempt.failure.is_none() && attempt.blocked.is_none(),
                _ => false,
            }
        }) else {
            return Ok(());
        };
        let attempt_id = attempt.attempt_id.clone();
        let refusal = model_kind.and_then(|model_kind| {
            classify_provider_refusal(
                model_kind,
                read.and_then(|read| read.retryability),
                committed_at_ms,
            )
        });
        let disposition = match refusal {
            Some(TurnRefusal::Blocked { quota, resets_at }) => {
                FailedTurnDisposition::Blocked(BlockedReason {
                    quota: quota.into(),
                    account: self.node(agent_id)?.account.clone(),
                    resets_at,
                })
            }
            Some(TurnRefusal::Failure { kind }) => FailedTurnDisposition::Failed { kind },
            None if model_kind == Some(plakous::model::ModelErrorKind::ContextLimit) => {
                FailedTurnDisposition::Failed {
                    kind: "context_exhausted",
                }
            }
            None => FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
        };
        let mut message = match read {
            Some(read) => {
                let mut detail = format!("child turn failed: {:?}", read.kind);
                if let Some(phase) = read.phase {
                    detail.push_str(&format!(" at {phase:?}"));
                }
                let mut provider = Vec::new();
                if let Some(status) = read.http_status {
                    provider.push(format!("HTTP {}", status.get()));
                }
                if let Some(code) = &read.provider_code {
                    provider.push(code.clone());
                }
                if !provider.is_empty() {
                    detail.push_str(&format!(" ({})", provider.join(" ")));
                }
                format!("{detail}: {}", read.message)
            }
            None => format!("child turn failed: {kind:?}"),
        };
        if matches!(
            disposition,
            FailedTurnDisposition::Failed {
                kind: "context_exhausted" | "refused"
            }
        ) {
            message.push_str(
                "; respawn with a handoff of completed work and the remaining task, not a \
                 follow-up into the same session",
            );
        }
        if let Some(notice) = self
            .operations
            .get(&failed_turn_notice_id(
                agent_id,
                attempt_id.as_str(),
                turn_id,
            ))
            .filter(|operation| operation.tool_name == FAILED_TURN_NOTICE_KIND)
            .and_then(|operation| operation.terminal_response.as_deref())
        {
            message.push_str(notice);
        }
        if let Some(attempt) = self.attempts.get_mut(&attempt_id) {
            attempt.ended_at.get_or_insert(committed_at_ms);
        }
        self.record_attempt_failure(
            agent_id,
            Some(&attempt_id),
            disposition,
            message,
            true,
            committed_at_ms,
        )
    }

    /// Folds a child turn's usage onto the attempt its command served (spec
    /// 03 §10, §14.3): tool calls and tokens add up, and the attempt's elapsed
    /// time reaches at least the terminal's commit. The turn itself was
    /// counted when its command bound it. A turn no recorded command of
    /// `agent_id` started folds onto the current attempt only while that
    /// attempt binds it; the root's turns fold nothing.
    pub(super) fn fold_turn_usage(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        command_id: &str,
        usage: plakous::agent::TurnUsage,
        committed_at_ms: u64,
    ) {
        if agent_id == self.root_agent_id {
            return;
        }
        let commanded = self
            .commands
            .get(command_id)
            .filter(|command| command.recipient == agent_id)
            .and_then(|command| command.attempt_id.clone());
        let Some(attempt_id) = commanded.or_else(|| {
            self.current_attempt(agent_id)
                .filter(|attempt| attempt.turn_id.as_deref() == Some(turn_id))
                .map(|attempt| attempt.attempt_id.clone())
        }) else {
            return;
        };
        let Some(attempt) = self.attempts.get_mut(&attempt_id) else {
            return;
        };
        let model = usage.model();
        let row = &mut attempt.usage;
        row.tool_calls = row
            .tool_calls
            .saturating_add(u32::try_from(usage.local_tool_attempts()).unwrap_or(u32::MAX));
        row.input_tokens = row.input_tokens.saturating_add(model.input_tokens());
        row.output_tokens = row.output_tokens.saturating_add(model.output_tokens());
        row.elapsed_ms = row
            .elapsed_ms
            .max(committed_at_ms.saturating_sub(attempt.started_at));
        self.bump();
    }

    /// A recovery closure of the bound turn of a child waiting on recovery
    /// settles `turn_abandoned` when its terminal read reports the
    /// abandonment.
    pub(super) fn apply_closure(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        committed_at_ms: u64,
        reads: &TreeFeedReads,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        let Some(read) = reads
            .terminal(turn_id)
            .filter(|read| read.abandoned.is_some())
        else {
            return Ok(());
        };
        if agent_id == self.root_agent_id
            || !matches!(self.recovery_need(agent_id)?, RecoveryNeed::Session { .. })
        {
            return Ok(());
        }
        let Some(attempt) = self
            .current_attempt(agent_id)
            .filter(|attempt| attempt.turn_id.as_deref() == Some(turn_id))
        else {
            return Ok(());
        };
        let attempt_id = attempt.attempt_id.clone();
        let stopped = attempt.status.is_stopped();
        if self.node(agent_id)?.active_turn {
            self.set_active_turn(agent_id, false)?;
        }
        self.settle_abandoned(
            agent_id,
            &attempt_id,
            stopped,
            true,
            String::new(),
            Some(read),
            committed_at_ms,
            projection,
        )
    }

    #[allow(clippy::too_many_arguments, reason = "one settlement's inputs")]
    fn settle_abandoned(
        &mut self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        stopped: bool,
        recovering: bool,
        fallback: String,
        read: Option<&TerminalRead>,
        committed_at_ms: u64,
        projection: &mut TreeProjection,
    ) -> Result<(), TopologyError> {
        let message = read.map_or(fallback, |read| read.message.clone());
        if recovering {
            self.apply_host_recovery(
                agent_id,
                &host_recovery::HostRecovery {
                    attempt_id: attempt_id.clone(),
                    resolution: RecoveryResolution::TurnAbandoned { message },
                    observed_at_ms: committed_at_ms,
                    closed: self
                        .current_attempt(agent_id)
                        .is_some_and(|attempt| attempt.status == AttemptStatus::Closed),
                    submitted_retirement: false,
                },
                true,
            )?;
        } else if !stopped
            && self.node(agent_id)?.status != NodeStatus::Closed
            && self
                .current_attempt(agent_id)
                .is_some_and(|attempt| attempt.status == AttemptStatus::Running)
        {
            self.record_attempt_failure(
                agent_id,
                Some(attempt_id),
                FailedTurnDisposition::Failed {
                    kind: TURN_ABANDONED,
                },
                message,
                true,
                committed_at_ms,
            )?;
        } else {
            return Ok(());
        }
        projection.abandoned.push(agent_id.to_owned());
        if let Some(abandoned) = read.and_then(|read| read.abandoned.clone()) {
            self.record_abandoned_turn(agent_id, abandoned)?;
        }
        Ok(())
    }
}
