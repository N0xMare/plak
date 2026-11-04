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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_prior_status: Option<NodeStatus>,
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

#[derive(Clone, Debug)]
pub(super) enum BasePinRecovery {
    Pending(Arc<()>),
    Failed(String),
    Complete,
}

impl super::BasePinTicket {
    /// Restores a present copy's base pin; `Ok(false)` when the copy is
    /// absent and has none. A port that cannot say whether the copy exists
    /// fails the reconciliation rather than skip it.
    pub(super) async fn run(
        &self,
        vcs: &dyn plakous_workspace::occupancy_vcs::VcsOps,
    ) -> Result<bool, VcsError> {
        if !vcs.path_exists(&self.path).await? {
            return Ok(false);
        }
        vcs.restore_copy_base(&self.path, &self.namespace, &self.agent_id, &self.base)
            .await
            .map(|()| true)
    }
}

impl ProductAgentTree {
    fn recovery_base_pins(&self) -> Vec<super::BasePinTicket> {
        self.workspaces
            .values()
            .filter(|row| {
                row.status == WorkspaceStatus::Adopting
                    && row.mode != WorkspaceMode::Inherit
                    && !self.reclaimed(&row.workspace_id)
            })
            .filter(|row| {
                self.operations.values().any(|op| {
                    op.tool_name == VCS_FAULT_KIND
                        && !self.vcs_fault_is_settled(&op.caller_agent_id, &op.operation_id, true)
                        && serde_json::from_str::<VcsFault>(&op.canonical_request).is_ok_and(
                            |fault| fault.workspace_id.as_deref() == Some(&row.workspace_id),
                        )
                })
            })
            .filter_map(|row| {
                Some(super::BasePinTicket {
                    ownership: Arc::new(()),
                    workspace_id: row.workspace_id.clone(),
                    path: row.path.clone(),
                    agent_id: self.workspace_producer(&row.workspace_id).ok()?,
                    namespace: super::digest_text(&self.tree_id),
                    base: row.base_commit.clone()?,
                })
            })
            .collect()
    }

    fn queue_recovery_base_pins(&mut self) {
        for ticket in self.recovery_base_pins() {
            if self.base_pin_recovery.contains_key(&ticket.workspace_id) {
                continue;
            }
            self.base_pin_recovery.insert(
                ticket.workspace_id.clone(),
                BasePinRecovery::Pending(ticket.ownership.clone()),
            );
            self.effects.base_pins.push(ticket);
        }
    }

    pub(super) fn check_base_pin_recovery(&self, workspace: &str) -> Result<(), TopologyError> {
        let detail = match self.base_pin_recovery.get(workspace) {
            Some(BasePinRecovery::Pending(_)) => "base-pin reconciliation is pending".to_owned(),
            Some(BasePinRecovery::Failed(reason)) => {
                format!("base-pin reconciliation failed: {reason}; reopen to retry")
            }
            _ => return Ok(()),
        };
        Err(TopologyError::new(
            TopologyErrorCode::WorkspaceRecoveryRequired,
            format!("workspace {workspace} {detail}; nothing was adopted"),
        ))
    }

    pub(super) fn finish_base_pin_recovery(
        &mut self,
        ticket: super::BasePinTicket,
        result: Result<bool, VcsError>,
    ) {
        if !matches!(self.base_pin_recovery.get(&ticket.workspace_id),
            Some(BasePinRecovery::Pending(owner)) if Arc::ptr_eq(owner, &ticket.ownership))
        {
            return;
        }
        match result {
            // An absent copy is skipped, as if it had never been queued.
            Ok(false) => {
                self.base_pin_recovery.remove(&ticket.workspace_id);
            }
            Ok(true) => {
                self.base_pin_recovery
                    .insert(ticket.workspace_id, BasePinRecovery::Complete);
            }
            Err(error) => {
                let reason = format!(
                    "workspace {} base pin recovery failed: {}",
                    ticket.workspace_id, error.detail
                );
                self.base_pin_recovery.insert(
                    ticket.workspace_id.clone(),
                    BasePinRecovery::Failed(error.detail),
                );
                self.fault_reasons
                    .insert(ticket.agent_id.clone(), reason.clone());
                let fault = VcsFault {
                    refresh_prior_status: None,
                    source_id: format!(
                        "base-pin-recovery:{}:{}",
                        ticket.workspace_id, self.host_generation
                    ),
                    workspace_id: Some(ticket.workspace_id),
                    attempt_id: self
                        .nodes
                        .get(&ticket.agent_id)
                        .and_then(|node| node.attempt_id.clone()),
                    reason,
                };
                self.record_vcs_fault(
                    &ticket.agent_id,
                    fault,
                    self.vcs_fault_anchor(&ticket.agent_id),
                );
                if let Err(error) = self.fold_due(&ticket.agent_id) {
                    tracing::error!(agent_id = %ticket.agent_id, refusal = %error.message, "base pin recovery fault fold refused");
                }
            }
        }
    }

    pub(super) fn workspace_has_vcs_fault(&self, workspace: &str) -> bool {
        self.operations.values().any(|op| {
            op.tool_name == VCS_FAULT_KIND
                && !self.vcs_fault_is_settled(&op.caller_agent_id, &op.operation_id, true)
                && serde_json::from_str::<VcsFault>(&op.canonical_request)
                    .is_ok_and(|fault| fault.workspace_id.as_deref() == Some(workspace))
        })
    }

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
        self.queue_recovery_base_pins();
        self.queue_refresh_recovery();
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
                    refresh_prior_status: None,
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
                        refresh_prior_status: None,
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

    /// Faults `agent_id` for stage `stage_id`, whose effect may have half
    /// run on its runner.
    pub(super) fn fault_vcs_stage(
        &mut self,
        agent_id: &str,
        stage_id: &str,
        attempt_id: Option<super::AttemptId>,
    ) {
        if !self.nodes.contains_key(agent_id) {
            return;
        }
        // The root is recorded like a root adopt in flight at reopen: its
        // checkout is named, since nothing recovers the root itself.
        let reason = if agent_id == self.root_agent_id {
            "a VCS step into this checkout on its runner may have half run; inspect it before relying on its contents"
        } else {
            "a VCS step on the node's runner may have half run; inspect its copy"
        };
        let anchor = self.vcs_fault_anchor(agent_id);
        self.record_vcs_fault(
            agent_id,
            VcsFault {
                refresh_prior_status: None,
                source_id: stage_id.to_owned(),
                workspace_id: None,
                attempt_id,
                reason: reason.to_owned(),
            },
            anchor,
        );
    }

    /// Records that a carry back into `agent_id`'s parent found the
    /// candidate reference `source_id` already naming another commit: a
    /// decision, never an overwrite.
    pub(crate) fn fault_carried_candidate(
        &mut self,
        agent_id: &str,
        source_id: &str,
        attempt_id: Option<super::AttemptId>,
    ) {
        if !self.nodes.contains_key(agent_id) {
            return;
        }
        let anchor = self.vcs_fault_anchor(agent_id);
        self.record_vcs_fault(
            agent_id,
            VcsFault {
                refresh_prior_status: None,
                source_id: source_id.to_owned(),
                workspace_id: None,
                attempt_id,
                reason: "the parent's checkout already names another commit under this candidate's reference; it was not moved".to_owned(),
            },
            anchor,
        );
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
        let settled: VcsFaultSettled = serde_json::from_str(&self.operations[id].canonical_request)
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "VCS settlement intent is invalid",
                )
            })?;
        let Some(record) = self.operations.get(&settled.fault_id) else {
            return Ok(());
        };
        let fault: VcsFault = serde_json::from_str(&record.canonical_request).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "settled VCS fault is invalid",
            )
        })?;
        let agent = record.caller_agent_id.clone();
        if let Some(status) = fault.refresh_prior_status
            && !self.has_faulted_vcs_obligation(&agent)
            && let Some(node) = self.nodes.get_mut(&agent)
            && node.attempt_id == settled.attempt_id
            && node.status == NodeStatus::RecoveryRequired
        {
            node.status = status;
            self.fault_reasons.remove(&agent);
            self.bump();
        }
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

    pub(super) fn record_refresh_fault(&mut self, workspace_id: &str, reason: &str) {
        let Ok(agent_id) = self.workspace_producer(workspace_id) else {
            return;
        };
        let fault = VcsFault {
            refresh_prior_status: self.nodes.get(&agent_id).map(|node| node.status),
            source_id: format!("workspace-refresh:{workspace_id}"),
            workspace_id: Some(workspace_id.to_owned()),
            attempt_id: self
                .nodes
                .get(&agent_id)
                .and_then(|node| node.attempt_id.clone()),
            reason: reason.to_owned(),
        };
        self.record_vcs_fault(&agent_id, fault, self.vcs_fault_anchor(&agent_id));
        if let Err(error) = self.fold_due(&agent_id) {
            self.record_fault(&agent_id, error.message);
        }
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
        // A stage left recorded is kept across reopen for node recovery,
        // and settles by its class when the tree resumes.
        self.settle_unfinished_vcs_stages();
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
        let binding = self.epoch_of(agent_id)?.binding(node.template.as_ref()?)?;
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
    #[allow(
        clippy::too_many_lines,
        reason = "discard validates candidate ownership and occupancy before reserving the exact workspace removal"
    )]
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
        if self.refresh_pending(&request.workspace_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "workspace refresh is pending; the copy was retained",
            ));
        }
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
                let based_here = row.input_candidate_id.as_ref().is_some_and(|input| {
                    self.candidates
                        .get(input)
                        .is_some_and(|candidate| candidate.workspace_id == request.workspace_id)
                }) || row
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
        self.reclaim_backoff.remove(&request.workspace_id);
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
    pub(crate) fn record_adopt_conflict(
        &mut self,
        candidate_id: &str,
        paths: Vec<String>,
        conflict: Option<String>,
    ) {
        self.adopt_conflicts
            .insert(candidate_id.to_owned(), (paths, conflict));
    }

    /// Takes the conflicting paths an adopt of `candidate_id` recorded.
    pub(crate) fn take_adopt_conflict(
        &mut self,
        candidate_id: &str,
    ) -> Option<(Vec<String>, Option<String>)> {
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
    /// occupants are closed and settled, and it backs no unadopted candidate.
    /// These are the `discard_workspace` checks,
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
                    && !self.refresh_pending(&row.workspace_id)
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
            .filter(|candidate| {
                candidate.workspace_id == workspace_id && candidate.adopted_by.is_none()
            })
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
        }
        // Every dependent copy pins its own base independently of this
        // checkout and of the producing candidate's retention lifetime.
        true
    }

    #[allow(
        clippy::too_many_lines,
        reason = "unmint acknowledgement preserves retry evidence on failure and retires only the matching workspace generation"
    )]
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
        input_id: Option<&str>,
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
            namespace: super::digest_text(&self.tree_id),
            launch_checkout: self.launch_checkout.clone(),
            agent_id: reserved_agent_id.to_owned(),
            caller_path: caller.path.clone(),
            mode,
            input_base: input_base.cloned(),
            merge_base: input_id
                .and_then(|id| {
                    self.candidates
                        .get(&super::CandidateId::from_persisted(id.to_owned()))
                })
                .filter(|candidate| candidate.kind == super::CandidateKind::Conflict)
                .map(|candidate| VcsSnapshot {
                    commit: candidate.base_commit.clone(),
                    content: candidate.base_content.clone(),
                    change_id: candidate.base_change_id.clone(),
                    ..VcsSnapshot::default()
                }),
            carry: None,
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
        input_candidate_id: Option<super::CandidateId>,
    ) -> BoundWorkspace {
        let workspace_id = format!("ws-{reserved_agent_id}");
        let conflict = input_candidate_id
            .as_ref()
            .and_then(|id| self.candidates.get(id))
            .filter(|row| row.kind == super::CandidateKind::Conflict);
        self.workspaces.insert(
            workspace_id.clone(),
            WorkspaceRecord {
                resolves: conflict.and_then(|row| row.source_candidate_id.clone()),
                conflict_base: conflict.map(|row| row.base_commit.clone()),
                conflict_target: conflict.and_then(|row| row.conflict_target.clone()),
                conflict_paths: conflict.map_or_else(Vec::new, |row| row.conflict_paths.clone()),
                input_candidate_id,
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

    /// Every workspace row's id and recorded path, in row order: what a
    /// caller resolves through its workspace probe, outside the tree lock,
    /// before [`Self::generation_counter_for_root`].
    #[must_use]
    pub fn workspace_paths(&self) -> Vec<(String, PathBuf)> {
        self.workspaces
            .values()
            .map(|row| (row.workspace_id.clone(), row.path.clone()))
            .collect()
    }

    /// Shared observational counter for Read/Grep stamps. Prefer the longest
    /// matching workspace path so minted copies do not inherit the launch root.
    /// `root` and `resolved` (each row id with its path) were resolved before
    /// the tree lock was taken; the tree itself does no I/O here.
    #[must_use]
    pub fn generation_counter_for_root(
        &self,
        root: &Path,
        resolved: &[(String, PathBuf)],
    ) -> Option<Arc<AtomicU64>> {
        let mut best: Option<(&str, usize)> = None;
        for (id, path) in resolved {
            if root == path || root.starts_with(path) {
                let depth = path.components().count();
                if best.is_none_or(|(_, current)| depth >= current) {
                    best = Some((id.as_str(), depth));
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
mod tests;
