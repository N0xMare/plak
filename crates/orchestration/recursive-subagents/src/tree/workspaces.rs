//! Workspace binding, minting, discard, and VCS fault handling.
//!

use super::{
    Arc, AtomicU64, AttemptFailure, BoundWorkspace, DiscardWorkspaceOutput, LiveGeneration,
    MintHook, MintTicket, MintedCopy, NodeStatus, Ordering, Path, PathBuf, ProductAgentTree,
    SpawnAgentRequest, SpawnWorkspace, Tier, TopologyError, TopologyErrorCode, TreeAdmissionState,
    UnmintTicket, VcsError, VcsSnapshot, WorkspaceIdRequest, WorkspaceMode, WorkspacePlan,
    WorkspaceRecord, WorkspaceStatus, bounded_text, copy_id, now_unix_millis,
};

#[cfg(any(test, feature = "test-support"))]
use super::CandidateRecord;

pub(super) const VCS_FAULT_KIND: &str = "vcs_fault";
pub(super) const VCS_FAULT_SETTLED_KIND: &str = "vcs_fault_settled";

#[derive(serde::Serialize, serde::Deserialize)]
struct VcsFault {
    source_id: String,
    workspace_id: Option<String>,
    attempt_id: Option<super::AttemptId>,
    reason: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VcsFaultSettled {
    fault_id: String,
    attempt_id: Option<super::AttemptId>,
}

impl ProductAgentTree {
    /// Lifts the restore overlay after acknowledged host consequences quiesce.
    pub(crate) fn lift_restored_pause_if_quiescent(&mut self) {
        if self.state == TreeAdmissionState::RestoredPaused && self.quiescence_refusal().is_none() {
            self.state = TreeAdmissionState::Live;
            self.bump();
        }
    }

    /// Reopen law for a persisted tree. Only a tree with work
    /// still in flight — a non-terminal descendant or a pending spawn — has
    /// anything to resume, so only that shape becomes `RestoredPaused`. A
    /// quiescent tree (the attached singleton, or every descendant terminal)
    /// reopens on its persisted admission state; a stale pause or interrupt
    /// fence from an earlier process is lifted because there is no descendant
    /// left for it to fence. Painting such a tree paused offered the operator a
    /// resume with nothing to resume and refused every rebind behind it.
    pub fn restore_paused(&mut self) {
        // Nothing recovers the root (`recovery_need` is `NotRequired` for it),
        // so a root an earlier build faulted for a child copy's VCS step
        // reopens running instead of refusing its own waits for good.
        if let Some(root) = self.nodes.get_mut(&self.root_agent_id)
            && root.status == NodeStatus::RecoveryRequired
        {
            root.status = NodeStatus::Running;
        }
        self.fault_in_flight_vcs();
        let agents: std::collections::BTreeSet<_> = self
            .operations
            .values()
            .filter(|record| record.tool_name == VCS_FAULT_KIND)
            .map(|record| record.caller_agent_id.clone())
            .collect();
        for agent in agents {
            if let Err(error) = self.fold_due(&agent) {
                tracing::error!(agent_id = %agent, refusal = %error.message, "tree restore fold refused");
            }
        }
        if self.quiescence_refusal().is_none() {
            if matches!(
                self.state,
                TreeAdmissionState::RestoredPaused | TreeAdmissionState::Interrupted
            ) {
                self.state = TreeAdmissionState::Live;
            }
            self.bump();
            return;
        }
        self.state = TreeAdmissionState::RestoredPaused;
        self.bump();
    }

    /// A pending seal reservation or an `Adopting` or `Discarding` workspace row means
    /// a VCS subprocess was in flight (or a removal failed) when the process
    /// ended. Reopening one is `RecoveryRequired` and MUST NOT auto-retry —
    /// the snapshot or the three-way landing may have half-run.
    ///
    /// An in-flight row faults its nodes that are not closed: its occupants,
    /// and for an `Adopting` row the recorded adopter, whose checkout the
    /// landing writes however the producer ended (the copy's owner when the
    /// row names no adopter). A closed node
    /// was settled by `close_agent`, the remedy, and is never faulted again; a
    /// removal never touches its owner's checkout. The root is never faulted,
    /// because nothing can recover it: an adopt landing into the root is
    /// recorded as the root's fault reason instead.
    pub(super) fn fault_in_flight_vcs(&mut self) {
        let seals: Vec<_> = self
            .operations
            .values()
            .filter(|record| {
                record.tool_name == "seal_effect" && record.terminal_response.is_none()
            })
            .map(|record| {
                (
                    record.caller_agent_id.clone(),
                    record.operation_id.clone(),
                    record.canonical_request.clone(),
                )
            })
            .collect();
        for (agent_id, source_id, canonical) in seals {
            #[derive(serde::Deserialize)]
            struct SealReservation {
                attempt_id: Option<super::AttemptId>,
                reserved_at: super::FoldAnchor,
            }
            let Ok(reservation) = serde_json::from_str::<SealReservation>(&canonical) else {
                continue;
            };
            let mut anchor = self.vcs_fault_anchor(&agent_id);
            if anchor.seq == 0 {
                anchor.seq = reservation.reserved_at.seq;
            }
            self.record_vcs_fault(
                &agent_id,
                VcsFault {
                    source_id,
                    workspace_id: None,
                    attempt_id: reservation.attempt_id,
                    reason: "a VCS seal, adopt, or removal was in flight when the process ended"
                        .to_owned(),
                },
                anchor,
            );
        }
        let workspaces: Vec<_> = self
            .workspaces
            .values()
            .filter(|row| row.status.is_in_flight() && !self.reclamation_pending(&row.workspace_id))
            .cloned()
            .collect();
        for row in workspaces {
            let affected: Vec<_> = self
                .nodes
                .values()
                .filter(|node| {
                    node.workspace_id == row.workspace_id
                        || row.status == WorkspaceStatus::Adopting
                            && node.agent_id
                                == *row.adopter_agent_id.as_ref().unwrap_or(&row.owner_agent_id)
                })
                .map(|node| (node.agent_id.clone(), node.attempt_id.clone()))
                .collect();
            for (agent_id, attempt_id) in affected {
                let reason = if agent_id == self.root_agent_id
                    && row.status == WorkspaceStatus::Adopting
                {
                    "an adopt into this checkout was in flight when the process ended; inspect it before relying on its contents"
                } else {
                    "a VCS seal, adopt, or removal was in flight when the process ended"
                };
                let anchor = self.vcs_fault_anchor(&agent_id);
                self.record_vcs_fault(
                    &agent_id,
                    VcsFault {
                        source_id: format!("workspace:{}", row.workspace_id),
                        workspace_id: Some(row.workspace_id.clone()),
                        attempt_id,
                        reason: reason.to_owned(),
                    },
                    anchor,
                );
            }
        }
    }

    fn vcs_fault_anchor(&self, agent_id: &str) -> super::FoldAnchor {
        let mut anchor = self.fold_anchor_now(agent_id).unwrap_or(super::FoldAnchor {
            seq: 0,
            order: self.revision,
        });
        if anchor.seq == 0 {
            anchor.seq = self
                .operations
                .values()
                .filter(|record| record.caller_agent_id == agent_id)
                .filter_map(|record| record.fold.map(|fold| fold.seq))
                .max()
                .unwrap_or(0);
        }
        anchor
    }

    fn record_vcs_fault(&mut self, agent_id: &str, mut fault: VcsFault, anchor: super::FoldAnchor) {
        if self.operations.values().any(|record| {
            record.tool_name == VCS_FAULT_KIND
                && record.caller_agent_id == agent_id
                && !self.vcs_fault_is_settled(agent_id, &record.operation_id, false)
                && serde_json::from_str::<VcsFault>(&record.canonical_request).is_ok_and(
                    |previous| {
                        previous.source_id == fault.source_id
                            && previous.attempt_id == fault.attempt_id
                    },
                )
        }) {
            return;
        }
        if let Some(prior) = self.fault_reasons.get(agent_id) {
            fault.reason.clone_from(prior);
        }
        let Ok(canonical) = super::canonical_json(&fault) else {
            return;
        };
        let id = format!("plak:vcs-fault:{agent_id}:{}", anchor.order);
        self.operations.insert(
            id.clone(),
            super::OperationRecord {
                fold: Some(anchor),
                operation_id: id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: VCS_FAULT_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
    }

    pub(super) fn join_vcs_fault(&mut self, agent_id: &str, id: &str) -> Result<(), TopologyError> {
        let fault: VcsFault = serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "VCS fault intent is invalid",
                )
            })?;
        let node = self.node(agent_id)?;
        if node.attempt_id != fault.attempt_id
            || node.status == NodeStatus::Closed
            || self
                .current_attempt(agent_id)
                .is_some_and(super::AttemptRecord::recovery_was_abandoned)
        {
            return Ok(());
        }
        self.fault_reasons.insert(agent_id.to_owned(), fault.reason);
        if agent_id != self.root_agent_id
            && let Some(node) = self.nodes.get_mut(agent_id)
        {
            node.status = NodeStatus::RecoveryRequired;
            node.active_turn = false;
        }
        self.bump();
        Ok(())
    }

    pub(super) fn join_vcs_fault_settled(&mut self, id: &str) -> Result<(), TopologyError> {
        let _: VcsFaultSettled = serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "VCS settlement intent is invalid",
                )
            })?;
        Ok(())
    }

    fn vcs_fault_is_settled(&self, agent_id: &str, fault_id: &str, as_of_position: bool) -> bool {
        self.operations.values().any(|record| {
            record.tool_name == VCS_FAULT_SETTLED_KIND
                && record.caller_agent_id == agent_id
                && (!as_of_position
                    || record
                        .fold
                        .is_some_and(|anchor| self.fold_anchor_applied(agent_id, anchor)))
                && serde_json::from_str::<VcsFaultSettled>(&record.canonical_request)
                    .is_ok_and(|settled| settled.fault_id == fault_id)
        })
    }

    pub(super) fn seal_effect_faulted(&self, agent_id: &str) -> bool {
        self.has_vcs_fault(agent_id, true)
    }

    pub(super) fn has_faulted_vcs_obligation(&self, agent_id: &str) -> bool {
        self.has_vcs_fault(agent_id, false)
    }

    fn has_vcs_fault(&self, agent_id: &str, seal_only: bool) -> bool {
        let Some(node) = self.nodes.get(agent_id) else {
            return false;
        };
        self.operations.values().any(|record| {
            record.tool_name == VCS_FAULT_KIND
                && record.caller_agent_id == agent_id
                && record
                    .fold
                    .is_some_and(|anchor| self.fold_anchor_applied(agent_id, anchor))
                && !self.vcs_fault_is_settled(agent_id, &record.operation_id, true)
                && serde_json::from_str::<VcsFault>(&record.canonical_request).is_ok_and(|fault| {
                    fault.attempt_id == node.attempt_id
                        && (!seal_only || fault.workspace_id.is_none())
                })
        })
    }

    pub(super) fn settle_workspace_faults(&mut self, workspace_id: &str) {
        let faults: Vec<_> = self
            .operations
            .values()
            .filter_map(|record| {
                if record.tool_name != VCS_FAULT_KIND {
                    return None;
                }
                let fault: VcsFault = serde_json::from_str(&record.canonical_request).ok()?;
                (fault.workspace_id.as_deref() == Some(workspace_id)
                    && !self.vcs_fault_is_settled(
                        &record.caller_agent_id,
                        &record.operation_id,
                        false,
                    ))
                .then(|| {
                    (
                        record.operation_id.clone(),
                        record.caller_agent_id.clone(),
                        fault.attempt_id,
                    )
                })
            })
            .collect();
        for (fault_id, agent_id, attempt_id) in faults {
            let Ok(canonical) = super::canonical_json(&VcsFaultSettled {
                fault_id: fault_id.clone(),
                attempt_id,
            }) else {
                continue;
            };
            let id = format!("{fault_id}:settled");
            self.operations.insert(
                id.clone(),
                super::OperationRecord {
                    fold: Some(self.vcs_fault_anchor(&agent_id)),
                    operation_id: id,
                    caller_agent_id: agent_id.clone(),
                    tool_name: VCS_FAULT_SETTLED_KIND.to_owned(),
                    request_digest: super::digest_text(&canonical),
                    canonical_request: canonical,
                    terminal_code: None,
                    terminal_response: None,
                },
            );
            self.bump();
            if let Err(error) = self.fold_due(&agent_id) {
                tracing::error!(agent_id = %agent_id, refusal = %error.message, "workspace fault settlement fold refused");
            }
        }
    }

    /// Resumes a tree that reopened paused, after faulting anything the
    /// crash left mid-VCS.
    ///
    /// # Errors
    ///
    /// Refuses a tree that is closed or terminal — those never resume.
    pub fn resume_tree(&mut self) -> Result<(), TopologyError> {
        self.fault_in_flight_vcs();
        let agents: std::collections::BTreeSet<_> = self
            .operations
            .values()
            .filter(|record| record.tool_name == VCS_FAULT_KIND)
            .map(|record| record.caller_agent_id.clone())
            .collect();
        for agent in agents {
            self.fold_due(&agent)?;
        }
        if matches!(
            self.state,
            TreeAdmissionState::Closed | TreeAdmissionState::TreeTerminal
        ) {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "closed tree cannot resume",
            ));
        }
        self.state = TreeAdmissionState::Live;
        self.hold_parked_reopen_targets();
        self.bump();
        Ok(())
    }

    /// The `on_mint` hook a just-spawned node's copy owes, or
    /// `None` when the template declares none or the node shares its caller's
    /// checkout. `spawn_agent` queues it as an effect once, on a fresh (not
    /// replayed) receipt; the runtime runs it with the lock released.
    #[must_use]
    pub(super) fn mint_hook(
        &self,
        agent_id: &str,
        caller_id: &str,
        caller_path: &Path,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
    ) -> Option<MintHook> {
        let node = self.nodes.get(agent_id)?;
        let workspace = self.workspaces.get(&node.workspace_id)?;
        if workspace.mode == WorkspaceMode::Inherit {
            return None;
        }
        let binding = self.snapshot.binding(node.template.as_ref()?)?;
        let command = binding
            .on_mint
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())?;
        let mut env = binding.env.clone();
        env.insert(
            "PLAK_LAUNCH_CHECKOUT".to_owned(),
            self.launch_checkout.display().to_string(),
        );
        env.insert(
            "PLAK_PARENT_PATH".to_owned(),
            caller_path.display().to_string(),
        );
        env.insert("PLAK_COPY_ID".to_owned(), copy_id(&self.tree_id, agent_id));
        Some(MintHook {
            agent_id: agent_id.to_owned(),
            command: command.to_owned(),
            env,
            cwd: node.path.clone(),
            caller_id: caller_id.to_owned(),
            request: request.clone(),
            operation_id: operation_id.map(str::to_owned),
        })
    }

    /// The spawn's `on_mint` hook failed: copy removal is queued
    /// and the spawn fails closed with `mint_hook_failed`. The node stays as a
    /// `Failed` row so the caller's receipt resolves to something, and the
    /// operation is rewritten to the failure so a retry of the same call
    /// replays the refusal rather than a receipt for a copy that is gone.
    pub fn fail_mint_hook(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
        agent_id: &str,
        message: &str,
    ) -> TopologyError {
        let error = TopologyError::new(
            TopologyErrorCode::MintHookFailed,
            format!("on_mint hook failed for {agent_id}; copy cleanup was queued: {message}"),
        );
        self.fail_spawn_before_session(
            caller_id,
            request,
            operation_id,
            agent_id,
            AttemptFailure {
                kind: "mint_hook_failed".to_owned(),
                message: bounded_text(message, 512),
            },
            error,
        )
    }

    /// The binder proved it created no session and started no turn (for
    /// example the pinned account is not Ready). Nothing ran in the node,
    /// so this is an ordinary `Failed` spawn with `model_not_admitted`, not
    /// a recovery fence; any minted copy is unminted and a retry of the same
    /// operation replays the refusal.
    pub fn fail_spawn_refused(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
        agent_id: &str,
        message: &str,
    ) -> TopologyError {
        let error = TopologyError::new(
            TopologyErrorCode::ModelNotAdmitted,
            format!("child {agent_id} could not start: {message}; no session was created"),
        );
        self.fail_spawn_before_session(
            caller_id,
            request,
            operation_id,
            agent_id,
            AttemptFailure {
                kind: super::recovery::SPAWN_REFUSED.to_owned(),
                message: bounded_text(message, 512),
            },
            error,
        )
    }

    /// Queues removal of the copy a spawn minted when the spawn never
    /// produced a session: nothing ran in it, so there is nothing to seal.
    pub(super) fn unmint_unstarted_copy(&mut self, agent_id: &str) {
        let workspace_id = self
            .nodes
            .get(agent_id)
            .map(|node| node.workspace_id.clone());
        if let Some(workspace_id) = workspace_id
            && let Some(row) = self.workspaces.get_mut(&workspace_id)
            && row.mode != WorkspaceMode::Inherit
        {
            row.status = WorkspaceStatus::Discarding;
            let path = row.path.clone();
            self.unmint_path(&path, None);
        }
    }

    fn fail_spawn_before_session(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
        agent_id: &str,
        failure: AttemptFailure,
        error: TopologyError,
    ) -> TopologyError {
        self.unmint_unstarted_copy(agent_id);
        let attempt_id = self
            .nodes
            .get(agent_id)
            .and_then(|node| node.attempt_id.clone());
        if let Err(recording) = self.record_effect_outcome(
            agent_id,
            attempt_id,
            super::effect_outcomes::Outcome::SpawnRefused {
                ended_at_ms: now_unix_millis(),
                failure,
                diagnostic: error.message.clone(),
            },
        ) {
            return recording;
        }
        if let Some(id) = operation_id {
            let _ = self.remember_failure(id, caller_id, "spawn_agent", request, error.code);
        }
        self.bump();
        error
    }

    /// Gives up one copy and queues its unmint.
    ///
    /// Explicit discard gives up unadopted candidates and drops their pins
    /// after successful copy removal. Automatic cleanup retains such copies.
    ///
    /// # Errors
    ///
    /// Refuses a caller that does not own the row, an unknown row, and live
    /// occupants or dependencies on the copy.
    pub fn discard_workspace(
        &mut self,
        caller_id: &str,
        request: &WorkspaceIdRequest,
        operation_id: Option<&str>,
    ) -> Result<DiscardWorkspaceOutput, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "discard_workspace", request)? {
            return Ok(prior);
        }
        self.require_live_or_terminal_cleanup()?;
        let workspace = self.workspaces.get(&request.workspace_id).ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::WorkspaceNotOwned,
                "workspace id is unknown",
            )
        })?;
        if workspace.mode == WorkspaceMode::Inherit && workspace.owner_agent_id == caller_id {
            return Err(TopologyError::new(
                TopologyErrorCode::DiscardNotApplicable,
                "the only inherit copy cannot be discarded",
            ));
        }
        let grant = self.grant(caller_id)?;
        if workspace.owner_agent_id != caller_id
            && !self.nodes.values().any(|node| {
                node.workspace_id == request.workspace_id
                    && grant.managed_ids.contains(&node.agent_id)
            })
        {
            return Err(TopologyError::new(
                TopologyErrorCode::NotAuthorized,
                "caller does not own that workspace copy",
            ));
        }
        if workspace.status.is_in_flight() {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "workspace removal or adoption is pending; recovery must establish its outcome",
            ));
        }
        let path = workspace.path.clone();
        let mode = workspace.mode;
        let occupants: Vec<_> = self
            .nodes
            .values()
            .filter(|node| node.workspace_id == request.workspace_id)
            .map(|node| node.agent_id.clone())
            .collect();
        for id in occupants {
            let node = self.node(&id)?;
            if node.active_turn
                || self
                    .current_attempt(&id)
                    .is_some_and(super::AttemptRecord::has_unresolved_session_failure)
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "workspace still has an owned or unresolved session",
                ));
            }
            if !node.status.is_finished() {
                return Err(TopologyError::new(
                    TopologyErrorCode::DiscardNotApplicable,
                    "workspace occupants are not finished",
                ));
            }
            if self.current_attempt(&id).is_some_and(|attempt| {
                attempt.failure.as_ref().is_some_and(|failure| {
                    matches!(failure.kind.as_str(), "seal_failed" | "workspace_cleanup")
                })
            }) {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "workspace seal or cleanup remains unresolved; retained work and pins must be recovered",
                ));
            }
            if let Some(attempt) = self.current_attempt(&id)
                && !attempt.status.is_stopped()
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::DiscardNotApplicable,
                    "workspace attempt is not terminal",
                ));
            }
        }
        // Explicit discard gives up the sealed candidates as well as the copy.
        // A copy minted from this workspace's candidate still needs it as its
        // merge base while its own occupant is unfinished.
        let sealed_here: Vec<String> = self
            .candidates
            .values()
            .filter(|candidate| candidate.workspace_id == request.workspace_id)
            .map(|candidate| candidate.commit.clone())
            .collect();
        if !sealed_here.is_empty() {
            for row in self.workspaces.values() {
                if row.workspace_id == request.workspace_id {
                    continue;
                }
                let based_here = row
                    .base_commit
                    .as_ref()
                    .is_some_and(|base| sealed_here.contains(base));
                if !based_here {
                    continue;
                }
                let occupied = self.nodes.values().any(|node| {
                    node.workspace_id == row.workspace_id && !node.status.is_finished()
                });
                if occupied {
                    return Err(TopologyError::new(
                        TopologyErrorCode::WorkspaceReferenced,
                        format!(
                            "workspace {} is still based on a candidate from that copy",
                            row.workspace_id
                        ),
                    ));
                }
            }
        }
        let pending = mode != WorkspaceMode::Inherit;
        if pending {
            if let Some(id) = operation_id {
                self.prepare_operation(id, caller_id, "discard_workspace", request)?;
            }
            self.unmint_path(&path, operation_id);
        }
        let status = if pending {
            WorkspaceStatus::Discarding
        } else {
            WorkspaceStatus::Discarded
        };
        if let Some(row) = self.workspaces.get_mut(&request.workspace_id) {
            row.status = status;
        }
        self.bump();
        let output = DiscardWorkspaceOutput {
            workspace_id: request.workspace_id.clone(),
            status,
            discarded_candidates: self
                .candidates
                .values()
                .filter(|candidate| {
                    candidate.workspace_id == request.workspace_id && candidate.adopted_by.is_none()
                })
                .map(|candidate| candidate.candidate_id.clone())
                .collect(),
        };
        if !pending && let Some(id) = operation_id {
            self.remember_success(id, caller_id, "discard_workspace", request, &output)?;
        }
        Ok(output)
    }

    /// Records why `agent_id` cannot make progress. Paired with
    /// [`Self::record_host_materialization`] so the canvas can say why a child
    /// is `recovery_required` instead of leaving it silently non-terminal.
    pub fn record_fault(&mut self, agent_id: &str, reason: impl Into<String>) {
        self.fault_reasons
            .insert(agent_id.to_owned(), reason.into());
        self.bump();
    }

    /// Diagnostic for the canvas and the caller's tool result, not recovery
    /// authority. Fold intents capture the evidence needed to replay it.
    #[must_use]
    pub fn fault_reason(&self, agent_id: &str) -> Option<&str> {
        self.fault_reasons.get(agent_id).map(String::as_str)
    }

    /// Records every path of a `merge_conflict` adopt refusal for the call
    /// that began the adopt.
    pub(crate) fn record_adopt_conflict(&mut self, candidate_id: &str, paths: Vec<String>) {
        self.adopt_conflicts.insert(candidate_id.to_owned(), paths);
    }

    /// Takes the conflicting paths an adopt of `candidate_id` recorded.
    pub(crate) fn take_adopt_conflict(&mut self, candidate_id: &str) -> Option<Vec<String>> {
        self.adopt_conflicts.remove(candidate_id)
    }

    /// Plants a sealed candidate, so a test can stage an adopt without
    /// driving a real seal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_candidate_for_test(&mut self, candidate: CandidateRecord) {
        self.candidates
            .insert(candidate.candidate_id.clone(), candidate);
    }

    /// Stages the base a copy was minted from, so a test can exercise
    /// divergence and adoption without a real mint.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_workspace_base_for_test(
        &mut self,
        workspace_id: &str,
        base_commit: &str,
        base_content: &str,
        base_change_id: Option<&str>,
    ) {
        let row = self
            .workspaces
            .get_mut(workspace_id)
            .unwrap_or_else(|| unreachable!("test workspace {workspace_id} exists"));
        row.base_commit = Some(base_commit.to_owned());
        row.base_content = Some(base_content.to_owned());
        row.base_change_id = base_change_id.map(str::to_owned);
    }

    /// Leaves a copy mid-adopt by `adopter`, as a process lost during the
    /// landing does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_workspace_adopting_for_test(&mut self, workspace_id: &str, adopter: &str) {
        let row = self
            .workspaces
            .get_mut(workspace_id)
            .unwrap_or_else(|| unreachable!("test workspace {workspace_id} exists"));
        row.status = WorkspaceStatus::Adopting;
        row.adopter_agent_id = Some(adopter.to_owned());
    }

    /// Undoes a mint the spawn did not commit: drops the row and queues the
    /// copy's unmint.
    #[cfg(test)]
    pub(super) fn rollback_minted_workspace(&mut self, workspace_id: &str) {
        let Some(row) = self.workspaces.remove(workspace_id) else {
            return;
        };
        self.unmint_path(&row.path, None);
    }

    /// Queues the copy's unmint: the worktree or workspace
    /// goes when the runtime runs the effects with the mutex released.
    pub(super) fn unmint_path(&mut self, path: &Path, operation_id: Option<&str>) {
        let workspace_id = self
            .workspaces
            .values()
            .find(|row| row.path == path)
            .map(|row| row.workspace_id.clone());
        let candidate_ids = workspace_id
            .as_deref()
            .map(|id| self.candidate_ids_of(id))
            .unwrap_or_default();
        if let Some(id) = &workspace_id {
            self.removals_in_flight.insert(id.clone());
        }
        self.effects.unmints.push(UnmintTicket {
            path: path.to_path_buf(),
            workspace_id,
            operation_id: operation_id.map(str::to_owned),
            prune_of: None,
            candidate_ids,
            namespace: super::digest_text(&self.tree_id),
        });
    }

    /// Queues removal of every forked copy nothing needs any more: all its
    /// occupants are closed and settled, and it backs no unadopted candidate
    /// and no live copy's base. These are the `discard_workspace` checks,
    /// applied by the host after a close, a seal, or an adopt, so closed
    /// children do not leave worktrees behind. An adopted candidate stays
    /// reachable through its pin; the copy itself is disposable.
    ///
    /// A stale `discarding` row (a removal that failed, or that a lost process
    /// left in flight) is retried the same way once `close_agent` has closed
    /// its occupants, which is the operator's settlement of that removal.
    pub(crate) fn prune_closed_copies(&mut self) {
        let prunable: Vec<(String, PathBuf, WorkspaceStatus)> = self
            .workspaces
            .values()
            .filter(|row| {
                row.mode != WorkspaceMode::Inherit
                    && match row.status {
                        WorkspaceStatus::Active | WorkspaceStatus::Adopted => true,
                        WorkspaceStatus::Discarding => {
                            !self.removals_in_flight.contains(&row.workspace_id)
                        }
                        _ => false,
                    }
                    && !self.reclamation_pending(&row.workspace_id)
                    && !self.prune_failed.contains(&row.workspace_id)
                    && self.copy_is_disposable(&row.workspace_id)
            })
            .map(|row| (row.workspace_id.clone(), row.path.clone(), row.status))
            .collect();
        for (workspace_id, path, prior) in prunable {
            if let Some(row) = self.workspaces.get_mut(&workspace_id) {
                row.status = WorkspaceStatus::Discarding;
            }
            self.removals_in_flight.insert(workspace_id.clone());
            let candidate_ids = self.candidate_ids_of(&workspace_id);
            self.effects.unmints.push(UnmintTicket {
                path,
                workspace_id: Some(workspace_id),
                operation_id: None,
                prune_of: Some(prior),
                candidate_ids,
                namespace: super::digest_text(&self.tree_id),
            });
            self.bump();
        }
    }

    fn candidate_ids_of(&self, workspace_id: &str) -> Vec<String> {
        self.candidates
            .values()
            .filter(|candidate| candidate.workspace_id == workspace_id)
            .map(|candidate| candidate.candidate_id.as_str().to_owned())
            .collect()
    }

    fn copy_is_disposable(&self, workspace_id: &str) -> bool {
        // Retrying a stale removal must not be blocked by that removal's own
        // recorded failure.
        let retrying_removal = self
            .workspaces
            .get(workspace_id)
            .is_some_and(|row| row.status == WorkspaceStatus::Discarding);
        let mut occupants = self
            .nodes
            .values()
            .filter(|node| node.workspace_id == workspace_id)
            .peekable();
        if occupants.peek().is_none() {
            return false;
        }
        let settled = occupants.all(|node| {
            node.status == NodeStatus::Closed
                && !self.has_turn_owner(&node.agent_id)
                && !self.has_turn_waiter(&node.agent_id)
                && !self.seal_pending(&node.agent_id)
                && self.current_attempt(&node.agent_id).is_none_or(|attempt| {
                    attempt.status.is_stopped()
                        && !attempt.has_unresolved_session_failure()
                        && attempt.failure.as_ref().is_none_or(|failure| {
                            failure.kind != Self::SEAL_FAILED_KIND
                                && (retrying_removal
                                    || failure.kind != super::WORKSPACE_CLEANUP_KIND)
                        })
                })
        });
        if !settled {
            return false;
        }
        let mut sealed_here = Vec::new();
        for candidate in self
            .candidates
            .values()
            .filter(|candidate| candidate.workspace_id == workspace_id)
        {
            // The copy is where an unadopted candidate is still adopted
            // from; keep it. A `discarding` row's unadopted candidates were
            // given up by the explicit discard that reserved it.
            if candidate.adopted_by.is_none() && !retrying_removal {
                return false;
            }
            sealed_here.push(&candidate.commit);
        }
        !self.workspaces.values().any(|row| {
            row.workspace_id != workspace_id
                && row
                    .base_commit
                    .as_ref()
                    .is_some_and(|base| sealed_here.contains(&base))
                && self.nodes.values().any(|node| {
                    node.workspace_id == row.workspace_id && node.status != NodeStatus::Closed
                })
        })
    }

    pub(super) fn finish_unmint(
        &mut self,
        ticket: UnmintTicket,
        removed: Result<(), VcsError>,
    ) -> Result<(), TopologyError> {
        if let Some(workspace_id) = &ticket.workspace_id {
            self.removals_in_flight.remove(workspace_id);
        }
        let result = removed.map_err(|error| {
            TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                format!(
                    "workspace removal failed; reservation retained for recovery at {}: {}",
                    ticket.path.display(),
                    error.detail
                ),
            )
        });
        if result.is_ok()
            && let Some(workspace_id) = &ticket.workspace_id
        {
            self.settle_workspace_faults(workspace_id);
        }
        if let (Some(prior), Some(workspace_id)) = (ticket.prune_of, &ticket.workspace_id) {
            // An automatic prune that failed leaves the closed copy as it was.
            if let Some(row) = self.workspaces.get_mut(workspace_id) {
                row.status = if result.is_ok() {
                    WorkspaceStatus::Discarded
                } else {
                    prior
                };
            }
            if let Err(error) = &result {
                self.prune_failed.insert(workspace_id.clone());
                self.record_fault(&self.root_agent_id.clone(), error.message.clone());
            }
            self.bump();
            return Ok(());
        }
        if let Some(workspace_id) = &ticket.workspace_id {
            if result.is_ok()
                && let Some(row) = self.workspaces.get_mut(workspace_id)
            {
                row.status = WorkspaceStatus::Discarded;
            }
            if let Err(error) = &result {
                let occupants: Vec<_> = self
                    .nodes
                    .values()
                    .filter(|node| &node.workspace_id == workspace_id)
                    .map(|node| node.agent_id.clone())
                    .collect();
                for id in occupants {
                    let attempt_id = self.node(&id)?.attempt_id.clone();
                    self.record_effect_outcome(
                        &id,
                        attempt_id,
                        super::effect_outcomes::Outcome::WorkspaceCleanup {
                            failure: AttemptFailure {
                                kind: super::WORKSPACE_CLEANUP_KIND.into(),
                                message: bounded_text(&error.message, 512),
                            },
                            diagnostic: error.message.clone(),
                        },
                    )?;
                }
            }
        } else if let Err(error) = &result {
            self.record_fault(&self.root_agent_id.clone(), error.message.clone());
        }
        if let Some(id) = ticket.operation_id
            && let Some(receipt) = self.operations.get_mut(&id)
        {
            match &result {
                Ok(()) => {
                    receipt.terminal_response = Some(
                        serde_json::to_string(&DiscardWorkspaceOutput {
                            workspace_id: ticket.workspace_id.clone().ok_or_else(|| {
                                TopologyError::new(
                                    TopologyErrorCode::OperationRecoveryRequired,
                                    "discard completion has no workspace identity",
                                )
                            })?,
                            status: WorkspaceStatus::Discarded,
                            discarded_candidates: self
                                .candidates
                                .values()
                                .filter(|candidate| {
                                    Some(&candidate.workspace_id) == ticket.workspace_id.as_ref()
                                        && candidate.adopted_by.is_none()
                                })
                                .map(|candidate| candidate.candidate_id.clone())
                                .collect(),
                        })
                        .map_err(|_| {
                            TopologyError::new(
                                TopologyErrorCode::OperationRecoveryRequired,
                                "discard receipt could not be encoded",
                            )
                        })?,
                    );
                    receipt.terminal_code = None;
                }
                Err(error) => {
                    receipt.terminal_response = None;
                    receipt.terminal_code = Some(error.code);
                }
            }
        }
        self.bump();
        result
    }

    /// Plans the child's checkout. Default `inherit` shares the caller's copy
    /// live. `workspace=fork` mints an isolated copy: a git/jj worktree when
    /// the effective tier is `mutate`, otherwise a detached snapshot.
    ///
    /// A sealed `inputs` candidate always mints (there is no live inherit of
    /// another child's copy). Planning runs no subprocess: minting comes back
    /// as a [`MintTicket`] the runtime runs with the mutex released.
    pub(super) fn plan_workspace(
        &self,
        caller_id: &str,
        tier: Tier,
        workspace: SpawnWorkspace,
        reserved_agent_id: &str,
        input_base: Option<&VcsSnapshot>,
    ) -> Result<WorkspacePlan, TopologyError> {
        let caller = self.node(caller_id)?;
        let inherit = workspace.is_inherit() && input_base.is_none();
        if inherit {
            let workspace = self.workspaces.get(&caller.workspace_id).ok_or_else(|| {
                TopologyError::new(
                    TopologyErrorCode::WorkspaceIdentityMismatch,
                    "caller workspace row is missing",
                )
            })?;
            if workspace.status != WorkspaceStatus::Active {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceIdentityMismatch,
                    "caller workspace is not active",
                ));
            }
            return Ok(WorkspacePlan::Inherit(BoundWorkspace {
                workspace_id: workspace.workspace_id.clone(),
                path: workspace.path.clone(),
                branch: caller.branch.clone(),
                mode: WorkspaceMode::Inherit,
                base: None,
            }));
        }
        let mode = if tier == Tier::Mutate {
            WorkspaceMode::Fork
        } else {
            WorkspaceMode::Snapshot
        };
        Ok(WorkspacePlan::Mint(MintTicket {
            launch_checkout: self.launch_checkout.clone(),
            agent_id: reserved_agent_id.to_owned(),
            copy_id: copy_id(&self.tree_id, reserved_agent_id),
            caller_path: caller.path.clone(),
            mode,
            input_base: input_base.cloned(),
        }))
    }

    /// Records what a [`MintTicket`] returned as the reserved copy's own row.
    /// The row does not exist until the mint answers, so a
    /// spawn that never reaches here leaves nothing behind.
    pub(super) fn record_minted(
        &mut self,
        caller_id: &str,
        reserved_agent_id: &str,
        mode: WorkspaceMode,
        minted: MintedCopy,
    ) -> BoundWorkspace {
        let workspace_id = format!("ws-{reserved_agent_id}");
        self.workspaces.insert(
            workspace_id.clone(),
            WorkspaceRecord {
                workspace_id: workspace_id.clone(),
                owner_agent_id: caller_id.to_owned(),
                path: minted.path.clone(),
                mode,
                status: WorkspaceStatus::Active,
                base_commit: Some(minted.base.commit.clone()),
                base_content: Some(minted.base.content.clone()),
                base_change_id: minted.base.change_id.clone(),
                generation: 0,
                adopter_agent_id: None,
            },
        );
        self.live_generations
            .insert(workspace_id.clone(), LiveGeneration::new(0));
        BoundWorkspace {
            workspace_id,
            path: minted.path,
            branch: minted.branch,
            mode,
            base: Some(minted.base.commit),
        }
    }

    pub(super) fn bump_workspace_generation(&mut self, workspace_id: &str) {
        let Some(row) = self.workspaces.get_mut(workspace_id) else {
            return;
        };
        row.generation = row.generation.saturating_add(1);
    }

    pub(super) fn publish_workspace_generations(&mut self) {
        for (id, row) in &self.workspaces {
            if let Some(live) = self.live_generations.get(id) {
                live.0.store(row.generation, Ordering::SeqCst);
            } else {
                self.live_generations
                    .insert(id.clone(), LiveGeneration::new(row.generation));
            }
        }
    }

    /// Shared observational counter for Read/Grep stamps. Prefer the longest
    /// matching workspace path so minted copies do not inherit the launch root.
    pub fn generation_counter_for_root(&self, root: &Path) -> Option<Arc<AtomicU64>> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut best: Option<(&str, usize)> = None;
        for row in self.workspaces.values() {
            let path = std::fs::canonicalize(&row.path).unwrap_or_else(|_| row.path.clone());
            if root == path || root.starts_with(&path) {
                let depth = path.components().count();
                if best.is_none_or(|(_, current)| depth >= current) {
                    best = Some((row.workspace_id.as_str(), depth));
                }
            }
        }
        best.and_then(|(id, _)| {
            self.live_generations
                .get(id)
                .map(|live| Arc::clone(&live.0))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {

    use crate::protocol::{SpawnAgentRequest, SpawnWorkspace, Tier, WorkspaceMode, WriterMode};
    use crate::tree::test_support::*;
    use crate::tree::vcs_fixtures::*;
    use crate::tree::*;

    #[test]
    fn restored_pause_is_presented_without_rewriting_folded_node_statuses() {
        let (_tmp, mut tree) = tree();
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let statuses: BTreeMap<_, _> = tree
            .nodes
            .iter()
            .map(|(id, node)| (id.clone(), node.status))
            .collect();
        tree.restore_paused();
        assert_eq!(tree.state, TreeAdmissionState::RestoredPaused);
        for (id, status) in &statuses {
            assert_eq!(tree.node(id).unwrap().status, *status);
        }
        let view = view::project_tree_view(&tree);
        assert_eq!(
            view.nodes
                .iter()
                .find(|node| node.agent_id == child)
                .unwrap()
                .status,
            NodeStatus::Paused
        );
        tree.resume_tree().unwrap();
        for (id, status) in statuses {
            assert_eq!(tree.node(&id).unwrap().status, status);
        }
    }

    #[test]
    fn mutate_defaults_to_inheriting_the_caller_checkout() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let request = SpawnAgentRequest::test_named(
            "implement",
            Tier::Mutate,
            "implementer",
            "edit the parser",
        );
        let spawned = tree
            .spawn_agent("root", &request, Some("op-inherit-mutate"))
            .unwrap();
        assert_eq!(spawned.tier, Tier::Mutate);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
        assert_eq!(
            spawned.workspace.id,
            tree.node("root").unwrap().workspace_id
        );
    }

    #[test]
    fn execute_defaults_to_inheriting_the_caller_checkout() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let request =
            SpawnAgentRequest::test_named("implement", Tier::Execute, "runner", "run the checks");
        let spawned = tree
            .spawn_agent("root", &request, Some("op-inherit-execute"))
            .unwrap();
        assert_eq!(spawned.tier, Tier::Execute);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
        assert_eq!(
            spawned.workspace.id,
            tree.node("root").unwrap().workspace_id
        );
    }

    /// `workspace=fork` on `execute` mints a disposable snapshot copy —
    /// detached, no branch, and read-only as a source. Omit workspace to inherit.
    #[test]
    fn execute_tier_mints_a_detached_snapshot_copy() {
        let repo = git_repo();
        std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let mut request = mint_req();
        request.tier = Tier::Execute;
        request.workspace = SpawnWorkspace::Fork;
        let spawned = tree.spawn_agent("root", &request, Some("op-exec")).unwrap();
        assert_eq!(spawned.tier, Tier::Execute);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Snapshot);
        let path = tree.node(&spawned.agent_id).unwrap().path.clone();
        assert!(path.join("DIRTY.txt").is_file(), "the base is the snapshot");
        // A snapshot copy is thrown away unless it seals, so it takes no branch.
        assert_eq!(tree.node(&spawned.agent_id).unwrap().branch, None);
        let workspace = tree.workspace(&spawned.workspace.id).unwrap();
        assert_eq!(workspace.mode, WorkspaceMode::Snapshot);
        assert_eq!(workspace.base_commit.as_deref().map(str::len), Some(40));
        assert!(
            !tree
                .launch_checkout
                .join(".plak/worktrees")
                .join(&spawned.agent_id)
                .join(".git")
                .is_dir()
        );
    }

    /// A launch checkout with no git or jj identity cannot mint at all.
    #[test]
    fn mint_without_a_vcs_identity_is_typed_mint_unavailable() {
        let (_tmp, mut tree) = tree();
        let error = tree
            .spawn_agent("root", &mint_req(), Some("op-mint"))
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
        assert_eq!(
            tree.operations().get("op-mint").unwrap().terminal_code,
            Some(TopologyErrorCode::MintUnavailable)
        );
        assert_eq!(tree.nodes().len(), 1, "no node is left behind");
        assert!(
            tree.epoch_allows_rebind(),
            "the pending counter is released"
        );
    }

    #[test]
    fn resume_after_pause_does_not_restart_a_bound_turn() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.bind_session_locator(&spawned.agent_id, "session:c1")
            .unwrap();
        tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.restore_paused();
        assert!(tree.node(&spawned.agent_id).unwrap().active_turn);
        assert_eq!(
            tree.inflight_join_targets(),
            vec![(
                spawned.agent_id.clone(),
                "session:c1".into(),
                tree.node(&spawned.agent_id)
                    .unwrap()
                    .path
                    .display()
                    .to_string(),
                "turn-1".into(),
            )]
        );
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
        tree.resume_tree().unwrap();
        let mut deferred = mail(&spawned.agent_id, "again");
        deferred.priority = Some(MessagePriority::Deferred);
        let queued = tree.send_agent_message("root", &deferred, None).unwrap();
        assert_eq!(queued.disposition, MailDisposition::Queued);
        let mut urgent = mail(&spawned.agent_id, "now");
        urgent.priority = Some(MessagePriority::Urgent);
        let steered = tree.send_agent_message("root", &urgent, None).unwrap();
        assert_eq!(steered.disposition, MailDisposition::Steered);
    }

    #[test]
    fn git_mint_forks_from_the_parent_state() {
        let repo = git_repo();
        std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        assert_eq!(spawned.tier, Tier::Mutate);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Fork);
        let node = tree.node(&spawned.agent_id).unwrap();
        let path = node.path.clone();
        assert!(path.to_string_lossy().contains(".plak/worktrees/"));
        assert!(
            path.join(".plak/bin/git").is_file(),
            "minted copy must install the git-redirect shim"
        );
        assert!(
            path.join("DIRTY.txt").is_file(),
            "parent dirty state is copied"
        );
        assert!(path.join("SEED.txt").is_file());
        assert_ne!(path, repo.path());
        let porcelain = Command::new("git")
            .args(["status", "--porcelain", "-uall"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(porcelain.status.success());
        let status = String::from_utf8_lossy(&porcelain.stdout);
        assert!(
            !status.contains(".plak"),
            "mint copies must be gitignored, got:\n{status}"
        );
        let workspace = tree.workspace(&spawned.workspace.id).unwrap();
        assert_eq!(workspace.mode, WorkspaceMode::Fork);
        assert_eq!(workspace.status, WorkspaceStatus::Active);
        let base = workspace
            .base_commit
            .clone()
            .unwrap_or_else(|| unreachable!("the mint records its snapshot base"));
        assert_eq!(base.len(), 40, "{base}");
        assert_eq!(spawned.workspace.base.as_deref(), Some(base.as_str()));
        // The child's HEAD is the base snapshot, so the copy is clean: the
        // parent's dirty file is *in* the base, not copied on top of it.
        let dirty = Command::new("git")
            .args(["status", "--porcelain", "-uall"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(dirty.status.success());
        assert!(
            dirty.stdout.is_empty(),
            "the fork starts clean against its snapshot base: {}",
            String::from_utf8_lossy(&dirty.stdout)
        );
    }

    #[test]
    fn jj_mint_forks_under_plak_workspaces() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&spawned.agent_id).unwrap().path.clone();
        assert!(path.to_string_lossy().contains(".plak/workspaces/"));
        assert!(
            path.join(".plak/bin/jj").is_file(),
            "minted jj copy must install the jj-op shim"
        );
        assert!(
            path.join(".plak/bin/git").is_file(),
            "minted jj copy must also install git-redirect so colocated git cannot escape"
        );
    }

    #[test]
    fn read_only_binds_caller_copy_without_mint() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let root = tree.node("root").unwrap();
        assert_eq!(spawned.workspace.id, root.workspace_id);
        assert_eq!(tree.node(&spawned.agent_id).unwrap().path, root.path);
        assert!(!tree.launch_checkout.join(".plak/worktrees").exists());
        assert!(!tree.launch_checkout.join(".plak/bin/git").is_file());
    }

    #[test]
    fn mutate_completion_bumps_its_fork_generation() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let counter = tree
            .generation_counter_for_root(
                tree.workspace(&spawned.workspace.id)
                    .unwrap()
                    .path
                    .as_path(),
            )
            .expect("fork stamp");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(tree.workspace(&spawned.workspace.id).unwrap().generation, 1);
        // The root's own checkout is untouched by a child's fork completing.
        assert_eq!(
            tree.workspace(&tree.node("root").unwrap().workspace_id)
                .unwrap()
                .generation,
            0
        );
    }

    #[test]
    fn read_only_complete_does_not_bump_generation() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.workspace(&spawned.workspace.id).unwrap().generation, 0);
    }

    #[test]
    fn discard_requires_finished_occupants() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        assert_eq!(
            tree.discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: spawned.workspace.id.clone(),
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::DiscardNotApplicable
        );
        tree.settle_for_test();
        tree.close_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        let discarded = tree
            .discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: spawned.workspace.id,
                },
                None,
            )
            .unwrap();
        assert_eq!(discarded.status, WorkspaceStatus::Discarding);
    }

    /// Explicit discard relinquishes unadopted content as well as its copy.
    #[test]
    fn explicit_discard_gives_up_unadopted_candidates() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&spawned.agent_id)
                .unwrap()
                .path
                .join("retained.txt"),
            "candidate work",
        )
        .unwrap();
        tree.close_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        let pinning = candidate_of(&tree, &spawned.agent_id);
        let request = WorkspaceIdRequest {
            workspace_id: spawned.workspace.id.clone(),
        };
        let output = tree
            .discard_workspace("root", &request, Some("discard-candidates"))
            .unwrap();
        assert_eq!(output.status, WorkspaceStatus::Discarding);
        assert_eq!(
            serde_json::to_value(&output).unwrap()["discarded_candidates"],
            serde_json::json!([pinning])
        );
        tree.settle_for_test();
        let replay = tree
            .discard_workspace("root", &request, Some("discard-candidates"))
            .unwrap();
        assert_eq!(replay.status, WorkspaceStatus::Discarded);
        assert_eq!(replay.discarded_candidates, output.discarded_candidates);
        assert_eq!(
            tree.workspace(&spawned.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
        assert_eq!(
            tree.adopt_candidate("root", &adopt_req(&pinning), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::AdoptNotApplicable
        );
    }

    /// A `Sealing` node or an `Adopting` row reopens `RecoveryRequired`
    /// and never auto-retries.
    #[test]
    fn sealing_and_adopting_reopen_recovery_required() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let sealing = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let adopting = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&sealing.agent_id).unwrap();
        let _stranded = tree.take_effects();
        tree.workspaces
            .get_mut(&adopting.workspace.id)
            .unwrap()
            .status = WorkspaceStatus::Adopting;
        tree.restore_paused();
        assert_eq!(
            tree.node(&sealing.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.node(&adopting.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.node("root").unwrap().status,
            NodeStatus::Running,
            "the adopt's owner is the root, which nothing could recover"
        );
        assert!(
            tree.fault_reason("root")
                .is_some_and(|reason| reason.contains("adopt into this checkout"))
        );
        tree.resume_tree().unwrap();
        assert_eq!(
            tree.node(&sealing.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// An adopt lands in its owner's checkout however its producer ended, so
    /// a reopen faults a non-root owner even when every occupant of the
    /// adopting copy is closed; the closed producer itself stays closed.
    #[test]
    fn an_interrupted_adopt_faults_its_owner_even_after_the_producer_closed() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let hub = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&hub.agent_id).unwrap();
        tree.record_turn_completed(&producer.agent_id).unwrap();
        tree.settle_for_test();
        tree.close_agent("root", &close_req(&producer.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        let row = tree.workspaces.get_mut(&producer.workspace.id).unwrap();
        row.status = WorkspaceStatus::Adopting;
        row.owner_agent_id.clone_from(&hub.agent_id);
        tree.restore_paused();
        assert_eq!(
            tree.node(&hub.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.node(&producer.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);

        // Closing the owner after inspecting its checkout settles the adopt:
        // later reopens do not fault it again.
        tree.close_agent("root", &close_req(&hub.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&hub.agent_id).unwrap().status, NodeStatus::Closed);
        tree.workspaces
            .get_mut(&producer.workspace.id)
            .unwrap()
            .status = WorkspaceStatus::Adopting;
        tree.restore_paused();
        tree.resume_tree().unwrap();
        assert_eq!(tree.node(&hub.agent_id).unwrap().status, NodeStatus::Closed);
    }

    /// A copy minted from a candidate keeps that candidate's workspace
    /// alive while its own occupant is unfinished.
    #[test]
    fn discard_refuses_while_a_snapshot_reviewer_is_bound() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let implementer = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let implementer_path = tree.node(&implementer.agent_id).unwrap().path.clone();
        std::fs::write(implementer_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&implementer.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &implementer.agent_id);
        let commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();

        // A reviewer's snapshot copy is based on that candidate.
        let mut request = mint_req();
        request.tier = Tier::Execute;
        request.workspace = SpawnWorkspace::Fork;
        let reviewer = tree.spawn_agent("root", &request, None).unwrap();
        tree.set_workspace_base_for_test(&reviewer.workspace.id, &commit, "tree", None);
        let discard = WorkspaceIdRequest {
            workspace_id: implementer.workspace.id.clone(),
        };
        let error = tree.discard_workspace("root", &discard, None).unwrap_err();
        tree.settle_for_test();
        assert_eq!(error.code, TopologyErrorCode::WorkspaceReferenced);
        assert!(error.message.contains(&reviewer.workspace.id), "{error:?}");
        tree.close_agent("root", &close_req(&reviewer.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.discard_workspace("root", &discard, None)
                .unwrap()
                .status,
            WorkspaceStatus::Discarding
        );
        tree.settle_for_test();
    }

    /// A parent rewrite must never run destructive stale recovery over
    /// uncommitted child files. Refuse and retain both tracked and new work.
    #[test]
    fn jj_uncommitted_work_is_preserved_by_typed_stale_recovery_refusal() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let first_path = tree.node(&first.agent_id).unwrap().path.clone();
        std::fs::write(first_path.join("FIRST.txt"), "first\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let input = candidate_of(&tree, &first.agent_id);

        let mut over = mint_req();
        over.inputs = Some(vec![input.to_string()]);
        let second = tree.spawn_agent("root", &over, None).unwrap();
        let second_path = tree.node(&second.agent_id).unwrap().path.clone();
        // Something rewrites the copy's base out from under it — an adopt of
        // its input is now refused, but a human `jj abandon` in the launch
        // checkout is not — and the child then writes into a copy jj considers
        // stale.
        let change = tree
            .candidates()
            .get(&input)
            .unwrap()
            .change_id
            .clone()
            .unwrap();
        jj_in(repo.path(), &["abandon", change.as_str()]);
        std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();

        assert_eq!(
            std::fs::read_to_string(second_path.join("SECOND.txt")).unwrap(),
            "second\n"
        );
        let attempt = tree.attempt(&second.agent_id).unwrap();
        assert!(attempt.candidate_id.is_none());
        let failure = attempt.failure.as_ref().unwrap();
        assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
        assert!(
            failure.message.contains("recovery required"),
            "{}",
            failure.message
        );
        assert_eq!(
            tree.node(&second.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// B9: an idle finished child whose copy was discarded cannot be
    /// re-tasked into a path that no longer exists.
    #[test]
    fn followup_is_refused_into_a_discarded_workspace() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        // No writes: nothing seals, so the copy is discardable.
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap();
        tree.settle_for_test();
        let mut followup = mail(&child.agent_id, "one more thing");
        followup.purpose = Some(MessagePurpose::Followup);
        let error = tree
            .send_agent_message("root", &followup, None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::FollowupWorkspaceGone);
        assert!(error.message.contains("discarded"), "{}", error.message);
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().attempt_id,
            child.attempt_id,
            "no second attempt was opened"
        );
    }

    /// The tree drives minting and unminting through its `VcsOps` port and
    /// nothing else: on `FakeVcs` a mutate spawn on a plain temp dir mints a
    /// recorded copy with the fake's base, and a discard unmints it.
    #[test]
    fn fake_vcs_mints_and_unmints_without_a_repo() {
        let (root, vcs, mut tree) = fake_tree();
        assert_eq!(
            tree.node("root").unwrap().branch.as_deref(),
            Some("fake-main"),
            "the root label came from the port"
        );
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let node = tree.node(&child.agent_id).unwrap().clone();
        assert!(
            node.path
                .starts_with(root.path().join(".plak").join("fake"))
        );
        assert!(node.path.is_dir());
        assert_eq!(
            node.branch.as_deref(),
            Some(format!("plak/{}", child.agent_id).as_str())
        );
        let row = tree.workspace(&node.workspace_id).unwrap();
        assert_eq!(
            row.base_commit.as_deref(),
            Some(FakeVcs::base_for(&child.agent_id).commit.as_str())
        );
        assert_eq!(vcs.calls_of(FakeOp::Mint).len(), 1);
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        tree.adopt_candidate(
            "root",
            &adopt_req(&candidate_of(&tree, &child.agent_id)),
            None,
        )
        .unwrap();
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: node.workspace_id.clone(),
            },
            None,
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        assert!(!node.path.exists(), "the fake unmint removed the copy");
    }

    #[test]
    fn failed_removal_keeps_copy_and_prepared_discard_cannot_replay_success() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
        // Finished, not closed: a closed copy would already be pruned.
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let request = WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        };
        let pending = tree
            .discard_workspace("root", &request, Some("discard-fails"))
            .unwrap();
        assert_eq!(pending.status, WorkspaceStatus::Discarding);
        assert!(
            tree.operations()["discard-fails"]
                .terminal_response
                .is_none()
        );
        assert!(path.exists());
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
        let settled = tree.settle_for_test();
        assert_eq!(settled.cleanup_failures.len(), 1);
        assert!(path.exists());
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarding
        );
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.operations()["discard-fails"].terminal_code,
            Some(TopologyErrorCode::WorkspaceRecoveryRequired)
        );
        assert_eq!(
            tree.discard_workspace("root", &request, Some("discard-fails"))
                .unwrap_err()
                .code,
            TopologyErrorCode::WorkspaceRecoveryRequired
        );
        assert!(tree.take_effects().is_empty());
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
    }

    /// A finished copy with no candidate whose explicit discard fails: the row
    /// stays `discarding` and its occupant `recovery_required`.
    fn failed_discard_of_a_finished_copy(
        tree: &mut ProductAgentTree,
        vcs: &FakeVcs,
    ) -> (String, String, PathBuf) {
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap();
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
        assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        (child.agent_id, child.workspace.id, path)
    }

    /// Closing the occupants of a failed removal is the operator's settlement
    /// of it: the prune retries the removal once, and the copy ends
    /// `discarded`. A retry that fails again keeps the row reserved, faults
    /// nobody, and is not repeated in this process.
    #[test]
    fn closing_a_failed_removal_retries_it_as_a_prune_once() {
        let (_root, vcs, mut tree) = fake_tree();
        let (child, workspace, path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
        tree.close_agent("root", &close_req(&child), None).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        assert_eq!(
            tree.workspace(&workspace).unwrap().status,
            WorkspaceStatus::Discarded
        );
        assert!(!path.exists());
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 2);

        let (child, workspace, path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
        tree.close_agent("root", &close_req(&child), None).unwrap();
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("still refused")));
        tree.settle_for_test();
        assert_eq!(
            tree.workspace(&workspace).unwrap().status,
            WorkspaceStatus::Discarding
        );
        assert!(path.exists());
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);
        let removals = vcs.calls_of(FakeOp::Unmint).len();
        tree.close_agent("root", &close_req(&child), None).unwrap();
        tree.settle_for_test();
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), removals);
    }

    /// An explicit discard gives up the copy's unadopted candidates, so a
    /// failed one is still retried once its occupant closes; the candidate
    /// stays unadoptable.
    #[test]
    fn a_failed_discard_of_a_copy_with_a_candidate_is_retried_on_close() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate = candidate_of(&tree, &child.agent_id);
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap();
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
        assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
        assert_eq!(
            tree.adopt_candidate("root", &adopt_req(&candidate), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::AdoptNotApplicable
        );
    }

    /// The reopen law for a stale `discarding` row: its open occupant is
    /// faulted and the owner is not; once the occupant is closed nobody is,
    /// and a root an earlier build faulted for it reopens running, so its
    /// waits are not refused. Nothing is retried until a prune runs.
    #[test]
    fn a_stale_removal_never_faults_the_root_or_a_closed_occupant_on_reopen() {
        let (_root, vcs, mut tree) = fake_tree();
        let (child, workspace, _path) = failed_discard_of_a_finished_copy(&mut tree, &vcs);
        tree.restore_paused();
        assert_eq!(
            tree.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(tree.node("root").unwrap().status, NodeStatus::Running);

        tree.close_agent("root", &close_req(&child), None).unwrap();
        let _not_run = tree.take_effects();
        tree.nodes.get_mut("root").unwrap().status = NodeStatus::RecoveryRequired;
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(reopened.node("root").unwrap().status, NodeStatus::Running);
        assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
        assert_eq!(
            reopened.workspace(&workspace).unwrap().status,
            WorkspaceStatus::Discarding
        );
        assert!(reopened.take_effects().is_empty());
        reopened.resume_tree().unwrap();
        assert_eq!(reopened.node("root").unwrap().status, NodeStatus::Running);
        assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
        assert!(reopened.poll_wait("root", &wait_for(&child)).is_ok());

        reopened
            .close_agent("root", &close_req(&child), None)
            .unwrap();
        reopened.prune_closed_copies();
        assert_eq!(reopened.take_effects().unmints.len(), 1);
    }

    #[test]
    fn failed_hook_cleanup_retains_recovery_copy() {
        let (_root, vcs, mut tree) = fake_tree();
        let request = mint_req();
        let child = tree.spawn_agent("root", &request, None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        tree.fail_mint_hook(
            "root",
            &request,
            Some("hook-fails"),
            &child.agent_id,
            "injected hook failure",
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarding
        );
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected cleanup failure")));
        assert_eq!(tree.settle_for_test().cleanup_failures.len(), 1);
        assert!(path.exists());
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            tree.fault_reason(&child.agent_id)
                .unwrap()
                .contains("removal failed")
        );
    }

    /// On the port: the hook the tree hands out runs through
    /// `VcsOps::run_mint_hook`, and its `HookFailed` detail is what
    /// `fail_mint_hook` records while the fake unmints the copy.
    #[test]
    fn fake_vcs_mint_hook_failure_unmints_through_the_port() {
        let (_root, vcs, mut tree) = fake_tree();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("true".into());
        tree.pin_template("implement", binding);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        let caller_path = tree.node("root").unwrap().path.clone();
        let hook = tree
            .mint_hook(
                &child.agent_id,
                "root",
                &caller_path,
                &mint_req(),
                Some("op-1"),
            )
            .unwrap();
        vcs.hook_next_with(Err(VcsError::hook_failed("exited 3")));
        let failed =
            VcsOps::run_mint_hook(vcs.as_ref(), &hook.cwd, &hook.command, &hook.env).unwrap_err();
        assert_eq!(failed.kind, VcsErrorKind::HookFailed);
        assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
        let error = tree.fail_mint_hook(
            "root",
            &mint_req(),
            Some("op-1"),
            &child.agent_id,
            &failed.detail,
        );
        tree.settle_for_test();
        assert_eq!(error.code, TopologyErrorCode::MintHookFailed);
        assert!(error.message.contains("exited 3"));
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        assert!(!child_path.exists(), "the fake unmint removed the copy");
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Failed
        );
    }

    #[test]
    fn mint_hook_keeps_the_planned_parent_path_if_the_caller_row_is_missing() {
        let (_root, vcs, mut tree) = fake_tree();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("true".into());
        tree.pin_template("implement", binding);
        let parent_path = tree.node("root").unwrap().path.clone();
        let SpawnAdmission::Reserved(reservation, Some(ticket)) =
            tree.spawn_reserve("root", &mint_req(), None).unwrap()
        else {
            panic!("a fork spawn must reserve a mint");
        };
        let minted = ticket.mint(vcs.as_ref()).unwrap();
        tree.nodes.remove("root");
        let child = tree.spawn_commit(*reservation, Some(minted)).unwrap();
        let hook = tree
            .effects
            .hooks
            .pop()
            .expect("on_mint must not be skipped");
        assert_eq!(hook.agent_id, child.agent_id);
        assert_eq!(
            hook.env.get("PLAK_PARENT_PATH"),
            Some(&parent_path.display().to_string())
        );
    }

    #[test]
    fn mint_hook_receives_authoritative_copy_context_for_nested_spawns() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.spawns = vec![TemplateName::parse("implement").unwrap()];
        binding.on_mint = Some(
            "printf '%s\\n' \"$PLAK_LAUNCH_CHECKOUT\" \"$PLAK_PARENT_PATH\" \"$PLAK_COPY_ID\" \"$PLAK_TEST_MARK\" > context.txt".into(),
        );
        for name in ["PLAK_LAUNCH_CHECKOUT", "PLAK_PARENT_PATH", "PLAK_COPY_ID"] {
            binding.env.insert(name.into(), "template-override".into());
        }
        binding.env.insert("PLAK_TEST_MARK".into(), "minted".into());
        tree.pin_template("supervisor", binding.clone());
        tree.pin_template("implement", binding);

        let mut caller_id = "root".to_owned();
        for template in ["supervisor", "implement"] {
            let mut request = mint_req();
            request.template = TemplateName::parse(template).unwrap();
            let parent_path = tree.node(&caller_id).unwrap().path.clone();
            let child = tree.spawn_agent(&caller_id, &request, None).unwrap();
            let hook = tree
                .mint_hook(&child.agent_id, &caller_id, &parent_path, &request, None)
                .unwrap();
            let expected = [
                tree.launch_checkout.display().to_string(),
                parent_path.display().to_string(),
                copy_id(&tree.tree_id, &child.agent_id),
                "minted".into(),
            ];
            for (name, value) in ["PLAK_LAUNCH_CHECKOUT", "PLAK_PARENT_PATH", "PLAK_COPY_ID"]
                .into_iter()
                .zip(&expected)
            {
                assert_eq!(hook.env.get(name), Some(value), "{name}");
            }
            plakous_workspace::occupancy_vcs::run_mint_hook(&hook.cwd, &hook.command, &hook.env)
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(hook.cwd.join("context.txt")).unwrap(),
                format!("{}\n", expected.join("\n"))
            );
            caller_id = child.agent_id;
        }
    }

    /// the template's `on_mint` is read for a fresh copy, runs
    /// in it with the template env, and a failure unmints and fails closed.
    #[test]
    fn mint_hook_runs_in_the_copy_and_failure_unmints() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("printf %s \"$PLAK_TEST_MARK\" > MARK.txt".into());
        binding.env.insert("PLAK_TEST_MARK".into(), "minted".into());
        tree.pin_template("implement", binding);
        assert!(
            tree.mint_hook("root", "root", repo.path(), &mint_req(), None)
                .is_none(),
            "the root shares no minted copy"
        );
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        let hook = tree
            .mint_hook(
                &child.agent_id,
                "root",
                repo.path(),
                &mint_req(),
                Some("op-1"),
            )
            .expect("the template declares a hook");
        assert_eq!(hook.cwd, child_path);
        assert_eq!(
            hook.env.get("PLAK_TEST_MARK").map(String::as_str),
            Some("minted")
        );
        plakous_workspace::occupancy_vcs::run_mint_hook(&hook.cwd, &hook.command, &hook.env)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(child_path.join("MARK.txt")).unwrap(),
            "minted"
        );
        // A read_only child has no copy and therefore no hook.
        let reader = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(
            tree.mint_hook(&reader.agent_id, "root", repo.path(), &spawn_req(), None)
                .is_none()
        );

        // A failing hook: the copy is gone and the spawn is a typed failure.
        let failing = plakous_workspace::occupancy_vcs::run_mint_hook(
            &child_path,
            "echo no >&2; exit 3",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(failing.contains("exited 3"), "{failing}");
        assert!(failing.contains("no"), "{failing}");
        let error =
            tree.fail_mint_hook("root", &mint_req(), Some("op-1"), &child.agent_id, &failing);
        tree.settle_for_test();
        assert_eq!(error.code, TopologyErrorCode::MintHookFailed);
        assert!(!child_path.exists(), "the copy was unminted");
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Failed
        );
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.kind.as_str()),
            Some("mint_hook_failed")
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
        assert_eq!(
            tree.operations()["op-1"].terminal_code,
            Some(TopologyErrorCode::MintHookFailed),
            "a retry replays the refusal, not a receipt for a copy that is gone"
        );
    }
}
