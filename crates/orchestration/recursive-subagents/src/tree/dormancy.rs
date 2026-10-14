//! Lossless, revivable checkout reclamation; intents own every crash cut.

use super::{
    BTreeMap, CandidateId, CandidateRecord, Deserialize, MintedCopy, NodeRecord, NodeStatus,
    OperationRecord, Path, PathBuf, ProductAgentTree, SendAgentMessageRequest, Serialize,
    TopologyError, TopologyErrorCode, VcsError, VcsOps, VcsSnapshot, WorkspaceMode,
    WorkspaceRecord, WorkspaceStatus, canonical_json, copy_id, digest_text, now_unix_millis,
};

const KIND: &str = "workspace_reclamation";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Phase {
    Sealing,
    Removing,
    Reclaimed,
    Restoring,
    Present,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reclamation {
    phase: Phase,
    generation: u64,
    prior: WorkspaceStatus,
    commit: Option<String>,
    content: Option<String>,
    change_id: Option<String>,
    revival_attempt: Option<super::AttemptId>,
}

impl Reclamation {
    fn snapshot(&self) -> Result<VcsSnapshot, TopologyError> {
        Ok(VcsSnapshot {
            commit: self.commit.clone().ok_or_else(reclamation_corrupt)?,
            content: self.content.clone().ok_or_else(reclamation_corrupt)?,
            change_id: self.change_id.clone(),
        })
    }
}

fn reclamation_corrupt() -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::WorkspaceRecoveryRequired,
        "workspace reclamation evidence is incomplete; checkout and pins retained",
    )
}

#[derive(Clone, Debug)]
pub(crate) struct DormancyTicket {
    agent_id: String,
    workspace: WorkspaceRecord,
    evidence: Reclamation,
    namespace: String,
    copy_id: String,
    reference_id: String,
}

#[derive(Debug)]
pub(crate) enum DormancyAnswer {
    Sealed(VcsSnapshot),
    Removed,
    Restored(MintedCopy),
}

impl DormancyTicket {
    pub(crate) fn run(&self, vcs: &dyn VcsOps, launch: &Path) -> Result<DormancyAnswer, VcsError> {
        match self.evidence.phase {
            Phase::Sealing => {
                vcs.check_reclaimable(&self.workspace.path, None)?;
                vcs.seal(&self.workspace.path, &self.namespace, &self.reference_id)
                    .map(DormancyAnswer::Sealed)
            }
            Phase::Removing => {
                if self.workspace.path.exists() {
                    let snapshot = self
                        .evidence
                        .snapshot()
                        .map_err(|e| VcsError::mint_unavailable(e.message))?;
                    vcs.check_reclaimable(&self.workspace.path, Some(&snapshot))?;
                }
                vcs.unmint(launch, &self.workspace.path)?;
                Ok(DormancyAnswer::Removed)
            }
            Phase::Restoring => {
                let snapshot = self
                    .evidence
                    .snapshot()
                    .map_err(|e| VcsError::mint_unavailable(e.message))?;
                // A crash after mint but before publication never handed this
                // copy to a runtime. Reuse it only after exact coverage proof.
                if self.workspace.path.exists() {
                    vcs.check_reclaimable(&self.workspace.path, Some(&snapshot))?;
                    return Ok(DormancyAnswer::Restored(MintedCopy {
                        path: self.workspace.path.clone(),
                        branch: vcs.label(&self.workspace.path),
                        base: snapshot,
                    }));
                }
                vcs.mint_from(
                    launch,
                    &self.agent_id,
                    &self.copy_id,
                    launch,
                    self.workspace.mode,
                    Some(&snapshot),
                )
                .map(DormancyAnswer::Restored)
            }
            Phase::Reclaimed | Phase::Present => Err(VcsError::mint_unavailable(
                "workspace reclamation has no pending effect",
            )),
        }
    }
}

impl ProductAgentTree {
    pub(crate) fn abandon_reclamation_dispatch(&mut self) {
        let ids: Vec<_> = self
            .workspaces
            .keys()
            .filter(|id| self.reclamation_pending(id))
            .cloned()
            .collect();
        for id in ids {
            self.removals_in_flight.remove(&id);
        }
    }
    pub(super) fn dormant(&self, node: &NodeRecord) -> bool {
        node.parent_agent_id.is_some()
            && !matches!(
                node.status,
                NodeStatus::Closed | NodeStatus::RecoveryRequired
            )
            && !self.holds_direct_child_slot(node)
            && !self.has_turn_owner(&node.agent_id)
            && !self.has_turn_waiter(&node.agent_id)
            && !self.seal_pending(&node.agent_id)
    }

    fn reclamation_id(workspace: &str) -> String {
        format!("plak:{KIND}:{workspace}")
    }

    fn reclamation(&self, workspace: &str) -> Result<Option<Reclamation>, TopologyError> {
        self.operations
            .get(&Self::reclamation_id(workspace))
            .map(|op| {
                serde_json::from_str(&op.canonical_request).map_err(|_| reclamation_corrupt())
            })
            .transpose()
    }

    pub(super) fn reclaimed(&self, workspace: &str) -> bool {
        self.reclamation(workspace)
            .ok()
            .flatten()
            .is_some_and(|e| e.phase == Phase::Reclaimed)
    }

    pub(super) fn rehydration_awaits_followup(&self, workspace: &str) -> bool {
        self.reclamation(workspace)
            .ok()
            .flatten()
            .is_some_and(|evidence| {
                evidence.phase == Phase::Present
                    && evidence.revival_attempt.as_ref().is_some_and(|attempt| {
                        self.nodes.values().any(|node| {
                            node.workspace_id == workspace
                                && node.status != NodeStatus::Closed
                                && node.attempt_id.as_ref() == Some(attempt)
                        })
                    })
            })
    }

    pub(super) fn reclamation_pending(&self, workspace: &str) -> bool {
        self.reclamation(workspace).is_ok_and(|e| {
            e.is_some_and(|e| {
                matches!(e.phase, Phase::Sealing | Phase::Removing | Phase::Restoring)
            })
        })
    }

    fn reclamation_quiescent(&self, workspace: &str) -> bool {
        self.nodes
            .values()
            .filter(|node| node.workspace_id == workspace)
            .all(|node| {
                !node.active_turn
                    && !self.has_turn_owner(&node.agent_id)
                    && !self.has_turn_waiter(&node.agent_id)
                    && !self.seal_pending(&node.agent_id)
                    && self.current_attempt(&node.agent_id).is_some_and(|attempt| {
                        attempt.status.is_stopped() && !attempt.has_unresolved_session_failure()
                    })
            })
    }

    fn write_reclamation(
        &mut self,
        workspace: &str,
        evidence: &Reclamation,
    ) -> Result<(), TopologyError> {
        let canonical = canonical_json(evidence)?;
        let id = Self::reclamation_id(workspace);
        let caller = self.workspace_producer(workspace)?;
        self.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                caller_agent_id: caller,
                tool_name: KIND.into(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some("owned lifecycle intent".into()),
                fold: None,
            },
        );
        self.bump();
        Ok(())
    }

    fn queue_reclamation(
        &mut self,
        workspace: &str,
        evidence: Reclamation,
    ) -> Result<(), TopologyError> {
        if !self.removals_in_flight.insert(workspace.into()) {
            return Ok(());
        }
        let row = self
            .workspaces
            .get(workspace)
            .ok_or_else(reclamation_corrupt)?
            .clone();
        let agent_id = self.workspace_producer(workspace)?;
        self.effects.dormancy.push(DormancyTicket {
            reference_id: format!("reclaimed-{}-{}", agent_id, evidence.generation),
            copy_id: copy_id(&self.tree_id, &agent_id),
            agent_id,
            namespace: digest_text(&self.tree_id),
            workspace: row,
            evidence,
        });
        Ok(())
    }

    fn workspace_producer(&self, workspace: &str) -> Result<String, TopologyError> {
        // Workspace ownership is the parent's cleanup grant, not the child
        // whose copy was minted. Inheritors are always deeper than that child.
        self.nodes
            .values()
            .filter(|node| node.workspace_id == workspace)
            .min_by_key(|node| node.depth)
            .map(|node| node.agent_id.clone())
            .ok_or_else(reclamation_corrupt)
    }

    /// Pure checkout inventory for the host's unlocked disk-pressure probe.
    #[must_use]
    pub fn dormant_checkouts(&self) -> Vec<(String, PathBuf)> {
        self.workspaces
            .values()
            .filter(|row| {
                row.mode != WorkspaceMode::Inherit
                    && matches!(
                        row.status,
                        WorkspaceStatus::Active | WorkspaceStatus::Adopted
                    )
            })
            .map(|row| (row.workspace_id.clone(), row.path.clone()))
            .collect()
    }

    /// Resumes acknowledged lifecycle intents, and reserves idle/LRU copies.
    /// The host must stop each returned owner's jobs before draining effects.
    pub fn reclaim_dormant(
        &mut self,
        now: u64,
        sizes: &BTreeMap<String, u64>,
    ) -> Result<Vec<String>, TopologyError> {
        let pending: Vec<_> = self
            .workspaces
            .keys()
            .filter_map(|id| {
                self.reclamation(id)
                    .ok()
                    .flatten()
                    .filter(|e| {
                        matches!(e.phase, Phase::Sealing | Phase::Removing | Phase::Restoring)
                    })
                    .map(|e| (id.clone(), e))
            })
            .collect();
        for (id, evidence) in pending {
            if self.reclamation_quiescent(&id) {
                self.queue_reclamation(&id, evidence)?;
            }
        }
        let mut eligible: Vec<_> = self
            .workspaces
            .values()
            .filter(|row| {
                row.mode != WorkspaceMode::Inherit
                    && !self.rehydration_awaits_followup(&row.workspace_id)
                    && matches!(
                        row.status,
                        WorkspaceStatus::Active | WorkspaceStatus::Adopted
                    )
            })
            .filter_map(|row| {
                let occupants: Vec<_> = self
                    .nodes
                    .values()
                    .filter(|node| node.workspace_id == row.workspace_id)
                    .collect();
                if occupants.is_empty()
                    || !occupants.iter().all(|node| {
                        self.dormant(node)
                            || node.status == NodeStatus::Closed
                                && self.reclamation_quiescent(&row.workspace_id)
                    })
                {
                    return None;
                }
                let ended = occupants
                    .iter()
                    .filter_map(|node| self.current_attempt(&node.agent_id)?.ended_at)
                    .max()?;
                let ended = if occupants
                    .iter()
                    .all(|node| node.status == NodeStatus::Closed)
                {
                    0
                } else {
                    ended
                };
                Some((ended, row.workspace_id.clone()))
            })
            .collect();
        eligible.sort();
        let mut total: u64 = sizes.values().copied().fold(0, u64::saturating_add);
        let budget = u64::from(self.snapshot.workspace_budget_mib) * 1024 * 1024;
        for (ended, id) in eligible {
            if now.saturating_sub(ended) < u64::from(self.snapshot.workspace_idle_seconds) * 1000
                && total <= budget
            {
                continue;
            }
            let row = self.workspaces.get(&id).ok_or_else(reclamation_corrupt)?;
            let evidence = Reclamation {
                phase: Phase::Sealing,
                generation: row.generation,
                prior: row.status,
                commit: None,
                content: None,
                change_id: None,
                revival_attempt: None,
            };
            self.write_reclamation(&id, &evidence)?;
            self.workspaces
                .get_mut(&id)
                .ok_or_else(reclamation_corrupt)?
                .status = WorkspaceStatus::Discarding;
            self.queue_reclamation(&id, evidence)?;
            total = total.saturating_sub(sizes.get(&id).copied().unwrap_or(0));
        }
        Ok(self
            .effects
            .dormancy
            .iter()
            .flat_map(|ticket| {
                self.nodes
                    .values()
                    .filter(|node| node.workspace_id == ticket.workspace.workspace_id)
                    .map(|node| node.agent_id.clone())
            })
            .collect())
    }

    /// Prepares a follow-up's exact checkout before opening its next attempt.
    pub(crate) fn rehydrate_followup(
        &mut self,
        caller: &str,
        request: &SendAgentMessageRequest,
        operation: &str,
    ) -> Result<(), TopologyError> {
        let agent = &request.agent_id;
        let id = self.node(agent)?.workspace_id.clone();
        if self.operations.contains_key(operation) || !self.reclaimed(&id) {
            return Ok(());
        }
        self.require_live()?;
        request
            .validate(caller)
            .map_err(|code| TopologyError::new(code, "invalid followup request"))?;
        if !self.grant(caller)?.message_peers.contains(agent) {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "recipient is not a message peer",
            ));
        }
        let status = self.node(agent)?.status;
        self.admit_followup(
            agent,
            status,
            self.has_current_turn_owner(agent),
            request.output_schema_json.as_deref(),
        )?;
        let Some(mut evidence) = self
            .reclamation(&id)?
            .filter(|e| e.phase == Phase::Reclaimed)
        else {
            return Ok(());
        };
        evidence.phase = Phase::Restoring;
        evidence.revival_attempt = self.node(agent)?.attempt_id.clone();
        self.write_reclamation(&id, &evidence)?;
        self.workspaces
            .get_mut(&id)
            .ok_or_else(reclamation_corrupt)?
            .status = WorkspaceStatus::Discarding;
        self.queue_reclamation(&id, evidence)
    }

    pub(super) fn complete_dormancy(
        &mut self,
        ticket: DormancyTicket,
        answer: Result<DormancyAnswer, VcsError>,
    ) -> Result<(), TopologyError> {
        let id = &ticket.workspace.workspace_id;
        self.removals_in_flight.remove(id);
        let Some(mut evidence) = self.reclamation(id)? else {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "workspace reclamation intent disappeared",
            ));
        };
        if evidence.phase != ticket.evidence.phase
            || evidence.generation != ticket.evidence.generation
        {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                format!(
                    "workspace reclamation effect mismatch: {:?}/{} versus {:?}/{}",
                    ticket.evidence.phase,
                    ticket.evidence.generation,
                    evidence.phase,
                    evidence.generation
                ),
            ));
        }
        match answer {
            Ok(DormancyAnswer::Sealed(snapshot)) => {
                {
                    let node = self.node(&ticket.agent_id)?;
                    let attempt = node.attempt_id.clone().ok_or_else(|| {
                        TopologyError::new(
                            TopologyErrorCode::WorkspaceRecoveryRequired,
                            "reclamation owner has no attempt",
                        )
                    })?;
                    let candidate_id = CandidateId::from_persisted(ticket.reference_id.clone());
                    self.candidates.insert(
                        candidate_id.clone(),
                        CandidateRecord {
                            candidate_id,
                            workspace_id: id.clone(),
                            producing_agent_id: node.agent_id.clone(),
                            producing_attempt_id: attempt,
                            base_commit: ticket
                                .workspace
                                .base_commit
                                .clone()
                                .ok_or_else(reclamation_corrupt)?,
                            base_content: ticket
                                .workspace
                                .base_content
                                .clone()
                                .ok_or_else(reclamation_corrupt)?,
                            base_change_id: ticket.workspace.base_change_id.clone(),
                            commit: snapshot.commit.clone(),
                            content: snapshot.content.clone(),
                            change_id: snapshot.change_id.clone(),
                            sealed_at: now_unix_millis(),
                            adopted_by: None,
                            adopted_evidence: Vec::new(),
                            merged_at: None,
                        },
                    );
                }
                evidence.commit = Some(snapshot.commit);
                evidence.content = Some(snapshot.content);
                evidence.change_id = snapshot.change_id;
                evidence.phase = Phase::Removing;
                self.write_reclamation(id, &evidence)?;
                self.queue_reclamation(id, evidence)?;
            }
            Ok(DormancyAnswer::Removed) => {
                evidence.phase = Phase::Reclaimed;
                self.workspaces
                    .get_mut(id)
                    .ok_or_else(reclamation_corrupt)?
                    .status = WorkspaceStatus::CleanRemoved;
                self.write_reclamation(id, &evidence)?;
            }
            Ok(DormancyAnswer::Restored(copy)) => {
                if copy.path != ticket.workspace.path
                    || copy.base.content
                        != evidence.content.clone().ok_or_else(reclamation_corrupt)?
                {
                    return Err(reclamation_corrupt());
                }
                evidence.phase = Phase::Present;
                let row = self
                    .workspaces
                    .get_mut(id)
                    .ok_or_else(reclamation_corrupt)?;
                row.status = evidence.prior;
                row.generation = row.generation.saturating_add(1);
                self.write_reclamation(id, &evidence)?;
            }
            Err(error) => {
                // A refused seal deletes nothing. Failed removal retains its
                // durable seal and retry intent; failed mint retains its pin.
                match evidence.phase {
                    Phase::Sealing => {
                        evidence.phase = Phase::Present;
                        self.workspaces
                            .get_mut(id)
                            .ok_or_else(reclamation_corrupt)?
                            .status = evidence.prior;
                    }
                    Phase::Restoring => {
                        evidence.phase = Phase::Reclaimed;
                        self.workspaces
                            .get_mut(id)
                            .ok_or_else(reclamation_corrupt)?
                            .status = WorkspaceStatus::CleanRemoved;
                    }
                    Phase::Removing if error.kind == super::VcsErrorKind::MintUnavailable => {
                        evidence.phase = Phase::Present;
                        let row = self
                            .workspaces
                            .get_mut(id)
                            .ok_or_else(reclamation_corrupt)?;
                        row.status = evidence.prior;
                        row.generation = row.generation.saturating_add(1);
                    }
                    _ => {}
                }
                self.write_reclamation(id, &evidence)?;
                self.record_fault(&ticket.agent_id, error.detail);
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "workspace reclamation refused; checkout and sealed work retained",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
pub(super) mod tests {
    use super::*;
    use crate::tree::test_support::*;
    use crate::tree::{AwaitAgents, WriterMode};

    pub(in crate::tree) fn git_repo() -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@local",
                "commit",
                "--allow-empty",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(repo.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        repo
    }

    #[test]
    fn dormant_reclaim_preserves_uncommitted_work_and_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("WORK.txt"), "uncommitted\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate = candidate_of(&tree, &child.agent_id);
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        tree.mark_wait_delivered("root", &result);
        assert!(tree.dormant(tree.node(&child.agent_id).unwrap()));
        tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
        let settled = tree.settle_for_test();
        assert!(
            !path.exists(),
            "dormant checkout was not reclaimed: {:?}; {:?}; {:?}; {:?}",
            settled.cleanup_failures,
            tree.fault_reason(&child.agent_id),
            tree.workspaces,
            tree.operations.get(&ProductAgentTree::reclamation_id(
                &tree.node(&child.agent_id).unwrap().workspace_id
            ))
        );
        tree.adopt_candidate("root", &adopt_req(&candidate), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("WORK.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    fn finished() -> (tempfile::TempDir, ProductAgentTree, String, PathBuf) {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree
            .spawn_agent("root", &mint_req(), None)
            .unwrap()
            .agent_id;
        let path = tree.node(&child).unwrap().path.clone();
        std::fs::write(path.join("WORK.txt"), "sealed\n").unwrap();
        tree.record_turn_completed(&child).unwrap();
        tree.settle_for_test();
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        tree.mark_wait_delivered("root", &result);
        (repo, tree, child, path)
    }

    #[test]
    fn dormant_reclaim_crash_cuts_seal_remove_and_rehydrate() {
        for cut in 0..6 {
            let (_repo, mut tree, child, path) = finished();
            std::fs::write(path.join("AFTER.txt"), "late uncommitted\n").unwrap();
            tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
            if cut > 0 {
                let completed = tree.take_effects().run(tree.vcs());
                if cut > 1 {
                    tree.complete(completed);
                }
            }
            if cut > 2 {
                let completed = tree.take_effects().run(tree.vcs());
                if cut > 3 {
                    tree.complete(completed);
                }
            }
            let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
            reopened
                .reclaim_dormant(u64::MAX, &BTreeMap::new())
                .unwrap();
            assert!(
                reopened.settle_for_test().cleanup_failures.is_empty(),
                "cut {cut}; {:?}",
                reopened.fault_reason(&child)
            );
            assert!(!path.exists(), "cut {cut}");
            reopened.state = super::super::TreeAdmissionState::Live;
            let request = SendAgentMessageRequest {
                agent_id: child.clone(),
                message: "continue".into(),
                purpose: Some(super::super::MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            };
            reopened
                .rehydrate_followup("root", &request, "followup")
                .unwrap();
            if cut == 5 {
                let _unpublished = reopened.take_effects().run(reopened.vcs());
                reopened = ProductAgentTree::reopen(reopened.to_persisted());
                reopened
                    .reclaim_dormant(u64::MAX, &BTreeMap::new())
                    .unwrap();
            }
            assert!(
                reopened.settle_for_test().cleanup_failures.is_empty(),
                "rehydrate cut {cut}"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("AFTER.txt")).unwrap(),
                "late uncommitted\n"
            );
            reopened.state = super::super::TreeAdmissionState::Live;
            reopened.send_agent_message("root", &request, None).unwrap();
            assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Running);
        }
    }

    #[test]
    fn dormant_reclaim_retains_unknown_ignored_secret_and_staged_content() {
        for kind in 0..3 {
            let (_repo, mut tree, _child, path) = finished();
            match kind {
                0 => {
                    std::fs::write(path.join(".gitignore"), "private-cache\n").unwrap();
                    std::fs::write(path.join("private-cache"), "must retain\n").unwrap();
                }
                1 => {
                    std::fs::write(path.join(".env"), "not credentials\n").unwrap();
                }
                _ => {
                    std::fs::write(path.join("WORK.txt"), "staged\n").unwrap();
                    assert!(
                        std::process::Command::new("git")
                            .args(["add", "WORK.txt"])
                            .current_dir(&path)
                            .status()
                            .unwrap()
                            .success()
                    );
                    std::fs::write(path.join("WORK.txt"), "working\n").unwrap();
                }
            }
            tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
            assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
            assert!(path.exists());
        }
    }

    #[test]
    fn dormant_reclaim_retains_changes_between_seal_and_delete() {
        let (_repo, mut tree, _child, path) = finished();
        tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
        let completed = tree.take_effects().run(tree.vcs());
        tree.complete(completed);
        std::fs::write(path.join("AFTER.txt"), "must retain\n").unwrap();
        assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
        assert_eq!(
            std::fs::read_to_string(path.join("AFTER.txt")).unwrap(),
            "must retain\n"
        );
    }

    #[test]
    fn dormant_reclaim_disk_pressure_selects_oldest_and_never_shared_live_copy() {
        let (_repo, mut tree, child, path) = finished();
        let workspace = tree.node(&child).unwrap().workspace_id.clone();
        let now = tree.attempt(&child).unwrap().ended_at.unwrap();
        tree.reclaim_dormant(now, &BTreeMap::new()).unwrap();
        assert!(tree.take_effects().is_empty());
        tree.snapshot.workspace_budget_mib = 1;
        tree.reclaim_dormant(now, &BTreeMap::from([(workspace, 2 * 1024 * 1024)]))
            .unwrap();
        assert!(!tree.take_effects().is_empty());
        assert!(path.exists(), "preparation cannot delete the copy");
    }
}
