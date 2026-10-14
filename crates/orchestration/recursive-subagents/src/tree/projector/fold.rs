use super::{
    CHILD_SESSION_KIND, HOST_TURN_FAILURE_KIND, IntentJoinRefusal, ProductAgentTree, TopologyError,
    TopologyErrorCode, child_finalization, host_decision, host_recovery,
};
use crate::tree::{FoldAnchor, NodeStatus};

pub(super) type FoldIntent = (u64, u64, String, String);

impl ProductAgentTree {
    pub(crate) fn startup_fold_targets(&self) -> Vec<String> {
        let targets: std::collections::BTreeSet<_> = self
            .operations
            .values()
            .filter_map(|operation| {
                let anchor = operation.fold?;
                let agent = &operation.caller_agent_id;
                (self
                    .nodes
                    .get(agent)
                    .is_some_and(|node| node.fold_order.is_some())
                    && !self.fold_anchor_applied(agent, anchor))
                .then(|| agent.clone())
            })
            .collect();
        let mut targets: Vec<_> = targets.into_iter().collect();
        targets.retain(|agent| {
            let mut subtree = Vec::new();
            self.collect_subtree(agent, &mut subtree);
            subtree.iter().all(|id| self.nodes[id].fold_order.is_some())
        });
        targets.sort_by_key(|agent| std::cmp::Reverse(self.nodes[agent].depth));
        targets
    }

    pub(super) fn fold_session(&self, agent_id: &str) -> Result<Option<String>, TopologyError> {
        let node = self.node(agent_id)?;
        if agent_id == self.root_agent_id {
            return Ok(Some(self.tree_id.clone()));
        }
        let locator = node.session_locator.as_deref().or_else(|| {
            self.operations
                .get(&format!("{CHILD_SESSION_KIND}:{agent_id}"))
                .map(|operation| operation.canonical_request.as_str())
        });
        Ok(locator
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
            .map(|locator| locator.session_id().to_string()))
    }

    pub(in crate::tree) fn fold_anchor_now(
        &self,
        agent_id: &str,
    ) -> Result<FoldAnchor, TopologyError> {
        self.require_fold_started(agent_id)?;
        Ok(FoldAnchor {
            seq: self
                .fold_session(agent_id)?
                .map_or(0, |session| self.feed_cursor(&session)),
            order: self.revision,
        })
    }

    fn fold_position(&self, agent_id: &str) -> Result<(u64, Option<u64>), TopologyError> {
        Ok((
            self.fold_session(agent_id)?
                .map_or(0, |session| self.feed_cursor(&session)),
            self.node(agent_id)?.fold_order,
        ))
    }

    pub(in crate::tree) fn fold_position_now(
        &self,
        agent_id: &str,
    ) -> Result<FoldAnchor, TopologyError> {
        let (seq, order) = self.fold_position(agent_id)?;
        Ok(FoldAnchor {
            seq,
            order: order.unwrap_or(0),
        })
    }

    pub(in crate::tree) fn fold_anchor_applied(&self, agent_id: &str, anchor: FoldAnchor) -> bool {
        self.fold_position(agent_id).is_ok_and(|(cursor, order)| {
            order.is_some_and(|order| {
                anchor.seq < cursor || anchor.seq == cursor && anchor.order <= order
            })
        })
    }

    pub(super) fn ensure_fold_started(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        if self.node(agent_id)?.fold_order.is_none() {
            self.derive_current_attempt_schema(agent_id);
            if let Some(session) = self.fold_session(agent_id)? {
                self.feed_cursors.remove(&session);
            }
            self.nodes
                .get_mut(agent_id)
                .ok_or_else(|| {
                    TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
                })?
                .fold_order = Some(0);
            self.bump();
        }
        Ok(())
    }

    /// Applies recorded host intents at this agent's current feed cursor.
    /// Applied position is view state; effect reservations survive its deletion.
    ///
    /// # Errors
    /// Refuses unknown agents or malformed immutable intent data. Semantic
    /// refusals leave that intent's consequence unchanged and are returned.
    pub fn fold_due(&mut self, agent_id: &str) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        self.require_fold_started(agent_id)?;
        let (cursor, order) = self.fold_position(agent_id)?;
        let due = self.operations.values().any(|operation| {
            operation.caller_agent_id == agent_id
                && operation
                    .fold
                    .is_some_and(|anchor| anchor.seq == cursor && anchor.order > order.unwrap_or(0))
        });
        let session_join = self.node(agent_id)?.session_locator.is_none()
            && self
                .operations
                .contains_key(&format!("{CHILD_SESSION_KIND}:{agent_id}"));
        if !due && !session_join && !self.pending_seal_due(agent_id)? {
            return Ok(Vec::new());
        }
        let mut next = self.clone();
        #[cfg(test)]
        {
            next.fold_scratch_clones += 1;
        }
        let refused = next.fold_due_in_place(agent_id)?;
        *self = next;
        Ok(refused)
    }

    pub(in crate::tree) fn require_fold_started(
        &self,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        if self.node(agent_id)?.fold_order.is_none() {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "deleted agent fold must be rebuilt before recording or applying intents",
            ));
        }
        Ok(())
    }

    /// Rebuilds a deleted sessionless target without inventing runtime facts.
    ///
    /// # Errors
    /// Refuses agents whose fold requires a runtime session feed.
    pub fn rebuild_sessionless_fold(
        &mut self,
        agent_id: &str,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        if self.fold_session(agent_id)?.is_some() {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "session-backed fold requires its feed to rebuild",
            ));
        }
        let mut next = self.clone();
        next.ensure_fold_started(agent_id)?;
        let refused = next.fold_due_in_place(agent_id)?;
        *self = next;
        Ok(refused)
    }

    pub(crate) fn fold_reserved_closes(&mut self) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        if !self.pending_close_folds.iter().any(|target| {
            self.nodes
                .get(target)
                .is_some_and(|node| node.fold_order.is_some())
        }) {
            return Ok(Vec::new());
        }
        let mut next = self.clone();
        #[cfg(test)]
        {
            next.fold_scratch_clones += 1;
        }
        let refused = next.fold_reserved_closes_in_place()?;
        *self = next;
        Ok(refused)
    }

    pub(crate) fn fold_reserved_closes_in_place(
        &mut self,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        let mut refused = Vec::new();
        while let Some(target) = self
            .pending_close_folds
            .iter()
            .find(|target| {
                self.nodes
                    .get(*target)
                    .is_some_and(|node| node.fold_order.is_some())
            })
            .cloned()
        {
            self.pending_close_folds.remove(&target);
            refused.extend(self.fold_due_in_place(&target)?);
        }
        Ok(refused)
    }

    /// The caller owns an atomic rollback snapshot or an unjoined scratch tree.
    pub(crate) fn fold_due_in_place(
        &mut self,
        agent_id: &str,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        self.fold_agent_due_indexed(agent_id, None)
    }

    pub(super) fn fold_agent_due_indexed(
        &mut self,
        agent_id: &str,
        intents: Option<&[FoldIntent]>,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        self.require_fold_started(agent_id)?;
        if let Some(session) = self.fold_session(agent_id)? {
            self.join_child_session(agent_id, &session)?;
        }
        let (cursor, _) = self.fold_position(agent_id)?;
        let refused = self.fold_intents_through(agent_id, cursor, intents)?;
        self.lift_pending_seal_at_position(agent_id)?;
        Ok(refused)
    }

    fn pending_seal_due(&self, agent_id: &str) -> Result<bool, TopologyError> {
        Ok(self.node(agent_id)?.status != NodeStatus::Sealing
            && self
                .current_attempt(agent_id)
                .is_none_or(|attempt| !attempt.has_unresolved_session_failure())
            && self.seal_effect_pending(agent_id))
    }

    fn lift_pending_seal_at_position(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        if self.pending_seal_due(agent_id)? {
            if let Some(node) = self.nodes.get_mut(agent_id) {
                node.status = NodeStatus::Sealing;
            }
            self.bump();
        }
        Ok(())
    }

    pub(super) fn fold_after_record(
        &mut self,
        agent_id: &str,
        session_id: &str,
        seq: u64,
        intents: &[FoldIntent],
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        self.feed_cursors.insert(session_id.to_owned(), seq);
        self.nodes
            .get_mut(agent_id)
            .ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
            })?
            .fold_order = Some(0);
        self.bump();
        self.fold_agent_due_indexed(agent_id, Some(intents))
    }

    pub(in crate::tree) fn fold_onto_seed(
        &mut self,
        agent_id: &str,
        boundary: u64,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        if self.node(agent_id)?.fold_order.is_none() {
            self.ensure_fold_started(agent_id)?;
        }
        self.fold_intents_through(agent_id, boundary, None)
    }

    pub(in crate::tree) fn fold_before_head_seed(
        &mut self,
        agent_id: &str,
        session_id: &str,
        boundary: u64,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        let refused = if let Some(older) = boundary.checked_sub(1) {
            self.fold_onto_seed(agent_id, older)?
        } else {
            self.ensure_fold_started(agent_id)?;
            Vec::new()
        };
        self.feed_cursors.insert(session_id.to_owned(), boundary);
        self.nodes
            .get_mut(agent_id)
            .ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
            })?
            .fold_order = Some(0);
        self.bump();
        Ok(refused)
    }

    pub(super) fn index_fold_intents(&self, agent_id: &str) -> Vec<FoldIntent> {
        let mut intents: Vec<_> = self
            .operations
            .values()
            .filter_map(|operation| {
                let anchor = operation.fold?;
                (operation.caller_agent_id == agent_id).then(|| {
                    (
                        anchor.seq,
                        anchor.order,
                        operation.operation_id.clone(),
                        operation.tool_name.clone(),
                    )
                })
            })
            .collect();
        intents.sort();
        intents
    }

    fn fold_intents_through(
        &mut self,
        agent_id: &str,
        boundary: u64,
        index: Option<&[FoldIntent]>,
    ) -> Result<Vec<IntentJoinRefusal>, TopologyError> {
        let (cursor, applied_order) = self.fold_position(agent_id)?;
        let order = applied_order;
        let local_index;
        let intents = if let Some(index) = index {
            index
        } else {
            local_index = self.index_fold_intents(agent_id);
            &local_index
        };
        let start = intents.partition_point(|(seq, intent_order, _, _)| {
            order.is_some_and(|order| *seq < cursor || *seq == cursor && *intent_order <= order)
        });
        let end = intents
            .partition_point(|(seq, _, _, _)| *seq <= boundary)
            .max(start);
        let intents = intents[start..end].to_vec();
        #[cfg(debug_assertions)]
        let indexed_due = index.map(|_| intents.clone());
        #[cfg(debug_assertions)]
        if let Some(indexed_due) = &indexed_due {
            self.assert_page_intent_index(agent_id, cursor, applied_order, boundary, indexed_due);
        }
        let mut refused = Vec::new();
        for (seq, order, id, kind) in intents {
            // A semantic refusal drops only this consequence, not earlier folds.
            let replay = self.operations[&id].terminal_response.is_some();
            let session = self.fold_session(agent_id)?;
            let operation_count = self.operations.len();
            let replay_operation = replay.then(|| self.operations[&id].clone());
            #[cfg(debug_assertions)]
            let replay_operations = replay.then(|| self.operations.clone());
            let mut joined = if replay {
                // Fenced appliers cannot reserve or rewrite operation inputs.
                // Keep a view rollback snapshot without copying those inputs.
                let operations = std::mem::take(&mut self.operations);
                let mut joined = self.clone();
                joined.operations = operations;
                joined
            } else {
                self.clone()
            };
            if let Some(session) = session {
                joined.feed_cursors.insert(session, seq);
            }
            if let Some(node) = joined.nodes.get_mut(agent_id) {
                node.fold_order = Some(order);
            }
            let result = match kind.as_str() {
                crate::tree::followup::FOLLOWUP_OPENED_KIND => {
                    joined.join_followup_opening(agent_id, &id)
                }
                HOST_TURN_FAILURE_KIND => joined.join_host_turn_failure(agent_id, &id),
                host_decision::HOST_DECISION_KIND => joined.join_host_decision(agent_id, &id),
                host_decision::SESSION_BOUND_KIND => joined.join_session_bound(agent_id, &id),
                host_recovery::HOST_RECOVERY_KIND => joined.join_host_recovery(agent_id, &id),
                child_finalization::CHILD_FINALIZATION_KIND => {
                    joined.join_child_finalization(agent_id, &id)
                }
                crate::tree::candidates::SEAL_OUTCOME_KIND => {
                    joined.join_seal_outcome(agent_id, &id)
                }
                crate::tree::workspaces::VCS_FAULT_KIND => joined.join_vcs_fault(agent_id, &id),
                crate::tree::workspaces::VCS_FAULT_SETTLED_KIND => {
                    joined.join_vcs_fault_settled(&id)
                }
                crate::tree::effect_outcomes::SPAWN_REFUSED_KIND
                | crate::tree::effect_outcomes::ATTEMPT_ADOPTED_KIND
                | crate::tree::effect_outcomes::WORKSPACE_CLEANUP_KIND => {
                    joined.join_effect_outcome(agent_id, &id)
                }
                crate::tree::operator_intents::SUBMITTED_KIND
                | crate::tree::operator_intents::CLOSE_KIND
                | crate::tree::operator_intents::INTERRUPT_KIND
                | crate::tree::operator_intents::RECOVERY_ABANDONED_KIND => {
                    joined.join_operator_intent(agent_id, &id)
                }
                #[cfg(test)]
                "test_fenced_violation" => {
                    if joined.operations[&id].canonical_request == "count" {
                        let mut inserted = joined.operations[&id].clone();
                        inserted.operation_id = "injected".to_owned();
                        joined
                            .operations
                            .insert(inserted.operation_id.clone(), inserted);
                    } else {
                        joined
                            .operations
                            .get_mut(&id)
                            .unwrap()
                            .canonical_request
                            .push('!');
                    }
                    Err(TopologyError::new(
                        TopologyErrorCode::GrantDenied,
                        "injected refusal",
                    ))
                }
                _ => Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "unknown fold intent kind",
                )),
            };
            if replay
                && (joined.operations.len() != operation_count
                    || joined.operations.get(&id) != replay_operation.as_ref())
            {
                self.operations = std::mem::take(&mut joined.operations);
                return Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "a fenced applier rewrote operation inputs",
                ));
            }
            #[cfg(debug_assertions)]
            {
                if let Some(before) = &replay_operations {
                    assert!(
                        &joined.operations == before,
                        "a fenced applier rewrote operation inputs",
                    );
                }
                // Fenced replay moved the inputs into `joined`; compare against the copy.
                self.assert_agent_local_applier(
                    replay_operations.as_ref().unwrap_or(&self.operations),
                    &joined,
                    agent_id,
                    &id,
                );
            }
            match result {
                Ok(()) => {
                    if let Some(operation) = joined.operations.get_mut(&id)
                        && operation.terminal_response.is_none()
                    {
                        operation.terminal_response = Some("effects_reserved".to_owned());
                        joined.bump();
                    }
                    *self = joined;
                }
                Err(error) if error.code != TopologyErrorCode::OperationRecoveryRequired => {
                    if replay {
                        self.operations = std::mem::take(&mut joined.operations);
                    }
                    refused.push(IntentJoinRefusal {
                        operation_id: id,
                        error,
                    });
                }
                Err(error) => {
                    if replay {
                        self.operations = std::mem::take(&mut joined.operations);
                    }
                    return Err(error);
                }
            }
            if let Some(session) = self.fold_session(agent_id)? {
                self.feed_cursors.insert(session, seq);
            }
            self.nodes
                .get_mut(agent_id)
                .ok_or_else(|| {
                    TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
                })?
                .fold_order = Some(order);
            self.bump();
            // The next intent observes the seal reserved at this exact position,
            // just as a separate live fold_due call would.
            self.lift_pending_seal_at_position(agent_id)?;
        }
        #[cfg(debug_assertions)]
        if let Some(indexed_due) = &indexed_due {
            self.assert_page_intent_index(agent_id, cursor, applied_order, boundary, indexed_due);
        }
        if boundary > self.fold_anchor_now(agent_id)?.seq {
            if let Some(session) = self.fold_session(agent_id)? {
                self.feed_cursors.insert(session, boundary);
            }
            self.nodes
                .get_mut(agent_id)
                .ok_or_else(|| {
                    TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
                })?
                .fold_order = Some(0);
            self.bump();
        }
        Ok(refused)
    }

    #[cfg(debug_assertions)]
    fn assert_page_intent_index(
        &self,
        agent: &str,
        cursor: u64,
        order: Option<u64>,
        boundary: u64,
        indexed_due: &[FoldIntent],
    ) {
        let current: Vec<_> = self
            .index_fold_intents(agent)
            .into_iter()
            .filter(|(seq, next_order, _, _)| {
                *seq <= boundary
                    && !order.is_some_and(|order| {
                        *seq < cursor || *seq == cursor && *next_order <= order
                    })
            })
            .collect();
        assert!(
            indexed_due == current,
            "page fold intent index changed in its due range"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "deterministic injected integrity law")]
mod tests {
    use super::*;
    use crate::tools::TreeDriver;
    use crate::tree::{
        MemoryTreeStore, OperationRecord, TreeStore,
        test_support::{fake_tree, spawn_req},
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn an_idle_fold_still_joins_its_unprojected_session_allocation() {
        let (_tmp, _vcs, mut tree) = fake_tree();
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let locator = "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001";
        tree.record_child_session(&child, locator).unwrap();
        assert!(tree.node(&child).unwrap().session_locator.is_none());
        tree.fold_scratch_clones = 0;
        tree.fold_due(&child).unwrap();
        assert_eq!(
            tree.node(&child).unwrap().session_locator.as_deref(),
            Some(locator)
        );
        assert_eq!(tree.fold_scratch_clones, 1);
        tree.fold_due(&child).unwrap();
        assert_eq!(tree.fold_scratch_clones, 1);
    }

    #[test]
    fn idle_folds_and_only_deleted_close_targets_need_no_scratch_clone() {
        let (_tmp, _vcs, mut tree) = fake_tree();
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.fold_scratch_clones = 0;
        let before = format!("{tree:?}");
        assert_eq!(tree.fold_due(&child).unwrap(), [] as [IntentJoinRefusal; 0]);
        assert_eq!(format!("{tree:?}"), before);
        tree.nodes.get_mut(&child).unwrap().fold_order = None;
        tree.pending_close_folds.insert(child.clone());
        let before = format!("{tree:?}");
        assert_eq!(
            tree.fold_reserved_closes().unwrap(),
            [] as [IntentJoinRefusal; 0]
        );
        assert_eq!(format!("{tree:?}"), before);
        assert_eq!(tree.fold_scratch_clones, 0);
    }

    #[test]
    fn resume_tree_propagates_a_fold_integrity_refusal() {
        let (_tmp, _vcs, mut tree) = fake_tree();
        let id = "malformed-vcs-fault".to_owned();
        tree.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: "root".to_owned(),
                tool_name: crate::tree::workspaces::VCS_FAULT_KIND.to_owned(),
                canonical_request: "malformed".to_owned(),
                request_digest: String::new(),
                fold: Some(FoldAnchor {
                    seq: 0,
                    order: tree.revision,
                }),
                terminal_code: None,
                terminal_response: Some("effects_reserved".to_owned()),
            },
        );
        assert_eq!(
            tree.resume_tree().unwrap_err().code,
            TopologyErrorCode::OperationRecoveryRequired
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "page fold intent index changed in its due range")]
    fn page_index_rejects_a_new_same_agent_due_intent() {
        let (_tmp, _vcs, mut tree) = fake_tree();
        let index = tree.index_fold_intents("root");
        let id = "new-due".to_owned();
        tree.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: "root".to_owned(),
                tool_name: "interrupt".to_owned(),
                canonical_request: String::new(),
                request_digest: String::new(),
                fold: Some(FoldAnchor {
                    seq: 0,
                    order: tree.revision,
                }),
                terminal_code: None,
                terminal_response: None,
            },
        );
        let _ = tree.fold_agent_due_indexed("root", Some(&index));
    }

    #[test]
    fn fenced_integrity_violation_restores_raw_and_driver_boundaries() {
        for corruption in ["count", "operation"] {
            for driver_boundary in [false, true] {
                let (_tmp, vcs, mut tree) = fake_tree();
                let id = "injected-fenced".to_owned();
                tree.operations.insert(
                    id.clone(),
                    OperationRecord {
                        operation_id: id,
                        caller_agent_id: "root".to_owned(),
                        tool_name: "test_fenced_violation".to_owned(),
                        canonical_request: corruption.to_owned(),
                        request_digest: String::new(),
                        fold: Some(FoldAnchor {
                            seq: 0,
                            order: tree.revision,
                        }),
                        terminal_code: None,
                        terminal_response: Some("effects_reserved".to_owned()),
                    },
                );
                let store = Arc::new(MemoryTreeStore::new());
                tree.acknowledge_revision(store.save(&tree).unwrap());
                let before = format!("{tree:?}");
                let error = if driver_boundary {
                    let driver =
                        TreeDriver::new(Arc::new(Mutex::new(tree.clone())), store, vcs, None);
                    driver
                        .fold_and_commit(&mut tree, |tree| tree.fold_due_in_place("root"))
                        .unwrap_err()
                } else {
                    tree.fold_due("root").unwrap_err()
                };
                assert_eq!(error.code, TopologyErrorCode::OperationRecoveryRequired);
                assert_eq!(format!("{tree:?}"), before);
            }
        }
    }
}
