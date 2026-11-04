//! Lossless, revivable checkout reclamation; intents own every crash cut.

use super::{
    BTreeMap, CandidateId, CandidateRecord, Deserialize, MintedCopy, NodeRecord, NodeStatus,
    OperationRecord, Path, PathBuf, ProductAgentTree, SendAgentMessageRequest, Serialize,
    TopologyError, TopologyErrorCode, VcsError, VcsOps, VcsSnapshot, WorkspaceMode,
    WorkspaceRecord, WorkspaceStatus, canonical_json, digest_text, now_unix_millis,
};

const KIND: &str = "workspace_reclamation";

#[derive(Clone, Debug)]
pub(super) struct ReclaimBackoff {
    generation: u64,
    failures: u32,
    next_at: u64,
    detail: String,
}

impl ReclaimBackoff {
    fn failed(previous: Option<&Self>, generation: u64, now: u64, detail: String) -> Self {
        let failures = previous
            .filter(|prior| prior.generation == generation)
            .map_or(0, |prior| prior.failures)
            .saturating_add(1);
        let delay = 30_000_u64
            .saturating_mul(1_u64 << failures.saturating_sub(1).min(7))
            .min(3_600_000);
        Self {
            generation,
            failures,
            next_at: now.saturating_add(delay),
            detail,
        }
    }
}

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
            history_left_base: false,
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
    reference_id: String,
    trim_target: bool,
    trim_outcome: Option<TrimOutcome>,
}

impl DormancyTicket {
    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

#[derive(Debug)]
pub(crate) enum DormancyAnswer {
    Sealed(VcsSnapshot),
    Removed,
    Restored(MintedCopy),
}

#[derive(Clone, Copy, Debug)]
enum TrimOutcome {
    Trimmed,
    Retained,
    Refused,
}

impl TrimOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trimmed => "trimmed",
            Self::Retained => "retained",
            Self::Refused => "refused",
        }
    }
}

impl DormancyTicket {
    pub(crate) async fn run(
        &mut self,
        vcs: &dyn VcsOps,
        launch: &Path,
    ) -> Result<DormancyAnswer, VcsError> {
        if self.evidence.phase == Phase::Sealing
            && let Err(error) = vcs.check_reclaimable(&self.workspace.path, None).await
        {
            if self.trim_target
                && error.reclamation_refusal
                    == Some(plakous_workspace::occupancy_vcs::ReclamationRefusal::StaticCoverage)
            {
                self.trim_outcome = Some(
                    match vcs.trim_disposable_target(&self.workspace.path).await {
                        Ok(true) => TrimOutcome::Trimmed,
                        Ok(false) => TrimOutcome::Retained,
                        Err(_) => TrimOutcome::Refused,
                    },
                );
            }
            return Err(error);
        }
        self.run_step(vcs, launch).await
    }

    /// Whether the copy is present is asked of the port: an unanswered
    /// question fails the step, so a copy that only looks absent is never
    /// removed unproven or minted over.
    async fn run_step(&self, vcs: &dyn VcsOps, launch: &Path) -> Result<DormancyAnswer, VcsError> {
        match self.evidence.phase {
            Phase::Sealing => vcs
                .seal(
                    &self.workspace.path,
                    &self.namespace,
                    &self.reference_id,
                    self.workspace
                        .base_commit
                        .as_deref()
                        .ok_or_else(|| VcsError::seal_failed("copy has no recorded base"))?,
                )
                .await
                .map(DormancyAnswer::Sealed),
            Phase::Removing => {
                if vcs.path_exists(&self.workspace.path).await? {
                    let snapshot = self
                        .evidence
                        .snapshot()
                        .map_err(|e| VcsError::mint_unavailable(e.message))?;
                    vcs.check_reclaimable(&self.workspace.path, Some(&snapshot))
                        .await?;
                }
                vcs.unmint(launch, &self.workspace.path, &self.namespace)
                    .await?;
                Ok(DormancyAnswer::Removed)
            }
            Phase::Restoring => {
                let snapshot = self
                    .evidence
                    .snapshot()
                    .map_err(|e| VcsError::mint_unavailable(e.message))?;
                // A crash after mint but before publication never handed this
                // copy to a runtime. Reuse it only after exact coverage proof.
                if vcs.path_exists(&self.workspace.path).await? {
                    vcs.check_reclaimable(&self.workspace.path, Some(&snapshot))
                        .await?;
                    self.restore_base(vcs).await?;
                    return Ok(DormancyAnswer::Restored(MintedCopy {
                        path: self.workspace.path.clone(),
                        branch: vcs.label(&self.workspace.path).await,
                        base: snapshot,
                    }));
                }
                let minted = vcs
                    .mint_from(&super::MintRequest {
                        launch,
                        agent_id: &self.agent_id,
                        namespace: &self.namespace,
                        parent_cwd: launch,
                        mode: self.workspace.mode,
                        base: Some(&snapshot),
                    })
                    .await?;
                self.restore_base(vcs).await?;
                Ok(DormancyAnswer::Restored(minted))
            }
            Phase::Reclaimed | Phase::Present => Err(VcsError::mint_unavailable(
                "workspace reclamation has no pending effect",
            )),
        }
    }

    async fn restore_base(&self, vcs: &dyn VcsOps) -> Result<(), VcsError> {
        vcs.restore_copy_base(
            &self.workspace.path,
            &self.namespace,
            &self.agent_id,
            self.workspace
                .base_commit
                .as_deref()
                .ok_or_else(|| VcsError::seal_failed("copy has no recorded logical base"))?,
        )
        .await
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
        trim_target: bool,
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
            agent_id,
            namespace: digest_text(&self.tree_id),
            workspace: row,
            evidence,
            trim_target,
            trim_outcome: None,
        });
        Ok(())
    }

    pub(super) fn workspace_producer(&self, workspace: &str) -> Result<String, TopologyError> {
        // Workspace ownership is the parent's cleanup grant, not the child
        // whose copy was minted. Inheritors are always deeper than that child.
        self.nodes
            .values()
            .filter(|node| node.workspace_id == workspace)
            .min_by_key(|node| node.depth)
            .map(|node| node.agent_id.clone())
            .ok_or_else(reclamation_corrupt)
    }

    pub(crate) fn reclamation_retry_at(&self, workspace: &str) -> u64 {
        self.reclaim_backoff
            .get(workspace)
            .filter(|retry| {
                self.workspaces
                    .get(workspace)
                    .is_some_and(|row| row.generation == retry.generation)
            })
            .map_or(0, |retry| retry.next_at)
    }

    pub(crate) fn reset_reclamation_backoff(&mut self, agent: &str) {
        if let Some(node) = self.nodes.get(agent) {
            self.reclaim_backoff.remove(&node.workspace_id);
        }
    }

    pub(crate) fn checkout_size_probes(&self) -> Vec<(String, PathBuf, u64, u64, u64)> {
        // Do not scan a copy while an unlocked refresh is writing it. Reclaim
        // admission also fences the intent when sizes came from an older scan.
        self.workspaces
            .values()
            .filter(|row| {
                row.mode != WorkspaceMode::Inherit
                    && !self.refresh_pending(&row.workspace_id)
                    && matches!(
                        row.status,
                        WorkspaceStatus::Active
                            | WorkspaceStatus::Adopted
                            | WorkspaceStatus::Discarding
                    )
            })
            .map(|row| {
                (
                    row.workspace_id.clone(),
                    row.path.clone(),
                    row.generation,
                    self.reclamation_retry_at(&row.workspace_id),
                    self.reclaim_trim_epochs
                        .get(&row.workspace_id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect()
    }

    /// Resumes acknowledged lifecycle intents, and reserves idle/LRU copies.
    /// The host must stop each returned owner's jobs before draining effects.
    #[allow(
        clippy::too_many_lines,
        reason = "dormancy admission orders retained-candidate, live-turn, occupancy and trim-backoff checks"
    )]
    pub fn reclaim_dormant(
        &mut self,
        now: u64,
        sizes: &BTreeMap<String, u64>,
    ) -> Result<Vec<String>, TopologyError> {
        let over_budget = sizes.values().copied().fold(0_u64, u64::saturating_add)
            > u64::from(self.current_epoch().workspace_budget_mib) * 1024 * 1024;
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
            if now >= self.reclamation_retry_at(&id) && self.reclamation_quiescent(&id) {
                self.queue_reclamation(&id, evidence, over_budget)?;
            }
        }
        let mut eligible: Vec<_> = self
            .workspaces
            .values()
            .filter(|row| {
                row.mode != WorkspaceMode::Inherit
                    && !self.refresh_pending(&row.workspace_id)
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
        let budget = u64::from(self.current_epoch().workspace_budget_mib) * 1024 * 1024;
        for (ended, id) in eligible {
            if now < self.reclamation_retry_at(&id) {
                continue;
            }
            if now.saturating_sub(ended)
                < u64::from(self.current_epoch().workspace_idle_seconds) * 1000
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
            self.queue_reclamation(&id, evidence, over_budget)?;
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
        self.queue_reclamation(&id, evidence, false)
    }

    pub(super) fn complete_dormancy(
        &mut self,
        ticket: DormancyTicket,
        answer: Result<DormancyAnswer, VcsError>,
    ) -> Result<(), TopologyError> {
        self.complete_dormancy_at(ticket, answer, now_unix_millis())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "dormancy completion reconciles the same workspace generation with unmint and snapshot outcomes"
    )]
    fn complete_dormancy_at(
        &mut self,
        ticket: DormancyTicket,
        answer: Result<DormancyAnswer, VcsError>,
        now: u64,
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
        if let Some(outcome) = ticket.trim_outcome {
            if matches!(outcome, TrimOutcome::Trimmed | TrimOutcome::Refused) {
                let epoch = self.reclaim_trim_epochs.entry(id.clone()).or_default();
                *epoch = epoch.saturating_add(1);
            }
            tracing::info!(target: "plak.workspace", workspace_id = %id,
                result = outcome.as_str(), refusal_class = "static_coverage", "workspace target trim");
        }
        match answer {
            Ok(DormancyAnswer::Sealed(snapshot)) => {
                {
                    let resolver = self.resolver_input(id)?;
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
                            reverted_at: None,
                            reverted_by: None,
                            kind: super::CandidateKind::Work,
                            source_candidate_id: None,
                            conflict_target: resolver.as_ref().map(|input| input.target.clone()),
                            conflict_paths: resolver
                                .as_ref()
                                .map_or_else(Vec::new, |input| input.paths.clone()),
                            adopted_via: resolver
                                .as_ref()
                                .and_then(|input| input.superseded_by.clone()),
                            resolves: resolver.map(|input| input.source),
                            target_before: None,
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
                self.queue_reclamation(id, evidence, ticket.trim_target)?;
            }
            Ok(DormancyAnswer::Removed) => {
                self.reclaim_backoff.remove(id);
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
                self.reclaim_backoff.remove(id);
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
                let previous_generation = self
                    .workspaces
                    .get(id)
                    .ok_or_else(reclamation_corrupt)?
                    .generation;
                let previous = self
                    .reclaim_backoff
                    .get(id)
                    .filter(|prior| prior.generation == previous_generation)
                    .cloned();
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
                let detail: String = error
                    .detail
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(512)
                    .collect();
                let generation = self
                    .workspaces
                    .get(id)
                    .ok_or_else(reclamation_corrupt)?
                    .generation;
                let prior = previous.as_ref();
                if prior.is_none_or(|retry| retry.detail != detail) {
                    tracing::warn!(target: "plak.workspace", workspace_id = %id, %detail,
                        "workspace reclamation refused; checkout and sealed work retained");
                }
                let mut retry =
                    ReclaimBackoff::failed(prior, previous_generation, now, detail.clone());
                retry.generation = generation;
                self.reclaim_backoff.insert(id.clone(), retry);
                self.record_fault(&ticket.agent_id, detail.clone());
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    format!(
                        "workspace reclamation refused; checkout and sealed work retained: {detail}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
pub(super) mod tests;
