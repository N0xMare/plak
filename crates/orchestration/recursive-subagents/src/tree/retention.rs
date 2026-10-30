//! Retirement preserves sealed work independently of its conversation.

use super::{
    CandidateId, Deserialize, NodeStatus, OperationRecord, ProductAgentTree, Serialize,
    TopologyError, TopologyErrorCode, WorkspaceMode, WorkspaceStatus, canonical_json, digest_text,
};

const KIND: &str = "child_retirement";
const WATERMARK: &str = "plak:child_collection_watermark";
const PIN_CLEANUP: &str = "candidate_unpin";
const RESCUE_PIN: &str = "rescue_pin";

pub(crate) struct PinCleanup {
    pub(crate) operation_id: String,
    pub(crate) candidates: Vec<CandidateId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetirementTicket {
    pub(crate) agent_id: String,
    pub(crate) locator: Option<String>,
    collected: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct CollectionWatermark {
    child: u32,
    spent_attempts: usize,
}

impl ProductAgentTree {
    /// Records ownership before a failed-turn rescue snapshot is published.
    ///
    /// # Errors
    /// Refuses an unknown owner or a conflicting rescue identity.
    pub fn record_rescue_pin(
        &mut self,
        agent: &str,
        pin: CandidateId,
    ) -> Result<(), TopologyError> {
        self.node(agent)?;
        let id = format!("plak:rescue:{pin}");
        let canonical = canonical_json(&vec![pin])?;
        if let Some(prior) = self.operations.get(&id) {
            if prior.tool_name == RESCUE_PIN
                && prior.caller_agent_id == agent
                && prior.canonical_request == canonical
            {
                return Ok(());
            }
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "rescue snapshot ownership conflicts; refs retained",
            ));
        }
        self.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: agent.to_owned(),
                tool_name: RESCUE_PIN.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                fold: None,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
        Ok(())
    }

    fn rescue_pins(&self, agent: &str) -> Result<Vec<CandidateId>, TopologyError> {
        let mut pins = Vec::new();
        for operation in self
            .operations
            .values()
            .filter(|op| op.tool_name == RESCUE_PIN && op.caller_agent_id == agent)
        {
            pins.extend(
                serde_json::from_str::<Vec<CandidateId>>(&operation.canonical_request).map_err(
                    |_| {
                        TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "rescue snapshot ownership is invalid; refs retained",
                        )
                    },
                )?,
            );
        }
        Ok(pins)
    }

    fn queue_pin_cleanup(&mut self, candidates: Vec<CandidateId>) -> Result<(), TopologyError> {
        if candidates.is_empty() {
            return Ok(());
        }
        let canonical = canonical_json(&candidates)?;
        let digest = digest_text(&canonical);
        let id = format!("plak:unpin:{digest}");
        self.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: self.root_agent_id.clone(),
                tool_name: PIN_CLEANUP.to_owned(),
                request_digest: digest,
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
                fold: None,
            },
        );
        Ok(())
    }

    pub(crate) fn pending_pin_cleanup(&self) -> Result<Vec<PinCleanup>, TopologyError> {
        self.operations
            .values()
            .filter(|op| op.tool_name == PIN_CLEANUP)
            .map(|op| {
                let candidates = serde_json::from_str(&op.canonical_request).map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "candidate cleanup intent is invalid; refs retained",
                    )
                })?;
                Ok(PinCleanup {
                    operation_id: op.operation_id.clone(),
                    candidates,
                })
            })
            .collect()
    }

    pub(crate) fn acknowledge_pin_cleanup(
        &mut self,
        ticket: &PinCleanup,
    ) -> Result<(), TopologyError> {
        let Some(op) = self.operations.get(&ticket.operation_id) else {
            return Ok(());
        };
        if op.tool_name != PIN_CLEANUP
            || op.canonical_request != canonical_json(&ticket.candidates)?
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "candidate cleanup ownership changed; refs retained",
            ));
        }
        self.operations.remove(&ticket.operation_id);
        self.bump();
        Ok(())
    }

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
        if self.refresh_pending(&node.workspace_id)
            || node.parent_agent_id.is_none()
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
                        && (self.conflict_input_retained(candidate)
                            || (candidate.adopted_by.is_none()
                                && candidate.adopted_via.is_none()
                                && candidate.content != candidate.base_content
                                && !self.input_was_relinquished(candidate)))
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
                    && (self.conflict_input_retained(candidate)
                        || (candidate.adopted_by.is_none()
                            && candidate.adopted_via.is_none()
                            && candidate.content != candidate.base_content
                            && !self.input_was_relinquished(candidate)))
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
        let rescue_pins = self.rescue_pins(&ticket.agent_id)?;
        self.operations.retain(|key, operation| {
            key == WATERMARK
                || key == &id
                || operation.tool_name == "workspace_reclamation"
                || operation.caller_agent_id != ticket.agent_id
        });
        let mut collected: Vec<_> = self
            .candidates
            .values()
            .filter(|candidate| {
                candidate.producing_agent_id == ticket.agent_id
                    && !retained.contains(&candidate.candidate_id)
            })
            .map(|candidate| candidate.candidate_id.clone())
            .collect();
        collected.extend(rescue_pins);
        // Publish cleanup ownership in the same save that removes records.
        // VCS failures cannot turn collection into a lost unpin obligation.
        self.queue_pin_cleanup(collected)?;
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
            self.write_retention(id, ticket.agent_id.clone(), &completed)?;
        }
        self.bump();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
