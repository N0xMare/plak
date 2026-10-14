use super::{AttemptStatus, ProductAgentTree, RecoveryNeed, RecoveryResolution, TopologyError};
use crate::protocol::AttemptId;

pub(crate) const HOST_RECOVERY_KIND: &str = "host_recovery";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(in crate::tree) struct HostRecovery {
    pub(in crate::tree) attempt_id: AttemptId,
    pub(in crate::tree) resolution: RecoveryResolution,
    pub(in crate::tree) observed_at_ms: u64,
    pub(in crate::tree) closed: bool,
    pub(in crate::tree) submitted_retirement: bool,
}

impl ProductAgentTree {
    /// Saves the host's exact-open/release recovery decision. The first
    /// decision for an attempt stands; the write changes no attempt view.
    ///
    /// # Errors
    /// Refuses an attempt mismatch or a recovery that only VCS can settle.
    pub fn record_host_recovery(
        &mut self,
        agent_id: &str,
        attempt_id: &AttemptId,
        resolution: RecoveryResolution,
    ) -> Result<bool, TopologyError> {
        self.require_fold_started(agent_id)?;
        self.require_expected_attempt(agent_id, Some(attempt_id))?;
        let id = format!("plak:host-recovery:{agent_id}:{attempt_id}");
        match self.recovery_need(agent_id)? {
            RecoveryNeed::NotRequired => return Ok(false),
            RecoveryNeed::Workspace { code, reason } => {
                return Err(TopologyError::new(code, reason));
            }
            RecoveryNeed::Session { .. } => {}
        }
        if self.operations.contains_key(&id) {
            return Ok(false);
        }
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(
                super::TopologyErrorCode::GrantDenied,
                "agent has no current attempt",
            )
        })?;
        let decision = HostRecovery {
            attempt_id: attempt_id.clone(),
            observed_at_ms: attempt
                .ended_at
                .unwrap_or_else(super::super::now_unix_millis),
            closed: attempt.status == AttemptStatus::Closed,
            submitted_retirement: matches!(
                resolution,
                RecoveryResolution::RecoveredWithoutTurn { .. }
            ) && attempt.output_json.is_some()
                && attempt
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.kind == "session_retirement"),
            resolution,
        };
        let canonical = super::super::canonical_json(&decision)?;
        // The recovery and its opening-envelope retirement publish together.
        self.record_recovered_followup_mail(attempt_id)?;
        self.operations.insert(
            id.clone(),
            super::super::OperationRecord {
                fold: Some(self.fold_anchor_now(agent_id)?),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: HOST_RECOVERY_KIND.to_owned(),
                request_digest: super::super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(true)
    }

    pub(super) fn join_host_recovery(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let intents: Vec<_> = self
            .operations
            .get(operation_id)
            .into_iter()
            .filter(|op| {
                op.operation_id == operation_id
                    && op.tool_name == HOST_RECOVERY_KIND
                    && op.caller_agent_id == agent_id
            })
            .cloned()
            .collect();
        for intent in intents {
            let decision: HostRecovery =
                serde_json::from_str(&intent.canonical_request).map_err(|_| {
                    TopologyError::new(
                        super::TopologyErrorCode::OperationRecoveryRequired,
                        "host recovery intent is invalid",
                    )
                })?;
            let Some(attempt) = self.attempts.get(&decision.attempt_id) else {
                continue;
            };
            if attempt.status == AttemptStatus::Adopted
                || attempt.status == AttemptStatus::Closed && !decision.closed
            {
                continue;
            }
            if self.node(agent_id)?.attempt_id.as_ref() != Some(&decision.attempt_id) {
                self.apply_host_recovery_attempt(&decision);
                if let Some(operation) = self.operations.get_mut(&intent.operation_id) {
                    operation.terminal_response = Some("effects_reserved".to_owned());
                }
                self.bump();
                continue;
            }
            self.apply_host_recovery(agent_id, &decision, intent.terminal_response.is_none())?;
            if let Some(operation) = self.operations.get_mut(&intent.operation_id) {
                operation.terminal_response = Some("effects_reserved".to_owned());
            }
        }
        self.project_mail_decisions(agent_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Records and projects a topology-only fixture's host decision.
    ///
    /// # Errors
    /// Returns the intent writer's or projector's refusal.
    pub fn recover_host_decision(
        &mut self,
        agent_id: &str,
        attempt_id: &AttemptId,
        resolution: RecoveryResolution,
    ) -> Result<super::super::RecoverAgentOutput, TopologyError> {
        let recorded = self.record_host_recovery(agent_id, attempt_id, resolution)?;
        self.fold_due(agent_id)?;
        self.recover_receipt(agent_id, Some(attempt_id), recorded)
    }
}
