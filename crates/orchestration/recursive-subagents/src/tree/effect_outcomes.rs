//! Agent-local view consequences of acknowledged workspace effects.

use super::{
    AttemptFailure, AttemptId, AttemptStatus, NodeStatus, OperationRecord, ProductAgentTree,
    TopologyError, TopologyErrorCode, canonical_json, digest_text,
};

pub(super) const SPAWN_REFUSED_KIND: &str = "spawn_refused";
pub(super) const ATTEMPT_ADOPTED_KIND: &str = "attempt_adopted";
pub(super) const WORKSPACE_CLEANUP_KIND: &str = "workspace_cleanup";

#[derive(serde::Serialize, serde::Deserialize)]
struct EffectOutcome {
    attempt_id: Option<AttemptId>,
    outcome: Outcome,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum Outcome {
    SpawnRefused {
        ended_at_ms: u64,
        failure: AttemptFailure,
        diagnostic: String,
    },
    AttemptAdopted,
    WorkspaceCleanup {
        failure: AttemptFailure,
        diagnostic: String,
    },
}

impl ProductAgentTree {
    pub(super) fn record_effect_outcome(
        &mut self,
        agent_id: &str,
        attempt_id: Option<AttemptId>,
        outcome: Outcome,
    ) -> Result<(), TopologyError> {
        let kind = match &outcome {
            Outcome::SpawnRefused { .. } => SPAWN_REFUSED_KIND,
            Outcome::AttemptAdopted => ATTEMPT_ADOPTED_KIND,
            Outcome::WorkspaceCleanup { .. } => WORKSPACE_CLEANUP_KIND,
        };
        let anchor = self.fold_anchor_now(agent_id)?;
        let id = format!("plak:{kind}:{agent_id}:{}", anchor.order);
        let canonical = canonical_json(&EffectOutcome {
            attempt_id,
            outcome,
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
        self.fold_due(agent_id)?;
        Ok(())
    }

    pub(super) fn join_effect_outcome(
        &mut self,
        agent_id: &str,
        id: &str,
    ) -> Result<(), TopologyError> {
        let intent: EffectOutcome = serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "workspace outcome intent is invalid",
                )
            })?;
        let current = self.node(agent_id)?.attempt_id == intent.attempt_id;
        let attempt = intent
            .attempt_id
            .as_ref()
            .and_then(|id| self.attempts.get_mut(id));
        match intent.outcome {
            Outcome::SpawnRefused {
                ended_at_ms,
                failure,
                diagnostic,
            } => {
                if let Some(attempt) = attempt
                    && !attempt.status.is_stopped()
                {
                    attempt.status = AttemptStatus::Failed;
                    attempt.ended_at = Some(ended_at_ms);
                    attempt.failure = Some(failure);
                }
                if current {
                    if let Some(node) = self.nodes.get_mut(agent_id) {
                        node.status = NodeStatus::Failed;
                        node.active_turn = false;
                    }
                    self.fault_reasons.insert(agent_id.to_owned(), diagnostic);
                }
            }
            Outcome::AttemptAdopted => {
                if let Some(attempt) = attempt
                    && !attempt.has_unresolved_session_failure()
                {
                    attempt.status = AttemptStatus::Adopted;
                }
            }
            Outcome::WorkspaceCleanup {
                failure,
                diagnostic,
            } => {
                if let Some(attempt) = attempt {
                    attempt.status = AttemptStatus::RecoveryRequired;
                    attempt.failure = Some(failure);
                }
                if current {
                    if let Some(node) = self.nodes.get_mut(agent_id) {
                        node.status = NodeStatus::RecoveryRequired;
                    }
                    self.fault_reasons.insert(agent_id.to_owned(), diagnostic);
                }
            }
        }
        self.bump();
        Ok(())
    }
}
