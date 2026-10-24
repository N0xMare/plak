//! Drive interpreter cells and nested tool dispatch in the actor pump.

use crate::local_composition::{
    LocalCompositionContentItem, LocalCompositionEvent, LocalCompositionExecuteRequest,
    LocalCompositionResponse, LocalCompositionSessionLoss, LocalCompositionShutdownResult,
    LocalCompositionTerminateResult, LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use crate::manifest::InterpreterObservation;
use crate::runtime::cleanup::{emit_shutdown_child_exit, error_with_cleanup_disposition};
use crate::runtime::composition::nested::{
    fence_pending_local_composition_nested, queue_local_composition_nested_results,
    take_local_composition_nested_waiters,
};
use crate::runtime::composition::projection::{
    clip_local_composition_update_text, local_composition_child_call_id,
    local_composition_nested_call_arguments, local_composition_nested_decision_unsupported,
    local_composition_nested_execution_owner, local_composition_nested_result_value,
    local_composition_prestart_error, local_composition_recovery_authoritative,
    local_composition_response_cell, local_composition_response_parts,
    local_composition_response_terminal, local_composition_started_error,
    local_composition_wait_response,
};
use crate::runtime::composition::record::{
    DurableLocalCompositionContinuationRequestV1, DurableLocalCompositionEvaluationRequestV2,
    encode_local_composition_continuation_request, encode_local_composition_evaluation_request,
};
use crate::runtime::composition::resources::{
    InterpreterResources, join_extracted_local_composition_owner,
    local_composition_session_loss_error, observe_local_composition_session_loss,
    take_local_composition_owner,
};
use crate::runtime::composition::{
    CompositionOwner, LocalCompositionNestedDisposition, LocalCompositionNestedFuture,
    LocalCompositionNestedReadOnlyCompletion, LocalCompositionNestedReady,
    LocalCompositionShutdownTelemetry, PendingLocalCompositionNested,
};
use crate::runtime::continuation::runtime_component;
use crate::runtime::driver::ToolDispatchOrigin;
use crate::runtime::driver::execution::{
    canonicalize_observed_tool_execution, execute_indexed_read_only, publish_observation_clip,
};
use crate::runtime::driver::{
    ActorToolDispatchObserver, AdmittedToolExecution, Driver, IndexedReadOnlyRequest,
    LocalCompositionCallResponse, LocalCompositionOuterExecution, ToolAdmission,
    ToolDispatchTelemetry,
};
use crate::runtime::observation::{
    canonical_tool_result_item, clip_tool_result, normalized_tool_failure,
    tool_result_observation_with_subject,
};
use crate::runtime::operation_record::{
    canonical_tool_operation_binding, tool_operation_terminal_state,
};
use crate::runtime::terminal::{actor_panic_error, cancelled_error};
use crate::runtime::{OperationId, PromptCommand, ToolPresentation};
use crate::{
    AgentError, AgentErrorKind, AgentEventKind, DurableOperationKind, DurableOperationState,
    EventContext, RuntimeComponentKind, SessionStoreError, StoreAdmission, ToolAuditIdentity,
    ToolCallObservation, ToolCallerLineageView,
};
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use plakous_model::{
    Actor, AttemptId, CallId, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId,
    Provenance, ToolCallBlock, ToolCallState, ToolCorrelation, ToolEffect, ToolExecutionOwner,
    ToolResultUpdateBlock, TurnId,
};
use plakous_tools::{ToolInvocation, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod nested;
mod outer;
mod read_only;
mod resources;

const LOCAL_COMPOSITION_FAILURE_TAG: &str = "plakous-local-composition-failure-v1";

/// What a failed local composition records on its marker operation. It is
/// encoded as the tagged tuple `[tag, kind, message, child operation ids]`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct LocalCompositionFailureEvidence(String, AgentErrorKind, String, Vec<u64>);

impl LocalCompositionFailureEvidence {
    fn new(kind: AgentErrorKind, message: &str, child_operations: Vec<u64>) -> Self {
        Self(
            LOCAL_COMPOSITION_FAILURE_TAG.to_owned(),
            kind,
            message.to_owned(),
            child_operations,
        )
    }

    /// Decodes failure evidence; `None` when the bytes carry another tag.
    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(|evidence| evidence.0 == LOCAL_COMPOSITION_FAILURE_TAG)
    }

    pub(super) fn detail(&self) -> &str {
        &self.2
    }

    pub(super) fn child_operations(&self) -> &[u64] {
        &self.3
    }
}

impl Driver {
    pub(super) fn is_local_composition_invocation(&self, invocation: &ToolInvocation) -> bool {
        self.local_composition.as_ref().is_some_and(|state| {
            invocation.identity().executor_id() == &state.descriptor().marker_executor
        })
    }

    pub(super) fn is_local_composition_marker(&self, execution: &AdmittedToolExecution) -> bool {
        match execution {
            AdmittedToolExecution::ReadOnly { invocation, .. }
            | AdmittedToolExecution::Mutating { invocation, .. } => {
                self.is_local_composition_invocation(invocation)
            }
        }
    }

    /// Returns the outer marker call owning the driver's active cell, if any.
    ///
    /// Read before `exec_call_id` is cleared at the cell's terminal.
    pub(super) fn active_composition_marker_call(&self) -> Option<CallId> {
        self.local_composition
            .as_ref()
            .and_then(CompositionOwner::exec_call_id)
    }

    /// Projects the caller lineage published on this call's lifecycle events.
    ///
    /// The canonical lineage minted on the child `ToolCallBlock` supplies the
    /// cell, ordinal, and depth; the owning marker call comes from the driver
    /// rather than from canonical content.
    pub(super) fn tool_caller_lineage(&self, call: &ToolCallBlock) -> ToolCallerLineageView {
        ToolCallerLineageView::from_lineage(
            call.correlation().caller_lineage(),
            self.active_composition_marker_call(),
        )
    }

    /// Projects the caller lineage of an outer local-composition marker call.
    ///
    /// The marker owns the cell, so it never carries a parent, an ordinal, or a
    /// depth. `cell_id` is the driver's active cell, which the interpreter has
    /// not yet minted when the first `exec*` of a cell is dispatched.
    pub(super) fn local_composition_marker_lineage(&self) -> ToolCallerLineageView {
        ToolCallerLineageView::local_composition_marker(
            self.local_composition
                .as_ref()
                .and_then(CompositionOwner::cell),
        )
    }

    /// The marker lineage published on an outer marker's own `ToolCompleted`.
    ///
    /// The completion is published after the cell has been retired, so the
    /// driver's live `cell` is `None` there. The caller lineage marker must
    /// carry the cell it owned, and a `wait*` continuation's terminal is the
    /// only event that says which cell it closed, so the retired cell is used
    /// when the live one is already gone. Nothing is guessed: with no cell in
    /// either slot the lineage still carries `None`.
    pub(super) fn local_composition_marker_terminal_lineage(&self) -> ToolCallerLineageView {
        ToolCallerLineageView::local_composition_marker(
            self.local_composition
                .as_ref()
                .and_then(|state| state.cell().or(state.retired_cell())),
        )
    }

    pub(super) fn tool_dispatch_observer(
        &self,
        command: &PromptCommand,
        attempt_id: AttemptId,
        call_id: CallId,
        audit_identity: ToolAuditIdentity,
        lineage: ToolCallerLineageView,
        observation: ToolCallObservation,
    ) -> Arc<ActorToolDispatchObserver> {
        let dispatch_witness = Arc::new(AtomicBool::new(false));
        let telemetry_context = self
            .telemetry_context
            .with_turn(command.turn_id)
            .with_attempt(attempt_id)
            .with_call(call_id);
        Arc::new(ActorToolDispatchObserver::new(
            self.start_authority(),
            command.cancellation.clone(),
            Arc::clone(&command.shared),
            EventContext::call(command.turn_id, attempt_id, call_id),
            audit_identity,
            lineage,
            observation,
            ToolDispatchTelemetry {
                emitter: Arc::clone(&self.telemetry),
                context: telemetry_context,
                clock: Arc::clone(&self.clock),
            },
            dispatch_witness,
        ))
    }
}

#[cfg(test)]
mod failure_evidence_tests {
    use super::LocalCompositionFailureEvidence;
    use crate::AgentErrorKind;

    #[test]
    fn failure_evidence_keeps_its_tagged_tuple_bytes() -> Result<(), serde_json::Error> {
        let evidence = LocalCompositionFailureEvidence::new(
            AgentErrorKind::Tool,
            "the cell failed",
            vec![7, 9],
        );
        let bytes = serde_json::to_vec(&evidence)?;
        assert_eq!(
            bytes,
            serde_json::to_vec(&(
                "plakous-local-composition-failure-v1",
                AgentErrorKind::Tool,
                "the cell failed",
                vec![7_u64, 9],
            ))?
        );
        let decoded = LocalCompositionFailureEvidence::decode(&bytes).ok_or_else(|| {
            <serde_json::Error as serde::de::Error>::custom("failure evidence did not decode")
        })?;
        assert_eq!(decoded.detail(), "the cell failed");
        assert_eq!(decoded.child_operations(), [7, 9]);
        let other = serde_json::to_vec(&(
            "plakous-local-composition-cell-v2",
            AgentErrorKind::Tool,
            "x",
            Vec::<u64>::new(),
        ))?;
        assert!(LocalCompositionFailureEvidence::decode(&other).is_none());
        Ok(())
    }
}
