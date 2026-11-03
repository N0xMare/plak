//! Successor construction from immutable workflow requests and feed facts.

use super::SessionWorkflowKind;
use crate::sessions::{QueueDrainState, SessionRecord, new_session_create_operation_id};
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
        if request.kind == SessionWorkflowKind::Fork {
            target.prompt_history.clear();
        }
        // Today a fork shares its source's tree, so it stays its node and
        // profile; a rebind takes the node and profile of the tree it
        // compiled against; a switch continues the same node with its new
        // profile.
        match &request.kind {
            SessionWorkflowKind::Rebind(selection) => {
                if let Some(profile) = &selection.own_tree {
                    target.node = crate::sessions::SessionNode::genesis(&target.id);
                    target.root_profile = Some(profile.clone());
                }
                target.bind_selection(selection);
            }
            SessionWorkflowKind::Switch(switch) => {
                target.bind_selection(&switch.rebind);
                target.root_profile = Some(switch.root.clone());
            }
            _ => {}
        }
        target.validate_for_store()?;
        Ok(target)
    }

    /// Builds the empty successor reserved by an immutable clear intent.
    pub fn new_session_from_request(
        &self,
        request: &crate::sessions::projector::WorkflowRequest,
    ) -> Result<Self, String> {
        if request.kind != SessionWorkflowKind::New
            || self.plakous_locator != Some(request.source_locator)
        {
            return Err("clear request does not match its source".to_owned());
        }
        let operation = plakous::sessions::SessionOperationId::new(
            new_session_create_operation_id(&request.operation_id)?,
        )
        .map_err(|error| error.to_string())?;
        let locator = plakous::sessions::AgentSessions::genesis_locator_for(&operation)
            .map_err(|error| error.to_string())?;
        let mut target = self.clone();
        target.id = request.successor_product_id();
        target.node = crate::sessions::SessionNode::genesis(&target.id);
        // The new session's genesis freezes the home's profile; the host
        // writes it into the reservation before the runtime create.
        target.root_profile = None;
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

    fn bind_selection(&mut self, selection: &crate::sessions::RebindSelection) {
        self.account = Some(selection.account.clone());
        self.model = Some(selection.model.clone());
        self.effort.clone_from(&selection.effort);
        self.composition = selection.composition;
        self.target_selection = selection.target_selection.clone();
    }
}
