//! Successor construction from immutable workflow requests and feed facts.

use super::SessionWorkflowKind;
use crate::sessions::{QueueDrainState, SessionRecord, clear_create_operation_id};
use plakous::sessions::SessionLocator;

impl SessionRecord {
    /// Constructs a successor from an immutable request and its branch fact.
    /// Neither the source's mutable journal nor the host clock is authority.
    pub fn successor_from_request(
        &self,
        request: &crate::sessions::projector::WorkflowRequest,
        successor: &crate::sessions::projector::Successor,
    ) -> Result<Self, String> {
        if !request.kind.branches()
            || successor.operation_id.as_deref() != Some(&request.operation_id)
        {
            return Err("successor does not match its workflow request".to_owned());
        }
        let head = successor.target;
        let mut target = self.clone();
        target.id = request.successor_product_id();
        target.create_operation_id = format!("plak:create:{}", target.id);
        target.store_revision = 0;
        target.created_at_unix = successor.created_at_unix;
        target.updated_at_unix = successor.created_at_unix;
        target.source_product_session_id = Some(self.id.clone());
        target.source_workflow_operation_id = Some(request.operation_id.clone());
        target.plakous_locator = Some(SessionLocator::new(
            head.session_id,
            head.branch_id,
            head.checkpoint_id,
        ));
        target.transcript = crate::sessions::ProductTranscript::default();
        target.pending_queue.clear();
        target.queue_commands = crate::sessions::QueueCommands::default();
        target.queue_projection_debt = false;
        target.queue_drain_state = QueueDrainState::LiveAutoDrain;
        if let SessionWorkflowKind::Rebind(selection) = &request.kind {
            target.account = Some(selection.account.clone());
            target.model = Some(selection.model.clone());
            target.effort.clone_from(&selection.effort);
            target.composition = selection.composition;
            target.target_selection = selection.target_selection.clone();
        }
        target.validate_for_store()?;
        Ok(target)
    }

    /// Builds the empty successor reserved by an immutable clear intent.
    pub fn clear_successor_from_request(
        &self,
        request: &crate::sessions::projector::WorkflowRequest,
    ) -> Result<Self, String> {
        if request.kind != SessionWorkflowKind::Clear
            || self.plakous_locator != Some(request.source_locator)
        {
            return Err("clear request does not match its source".to_owned());
        }
        let operation = plakous::sessions::SessionOperationId::new(clear_create_operation_id(
            &request.operation_id,
        )?)
        .map_err(|error| error.to_string())?;
        let locator = plakous::sessions::AgentSessions::genesis_locator_for(&operation)
            .map_err(|error| error.to_string())?;
        let mut target = self.clone();
        target.id = request.successor_product_id();
        target.created_at_unix = plakous::model::now_unix_secs();
        target.updated_at_unix = target.created_at_unix;
        target.create_operation_id = operation.as_str().to_owned();
        target.store_revision = 0;
        target.source_product_session_id = Some(self.id.clone());
        target.source_workflow_operation_id = Some(request.operation_id.clone());
        target.plakous_locator = Some(locator);
        target.title = None;
        target.transcript = crate::sessions::ProductTranscript::default();
        target.first_prompt = None;
        target.prompt_history.clear();
        target.pending_queue.clear();
        target.queue_commands = crate::sessions::QueueCommands::default();
        target.queue_projection_debt = false;
        target.queue_drain_state = QueueDrainState::LiveAutoDrain;
        target.root_turn_count = 0;
        target.child_wake_suppressed = false;
        target.tool_call_count = 0;
        target.compaction_count = 0;
        target.validate_for_store()?;
        Ok(target)
    }
}
