//! Captured tool and operator decisions, folded into agent-local views.

use super::{
    AttemptFailure, AttemptId, AttemptStatus, NodeStatus, OperationRecord, ProductAgentTree,
    TopologyError, TopologyErrorCode, canonical_json, digest_text,
};

pub(super) const INTERRUPT_KIND: &str = "interrupt";
pub(super) const RECOVERY_ABANDONED_KIND: &str = "recovery_abandoned";
pub(super) const CLOSE_KIND: &str = "close";
pub(super) const SUBMITTED_KIND: &str = "submitted";

#[derive(serde::Serialize, serde::Deserialize)]
struct OperatorIntent {
    attempt_id: Option<AttemptId>,
    decision: Decision,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum Decision {
    Submitted {
        output_json: String,
    },
    Close {
        held_recovery_required: bool,
        terminal_refusal: Option<AttemptFailure>,
        retain_sealing: bool,
        defer_seal: bool,
        closed_at_ms: u64,
    },
    Interrupt {
        effective: bool,
    },
    RecoveryAbandoned {
        abandoned_at_ms: u64,
        failure: AttemptFailure,
    },
}

impl ProductAgentTree {
    pub(super) fn record_operator_intent(
        &mut self,
        agent_id: &str,
        attempt_id: Option<AttemptId>,
        decision: Decision,
    ) -> Result<(), TopologyError> {
        self.insert_operator_intent(agent_id, attempt_id, decision)?;
        self.fold_due(agent_id)?;
        Ok(())
    }

    fn insert_operator_intent(
        &mut self,
        agent_id: &str,
        attempt_id: Option<AttemptId>,
        decision: Decision,
    ) -> Result<(), TopologyError> {
        let kind = match &decision {
            Decision::Submitted { .. } => SUBMITTED_KIND,
            Decision::Close { .. } => CLOSE_KIND,
            Decision::Interrupt { .. } => INTERRUPT_KIND,
            Decision::RecoveryAbandoned { .. } => RECOVERY_ABANDONED_KIND,
        };
        let anchor = self.fold_anchor_now(agent_id)?;
        let id = format!("plak:{kind}:{agent_id}:{}", anchor.order);
        let canonical = canonical_json(&OperatorIntent {
            attempt_id,
            decision,
        })?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                fold: Some(anchor),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: kind.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    pub(super) fn record_close_target(
        &mut self,
        agent_id: &str,
        closed_at_ms: u64,
    ) -> Result<(), TopologyError> {
        let terminal_refusal = self
            .terminal_recovery_refusal(agent_id)
            .cloned()
            .filter(|_| {
                self.current_attempt(agent_id)
                    .is_some_and(super::AttemptRecord::session_outcome_unresolved)
            });
        let held_recovery_required = terminal_refusal.is_none()
            && self
                .current_attempt(agent_id)
                .is_some_and(super::AttemptRecord::has_unresolved_session_failure);
        self.insert_operator_intent(
            agent_id,
            self.node(agent_id)?.attempt_id.clone(),
            Decision::Close {
                held_recovery_required,
                terminal_refusal,
                retain_sealing: self.seal_pending(agent_id),
                defer_seal: self.has_turn_owner(agent_id),
                closed_at_ms,
            },
        )
    }

    pub(super) fn join_operator_intent(
        &mut self,
        agent_id: &str,
        id: &str,
    ) -> Result<(), TopologyError> {
        let intent: OperatorIntent = serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "operator intent is invalid",
            )
        })?;
        let current = self.node(agent_id)?.attempt_id == intent.attempt_id;
        let reserve_effects = self.operations[id].terminal_response.is_none();
        match intent.decision {
            Decision::Submitted { output_json } => {
                if let Some(attempt) = intent
                    .attempt_id
                    .as_ref()
                    .and_then(|id| self.attempts.get_mut(id))
                {
                    attempt.output_json = Some(output_json);
                    attempt.status = AttemptStatus::Submitted;
                }
            }
            Decision::Close {
                held_recovery_required,
                terminal_refusal,
                retain_sealing,
                defer_seal,
                closed_at_ms,
            } => {
                if held_recovery_required {
                    return Ok(());
                }
                let settled_terminal = terminal_refusal.is_some();
                if let Some(attempt) = intent
                    .attempt_id
                    .as_ref()
                    .and_then(|id| self.attempts.get_mut(id))
                {
                    if let Some(failure) = terminal_refusal {
                        attempt.status = AttemptStatus::Failed;
                        attempt.ended_at.get_or_insert(closed_at_ms);
                        attempt.failure = Some(failure);
                    }
                    if !attempt.status.is_stopped() || retain_sealing {
                        attempt.status = AttemptStatus::Closed;
                        attempt.ended_at.get_or_insert(closed_at_ms);
                    }
                }
                if current {
                    // Replay must reconstruct the seal this close already reserved.
                    let sealing = retain_sealing || self.seal_effect_pending(agent_id);
                    if let Some(node) = self.nodes.get_mut(agent_id) {
                        node.status = if sealing {
                            NodeStatus::Sealing
                        } else {
                            NodeStatus::Closed
                        };
                        if settled_terminal {
                            node.active_turn = false;
                        }
                    }
                    if reserve_effects {
                        self.terminal_recovery.remove(agent_id);
                        self.parked.remove(agent_id);
                        if !defer_seal {
                            self.begin_seal(agent_id);
                        }
                    }
                }
            }
            Decision::Interrupt { effective } => {
                if current && effective && self.node(agent_id)?.status == NodeStatus::Running {
                    if let Some(node) = self.nodes.get_mut(agent_id) {
                        node.status = NodeStatus::Interrupted;
                    }
                    if reserve_effects {
                        self.interrupt_park(agent_id);
                    }
                }
            }
            Decision::RecoveryAbandoned {
                abandoned_at_ms,
                failure,
            } => {
                if let Some(attempt) = intent
                    .attempt_id
                    .as_ref()
                    .and_then(|id| self.attempts.get_mut(id))
                {
                    attempt.status = AttemptStatus::Failed;
                    attempt.ended_at.get_or_insert(abandoned_at_ms);
                    attempt.failure = Some(failure);
                }
                if current {
                    if let Some(node) = self.nodes.get_mut(agent_id) {
                        node.status = NodeStatus::Closed;
                        node.active_turn = false;
                    }
                    self.terminal_recovery.remove(agent_id);
                    self.fault_reasons.remove(agent_id);
                }
            }
        }
        self.bump();
        Ok(())
    }
}
