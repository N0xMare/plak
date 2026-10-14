use super::{AttemptStatus, NodeStatus, ProductAgentTree, TopologyError, TopologyErrorCode};
use crate::protocol::AttemptId;
use crate::tree::{AttemptFailure, OperationRecord, bounded_text, canonical_json, digest_text};

pub(in crate::tree) const HOST_DECISION_KIND: &str = "host_decision";
pub(in crate::tree) const SESSION_BOUND_KIND: &str = "session_bound_after_failure";

#[derive(serde::Serialize, serde::Deserialize)]
struct SessionBound {
    attempt_id: AttemptId,
    materialization_id: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Decision {
    Cancelled,
    Retirement,
    Materialization,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct HostDecision {
    attempt_id: Option<AttemptId>,
    turn_id: Option<String>,
    decision: Decision,
    observed_at_ms: u64,
    closed: bool,
    submitted: bool,
    failure: AttemptFailure,
}

impl ProductAgentTree {
    /// Records a successful bind after materialization failed. Save this
    /// intent before projecting it; recording never changes an attempt view.
    ///
    /// # Errors
    /// Refuses a missing session or invalid materialization intent.
    pub(crate) fn record_session_bound_after_failure(
        &mut self,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        self.require_fold_started(agent_id)?;
        let node = self.node(agent_id)?;
        if node.session_locator.is_none()
            && !self
                .operations
                .contains_key(&format!("{}:{agent_id}", super::CHILD_SESSION_KIND))
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "session locator is missing",
            ));
        }
        let materializations = self
            .operations
            .values()
            .filter(|op| op.tool_name == HOST_DECISION_KIND && op.caller_agent_id == agent_id)
            .map(|op| {
                serde_json::from_str::<HostDecision>(&op.canonical_request).map(|intent| {
                    (
                        op.operation_id.clone(),
                        op.fold.map_or(0, |fold| fold.order),
                        intent,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "host decision intent is invalid",
                )
            })?;
        let Some((materialization_id, _, failure)) = materializations
            .into_iter()
            .filter(|(_, _, intent)| {
                matches!(intent.decision, Decision::Materialization)
                    && intent.attempt_id == node.attempt_id
                    && !intent.submitted
            })
            .max_by_key(|(_, order, _)| *order)
        else {
            return Ok(());
        };
        let Some(attempt_id) = failure.attempt_id else {
            return Ok(());
        };
        let id = format!("plak:session-bound:{materialization_id}");
        if self.operations.contains_key(&id) {
            return Ok(());
        }
        let canonical = canonical_json(&SessionBound {
            attempt_id,
            materialization_id,
        })?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                fold: Some(self.fold_anchor_now(agent_id)?),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: SESSION_BOUND_KIND.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    fn bind_generation(
        &self,
        agent_id: &str,
        attempt_id: Option<&AttemptId>,
    ) -> Result<u64, TopologyError> {
        let restores = self
            .operations
            .values()
            .filter(|op| op.tool_name == SESSION_BOUND_KIND && op.caller_agent_id == agent_id)
            .map(|op| {
                serde_json::from_str::<SessionBound>(&op.canonical_request)
                    .map(|intent| (intent, op.fold.map_or(0, |fold| fold.order)))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "session bind intent is invalid",
                )
            })?;
        Ok(restores
            .into_iter()
            .filter(|(intent, _)| Some(&intent.attempt_id) == attempt_id)
            .map(|(_, order)| order)
            .max()
            .unwrap_or(0))
    }

    pub(super) fn join_session_bound(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let operation = &self.operations[operation_id];
        let intent: SessionBound =
            serde_json::from_str(&operation.canonical_request).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "session bind intent is invalid",
                )
            })?;
        let failure = self
            .operations
            .get(&intent.materialization_id)
            .ok_or_else(|| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "materialization intent is missing",
                )
            })?;
        let failure: HostDecision =
            serde_json::from_str(&failure.canonical_request).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "host decision intent is invalid",
                )
            })?;
        if self.has_faulted_vcs_obligation(agent_id) {
            return Ok(());
        }
        let Some(attempt) = self.attempts.get_mut(&intent.attempt_id) else {
            return Ok(());
        };
        if attempt.status != AttemptStatus::RecoveryRequired
            || attempt.failure.as_ref() != Some(&failure.failure)
            || attempt.ended_at != Some(failure.observed_at_ms)
            || attempt.output_json.is_some()
            || attempt.has_unresolved_session_failure()
        {
            return Ok(());
        }
        attempt.status = AttemptStatus::Running;
        attempt.failure = None;
        attempt.ended_at = None;
        if let Some(node) = self.nodes.get_mut(agent_id)
            && node.attempt_id.as_ref() == Some(&intent.attempt_id)
            && node.status == NodeStatus::RecoveryRequired
        {
            node.status = NodeStatus::Running;
            self.fault_reasons.remove(agent_id);
        }
        self.bump();
        Ok(())
    }

    /// Records cancellation outside the host as an immutable intent.
    ///
    /// # Errors
    /// Refuses an unknown agent or a missing attempt.
    pub fn record_host_cancellation(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
        })?;
        if attempt.status.is_stopped() && attempt.output_json.is_none() {
            return Ok(());
        }
        let message = self
            .fault_reasons
            .get(agent_id)
            .cloned()
            .unwrap_or_else(|| {
                "the turn was cancelled outside the host; nothing interrupted it".to_owned()
            });
        self.record_host_decision(
            agent_id,
            Decision::Cancelled,
            AttemptFailure {
                kind: super::super::CANCELLED_KIND.to_owned(),
                message: bounded_text(&message, 512),
            },
        )
    }

    /// Records uncertain owner retirement without claiming the copy stopped.
    ///
    /// # Errors
    /// Refuses an unknown agent.
    pub fn record_host_retirement(
        &mut self,
        agent_id: &str,
        message: String,
    ) -> Result<(), TopologyError> {
        let attempt = self.current_attempt(agent_id);
        let retirement_only = attempt.is_none_or(|attempt| {
            attempt.blocked.is_none()
                && attempt
                    .failure
                    .as_ref()
                    .is_none_or(|failure| failure.kind == "session_retirement")
        });
        let message = attempt
            .and_then(|attempt| attempt.failure.as_ref())
            .map_or_else(
                || message.clone(),
                |prior| {
                    if prior.message.ends_with(&message) {
                        prior.message.clone()
                    } else {
                        format!("{}; {message}", prior.message)
                    }
                },
            );
        self.record_host_decision(
            agent_id,
            Decision::Retirement,
            AttemptFailure {
                kind: if retirement_only {
                    "session_retirement"
                } else {
                    "turn_recovery_required"
                }
                .to_owned(),
                message,
            },
        )
    }

    /// A delayed joiner cannot fence a successor or undo recovery settlement.
    ///
    /// # Errors
    /// Refuses an unknown agent.
    pub fn record_host_retirement_for_attempt(
        &mut self,
        agent_id: &str,
        expected: Option<&AttemptId>,
        message: String,
    ) -> Result<bool, TopologyError> {
        self.require_fold_started(agent_id)?;
        let node = self.node(agent_id)?;
        let attempt = self.current_attempt(agent_id);
        if attempt.map(|attempt| &attempt.attempt_id) != expected
            || (!node.active_turn
                && attempt.is_some_and(|attempt| {
                    attempt.status.is_stopped() && !attempt.session_outcome_unresolved()
                }))
        {
            return Ok(false);
        }
        self.record_host_retirement(agent_id, message)?;
        Ok(true)
    }

    /// Records materialization failure, including the host's bounded diagnostic.
    ///
    /// # Errors
    /// Refuses an unknown agent.
    pub fn record_host_materialization(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        if self
            .current_attempt(agent_id)
            .is_some_and(super::super::AttemptRecord::has_unresolved_session_failure)
        {
            return Ok(());
        }
        let failure = AttemptFailure {
            kind: "session_materialization".to_owned(),
            message: self
                .fault_reasons
                .get(agent_id)
                .cloned()
                .unwrap_or_else(|| {
                    "the child session or its turn failed and left no result".to_owned()
                }),
        };
        self.record_host_decision(agent_id, Decision::Materialization, failure)
    }

    fn record_host_decision(
        &mut self,
        agent_id: &str,
        decision: Decision,
        failure: AttemptFailure,
    ) -> Result<(), TopologyError> {
        self.require_fold_started(agent_id)?;
        let node = self.node(agent_id)?;
        let attempt = self.current_attempt(agent_id);
        if node.status == NodeStatus::RecoveryRequired
            && attempt.is_some_and(|attempt| attempt.output_json.is_some())
            && matches!(decision, Decision::Cancelled | Decision::Materialization)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientRecoveryRequired,
                "submitted output cannot settle a recovery-required recipient",
            ));
        }
        let name = match decision {
            Decision::Cancelled => "cancelled",
            Decision::Retirement => "retirement",
            Decision::Materialization => "materialization",
        };
        let mut id = format!(
            "plak:host-decision:{agent_id}:{}:{}:{name}",
            attempt.map_or("none", |attempt| attempt.attempt_id.as_str()),
            attempt
                .and_then(|attempt| attempt.turn_id.as_deref())
                .unwrap_or("unbound")
        );
        if matches!(decision, Decision::Materialization) {
            id.push_str(&format!(
                ":bind-{}",
                self.bind_generation(agent_id, node.attempt_id.as_ref())?
            ));
        }
        if self.operations.contains_key(&id) {
            return Ok(());
        }
        let intent = HostDecision {
            attempt_id: attempt.map(|attempt| attempt.attempt_id.clone()),
            turn_id: attempt.and_then(|attempt| attempt.turn_id.clone()),
            decision,
            observed_at_ms: super::super::now_unix_millis(),
            closed: node.status == NodeStatus::Closed
                || attempt.is_some_and(|attempt| attempt.status == AttemptStatus::Closed),
            submitted: attempt.is_some_and(|attempt| attempt.output_json.is_some()),
            failure,
        };
        let canonical = canonical_json(&intent)?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                fold: Some(self.fold_anchor_now(agent_id)?),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: HOST_DECISION_KIND.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    pub(super) fn join_host_decision(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let op = &self.operations[operation_id];
        let intent: HostDecision = serde_json::from_str(&op.canonical_request).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "host decision intent is invalid",
            )
        })?;
        self.apply_host_decision(agent_id, &intent, op.terminal_response.is_none())?;
        Ok(())
    }

    fn apply_host_decision(
        &mut self,
        agent_id: &str,
        intent: &HostDecision,
        reserve_effects: bool,
    ) -> Result<bool, TopologyError> {
        let retirement = matches!(intent.decision, Decision::Retirement);
        let current = self.node(agent_id)?.attempt_id == intent.attempt_id;
        let reserve_effects = reserve_effects && current;
        if let Some(turn) = &intent.turn_id
            && let Some(attempt) = intent
                .attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get_mut(id))
        {
            attempt.turn_id = Some(turn.clone());
        }
        if !retirement
            && intent
                .attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get(id))
                .is_some_and(|attempt| attempt.status.is_stopped())
        {
            return Ok(false);
        }
        if intent.submitted && !matches!(intent.decision, Decision::Retirement) {
            if let Some(attempt_id) = &intent.attempt_id {
                self.record_attempt_completed_at(
                    agent_id,
                    attempt_id,
                    intent.observed_at_ms,
                    reserve_effects,
                    None,
                )?;
            }
            return Ok(true);
        }
        if let Some(attempt) = intent
            .attempt_id
            .as_ref()
            .and_then(|id| self.attempts.get_mut(id))
        {
            attempt.status = if retirement && attempt.status.is_stopped() {
                attempt.status
            } else if retirement && intent.closed {
                AttemptStatus::Closed
            } else if matches!(intent.decision, Decision::Cancelled) {
                AttemptStatus::Failed
            } else {
                AttemptStatus::RecoveryRequired
            };
            if retirement {
                attempt.ended_at.get_or_insert(intent.observed_at_ms);
            } else {
                attempt.ended_at = Some(intent.observed_at_ms);
                attempt.usage.elapsed_ms = intent
                    .observed_at_ms
                    .saturating_sub(attempt.started_at)
                    .max(attempt.usage.elapsed_ms);
            }
            let failure = if retirement && let Some(prior) = &attempt.failure {
                if prior.message.contains(&intent.failure.message) {
                    prior.clone()
                } else if intent.failure.message.contains(&prior.message) {
                    intent.failure.clone()
                } else {
                    AttemptFailure {
                        kind: intent.failure.kind.clone(),
                        message: format!("{}; {}", prior.message, intent.failure.message),
                    }
                }
            } else {
                intent.failure.clone()
            };
            attempt.failure = Some(failure);
        }
        if current && let Some(node) = self.nodes.get_mut(agent_id) {
            if !retirement || intent.turn_id.is_none() {
                node.active_turn = false;
            }
            if retirement || reserve_effects || node.status != NodeStatus::Sealing {
                node.status = if matches!(intent.decision, Decision::Cancelled) {
                    NodeStatus::Failed
                } else {
                    NodeStatus::RecoveryRequired
                };
            }
        }
        if current {
            self.record_fault(agent_id, intent.failure.message.clone());
        }
        if current && matches!(intent.decision, Decision::Cancelled) {
            self.retire_wait_receipts(agent_id);
            if reserve_effects {
                self.record_close_live_descendants(agent_id, intent.observed_at_ms)?;
                if !self.begin_seal(agent_id) {
                    self.push_result_unless_sealing(agent_id);
                }
            }
        } else if !retirement && reserve_effects {
            self.begin_seal(agent_id);
        }
        Ok(true)
    }
}
