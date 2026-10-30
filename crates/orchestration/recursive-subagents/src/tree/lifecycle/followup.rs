use super::{
    AttemptId, AttemptRecord, AttemptStatus, BTreeSet, FollowupRollback, MAX_DIRECT_CHILDREN,
    MAX_TREE_NODES, NodeStatus, ProductAgentTree, SubmitNudgeBudget, TopologyError,
    TopologyErrorCode, UsageRow, WorkspaceStatus, admit_output_schema, now_unix_millis,
};

impl ProductAgentTree {
    /// Admits a `followup` and resolves the schema the new
    /// attempt will carry.
    ///
    /// `followup` re-tasks an **idle** child: finished, or interrupted with
    /// its turn already ended (an interrupt cancels a turn,
    /// not the node). A running child is not re-tasked — `priority=urgent`
    /// steers the turn it is already in, and a second brief on a live turn
    /// would silently replace the first. `Closed` and `RecoveryRequired` can
    /// never run again, and neither can a failed spawn that never produced a
    /// session: there is nothing to re-task.
    #[allow(
        clippy::too_many_lines,
        reason = "followup admission validates schema, budget, workspace and prior attempt state before installing its rollback boundary"
    )]
    pub(in crate::tree) fn admit_followup(
        &self,
        agent_id: &str,
        status: NodeStatus,
        active_turn: bool,
        output_schema_json: Option<&str>,
    ) -> Result<(String, String), TopologyError> {
        if self
            .nodes
            .get(agent_id)
            .is_some_and(|node| self.refresh_pending(&node.workspace_id))
        {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "workspace refresh is pending; settle it before following up",
            ));
        }
        if self.seal_pending(agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::FollowupNotIdle,
                "recipient still owns an unfinished seal",
            ));
        }
        match status {
            NodeStatus::Failed
                if self
                    .current_attempt(agent_id)
                    .is_some_and(AttemptRecord::never_created) =>
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupClosed,
                    "this agent never produced a session and cannot be re-tasked; spawn a new \
                     agent",
                ));
            }
            NodeStatus::Completed | NodeStatus::Failed if !active_turn => {}
            NodeStatus::Interrupted
                if !active_turn
                    && self
                        .current_attempt(agent_id)
                        .is_none_or(|attempt| attempt.turn_id.is_none()) => {}
            NodeStatus::Closed => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupClosed,
                    if self.conversation_retired(agent_id) {
                        "conversation was retired and cannot accept follow-up; its sealed work remains usable"
                    } else {
                        "recipient is closed and cannot accept another attempt"
                    },
                ));
            }
            NodeStatus::RecoveryRequired => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupRecoveryRequired,
                    "recipient requires recovery before another attempt can be admitted",
                ));
            }
            _ => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupNotIdle,
                    "recipient is not an idle finished child; steer a running child with \
                     priority=urgent instead",
                ));
            }
        }
        // A re-task runs in the child's copy. A discarded or adopting copy has
        // no live path to run in, so the node is idle but not re-taskable. An
        // adopted copy still exists: the followup's seal is a new candidate
        // that diverges from the adopted state and adopting it integrates
        // only the delta.
        let workspace_status = self
            .nodes
            .get(agent_id)
            .and_then(|node| self.workspaces.get(&node.workspace_id))
            .map(|row| row.status);
        if let Some(status) = workspace_status
            && !matches!(status, WorkspaceStatus::Active | WorkspaceStatus::Adopted)
            && !self
                .nodes
                .get(agent_id)
                .is_some_and(|node| self.reclaimed(&node.workspace_id))
        {
            return Err(TopologyError::new(
                if matches!(
                    status,
                    WorkspaceStatus::Discarded | WorkspaceStatus::CleanRemoved
                ) {
                    TopologyErrorCode::FollowupWorkspaceGone
                } else {
                    TopologyErrorCode::FollowupNotIdle
                },
                format!(
                    "recipient's workspace is {} and cannot host another attempt; spawn a new \
                     child instead",
                    serde_json::to_value(status)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default()
                ),
            ));
        }
        let node = self.node(agent_id)?;
        if !self.holds_tree_slot(node)
            && let Some(parent) = node.parent_agent_id.as_deref()
        {
            let ceiling = MAX_TREE_NODES.min(usize::from(self.grant(parent)?.max_nodes));
            let held = self
                .nodes
                .values()
                .filter(|node| self.holds_tree_slot(node))
                .count();
            if held >= ceiling {
                return Err(TopologyError::new(
                    TopologyErrorCode::SpawnBudget,
                    "tree node budget is exhausted; followup would reactivate a dormant child",
                ));
            }
        }
        if !self.holds_direct_child_slot(node)
            && let Some(parent) = node.parent_agent_id.as_deref()
        {
            let ceiling = MAX_DIRECT_CHILDREN.min(usize::from(self.grant(parent)?.max_children));
            let held = self
                .nodes
                .values()
                .filter(|child| {
                    child.parent_agent_id.as_deref() == Some(parent)
                        && self.holds_direct_child_slot(child)
                })
                .count();
            if held >= ceiling {
                return Err(TopologyError::new(
                    TopologyErrorCode::SpawnBudget,
                    "direct child budget is exhausted; followup would reactivate a released slot",
                ));
            }
        }
        if self.attempt_budget_exhausted() {
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                format!(
                    "attempt budget for this prompt is exhausted ({} of {})",
                    self.attempts_this_prompt(),
                    self.max_attempts
                ),
            ));
        }
        if let Some(raw) = output_schema_json.filter(|value| !value.is_empty()) {
            self.bound_text(raw)?;
            admit_output_schema(raw)
        } else {
            let node = self.node(agent_id)?;
            Ok((node.output_schema_json.clone(), node.schema_digest.clone()))
        }
    }

    /// Opens the next attempt on an idle finished node.
    ///
    /// The previous attempt row keeps its own `output_json`, `usage`, and
    /// candidate: the node is re-tasked, its history is not overwritten. The
    /// turn token advances so a late submission against the old attempt is
    /// still refused as stale.
    pub(in crate::tree) fn open_followup_attempt(
        &mut self,
        agent_id: &str,
        opener: &str,
        output_schema_json: String,
        schema_digest: String,
    ) -> Result<AttemptId, TopologyError> {
        self.require_fold_started(agent_id)?;
        let previous = self.node(agent_id)?.attempt_id.clone();
        let rollback = {
            let node = self.node(agent_id)?;
            FollowupRollback {
                opener: opener.to_owned(),
                // Filled in below, once the new attempt has an id.
                opened: AttemptId::new(agent_id, 0),
                previous: previous
                    .as_ref()
                    .and_then(|id| self.attempts.get(id))
                    .map(|attempt| (attempt.status, attempt.ended_at)),
                node_attempt_id: node.attempt_id.clone(),
                node_status: node.status,
                node_active_turn: node.active_turn,
                workspace_id: node.workspace_id.clone(),
                workspace_status: self
                    .workspaces
                    .get(&node.workspace_id)
                    .map(|row| row.status),
                workspace_generation: self
                    .workspaces
                    .get(&node.workspace_id)
                    .map(|row| row.generation),
                fault_reason: self.fault_reasons.get(agent_id).cloned(),
                node_fold_order: node.fold_order,
                prior_operations: self.operations.keys().cloned().collect(),
                prior_commands: self.commands.keys().cloned().collect(),
            }
        };
        let seq = self
            .attempts
            .values()
            .filter(|attempt| attempt.agent_id == agent_id)
            .count()
            .saturating_add(1);
        let attempt_id = AttemptId::new(agent_id, u32::try_from(seq).unwrap_or(u32::MAX));
        let turn_token = previous
            .as_ref()
            .and_then(|id| self.attempts.get(id))
            .map_or(1, |attempt| attempt.turn_token.saturating_add(1));
        self.attempts.insert(
            attempt_id.clone(),
            AttemptRecord {
                output_schema_json: output_schema_json.clone(),
                schema_digest: schema_digest.clone(),
                attempt_id: attempt_id.clone(),
                agent_id: agent_id.to_owned(),
                retry_of: previous,
                request_key: None,
                spawn_identity: None,
                started_at: now_unix_millis(),
                ended_at: None,
                blocked: None,
                failure: None,
                usage: UsageRow::default(),
                status: AttemptStatus::Running,
                turn_token,
                turn_id: None,
                applied_terminal_read_seqs: BTreeSet::new(),
                output_json: None,
                submit_harness_nudge: SubmitNudgeBudget::NONE,
                candidate_id: None,
            },
        );
        self.record_followup_opening(
            agent_id,
            rollback,
            attempt_id.clone(),
            output_schema_json,
            schema_digest,
        )?;
        Ok(attempt_id)
    }
}
