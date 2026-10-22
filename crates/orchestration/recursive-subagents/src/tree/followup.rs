//! Followup opening and the guarded withdrawal of its unfacted Start.

use super::{
    AttemptId, AttemptStatus, FollowupRollback, NodeStatus, OperationRecord, ProductAgentTree,
    TopologyError, TopologyErrorCode, UsageRow, WorkspaceStatus, canonical_json, digest_text,
};

pub(super) const FOLLOWUP_OPENED_KIND: &str = "followup_opened";

#[derive(serde::Serialize, serde::Deserialize)]
struct FollowupOpened {
    previous: Option<AttemptId>,
    opened: AttemptId,
    superseded_at_ms: u64,
    supersedes_running: bool,
    output_schema_json: String,
    schema_digest: String,
    rollback: FollowupRollback,
}

impl ProductAgentTree {
    pub(super) fn record_followup_opening(
        &mut self,
        agent_id: &str,
        mut rollback: FollowupRollback,
        opened: AttemptId,
        output_schema_json: String,
        schema_digest: String,
    ) -> Result<(), TopologyError> {
        rollback.opened = opened.clone();
        let anchor = self.fold_anchor_now(agent_id)?;
        let id = format!("plak:{FOLLOWUP_OPENED_KIND}:{opened}");
        let canonical = canonical_json(&FollowupOpened {
            previous: rollback.node_attempt_id.clone(),
            opened,
            superseded_at_ms: super::now_unix_millis(),
            supersedes_running: rollback
                .previous
                .as_ref()
                .is_some_and(|(status, _)| !status.is_stopped()),
            output_schema_json,
            schema_digest,
            rollback,
        })?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                fold: Some(anchor),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: FOLLOWUP_OPENED_KIND.to_owned(),
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

    pub(super) fn join_followup_opening(
        &mut self,
        agent_id: &str,
        id: &str,
    ) -> Result<(), TopologyError> {
        let intent = self.followup_opening(id)?;
        let reserve_effects = self.operations[id].terminal_response.is_none();
        let opened = self
            .attempts
            .get(&intent.opened)
            .ok_or_else(withdrawal_refused)?;
        if opened.output_schema_json != intent.output_schema_json
            || opened.schema_digest != intent.schema_digest
        {
            return Err(withdrawal_refused());
        }
        let schema = (
            opened.output_schema_json.clone(),
            opened.schema_digest.clone(),
        );
        if intent.supersedes_running
            && let Some(previous) = intent
                .previous
                .as_ref()
                .and_then(|id| self.attempts.get_mut(id))
        {
            previous.status = AttemptStatus::Cancelled;
            previous.ended_at = Some(intent.superseded_at_ms);
        }
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(withdrawal_refused)?;
        node.attempt_id = Some(intent.opened);
        node.status = NodeStatus::Running;
        node.active_turn = false;
        node.output_schema_json = schema.0;
        node.schema_digest = schema.1;
        if reserve_effects
            && intent.rollback.workspace_status == Some(WorkspaceStatus::Adopted)
            && let Some(workspace) = self.workspaces.get_mut(&node.workspace_id)
            && workspace.status == WorkspaceStatus::Adopted
        {
            workspace.status = WorkspaceStatus::Active;
        }
        self.fault_reasons.remove(agent_id);
        self.reset_reclamation_backoff(agent_id);
        self.bump();
        Ok(())
    }

    fn followup_opening(&self, id: &str) -> Result<FollowupOpened, TopologyError> {
        serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| withdrawal_refused())
    }

    pub(crate) fn owned_followup_attempt(
        &self,
        agent_id: &str,
        message_id: &str,
    ) -> Result<Option<AttemptId>, TopologyError> {
        let Some(attempt) = self.node(agent_id)?.attempt_id.as_ref() else {
            return Ok(None);
        };
        let id = format!("plak:{FOLLOWUP_OPENED_KIND}:{attempt}");
        if !self.operations.contains_key(&id) {
            return Ok(None);
        }
        let intent = self.followup_opening(&id)?;
        Ok((intent.rollback.opener == message_id).then(|| attempt.clone()))
    }

    pub(super) fn record_recovered_followup_mail(
        &mut self,
        attempt: &AttemptId,
    ) -> Result<(), TopologyError> {
        let id = format!("plak:{FOLLOWUP_OPENED_KIND}:{attempt}");
        if !self.operations.contains_key(&id) {
            return Ok(());
        }
        let opening = self.followup_opening(&id)?.rollback.opener;
        if self
            .mail(&opening)
            .is_some_and(|mail| mail.durable == super::MailDurableState::Accepted)
        {
            self.record_undeliverable_mail(&opening)?;
        }
        Ok(())
    }

    pub(super) fn derive_current_attempt_schema(&mut self, agent_id: &str) {
        if let Some(schema) = self.current_attempt(agent_id).map(|attempt| {
            (
                attempt.output_schema_json.clone(),
                attempt.schema_digest.clone(),
            )
        }) && let Some(node) = self.nodes.get_mut(agent_id)
        {
            node.output_schema_json = schema.0;
            node.schema_digest = schema.1;
        }
    }

    pub(crate) fn withdraw_unstarted_followup(
        &mut self,
        agent_id: &str,
        message_id: &str,
        session_id: &str,
        tail: u64,
    ) -> Result<(), TopologyError> {
        let opened = self
            .node(agent_id)?
            .attempt_id
            .as_ref()
            .ok_or_else(withdrawal_refused)?;
        let id = format!("plak:{FOLLOWUP_OPENED_KIND}:{opened}");
        let operation = self.operations.get(&id).ok_or_else(withdrawal_refused)?;
        let intent = self.followup_opening(&id)?;
        let anchor = operation.fold.ok_or_else(withdrawal_refused)?;
        let attempt = self.attempts.get(opened).ok_or_else(withdrawal_refused)?;
        let locator = self
            .node(agent_id)?
            .session_locator
            .as_deref()
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok());
        if intent.rollback.opener != message_id
            || locator.is_none_or(|locator| locator.session_id().to_string() != session_id)
            || tail != anchor.seq
            || self.feed_cursor(session_id) != anchor.seq
            || attempt.turn_id.is_some()
            || attempt.candidate_id.is_some()
            || attempt.output_json.is_some()
            || attempt.status != AttemptStatus::Running
            || attempt.usage != UsageRow::default()
            || self
                .workspaces
                .get(&intent.rollback.workspace_id)
                .is_some_and(|workspace| {
                    Some(workspace.generation) != intent.rollback.workspace_generation
                        || Some(workspace.status)
                            != intent.rollback.workspace_status.map(|status| {
                                if status == WorkspaceStatus::Adopted {
                                    WorkspaceStatus::Active
                                } else {
                                    status
                                }
                            })
                })
            || self.operations.values().any(|other| {
                other.operation_id != id
                    && other.caller_agent_id == agent_id
                    && other.fold.is_some_and(|position| position >= anchor)
            })
            || self.commands.values().any(|command| {
                command.recipient == agent_id
                    && !intent.rollback.prior_commands.contains(&command.command_id)
                    && (command.attempt_id.as_ref() != Some(opened)
                        || command.fact.is_some()
                        || command.kind != super::AgentCommandKind::Followup
                        || command.message_ids != [message_id.to_owned()])
            })
            || self.operations.values().any(|other| {
                other.caller_agent_id == agent_id
                    && other.fold.is_none()
                    && !intent
                        .rollback
                        .prior_operations
                        .contains(&other.operation_id)
            })
        {
            return Err(withdrawal_refused());
        }
        let opened = opened.clone();
        let rollback = intent.rollback;
        self.operations.remove(&id);
        self.commands
            .retain(|_, command| command.attempt_id.as_ref() != Some(&opened));
        self.attempts.remove(&opened);
        if let Some((status, ended_at)) = rollback.previous
            && let Some(previous) = rollback
                .node_attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get_mut(id))
        {
            previous.status = status;
            previous.ended_at = ended_at;
        }
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(withdrawal_refused)?;
        node.attempt_id = rollback.node_attempt_id;
        node.status = rollback.node_status;
        node.active_turn = rollback.node_active_turn;
        node.fold_order = rollback.node_fold_order;
        if let Some(status) = rollback.workspace_status
            && let Some(workspace) = self.workspaces.get_mut(&rollback.workspace_id)
        {
            workspace.status = status;
        }
        match rollback.fault_reason {
            Some(reason) => {
                self.fault_reasons.insert(agent_id.to_owned(), reason);
            }
            None => {
                self.fault_reasons.remove(agent_id);
            }
        }
        self.derive_current_attempt_schema(agent_id);
        self.bump();
        Ok(())
    }
}

fn withdrawal_refused() -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::OperationRecoveryRequired,
        "followup withdrawal requires an unchanged, unfacted opening with no other intents or effects",
    )
}
