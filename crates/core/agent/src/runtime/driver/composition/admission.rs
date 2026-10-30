use crate::AgentError;
use crate::AgentErrorKind;
use crate::AgentEventKind;
use crate::DurableOperationKind;
use crate::DurableOperationRecord;
use crate::DurableOperationState;
use crate::EventContext;
use crate::SessionStoreError;
use crate::ToolAuditIdentity;
use crate::ToolCallAttemptIdentity;
use crate::runtime::OperationId;
use crate::runtime::PromptCommand;
use crate::runtime::ToolExecutionResult;
use crate::runtime::composition::projection::local_composition_exec_registry_arguments;
use crate::runtime::driver::AuthorizationOutcome;
use crate::runtime::driver::Driver;
use crate::runtime::driver::ToolAdmission;
use crate::runtime::driver::composition::LocalCompositionFailureEvidence;
use crate::runtime::observation::clip_tool_result;
use crate::runtime::observation::normalized_tool_failure;
use plakous_model::Actor;
use plakous_model::AttemptId;
use plakous_model::ContentBlock;
use plakous_model::ConversationItem;
use plakous_model::EntryId;
use plakous_model::Provenance;
use plakous_model::TextBlock;
use plakous_model::ToolCallBlock;
use plakous_model::ToolResultBlock;
use plakous_model::ToolResultKind;
use plakous_model::ToolResultPart;
use plakous_model::ToolResultUpdateBlock;
use plakous_tools::ToolInvocationRequest;
use std::sync::Arc;

impl Driver {
    #[allow(
        clippy::too_many_lines,
        reason = "the composition marker and nested evidence must be checked together"
    )]
    pub(in crate::runtime::driver) fn cancelled_composition_result(
        &mut self,
        call: &ToolCallBlock,
        result_entry_id: EntryId,
        record: &DurableOperationRecord,
        live_yield: bool,
    ) -> Result<(ConversationItem, Vec<OperationId>), AgentError> {
        let evidence = match record.state() {
            DurableOperationState::Completed(evidence)
            | DurableOperationState::ProvenNotDispatched(evidence) => evidence,
            DurableOperationState::LocatorBound(evidence) if live_yield => evidence,
            DurableOperationState::Finalized(_) => record
                .transitions()
                .iter()
                .rev()
                .find_map(|transition| match transition.state() {
                    DurableOperationState::Completed(evidence)
                    | DurableOperationState::ProvenNotDispatched(evidence) => Some(evidence),
                    _ => None,
                })
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
            _ => {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "cancelled composition marker has an unresolved outcome",
                ));
            }
        };
        let (kind, parts, child_ids, updates) = if let Some(failure) =
            LocalCompositionFailureEvidence::decode(evidence.bytes())
        {
            const MAX_CANCELLED_REPORT_BYTES: usize = 1_048_576;
            let child_ids = failure.child_operations().to_vec();
            let detail = failure.detail();
            let mut report = format!("local composition was interrupted: {detail}");
            let ledger = self.authority.checkpoint();
            for child_id in &child_ids {
                let child = ledger
                    .manifest()
                    .ledger()
                    .operation(*child_id)
                    .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                let (DurableOperationState::Completed(evidence)
                | DurableOperationState::ProvenNotDispatched(evidence)
                | DurableOperationState::Finalized(evidence)) = child.state()
                else {
                    return Err(AgentError::new(
                        AgentErrorKind::OutcomeUnknown,
                        "cancelled composition has an unresolved nested effect",
                    ));
                };
                if let DurableOperationKind::Effect { call_id, .. } = child.kind() {
                    let outcome: serde_json::Value = serde_json::from_slice(evidence.bytes())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let outcome_fields = outcome
                        .as_array()
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
                    if outcome_fields.first().and_then(serde_json::Value::as_str)
                        == Some("completed-tool-outcome-v1")
                    {
                        let execution: ToolExecutionResult =
                            serde_json::from_value(outcome_fields.get(2).cloned().ok_or_else(
                                || AgentError::from_store(SessionStoreError::corrupt()),
                            )?)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let rendered = serde_json::to_string(&execution.result)
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                        let line = format!("\ncompleted nested call {call_id}: {rendered}");
                        let remaining = MAX_CANCELLED_REPORT_BYTES.saturating_sub(report.len());
                        if remaining == 0 {
                            break;
                        }
                        let mut end = remaining.min(line.len());
                        while !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        report.push_str(&line[..end]);
                        if end < line.len() {
                            report.push_str("\n[nested results truncated]");
                            break;
                        }
                    }
                }
            }
            (
                ToolResultKind::Failure,
                vec![ToolResultPart::Text(TextBlock::new(report, None).map_err(
                    |_| AgentError::from_store(SessionStoreError::corrupt()),
                )?)],
                child_ids,
                Vec::new(),
            )
        } else {
            let value: serde_json::Value = serde_json::from_slice(evidence.bytes())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let fields = value
                .as_array()
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let tag = fields
                .first()
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            match tag {
                "plakous-local-composition-cell-v2" if fields.len() == 8 => {
                    let child_ids: Vec<u64> = serde_json::from_value(fields[4].clone())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let parts: Vec<ToolResultPart> = serde_json::from_value(fields[7].clone())
                        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    let updates: Vec<ToolResultUpdateBlock> =
                        serde_json::from_value(fields[6].clone())
                            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
                    (ToolResultKind::Success, parts, child_ids, updates)
                }
                _ => return Err(AgentError::from_store(SessionStoreError::corrupt())),
            }
        };
        let result = ToolResultBlock::new(
            call.call_id(),
            call.owner().clone(),
            kind,
            parts,
            call.correlation().clone(),
        )
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let (result, _) = clip_tool_result(result, &mut self.observation_budget)?;
        let mut blocks = updates
            .into_iter()
            .map(ContentBlock::ToolResultUpdate)
            .collect::<Vec<_>>();
        blocks.push(ContentBlock::ToolResult(result));
        let item = ConversationItem::new(
            result_entry_id,
            Actor::Tool,
            blocks,
            Provenance::ToolExecution,
        )
        .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let ledger = self.authority.checkpoint();
        let mut operations = Vec::new();
        for child_id in child_ids {
            let child = ledger
                .manifest()
                .ledger()
                .operation(child_id)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            match child.state() {
                DurableOperationState::Completed(_)
                | DurableOperationState::ProvenNotDispatched(_) => {
                    operations.push(OperationId(child_id));
                }
                DurableOperationState::Finalized(_) => {}
                _ => {
                    return Err(AgentError::new(
                        AgentErrorKind::OutcomeUnknown,
                        "cancelled composition has an unresolved nested operation",
                    ));
                }
            }
        }
        Ok((item, operations))
    }

    pub(in crate::runtime::driver) async fn admit_local_composition_exec(
        &mut self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call: &ToolCallBlock,
    ) -> Result<ToolAdmission, AgentError> {
        let definition = self
            .tools
            .definitions()
            .iter()
            .find(|definition| definition.name() == call.canonical_name())
            .ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "local composition exec marker is absent from its frozen registry",
                )
            })?;
        let registry_arguments =
            match local_composition_exec_registry_arguments(call, definition.input()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return Ok(ToolAdmission::RejectedBeforeValidation {
                        execution: ToolExecutionResult::new(
                            normalized_tool_failure(call, &error)?,
                            false,
                        ),
                        attempt: ToolCallAttemptIdentity::new(
                            call.exposed_name().clone(),
                            Some(self.tool_presentation.digest()),
                        ),
                        reason: error.kind(),
                        detail: error.to_string(),
                    });
                }
            };
        let request = ToolInvocationRequest::new(
            self.run_id,
            command.turn_id,
            attempt_id,
            call.call_id(),
            call.canonical_name().clone(),
            call.correlation().clone(),
            Arc::<str>::from(registry_arguments),
        );
        let invocation = match self.tools.prepare(request) {
            Ok(invocation) => invocation,
            Err(error) => {
                return Ok(ToolAdmission::RejectedBeforeValidation {
                    execution: ToolExecutionResult::new(
                        normalized_tool_failure(call, &error)?,
                        false,
                    ),
                    attempt: ToolCallAttemptIdentity::new(
                        call.exposed_name().clone(),
                        Some(self.tool_presentation.digest()),
                    ),
                    reason: error.kind(),
                    detail: error.to_string(),
                });
            }
        };
        if !self.is_local_composition_invocation(&invocation) {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "exec did not resolve to the exact local composition marker binding",
            ));
        }
        let audit_identity = ToolAuditIdentity::from_invocation(&invocation);
        match self
            .authorize_tool(command, call, invocation.clone())
            .await?
        {
            AuthorizationOutcome::Authorized(authorized) => {
                let _ = command.shared.events.publish(
                    EventContext::call(command.turn_id, attempt_id, call.call_id()),
                    AgentEventKind::ToolAuthorized {
                        identity: audit_identity.clone(),
                        lineage: self.local_composition_marker_lineage(),
                    },
                );
                Ok(ToolAdmission::Admitted {
                    execution: Box::new(self.admitted_tool_execution(
                        invocation,
                        None,
                        *authorized,
                    )?),
                    audit_identity,
                })
            }
            AuthorizationOutcome::Pending(pending) => Ok(ToolAdmission::DecisionRequired {
                invocation: Box::new(invocation),
                projection_receipt: None,
                pending,
                audit_identity,
            }),
            AuthorizationOutcome::Rejected(result) => Ok(ToolAdmission::RejectedAfterValidation {
                execution: ToolExecutionResult::new(result, false),
            }),
        }
    }
}
