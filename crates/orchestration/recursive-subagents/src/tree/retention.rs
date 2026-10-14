//! Retirement preserves sealed work independently of its conversation.

use super::{
    CandidateId, Deserialize, NodeStatus, OperationRecord, ProductAgentTree, Serialize,
    TopologyError, TopologyErrorCode, WorkspaceMode, WorkspaceStatus, canonical_json, digest_text,
};

const KIND: &str = "child_retirement";
const WATERMARK: &str = "plak:child_collection_watermark";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetirementTicket {
    pub(crate) agent_id: String,
    pub(crate) locator: Option<String>,
    pub(crate) pins: Vec<CandidateId>,
    collected: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct CollectionWatermark {
    child: u32,
    spent_attempts: usize,
}

impl ProductAgentTree {
    fn retirement_id(agent: &str) -> String {
        format!("plak:{KIND}:{agent}")
    }

    pub(super) fn conversation_retired(&self, agent: &str) -> bool {
        self.operations.contains_key(&Self::retirement_id(agent))
    }

    pub(super) fn conversation_collected(&self, agent: &str) -> bool {
        self.operations
            .get(&Self::retirement_id(agent))
            .and_then(|operation| {
                serde_json::from_str::<RetirementTicket>(&operation.canonical_request).ok()
            })
            .is_some_and(|ticket| ticket.collected)
    }

    pub(super) fn collected_child_floor(&self) -> u32 {
        self.operations
            .get(WATERMARK)
            .and_then(|op| serde_json::from_str::<CollectionWatermark>(&op.canonical_request).ok())
            .map_or(0, |watermark| watermark.child)
    }

    pub(super) fn collected_attempts_spent(&self) -> usize {
        self.operations
            .get(WATERMARK)
            .and_then(|op| serde_json::from_str::<CollectionWatermark>(&op.canonical_request).ok())
            .map_or(0, |watermark| watermark.spent_attempts)
    }

    fn write_retention<T: Serialize>(
        &mut self,
        id: String,
        agent: String,
        value: &T,
    ) -> Result<(), TopologyError> {
        let canonical = canonical_json(value)?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: agent,
                tool_name: KIND.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                fold: None,
                terminal_code: None,
                terminal_response: Some("owned lifecycle intent".to_owned()),
            },
        );
        self.bump();
        Ok(())
    }

    fn retention_ready(&self, agent: &str) -> bool {
        let Some(node) = self.nodes.get(agent) else {
            return false;
        };
        if node.parent_agent_id.is_none()
            || node.active_turn
            || self.has_turn_owner(agent)
            || self.has_turn_waiter(agent)
            || self.seal_pending(agent)
            || self.start_reservations.contains_key(agent)
            || self.rehydration_awaits_followup(&node.workspace_id)
            || self
                .parked_waits
                .values()
                .any(|targets| targets.iter().any(|id| id == agent))
            || self.nodes.values().any(|child| {
                child.parent_agent_id.as_deref() == Some(agent)
                    && !self.conversation_collected(&child.agent_id)
            })
            || self.pending_mail(agent).iter().any(|mail| {
                !matches!(
                    mail.durable,
                    super::MailDurableState::Acknowledged | super::MailDurableState::Terminal
                )
            })
            || self
                .commands
                .values()
                .any(|cmd| cmd.recipient == agent && !cmd.withdrawn && cmd.fact.is_none())
            || self
                .current_attempt(agent)
                .map_or(!self.conversation_retired(agent), |attempt| {
                    !attempt.status.is_stopped()
                        || attempt.ended_at.is_none()
                        || attempt.has_unresolved_session_failure()
                })
        {
            return false;
        }
        if !self.dormant(node) && node.status != NodeStatus::Closed {
            return false;
        }
        self.workspaces
            .get(&node.workspace_id)
            .is_some_and(|workspace| {
                workspace.mode == WorkspaceMode::Inherit
                    || self.reclaimed(&node.workspace_id)
                    || matches!(
                        workspace.status,
                        WorkspaceStatus::Discarded | WorkspaceStatus::CleanRemoved
                    ) && !self.reclamation_pending(&node.workspace_id)
            })
    }

    pub(crate) fn prepare_child_retention(
        &mut self,
        now: u64,
    ) -> Result<Vec<RetirementTicket>, TopologyError> {
        let tickets: Vec<_> = self
            .operations
            .values()
            .filter(|op| op.tool_name == KIND && op.operation_id != WATERMARK)
            .map(|op| {
                serde_json::from_str::<RetirementTicket>(&op.canonical_request).map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "child retirement evidence is invalid; sessions and work retained",
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        let mut pending = Vec::new();
        for mut ticket in tickets {
            if ticket.collected {
                let protected = self.candidates.values().any(|candidate| {
                    candidate.producing_agent_id == ticket.agent_id
                        && candidate.adopted_by.is_none()
                        && candidate.content != candidate.base_content
                });
                if protected
                    || self
                        .nodes
                        .values()
                        .any(|node| node.parent_agent_id.as_deref() == Some(&ticket.agent_id))
                {
                    continue;
                }
                ticket.collected = false;
                ticket.pins = self
                    .candidates
                    .values()
                    .filter(|candidate| candidate.producing_agent_id == ticket.agent_id)
                    .map(|candidate| candidate.candidate_id.clone())
                    .collect();
                self.write_retention(
                    Self::retirement_id(&ticket.agent_id),
                    ticket.agent_id.clone(),
                    &ticket,
                )?;
            }
            if self.retention_ready(&ticket.agent_id) {
                pending.push(ticket);
            }
        }
        let mut tickets = pending;
        let mut eligible: Vec<_> = self
            .nodes
            .values()
            .filter(|node| {
                !self.conversation_retired(&node.agent_id) && self.retention_ready(&node.agent_id)
            })
            .filter_map(|node| {
                self.current_attempt(&node.agent_id)?
                    .ended_at
                    .map(|ended| (ended, node.agent_id.clone()))
            })
            .collect();
        eligible.sort();
        let excess = eligible
            .len()
            .saturating_sub(usize::from(self.snapshot.child_retention_count));
        for (index, (ended, agent)) in eligible.into_iter().enumerate() {
            if index >= excess
                && self.nodes[&agent].status != NodeStatus::Closed
                && now.saturating_sub(ended)
                    < u64::from(self.snapshot.child_retention_days) * 86_400_000
            {
                continue;
            }
            let node = &self.nodes[&agent];
            let ticket = RetirementTicket {
                agent_id: agent.clone(),
                locator: node.session_locator.clone(),
                collected: false,
                pins: self
                    .candidates
                    .values()
                    .filter(|candidate| {
                        candidate.producing_agent_id == agent
                            && (candidate.adopted_by.is_some()
                                || candidate.content == candidate.base_content)
                    })
                    .map(|candidate| candidate.candidate_id.clone())
                    .collect(),
            };
            self.write_retention(Self::retirement_id(&agent), agent.clone(), &ticket)?;
            self.nodes
                .get_mut(&agent)
                .ok_or_else(|| {
                    TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
                })?
                .status = NodeStatus::Closed;
            tickets.push(ticket);
        }
        Ok(tickets)
    }

    pub(crate) fn finish_child_retention(
        &mut self,
        ticket: &RetirementTicket,
    ) -> Result<(), TopologyError> {
        let id = Self::retirement_id(&ticket.agent_id);
        let Some(op) = self.operations.get(&id) else {
            return Ok(());
        };
        if op.canonical_request != canonical_json(ticket)?
            || !self.retention_ready(&ticket.agent_id)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "child retirement ownership changed; records and refs retained",
            ));
        }
        let node = self.nodes[&ticket.agent_id].clone();
        let retained: Vec<_> = self
            .candidates
            .values()
            .filter(|candidate| {
                candidate.producing_agent_id == ticket.agent_id
                    && candidate.adopted_by.is_none()
                    && candidate.content != candidate.base_content
            })
            .map(|candidate| candidate.candidate_id.clone())
            .collect();
        let spent = self
            .attempts
            .values()
            .filter(|attempt| {
                attempt.agent_id == ticket.agent_id
                    && attempt
                        .failure
                        .as_ref()
                        .is_none_or(|failure| failure.kind != super::TURN_ABANDONED)
            })
            .count();
        let watermark = CollectionWatermark {
            child: self
                .nodes
                .keys()
                .filter_map(|id| id.strip_prefix('c')?.parse::<u32>().ok())
                .max()
                .unwrap_or(0)
                .max(self.collected_child_floor()),
            spent_attempts: self.collected_attempts_spent().saturating_add(spent),
        };
        self.write_retention(WATERMARK.to_owned(), self.root_agent_id.clone(), &watermark)?;
        let attempts: std::collections::BTreeSet<_> = self
            .attempts
            .values()
            .filter(|attempt| attempt.agent_id == ticket.agent_id)
            .map(|attempt| attempt.attempt_id.clone())
            .collect();
        self.compaction_views
            .retain(|attempt, _| !attempts.contains(attempt));
        self.attempts
            .retain(|_, attempt| attempt.agent_id != ticket.agent_id);
        self.commands
            .retain(|_, command| command.recipient != ticket.agent_id);
        self.operations.retain(|key, operation| {
            key == WATERMARK
                || key == &id
                || operation.tool_name == "workspace_reclamation"
                || operation.caller_agent_id != ticket.agent_id
        });
        self.candidates.retain(|candidate, row| {
            row.producing_agent_id != ticket.agent_id || retained.contains(candidate)
        });
        self.messages
            .retain(|_, mail| mail.to != ticket.agent_id && mail.from != ticket.agent_id);
        let messages = &self.messages;
        self.inbound.retain(|recipient, ids| {
            ids.retain(|id| messages.contains_key(id));
            recipient != &ticket.agent_id && !ids.is_empty()
        });
        self.delivered
            .retain(|(caller, child), _| caller != &ticket.agent_id && child != &ticket.agent_id);
        self.grants.remove(&ticket.agent_id);
        for grant in self.grants.values_mut() {
            if retained.is_empty() {
                grant.visible_ids.retain(|id| id != &ticket.agent_id);
                grant.managed_ids.retain(|id| id != &ticket.agent_id);
                grant.message_peers.retain(|id| id != &ticket.agent_id);
            }
        }
        if let Some(locator) = ticket
            .locator
            .as_deref()
            .and_then(|raw| raw.parse::<plakous::sessions::SessionLocator>().ok())
        {
            let session = locator.session_id().to_string();
            self.feed_cursors.remove(&session);
            self.compaction_cursors.remove(&session);
        }
        if retained.is_empty()
            && !self
                .nodes
                .values()
                .any(|child| child.parent_agent_id.as_deref() == Some(&ticket.agent_id))
        {
            self.nodes.remove(&ticket.agent_id);
            self.operations.remove(&id);
            if !self
                .nodes
                .values()
                .any(|other| other.workspace_id == node.workspace_id)
            {
                self.workspaces.remove(&node.workspace_id);
                self.operations
                    .remove(&format!("plak:workspace_reclamation:{}", node.workspace_id));
                self.live_generations.remove(&node.workspace_id);
            }
        } else {
            let row = self.nodes.get_mut(&ticket.agent_id).ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
            })?;
            row.description = "conversation retired; sealed candidates retained".to_owned();
            row.prompt.clear();
            row.output_schema_json.clear();
            row.schema_digest.clear();
            row.tuple_id.clear();
            row.account.clear();
            row.effort = None;
            row.template = None;
            row.attempt_id = None;
            row.session_locator = None;
            row.fold_order = None;
            row.abandoned_turn = None;
            let mut completed = ticket.clone();
            completed.collected = true;
            completed.locator = None;
            completed.pins.clear();
            self.write_retention(id, ticket.agent_id.clone(), &completed)?;
        }
        self.bump();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::tree::AwaitAgents;
    use crate::tree::test_support::{adopt_req, candidate_of, mint_req, spawn_req, tree, tree_at};
    use crate::tree::{ListAgentsRequest, NodeStatus, WorkspaceStatus, WriterMode};

    #[test]
    fn child_retention_requires_delivery_and_sealed_work() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert!(tree.prepare_child_retention(u64::MAX).unwrap().is_empty());
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        assert_eq!(
            tree.prepare_child_retention(u64::MAX).unwrap()[0].agent_id,
            child.agent_id
        );
    }
    fn delivered(tree: &mut super::ProductAgentTree) {
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
    }

    #[test]
    fn child_retention_bounds_records_and_never_reuses_identity_or_refunds_budget() {
        let (_tmp, mut tree) = tree();
        tree.snapshot.child_retention_count = 1;
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        delivered(&mut tree);
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        delivered(&mut tree);
        let spent = tree.attempts_spent();
        let tickets = tree
            .prepare_child_retention(super::super::now_unix_millis())
            .unwrap();
        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets[0].agent_id, first.agent_id);
        tree.finish_child_retention(&tickets[0]).unwrap();
        assert!(tree.node(&first.agent_id).is_err());
        assert!(tree.node(&second.agent_id).is_ok());
        assert_eq!(tree.attempts_spent(), spent);
        let next = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_ne!(next.agent_id, first.agent_id);
        assert_ne!(next.agent_id, second.agent_id);
    }

    #[test]
    fn child_retention_keeps_unadopted_work_listable_adoptable_and_as_spawn_input() {
        let repo = super::super::dormancy::tests::git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("WORK.txt"), "retained work\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        delivered(&mut tree);
        let candidate = candidate_of(&tree, &child.agent_id);
        tree.reclaim_dormant(u64::MAX, &std::collections::BTreeMap::new())
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.workspaces[&tree.nodes[&child.agent_id].workspace_id].status,
            WorkspaceStatus::CleanRemoved
        );
        let ticket = tree.prepare_child_retention(u64::MAX).unwrap().remove(0);
        tree.finish_child_retention(&ticket).unwrap();
        tree.clear_feed_views();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert!(tree.node(&child.agent_id).unwrap().prompt.is_empty());
        assert!(
            tree.resolve_input_base(&[candidate.to_string()])
                .unwrap()
                .is_some()
        );
        let listing = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert!(
            serde_json::to_string(&listing)
                .unwrap()
                .contains("conversation_retired")
        );
        assert!(
            serde_json::to_string(&listing)
                .unwrap()
                .contains(candidate.as_str())
        );
        tree.adopt_candidate("root", &adopt_req(&candidate), None)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("WORK.txt")).unwrap(),
            "retained work\n"
        );
    }

    #[test]
    fn child_retention_intent_is_idempotent_and_close_is_immediate() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        delivered(&mut tree);
        tree.nodes.get_mut(&child.agent_id).unwrap().status = NodeStatus::Closed;
        let ticket = tree.prepare_child_retention(0).unwrap().remove(0);
        assert_eq!(
            tree.prepare_child_retention(0).unwrap(),
            vec![ticket.clone()]
        );
        tree.finish_child_retention(&ticket).unwrap();
        tree.finish_child_retention(&ticket).unwrap();
        assert!(tree.prepare_child_retention(u64::MAX).unwrap().is_empty());
    }
}
