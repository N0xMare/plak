use super::{
    FeedFact, FeedRecord, FeedTurnOutcome, NodeStatus, ProductAgentTree, TopologyError,
    TopologyErrorCode,
};

pub(crate) const CHILD_FINALIZATION_KIND: &str = "child_finalization";

/// The host's decision after an exact child turn ended. It is not a runtime fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ChildFinalizationDecision {
    /// Live work keeps the attempt open for a wake.
    Park,
    /// The next corrective Start may be submitted.
    Nudge,
    /// Continue this attempt after a failed terminal, with a separately spent budget.
    Resume {
        /// One-based continuation round, immutable across reopen.
        round: u8,
    },
    /// End the attempt and publish its result after sealing.
    Finalize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ChildFinalization {
    attempt_id: crate::protocol::AttemptId,
    turn_id: String,
    decision: ChildFinalizationDecision,
    owner_released: bool,
    terminal: Option<ChildTerminal>,
    turns_spent: bool,
}

/// Exact committed terminal evidence returned by the tree projector.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChildTerminal {
    /// Attempt served by the terminal command.
    pub attempt_id: crate::protocol::AttemptId,
    /// Exact ended runtime turn.
    pub turn_id: String,
    /// Runtime commit timestamp, in milliseconds since the Unix epoch.
    pub committed_at_ms: u64,
    /// Authoritative runtime outcome.
    pub outcome: FeedTurnOutcome,
    /// Sequence of the terminal record in the child session feed.
    pub seq: u64,
}

impl ProductAgentTree {
    /// Records the host decision, keyed by child, attempt and exact ended turn.
    /// No view or external effect is changed by this write.
    ///
    /// # Errors
    /// Refuses an unknown attempt or a different decision under the same key.
    pub fn record_child_finalization(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        decision: ChildFinalizationDecision,
        owner_released: bool,
        terminal: Option<ChildTerminal>,
    ) -> Result<(), TopologyError> {
        self.require_fold_started(agent_id)?;
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
        })?;
        if (decision == ChildFinalizationDecision::Finalize
            && !terminal.as_ref().is_some_and(|evidence| {
                evidence.attempt_id == attempt.attempt_id && evidence.turn_id == turn_id
            }))
            || (decision != ChildFinalizationDecision::Finalize && terminal.is_some())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child finalization requires its exact committed terminal",
            ));
        }
        let id = format!(
            "{CHILD_FINALIZATION_KIND}:{agent_id}:{}:{turn_id}:{decision:?}",
            attempt.attempt_id
        );
        if let Some(existing) = self.operations.get(&id) {
            let prior: ChildFinalization = serde_json::from_str(&existing.canonical_request)
                .map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "child finalization intent is invalid",
                    )
                })?;
            if prior.owner_released == owner_released
                && prior.decision == decision
                && prior.attempt_id == attempt.attempt_id
                && prior.turn_id == turn_id
                && prior.terminal == terminal
            {
                return Ok(());
            }
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child turn already has a different finalization decision",
            ));
        }
        let intent = ChildFinalization {
            attempt_id: attempt.attempt_id.clone(),
            turn_id: turn_id.to_owned(),
            decision,
            owner_released,
            terminal,
            turns_spent: decision == ChildFinalizationDecision::Finalize
                && self.attempt_turns_spent(agent_id),
        };
        if let ChildFinalizationDecision::Resume { round } = decision
            && (round == 0
                || round > 2
                || round
                    != self
                        .transient_resume_count(agent_id, &intent.attempt_id)?
                        .saturating_add(1))
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "transient resume round is not the next bounded round",
            ));
        }
        if decision != ChildFinalizationDecision::Finalize
            && self
                .child_finalizations(agent_id, &intent.attempt_id)?
                .iter()
                .any(|prior| {
                    prior.turn_id == turn_id
                        && prior.decision != ChildFinalizationDecision::Finalize
                })
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child turn cannot both park, nudge or resume",
            ));
        }
        let canonical = super::super::canonical_json(&intent)?;
        self.operations.insert(
            id.clone(),
            super::super::OperationRecord {
                fold: Some(self.fold_anchor_now(agent_id)?),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: CHILD_FINALIZATION_KIND.to_owned(),
                request_digest: super::super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    fn child_finalizations(
        &self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
    ) -> Result<Vec<ChildFinalization>, TopologyError> {
        self.operations
            .values()
            .filter(|operation| {
                operation.caller_agent_id == agent_id
                    && operation.tool_name == CHILD_FINALIZATION_KIND
            })
            .map(|operation| {
                serde_json::from_str::<ChildFinalization>(&operation.canonical_request).map_err(
                    |_| {
                        TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "child finalization intent is invalid",
                        )
                    },
                )
            })
            .filter_map(|intent| match intent {
                Ok(intent) if &intent.attempt_id == attempt_id => Some(Ok(intent)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Continuation rounds spent by immutable decisions, not corrective commands.
    pub fn transient_resume_count(
        &self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
    ) -> Result<u8, TopologyError> {
        Ok(self
            .child_finalizations(agent_id, attempt_id)?
            .iter()
            .filter_map(|intent| {
                if let ChildFinalizationDecision::Resume { round } = intent.decision {
                    Some(round)
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0))
    }

    pub(super) fn has_transient_resume(
        &self,
        agent_id: &str,
        attempt_id: &crate::protocol::AttemptId,
        turn_id: &str,
    ) -> Result<bool, TopologyError> {
        Ok(self
            .child_finalizations(agent_id, attempt_id)?
            .iter()
            .any(|intent| {
                intent.turn_id == turn_id
                    && matches!(intent.decision, ChildFinalizationDecision::Resume { .. })
            }))
    }

    /// A saved resume decision with no admitted continuation. Reopen re-enters
    /// its gate and submits the same identity, never a generation-dependent one.
    pub fn pending_transient_resume(
        &self,
        agent_id: &str,
    ) -> Result<Option<(String, u8)>, TopologyError> {
        let Some(attempt) = self
            .current_attempt(agent_id)
            .filter(|attempt| attempt.status == super::AttemptStatus::Running)
        else {
            return Ok(None);
        };
        let latest = self
            .child_finalizations(agent_id, &attempt.attempt_id)?
            .into_iter()
            .filter_map(|intent| {
                if let ChildFinalizationDecision::Resume { round } = intent.decision {
                    Some((intent.turn_id, round))
                } else {
                    None
                }
            })
            .max_by_key(|(_, round)| *round);
        let Some((turn_id, round)) = latest else {
            return Ok(None);
        };
        let command_id = format!(
            "plak:resume-transient:{agent_id}:{}:{round}",
            attempt.attempt_id
        );
        if self
            .commands
            .get(&command_id)
            .is_some_and(|command| command.accepted_seq.is_some())
        {
            return Ok(None);
        }
        Ok(Some((turn_id, round)))
    }

    /// Extracts terminal evidence from one bounded feed record and command rows.
    /// Does not fold intents, change views, or reserve effects.
    pub fn child_terminal_evidence(
        &self,
        agent_id: &str,
        record: &FeedRecord,
    ) -> Vec<ChildTerminal> {
        let mut terminals = Vec::new();
        for fact in record.facts() {
            let FeedFact::TurnTerminal {
                turn_id,
                command_id,
                outcome,
                ..
            } = fact
            else {
                continue;
            };
            let Some(attempt_id) = self
                .commands
                .get(command_id.as_str())
                .filter(|command| command.recipient == agent_id)
                .and_then(|command| command.attempt_id.clone())
                .or_else(|| {
                    self.current_attempt(agent_id)
                        .filter(|attempt| {
                            attempt.turn_id.as_deref() == Some(turn_id.to_string().as_str())
                        })
                        .map(|attempt| attempt.attempt_id.clone())
                })
            else {
                continue;
            };
            terminals.push(ChildTerminal {
                attempt_id,
                turn_id: turn_id.to_string(),
                committed_at_ms: record.header().committed_at_ms,
                outcome: *outcome,
                seq: record.header().seq.get(),
            });
        }
        terminals
    }

    pub(super) fn join_child_finalization(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let Some(operation) = self.operations.get(operation_id) else {
            return Ok(());
        };
        let reserve_effects = operation.terminal_response.is_none();
        let intent: ChildFinalization = serde_json::from_str(&operation.canonical_request)
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "child finalization intent is invalid",
                )
            })?;
        let Some(terminal) = &intent.terminal else {
            return Ok(());
        };
        if terminal.attempt_id != intent.attempt_id || terminal.turn_id != intent.turn_id {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child finalization terminal differs from its intent",
            ));
        }
        let id = format!(
            "{CHILD_FINALIZATION_KIND}:{agent_id}:{}:{}:Finalize",
            terminal.attempt_id, terminal.turn_id
        );
        if id != operation_id {
            return Ok(());
        }
        let current = self
            .current_attempt(agent_id)
            .is_some_and(|attempt| attempt.attempt_id == intent.attempt_id);
        if self
            .attempts
            .get(&intent.attempt_id)
            .is_some_and(|attempt| attempt.status.is_stopped())
            && (!current || self.node(agent_id)?.status != NodeStatus::Closed)
        {
            return Ok(());
        }
        if intent.decision == ChildFinalizationDecision::Finalize
            && (!current || self.node(agent_id)?.status != NodeStatus::RecoveryRequired)
            && intent.owner_released
            && !matches!(
                terminal.outcome,
                FeedTurnOutcome::Failed { .. } | FeedTurnOutcome::RecoveryRequired
            )
        {
            self.record_attempt_completed_at(
                agent_id,
                &intent.attempt_id,
                terminal.committed_at_ms,
                reserve_effects,
                Some(intent.turns_spent),
            )?;
            // A racing close could not seal while the waiter owned the copy.
            // This effect is fenced by the saved intent, not by active_turn.
            if reserve_effects && current && self.node(agent_id)?.status == NodeStatus::Closed {
                self.begin_seal(agent_id);
            }
            if reserve_effects {
                if let Some(operation) = self.operations.get_mut(&id) {
                    operation.terminal_response = Some("effects_reserved".to_owned());
                }
                self.bump();
            }
        }
        Ok(())
    }
}
