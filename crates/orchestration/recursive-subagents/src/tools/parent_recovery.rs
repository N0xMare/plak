//! Settle acknowledged child uncertainty before opening a fresh followup attempt.

use super::*;

#[cfg(test)]
mod presentation_tests;

pub(super) fn present_refusal(
    outcome: ToolOutcome,
    definition: &ToolDefinition,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if definition.name().as_str() != "send_agent_message" {
        return Ok(outcome);
    }
    let presented_acknowledgement = match definition.input() {
        ToolInputContract::Structured(schema) => {
            serde_json::from_slice::<Value>(schema.document().canonical_bytes())
                .is_ok_and(|document| !document["properties"]["acknowledge_recovery"].is_null())
        }
        _ => false,
    };
    if outcome.is_success() || presented_acknowledgement {
        return Ok(outcome);
    }
    let Ok(mut refusal) = serde_json::from_str::<Value>(&outcome.joined_text()) else {
        return Ok(outcome);
    };
    let message = refusal["message"].as_str().unwrap_or_default();
    if !message.contains("acknowledge_recovery") {
        return Ok(outcome);
    }
    refusal["message"] = Value::String("recipient has an unresolved lost turn; a tool may have changed this workspace. Ask the operator to Resume/recover the child, or rebind this session to the current tool definitions before acknowledging recovery. No fresh attempt was admitted.".into());
    ToolOutcome::failure_text(refusal.to_string())
}

pub(super) enum FollowupRecovery {
    Skipped,
    Settled,
    Refused(ToolOutcome),
}

impl TopologyTool {
    pub(super) async fn recover_followup(
        &self,
        caller: &str,
        request: &SendAgentMessageRequest,
        operation: &str,
    ) -> Result<FollowupRecovery, ToolRuntimeError> {
        if request.acknowledge_recovery.is_none() {
            return Ok(FollowupRecovery::Skipped);
        }
        let admitted = self
            .driver
            .read(|tree| tree.parent_followup_recovery(caller, request, Some(operation)))
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let expected = match admitted {
            Ok(Some(expected)) => expected,
            Ok(None) => return Ok(FollowupRecovery::Skipped),
            Err(error) => return Self::topology_failure(&error).map(FollowupRecovery::Refused),
        };
        let recovered = match self.deliverer() {
            Some(controller) => {
                controller
                    .recover_followup(request.agent_id.clone(), expected)
                    .await
            }
            None => Err(TopologyError::new(
                TopologyErrorCode::RecoverySessionUnavailable,
                "the host has no child recovery service; no fresh attempt was admitted",
            )),
        };
        match recovered {
            Ok(()) => Ok(FollowupRecovery::Settled),
            Err(mut error) => {
                error.message = format!(
                    "{} Recovery was refused: {}",
                    crate::tree::text::PARENT_RECOVERY_WARNING,
                    error.message
                );
                Self::topology_failure(&error).map(FollowupRecovery::Refused)
            }
        }
    }

    pub(super) fn warn_recovered_refusal(
        outcome: ToolOutcome,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        if outcome.is_success() {
            return Ok(outcome);
        }
        let Some(plakous::model::ToolResultPart::Text(block)) = outcome.parts().first() else {
            return Ok(outcome);
        };
        let mut refusal: Value = serde_json::from_str(block.text()).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.to_string())
        })?;
        refusal["message"] = Value::String(format!(
            "{} Recovery settled before this followup was refused; a recovered lost turn remains interrupted/held. {}",
            crate::tree::text::PARENT_RECOVERY_WARNING,
            refusal["message"].as_str().unwrap_or_default(),
        ));
        ToolOutcome::failure_text(refusal.to_string())
    }
}
