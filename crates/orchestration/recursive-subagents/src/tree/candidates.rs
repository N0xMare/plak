//! The candidate seam: sealing an attempt's copy and adopting it.
//!

use std::path::Path;

use super::{
    AdoptCandidateRequest, AdoptFailure, AdoptLanding, AdoptOutcome, AdoptTicket, AttemptFailure,
    AttemptId, AttemptStatus, CandidateId, CandidateRecord, NodeStatus, ProductAgentTree,
    SealTicket, TopologyError, TopologyErrorCode, VcsError, VcsSnapshot, WorkspaceMode,
    WorkspaceStatus, bounded_text, diverged_from_base, now_unix_millis, topology_error_from_vcs,
};

const SEAL_EFFECT_KIND: &str = "seal_effect";

pub(super) const SEAL_OUTCOME_KIND: &str = "seal_outcome";

#[derive(serde::Serialize, serde::Deserialize)]
struct SealEffectRequest {
    attempt_id: Option<AttemptId>,
    reserved_at: super::FoldAnchor,
    resume_status: NodeStatus,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SealOutcome {
    attempt_id: Option<AttemptId>,
    completed_at_ms: u64,
    failure: Option<SealFailure>,
    candidate_id: Option<CandidateId>,
    reserved_at: super::FoldAnchor,
    resume_status: NodeStatus,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SealFailure {
    code: TopologyErrorCode,
    message: String,
}

fn seal_effect_id(agent_id: &str, attempt_id: Option<&AttemptId>) -> String {
    format!(
        "plak:seal-effect:{agent_id}:{}",
        attempt_id.map_or("none", AttemptId::as_str)
    )
}

impl ProductAgentTree {
    /// Opens the seal for `agent_id`'s copy: marks the node
    /// `Sealing` and queues the ticket the VCS snapshot completes with, for
    /// the runtime to run with the mutex released. Returns whether a seal was
    /// queued.
    ///
    /// Inherit has nothing of its own to seal. A minted copy (`workspace=fork`)
    /// seals, including a `read_only` snapshot.
    pub fn begin_seal(&mut self, agent_id: &str) -> bool {
        self.open_seal(agent_id).is_some()
    }

    fn open_seal(&mut self, agent_id: &str) -> Option<()> {
        let node = self.nodes.get(agent_id)?;
        if self.seal_pending(agent_id)
            || self.seal_effect_unresolved(agent_id)
            || self
                .current_attempt(agent_id)
                .is_some_and(super::AttemptRecord::has_unresolved_session_failure)
        {
            return None;
        }
        let prior_status = node.status;
        let attempt_id = node.attempt_id.clone();
        let workspace_id = node.workspace_id.clone();
        let path = node.path.clone();
        let workspace = self.workspaces.get(&workspace_id)?;
        if workspace.mode == WorkspaceMode::Inherit || workspace.status != WorkspaceStatus::Active {
            // Nothing of its own, or already adopting/discarded.
            return None;
        }
        let base_commit = workspace.base_commit.clone();
        let base_change_id = workspace.base_change_id.clone();
        let base_commit = base_commit.unwrap_or_default();
        let base_content = workspace.base_content.clone().unwrap_or_default();
        if base_commit.is_empty() {
            // A copy minted before the snapshot base existed has no merge base,
            // so a candidate from it could never be adopted three-way.
            return None;
        }
        let candidate_id = match attempt_id.as_ref() {
            Some(attempt) => CandidateId::for_attempt(attempt),
            None => CandidateId::for_agent(agent_id),
        };
        if let Some(existing) = self.candidates.get(&candidate_id) {
            // This attempt already sealed; a followup opens a new attempt id.
            // The one exception is a candidate sealed *before* the attempt
            // ended: that snapshot predates the attempt's final writes, so the
            // seal at termination replaces it unless it has
            // already been adopted.
            let attempt_ended = attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get(id))
                .and_then(|attempt| attempt.ended_at);
            let sealed_early = existing.adopted_by.is_none()
                && attempt_ended.is_some_and(|ended| existing.sealed_at < ended);
            if !sealed_early {
                return None;
            }
        } else if self
            .operations
            .contains_key(&seal_effect_id(agent_id, attempt_id.as_ref()))
        {
            // A failed or empty seal also consumed this attempt's reservation.
            // Replay must not replace that external-effect fence.
            return None;
        }
        // Candidates cannot distinguish a pending seal from an empty success.
        let canonical = super::canonical_json(&SealEffectRequest {
            attempt_id: attempt_id.clone(),
            reserved_at: self.fold_position_now(agent_id).ok()?,
            resume_status: prior_status,
        })
        .ok()?;
        let effect_id = seal_effect_id(agent_id, attempt_id.as_ref());
        self.operations.insert(
            effect_id.clone(),
            super::OperationRecord {
                fold: None,
                operation_id: effect_id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: SEAL_EFFECT_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        let ownership = std::sync::Arc::new(());
        self.outstanding_seals
            .insert(agent_id.to_owned(), ownership.clone());
        let ticket = SealTicket {
            ownership,
            agent_id: agent_id.to_owned(),
            attempt_id,
            workspace_id,
            candidate_id,
            namespace: super::digest_text(&self.tree_id),
            path,
            base_commit,
            base_content,
            base_change_id,
            prior_status,
        };
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = NodeStatus::Sealing;
        }
        self.bump();
        self.effects.seals.push(ticket);
        Some(())
    }

    /// The failure `kind` an attempt carries when its copy could not be
    /// sealed.
    pub const SEAL_FAILED_KIND: &'static str = "seal_failed";

    /// Re-lock half of the seal: records the candidate the ticket produced and
    /// returns the node to the status its termination path chose. Returns the
    /// candidate id for the `result` mail.
    pub fn finish_seal_with(
        &mut self,
        ticket: SealTicket,
        sealed: Result<Option<VcsSnapshot>, VcsError>,
    ) -> Option<CandidateId> {
        if !self
            .outstanding_seals
            .get(&ticket.agent_id)
            .is_some_and(|owner| std::sync::Arc::ptr_eq(owner, &ticket.ownership))
        {
            return None;
        }
        self.outstanding_seals.remove(&ticket.agent_id);
        let SealTicket {
            agent_id,
            attempt_id,
            workspace_id,
            candidate_id,
            base_commit,
            base_content,
            base_change_id,
            ..
        } = ticket;
        let completed_at_ms = now_unix_millis();
        let sealed = match sealed {
            Ok(sealed) => sealed,
            Err(error) => {
                // a seal that could not *run* is a fault, not
                // "nothing diverged". Reporting `candidate: null` here told
                // the parent the copy held nothing while the work sat in it
                // (gate 3, jj stale copies; the same hole existed on git for a
                // failed `update-ref`). The copy is left alone — it still has
                // the work — and the node needs a human.
                let error = topology_error_from_vcs(error);
                self.record_seal_outcome(
                    &agent_id,
                    attempt_id.as_ref(),
                    completed_at_ms,
                    Some(&error),
                    None,
                );
                return None;
            }
        };
        let mut recorded = None;
        // A copy that never diverged from its base has nothing to keep: no
        // candidate, and therefore nothing pinning its workspace against
        // `discard_workspace`. `SealTicket::seal` already
        // applies this; a hand-built snapshot is filtered here.
        let sealed = sealed.filter(|snapshot| diverged_from_base(snapshot, &base_content));
        if let Some(snapshot) = sealed {
            let producing_attempt_id = attempt_id
                .clone()
                .unwrap_or_else(|| AttemptId::new(&agent_id, 1));
            self.candidates.insert(
                candidate_id.clone(),
                CandidateRecord {
                    candidate_id: candidate_id.clone(),
                    workspace_id: workspace_id.clone(),
                    producing_agent_id: agent_id.clone(),
                    producing_attempt_id,
                    base_commit,
                    base_content,
                    base_change_id,
                    commit: snapshot.commit,
                    content: snapshot.content,
                    change_id: snapshot.change_id,
                    sealed_at: completed_at_ms,
                    adopted_by: None,
                    merged_at: None,
                    adopted_evidence: Vec::new(),
                },
            );
            recorded = Some(candidate_id);
        }
        self.record_seal_outcome(
            &agent_id,
            attempt_id.as_ref(),
            completed_at_ms,
            None,
            recorded.clone(),
        );
        recorded
    }

    fn record_seal_outcome(
        &mut self,
        agent_id: &str,
        attempt_id: Option<&AttemptId>,
        completed_at_ms: u64,
        failure: Option<&TopologyError>,
        candidate_id: Option<CandidateId>,
    ) {
        let effect_id = seal_effect_id(agent_id, attempt_id);
        let Some(effect) = self.operations.get(&effect_id) else {
            return;
        };
        let Ok(request) = serde_json::from_str::<SealEffectRequest>(&effect.canonical_request)
        else {
            return;
        };
        let outcome = SealOutcome {
            attempt_id: attempt_id.cloned(),
            completed_at_ms,
            failure: failure.map(|error| SealFailure {
                code: error.code,
                message: error.message.clone(),
            }),
            candidate_id,
            reserved_at: request.reserved_at,
            resume_status: request.resume_status,
        };
        let Ok(canonical) = super::canonical_json(&outcome) else {
            return;
        };
        let Ok(anchor) = self.fold_anchor_now(agent_id) else {
            return;
        };
        let id = format!("{effect_id}:outcome:{}", anchor.order);
        self.operations.insert(
            id.clone(),
            super::OperationRecord {
                fold: Some(anchor),
                operation_id: id.clone(),
                caller_agent_id: agent_id.to_owned(),
                tool_name: SEAL_OUTCOME_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        if let Some(effect) = self.operations.get_mut(&effect_id) {
            effect.terminal_response = Some(id);
        }
        self.bump();
        // Completion is already acknowledged by the VCS port. Its local fold
        // reserves only result publication, never another VCS dispatch.
        if let Err(error) = self.fold_due(agent_id) {
            tracing::error!(agent_id, refusal = %error.message, "seal outcome fold refused");
        }
    }

    pub(super) fn join_seal_outcome(
        &mut self,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), TopologyError> {
        let operation = &self.operations[operation_id];
        let reserve_effects = operation.terminal_response.is_none();
        let outcome: SealOutcome =
            serde_json::from_str(&operation.canonical_request).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "seal outcome is invalid",
                )
            })?;
        if let Some(attempt) = outcome
            .attempt_id
            .as_ref()
            .and_then(|id| self.attempts.get_mut(id))
        {
            attempt.candidate_id = outcome.candidate_id.clone();
        }
        let current = self.node(agent_id)?.attempt_id == outcome.attempt_id;
        if let Some(failure) = &outcome.failure {
            let seal_reason = format!("the copy could not be sealed: {}", failure.message);
            let retirement_failure = outcome
                .attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get(id))
                .filter(|attempt| attempt.has_unresolved_session_failure())
                .and_then(|attempt| attempt.failure.clone());
            let reason = retirement_failure.as_ref().map_or_else(
                || seal_reason.clone(),
                |failure| format!("{}; {seal_reason}", failure.message),
            );
            if current {
                self.fault_reasons
                    .insert(agent_id.to_owned(), reason.clone());
                if let Some(node) = self.nodes.get_mut(agent_id) {
                    node.active_turn = false;
                    node.status = NodeStatus::RecoveryRequired;
                }
            }
            if let Some(attempt) = outcome
                .attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get_mut(id))
            {
                if attempt.status != AttemptStatus::Closed {
                    attempt.status = AttemptStatus::RecoveryRequired;
                }
                attempt.ended_at = Some(outcome.completed_at_ms);
                attempt.usage.elapsed_ms = outcome
                    .completed_at_ms
                    .saturating_sub(attempt.started_at)
                    .max(attempt.usage.elapsed_ms);
                attempt.failure = Some(AttemptFailure {
                    kind: retirement_failure
                        .map_or_else(|| Self::SEAL_FAILED_KIND.to_owned(), |failure| failure.kind),
                    message: bounded_text(&reason, 512),
                });
            }
        } else if current {
            let attempt = outcome
                .attempt_id
                .as_ref()
                .and_then(|id| self.attempts.get(id));
            let status = attempt.map(|attempt| attempt.status);
            let unresolved =
                attempt.is_some_and(super::AttemptRecord::has_unresolved_session_failure);
            let resolved = if unresolved || status == Some(AttemptStatus::RecoveryRequired) {
                NodeStatus::RecoveryRequired
            } else if status == Some(AttemptStatus::Closed) {
                NodeStatus::Closed
            } else if outcome.resume_status.is_finished() {
                outcome.resume_status
            } else {
                match status {
                    Some(AttemptStatus::Completed) => NodeStatus::Completed,
                    Some(
                        AttemptStatus::CompletedWithoutSubmission
                        | AttemptStatus::Failed
                        | AttemptStatus::Cancelled,
                    ) => NodeStatus::Failed,
                    _ => outcome.resume_status,
                }
            };
            if unresolved
                && let Some(attempt) = outcome
                    .attempt_id
                    .as_ref()
                    .and_then(|id| self.attempts.get_mut(id))
                && !matches!(
                    attempt.status,
                    AttemptStatus::Closed | AttemptStatus::Adopted
                )
            {
                attempt.status = AttemptStatus::RecoveryRequired;
            }
            if let Some(node) = self.nodes.get_mut(agent_id) {
                node.status = resolved;
            }
        }
        if current && reserve_effects {
            self.enqueue_result_mail(agent_id);
        }
        self.bump();
        Ok(())
    }

    pub(super) fn seal_effect_pending_now(&self, agent_id: &str) -> bool {
        self.nodes.get(agent_id).is_some_and(|node| {
            self.operations
                .get(&seal_effect_id(agent_id, node.attempt_id.as_ref()))
                .is_some_and(|record| record.terminal_response.is_none())
        })
    }

    pub(super) fn seal_effect_pending(&self, agent_id: &str) -> bool {
        let Some(node) = self.nodes.get(agent_id) else {
            return false;
        };
        let Some(effect) = self
            .operations
            .get(&seal_effect_id(agent_id, node.attempt_id.as_ref()))
        else {
            return false;
        };
        let Ok(request) = serde_json::from_str::<SealEffectRequest>(&effect.canonical_request)
        else {
            return false;
        };
        self.fold_anchor_applied(agent_id, request.reserved_at)
            && !self.operations.values().any(|operation| {
                operation.tool_name == SEAL_OUTCOME_KIND
                    && operation.caller_agent_id == agent_id
                    && operation
                        .fold
                        .is_some_and(|anchor| self.fold_anchor_applied(agent_id, anchor))
                    && serde_json::from_str::<SealOutcome>(&operation.canonical_request).is_ok_and(
                        |outcome| {
                            outcome.attempt_id == request.attempt_id
                                && outcome.reserved_at == request.reserved_at
                        },
                    )
            })
            && !self.seal_effect_faulted(agent_id)
    }

    fn seal_effect_unresolved(&self, agent_id: &str) -> bool {
        self.seal_effect_pending_now(agent_id) || self.seal_effect_faulted(agent_id)
    }

    /// `adopt_candidate` driven synchronously through the
    /// tree's own port, for tests: authority checks, the landing, and the
    /// receipt in one call. Production runs the landing as an effect with
    /// the mutex released (`begin_adopt` queues the ticket; the runtime
    /// completes it) and keeps the operation ledger itself.
    #[cfg(any(test, feature = "test-support"))]
    pub fn adopt_candidate(
        &mut self,
        caller_id: &str,
        request: &AdoptCandidateRequest,
        operation_id: Option<&str>,
    ) -> Result<AdoptOutcome, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "adopt_candidate", request)? {
            return Ok(prior);
        }
        self.begin_adopt(caller_id, request)?;
        let ticket = self
            .effects
            .adopts
            .pop()
            .unwrap_or_else(|| unreachable!("begin_adopt queued the ticket"));
        let landed = ticket.land(self.vcs());
        let outcome = self.finish_adopt(&ticket, landed);
        match outcome {
            Ok(outcome) => {
                if let Some(id) = operation_id {
                    self.remember_success(id, caller_id, "adopt_candidate", request, &outcome)?;
                }
                Ok(outcome)
            }
            Err(error) => {
                if let Some(id) = operation_id
                    && matches!(
                        error.code,
                        TopologyErrorCode::MergeConflict | TopologyErrorCode::TargetChanged
                    )
                {
                    self.remember_failure(id, caller_id, "adopt_candidate", request, error.code)?;
                }
                Err(error)
            }
        }
    }

    /// Authority, sealing, and freshness checks; marks the candidate's
    /// workspace row `Adopting` and queues the ticket the landing runs on.
    /// The receipt comes back from [`Self::complete`].
    pub fn begin_adopt(
        &mut self,
        caller_id: &str,
        request: &AdoptCandidateRequest,
    ) -> Result<(), TopologyError> {
        self.require_live()?;
        if !self.grant(caller_id)?.may_adopt {
            return Err(TopologyError::new(
                TopologyErrorCode::NotAuthorized,
                "caller has no adopt grant",
            ));
        }
        if request.candidate_id.as_str().is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "candidate_id is required",
            ));
        }
        let Some(candidate) = self.candidates.get(&request.candidate_id).cloned() else {
            return Err(TopologyError::new(
                TopologyErrorCode::NotSealed,
                "no sealed candidate carries that id; nothing was adopted",
            ));
        };
        if candidate.commit.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::NotSealed,
                "that candidate has no sealed commit; nothing was adopted",
            ));
        }
        if let Some(adopter) = &candidate.adopted_by {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!("that candidate was already adopted by {adopter}"),
            ));
        }
        if self
            .workspaces
            .get(&candidate.workspace_id)
            .is_some_and(|workspace| {
                !self.reclaimed(&workspace.workspace_id)
                    && matches!(
                        workspace.status,
                        WorkspaceStatus::Discarding
                            | WorkspaceStatus::Discarded
                            | WorkspaceStatus::CleanRemoved
                    )
            })
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                "that candidate's workspace was discarded; nothing was adopted",
            ));
        }
        // the copy's base advances to whatever was adopted, so a
        // *second* adopter of the same copy would three-way against a base
        // only the first adopter holds and receive that adopter's delta alone.
        // The earlier delta is not lost, but it is not here either, so the
        // second landing is refused and pointed at the node that has it.
        if let Some((earlier, adopter)) = self
            .candidates
            .values()
            .filter(|record| record.workspace_id == candidate.workspace_id)
            .find_map(|record| {
                record
                    .adopted_by
                    .as_ref()
                    .filter(|adopter| *adopter != caller_id)
                    .map(|adopter| (record.candidate_id.clone(), adopter.clone()))
            })
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!(
                    "an earlier candidate from that copy ({earlier}) landed in {adopter}; adopt                      {adopter}'s candidate instead, or nothing carries both deltas"
                ),
            ));
        }
        // Authority is subtree membership, not ownership: a read-only
        // supervisor may land what its own children sealed.
        let mut subtree = Vec::new();
        self.collect_subtree(caller_id, &mut subtree);
        if !subtree.iter().any(|id| id == &candidate.producing_agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::NotAuthorized,
                "that candidate was not sealed inside the caller's subtree",
            ));
        }
        if candidate.producing_agent_id == caller_id {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                "a node cannot adopt its own copy into itself",
            ));
        }
        let producer_finished = self
            .nodes
            .get(&candidate.producing_agent_id)
            .is_some_and(|node| node.status.is_finished());
        let attempt_finished = self
            .attempts
            .get(&candidate.producing_attempt_id)
            .is_some_and(|attempt| attempt.status.is_stopped())
            || self.conversation_collected(&candidate.producing_agent_id);
        if !attempt_finished {
            return Err(TopologyError::new(
                TopologyErrorCode::NotSealed,
                "the producing attempt has not finished; nothing was adopted",
            ));
        }
        if !producer_finished {
            // The attempt that sealed this candidate *is* finished; the node
            // has since opened a later one. Saying "has not finished" here
            // sent adopters looking for a stuck attempt that did not exist.
            return Err(TopologyError::new(
                TopologyErrorCode::NotSealed,
                format!(
                    "{} sealed that candidate on {} but is running a later attempt; adopt after                      the node finishes, or the two attempts' copies diverge under you",
                    candidate.producing_agent_id, candidate.producing_attempt_id
                ),
            ));
        }
        // A copy minted with `inputs:[<this candidate>]` holds it as its base. Landing
        // the candidate rewrites that base under a child that is still running,
        // and on jj the copy goes stale — recovery checks out a fresh divergent
        // working-copy commit and resets the files with it, so whatever the
        // child had written and not committed is gone before any host code
        // runs. Git is harmless here; the rule is applied to both so there is
        // one shape to learn rather than a backend-conditional one.
        let dependent = self.nodes.values().find(|node| {
            if node.status.is_finished() {
                return false;
            }
            let Some(row) = self.workspaces.get(&node.workspace_id) else {
                return false;
            };
            if row.mode == WorkspaceMode::Inherit {
                return false;
            }
            // git records the candidate's own commit as the copy's base.
            if row.base_commit.as_deref() == Some(candidate.commit.as_str()) {
                return true;
            }
            // jj records the copy's own working-copy commit instead, so the
            // dependency is ancestry in the launch repo.
            let Some(copy_id) = row.path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            self.vcs().copy_is_over_candidate(
                &self.launch_checkout,
                copy_id,
                &VcsSnapshot {
                    commit: candidate.commit.clone(),
                    content: candidate.content.clone(),
                    change_id: candidate.change_id.clone(),
                },
            )
        });
        if let Some(node) = dependent {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!(
                    "a running child ({}) was minted over this candidate; wait for it or close it",
                    node.agent_id
                ),
            ));
        }
        let mut reviewed = false;
        if let Some(evidence) = request.evidence.as_ref() {
            for id in evidence {
                let attempt_id = AttemptId::from_persisted(id.clone());
                let Some(attempt) = self
                    .attempts
                    .get(&attempt_id)
                    .filter(|attempt| subtree.contains(&attempt.agent_id))
                else {
                    return Err(TopologyError::new(
                        TopologyErrorCode::EvidenceMismatch,
                        format!("evidence {id} is not an attempt in the caller's subtree"),
                    ));
                };
                // A review that names the candidate in its output is evidence
                // *about this candidate*; any other attempt is merely known.
                reviewed |= attempt
                    .output_json
                    .as_deref()
                    .is_some_and(|output| output.contains(candidate.candidate_id.as_str()));
            }
        }
        // `require_review_evidence`: off by default; when on, an
        // adopt must cite a subtree attempt whose output references the
        // candidate, or nothing lands.
        if self.snapshot.require_review_evidence && !reviewed {
            return Err(TopologyError::new(
                TopologyErrorCode::EvidenceMismatch,
                format!(
                    "this epoch requires review evidence: pass `evidence` naming an attempt in \
                     your subtree whose output_json references {}; nothing was adopted",
                    candidate.candidate_id
                ),
            ));
        }
        let Some(child) = self.workspaces.get(&candidate.workspace_id) else {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceNotOwned,
                "the candidate's workspace row is missing",
            ));
        };
        let child_path = if self.reclaimed(&child.workspace_id) {
            self.launch_checkout.clone()
        } else {
            child.path.clone()
        };
        let target_path = self.node(caller_id)?.path.clone();
        if let Some(row) = self.workspaces.get_mut(&candidate.workspace_id) {
            row.status = WorkspaceStatus::Adopting;
            row.adopter_agent_id = Some(caller_id.to_owned());
        }
        self.bump();
        self.take_adopt_conflict(candidate.candidate_id.as_str());
        self.effects.adopts.push(AdoptTicket {
            caller_id: caller_id.to_owned(),
            candidate,
            target_path,
            child_path,
            expected_target: request.expected_target.clone(),
            evidence: request.evidence.clone().unwrap_or_default(),
        });
        Ok(())
    }

    /// Re-lock half of the adopt: commits the receipt or restores the row.
    pub fn finish_adopt(
        &mut self,
        ticket: &AdoptTicket,
        landed: Result<AdoptLanding, AdoptFailure>,
    ) -> Result<AdoptOutcome, TopologyError> {
        let workspace_id = ticket.candidate.workspace_id.clone();
        self.settle_workspace_faults(&workspace_id);
        let reclaimed = self.reclaimed(&workspace_id);
        match landed {
            Ok(landing) => {
                if let Some(row) = self.workspaces.get_mut(&workspace_id) {
                    row.status = if reclaimed {
                        WorkspaceStatus::CleanRemoved
                    } else {
                        WorkspaceStatus::Adopted
                    };
                    row.adopter_agent_id = None;
                    // The copy's next candidate diverges from, and merges
                    // against, what was just adopted — not the mint base, or
                    // a second adopt would re-apply the first.
                    let (commit, content, change_id) = match &landing.child_base {
                        Some(base) => (
                            base.commit.clone(),
                            base.content.clone(),
                            base.change_id.clone(),
                        ),
                        None => (
                            ticket.candidate.commit.clone(),
                            ticket.candidate.content.clone(),
                            ticket.candidate.change_id.clone(),
                        ),
                    };
                    row.base_commit = Some(commit);
                    row.base_content = Some(content);
                    // The jj squash abandoned the change the copy was minted
                    // on; the next seal must ask its divergence question
                    // against the change the copy sits on *now*.
                    row.base_change_id = change_id;
                }
                if let Some(record) = self.candidates.get_mut(&ticket.candidate.candidate_id) {
                    record.adopted_by = Some(ticket.caller_id.clone());
                    record.merged_at = Some(
                        landing
                            .target_after
                            .split(':')
                            .next()
                            .unwrap_or(&landing.target_after)
                            .to_owned(),
                    );
                    record.adopted_evidence.clone_from(&ticket.evidence);
                }
                if !self.conversation_collected(&ticket.candidate.producing_agent_id) {
                    self.record_effect_outcome(
                        &ticket.candidate.producing_agent_id,
                        Some(ticket.candidate.producing_attempt_id.clone()),
                        super::effect_outcomes::Outcome::AttemptAdopted,
                    )?;
                }
                self.bump();
                // A closed child's copy was only waiting on this landing.
                self.prune_closed_copies();
                Ok(AdoptOutcome {
                    candidate_id: ticket.candidate.candidate_id.clone(),
                    target_before: landing.target_before,
                    target_after: landing.target_after,
                    files_changed: landing.files_changed,
                })
            }
            Err(failure) => {
                if let Some(row) = self.workspaces.get_mut(&workspace_id) {
                    row.status = if reclaimed {
                        WorkspaceStatus::CleanRemoved
                    } else {
                        WorkspaceStatus::Active
                    };
                    row.adopter_agent_id = None;
                }
                self.bump();
                Err(match failure {
                    AdoptFailure::Conflict(paths) => {
                        let error = TopologyError::new(
                            TopologyErrorCode::MergeConflict,
                            merge_conflict_message(&paths, &ticket.child_path),
                        );
                        self.record_adopt_conflict(ticket.candidate.candidate_id.as_str(), paths);
                        error
                    }
                    AdoptFailure::TargetChanged(current) => TopologyError::new(
                        TopologyErrorCode::TargetChanged,
                        format!("the target moved: current is {current}; nothing was applied"),
                    ),
                    AdoptFailure::RecoveryRequired(reason) => {
                        TopologyError::new(TopologyErrorCode::WorkspaceRecoveryRequired, reason)
                    }
                    AdoptFailure::Unavailable(reason) => TopologyError::new(
                        TopologyErrorCode::MintUnavailable,
                        format!("the candidate could not be landed: {reason}"),
                    ),
                })
            }
        }
    }
}

const CONFLICT_PATHS_LEAD: &str = " path(s): ";
const CONFLICT_PATHS_END: &str = "; the child copy at ";

/// The `merge_conflict` refusal of an adopt. The message is bounded; the
/// adopting call's host reads every path from its call facts instead.
#[must_use]
pub fn merge_conflict_message(paths: &[String], child_path: &Path) -> String {
    format!(
        "merge_conflict in {}{CONFLICT_PATHS_LEAD}{}{CONFLICT_PATHS_END}{} is retained untouched \
         and nothing was applied",
        paths.len(),
        paths.join(", "),
        child_path.display()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::tree::test_support::*;
    use crate::tree::vcs_fixtures::*;

    #[test]
    fn failed_empty_candidate_unpin_is_recovery_and_cannot_discard_retained_pin() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&child.agent_id)));
        vcs.unpin_next_with(Err(VcsError::seal_failed(
            "injected private pin removal failure",
        )));
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            tree.attempt(&child.agent_id)
                .unwrap()
                .candidate_id
                .is_none()
        );
        assert!(
            tree.fault_reason(&child.agent_id)
                .unwrap()
                .contains("private pin removal failure")
        );
        let request = WorkspaceIdRequest {
            workspace_id: child.workspace.id,
        };
        assert_eq!(
            tree.discard_workspace("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::WorkspaceRecoveryRequired
        );
        assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), 1);
        assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
    }

    /// A closed child's copy is kept while its candidate is unadopted, and
    /// removed once the candidate lands; a copy that sealed nothing goes as
    /// soon as it is closed.
    #[test]
    fn closed_copies_are_pruned_once_nothing_needs_them() {
        let (_root, vcs, mut tree) = fake_tree();
        let worker = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&worker.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &worker.agent_id);
        tree.close_agent("root", &close_req(&worker.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert!(
            vcs.calls_of(FakeOp::Unmint).is_empty(),
            "an unadopted candidate keeps its copy"
        );
        assert_eq!(
            tree.workspace(&worker.workspace.id).unwrap().status,
            WorkspaceStatus::Active
        );
        let unpins_before = vcs.calls_of(FakeOp::Unpin).len();
        let adopted = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(
            tree.candidates()[&candidate_id].merged_at.as_deref(),
            Some(adopted.target_after.split(':').next().unwrap())
        );
        tree.settle_for_test();
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        assert_eq!(
            tree.workspace(&worker.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
        // The adopted candidate's pin goes with its copy, and only after it.
        let ops: Vec<FakeOp> = vcs.calls().into_iter().map(|call| call.op).collect();
        assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), unpins_before + 1);
        assert_eq!(ops.last(), Some(&FakeOp::Unpin));
        assert_eq!(ops[ops.len() - 2], FakeOp::Unmint);
        // Its commit is no longer pinned, so it is no longer a spawn input.
        let mut over = mint_req();
        over.inputs = Some(vec![candidate_id.to_string()]);
        assert_eq!(
            tree.spawn_agent("root", &over, None).unwrap_err().code,
            TopologyErrorCode::InputNotSealed
        );

        let idle = tree.spawn_agent("root", &mint_req(), None).unwrap();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&idle.agent_id)));
        tree.close_agent("root", &close_req(&idle.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 2);
        assert_eq!(
            tree.workspace(&idle.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
    }

    /// A prune that fails leaves the closed copy and its node as they were
    /// and is not retried in a loop.
    #[test]
    fn a_failed_prune_keeps_the_copy_without_recovery() {
        let (_root, vcs, mut tree) = fake_tree();
        let idle = tree.spawn_agent("root", &mint_req(), None).unwrap();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&idle.agent_id)));
        vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected removal failure")));
        tree.close_agent("root", &close_req(&idle.agent_id), None)
            .unwrap();
        let settled = tree.settle_for_test();
        assert_eq!(settled.cleanup_failures, [] as [TopologyError; 0]);
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        assert_eq!(
            tree.workspace(&idle.workspace.id).unwrap().status,
            WorkspaceStatus::Active
        );
        assert_eq!(
            tree.node(&idle.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        tree.close_agent("root", &close_req(&idle.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            vcs.calls_of(FakeOp::Unmint).len(),
            1,
            "not retried in this process"
        );
    }

    /// The seal and the adopt go through the `VcsOps` port: on `FakeVcs` a
    /// closed copy seals the fake's snapshot as its candidate, a seal that
    /// does not diverge records none and unpins, and an adopt lands the
    /// fake's receipt.
    #[test]
    fn fake_vcs_seals_and_adopts_through_the_port() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        assert_eq!(
            candidate_id,
            CandidateId::from_persisted(format!("cand-{}", child.attempt_id))
        );
        let sealed_commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
        assert_eq!(sealed_commit, format!("sealed-{candidate_id}"));
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        let outcome = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(outcome.target_after, sealed_commit);
        assert_eq!(outcome.files_changed, 1);
        assert_eq!(vcs.calls_of(FakeOp::Adopt).len(), 1);

        // A copy that never diverged: the fake answers the base itself.
        let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
        vcs.seal_next_with(Ok(FakeVcs::base_for(&second.agent_id)));
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        assert!(
            tree.attempt(&second.agent_id)
                .unwrap()
                .candidate_id
                .is_none()
        );
        assert_eq!(vcs.calls_of(FakeOp::Unpin).len(), 1);

        // A seal that could not run is a fault, not "nothing diverged".
        let third = tree.spawn_agent("root", &mint_req(), None).unwrap();
        vcs.seal_next_with(Err(VcsError::seal_failed("fake refused the pin")));
        tree.record_turn_completed(&third.agent_id).unwrap();
        tree.settle_for_test();
        let attempt = tree.attempt(&third.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.kind.as_str()),
            Some(ProductAgentTree::SEAL_FAILED_KIND)
        );
        assert!(
            tree.fault_reason(&third.agent_id)
                .is_some_and(|reason| reason.contains("fake refused the pin"))
        );
    }

    /// An adopt the port refuses restores the row and reports the port's
    /// reason under the matching wire code, without a repo.
    #[test]
    fn fake_vcs_adopt_failures_map_onto_wire_codes() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let workspace_id = tree.node(&child.agent_id).unwrap().workspace_id.clone();
        for (failure, code) in [
            (
                AdoptFailure::Conflict(vec!["a.rs".to_owned()]),
                TopologyErrorCode::MergeConflict,
            ),
            (
                AdoptFailure::TargetChanged("moved".to_owned()),
                TopologyErrorCode::TargetChanged,
            ),
            (
                AdoptFailure::Unavailable("no jj".to_owned()),
                TopologyErrorCode::MintUnavailable,
            ),
        ] {
            vcs.adopt_next_with(Err(failure));
            let refused = tree
                .adopt_candidate("root", &adopt_req(&candidate_id), None)
                .unwrap_err();
            assert_eq!(refused.code, code);
            assert_eq!(
                tree.workspace(&workspace_id).unwrap().status,
                WorkspaceStatus::Active,
                "the refused adopt restored the row"
            );
        }
        assert_eq!(vcs.calls_of(FakeOp::Adopt).len(), 3);
        assert!(
            tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
                .is_ok()
        );
    }
    use crate::tree::*;

    #[test]
    fn candidate_cleanup_cannot_unpin_another_trees_adoptable_work() {
        let repo = git_repo();
        let mut first = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let mut second = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        second.tree_id = "second-tree".to_owned();
        let a = first.spawn_agent("root", &mint_req(), None).unwrap();
        let b = second.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(first.node(&a.agent_id).unwrap().path.join("A.txt"), "a\n").unwrap();
        std::fs::write(second.node(&b.agent_id).unwrap().path.join("B.txt"), "b\n").unwrap();
        first.record_turn_completed(&a.agent_id).unwrap();
        second.record_turn_completed(&b.agent_id).unwrap();
        first.settle_for_test();
        second.settle_for_test();
        let first_candidate = candidate_of(&first, &a.agent_id);
        let second_candidate = candidate_of(&second, &b.agent_id);
        assert_eq!(first_candidate, second_candidate);
        assert_eq!(first_candidate.as_str(), "cand-c1-a1");
        first
            .close_agent("root", &close_req(&a.agent_id), None)
            .unwrap();
        first.settle_for_test();
        let adopted = first
            .adopt_candidate("root", &adopt_req(&first_candidate), None)
            .unwrap();
        first.settle_for_test();
        assert_eq!(
            first.candidates()[&first_candidate].merged_at.as_deref(),
            Some(adopted.target_after.split(':').next().unwrap())
        );
        assert!(!first.node(&a.agent_id).unwrap().path.exists());
        assert!(second.node(&b.agent_id).unwrap().path.exists());
        let listed = first
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_completed: Some(true),
                    include_self: None,
                    include_read: None,
                },
            )
            .unwrap();
        assert_eq!(
            listed.agents[0]["merged_at"],
            adopted.target_after.split(':').next().unwrap()
        );
        let reference = plakous_workspace::occupancy_vcs::candidate_ref(
            &digest_text(&second.tree_id),
            second_candidate.as_str(),
        );
        assert!(
            Command::new("git")
                .args(["show-ref", "--verify", "--quiet", &reference])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn candidate_inputs_are_refused_input_not_sealed() {
        let (_tmp, mut tree) = tree();
        let mut request = mint_req();
        request.inputs = Some(vec!["cand-c1-1".into()]);
        let error = tree.spawn_agent("root", &request, None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::InputNotSealed);
        assert_eq!(tree.nodes().len(), 1);
        // read_only with an input is a shape error, before anything else runs.
        let mut read_only = spawn_req();
        read_only.inputs = Some(vec!["cand-c1-1".into()]);
        assert_eq!(
            tree.spawn_agent("root", &read_only, None).unwrap_err().code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    /// A candidate that a *running* child
    /// was minted over is not adoptable. Landing it rewrites that child's base
    /// under it, and on jj the copy then goes stale — recovery resets its files
    /// and the child's uncommitted writes are gone before any host code runs.
    /// Git survives it, but the refusal is uniform so there is one rule.
    #[test]
    fn adopt_is_refused_while_a_child_minted_over_the_candidate_runs() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let first_path = tree.node(&first.agent_id).unwrap().path.clone();
        std::fs::write(first_path.join("H.txt"), "first pass\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &first.agent_id);

        let mut over = mint_req();
        over.inputs = Some(vec![candidate_id.to_string()]);
        let second = tree.spawn_agent("root", &over, None).unwrap();
        let refused = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
        assert!(
            refused.message.contains(&second.agent_id)
                && refused.message.contains("wait for it or close it"),
            "{}",
            refused.message
        );
        assert!(
            !repo.path().join("H.txt").exists(),
            "the refused adopt landed nothing"
        );
        assert_eq!(
            tree.workspace(&first.workspace.id).unwrap().status,
            WorkspaceStatus::Active,
            "the refused adopt left the candidate's row alone"
        );

        // The dependent child finishes; the same adopt is now applicable.
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "first pass\n"
        );
    }

    /// an `inputs` entry starts the child from a sealed
    /// candidate rather than from its parent's live state. The pipeline shape
    /// — one agent's copy handed to the next — is exactly this.
    #[test]
    fn spawn_over_a_sealed_candidate_mints_from_it() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let first_path = tree.node(&first.agent_id).unwrap().path.clone();
        std::fs::write(first_path.join("H.txt"), "first pass\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &first.agent_id);

        let mut over = mint_req();
        over.inputs = Some(vec![candidate_id.to_string()]);
        let second = tree.spawn_agent("root", &over, None).unwrap();
        let second_path = tree.node(&second.agent_id).unwrap().path.clone();
        assert_eq!(
            std::fs::read_to_string(second_path.join("H.txt")).unwrap(),
            "first pass\n",
            "the copy starts from the candidate, not from the parent"
        );
        assert!(
            !repo.path().join("H.txt").exists(),
            "handing a candidate to a child lands nothing in the human checkout"
        );
        assert_eq!(
            tree.workspace(&second.workspace.id)
                .unwrap()
                .base_commit
                .as_deref(),
            Some(
                tree.candidates()
                    .get(&candidate_id)
                    .unwrap()
                    .commit
                    .as_str()
            ),
            "the candidate is the merge base a later adopt reads"
        );

        // Two candidates would be a merge the spawn is not authorized to do.
        let mut two = mint_req();
        two.inputs = Some(vec![candidate_id.to_string(), "cand-other-a1".to_owned()]);
        assert_eq!(
            tree.spawn_agent("root", &two, None).unwrap_err().code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    /// Authority is checked before anything else, and an unknown candidate id
    /// is `not_sealed` with nothing adopted.
    #[test]
    fn adopt_candidate_checks_authority_then_refuses_not_sealed() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let request = AdoptCandidateRequest {
            candidate_id: CandidateId::from_persisted("cand-c1-1".to_owned()),
            expected_target: None,
            evidence: None,
        };
        assert_eq!(
            tree.adopt_candidate(&spawned.agent_id, &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::NotAuthorized
        );
        let error = tree
            .adopt_candidate("root", &request, Some("op-adopt"))
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::NotSealed);
        assert!(
            error.message.contains("nothing was adopted"),
            "{}",
            error.message
        );
        assert!(tree.candidates().is_empty());
        assert!(
            !tree
                .compiled_tool_names(&spawned.agent_id)
                .unwrap()
                .contains(&"adopt_candidate")
        );
        assert!(
            tree.compiled_tool_names("root")
                .unwrap()
                .contains(&"adopt_candidate")
        );
    }

    /// A quota refusal is not an ordinary failed turn: the
    /// **attempt** carries the reason and the **node** goes `Failed`, because a
    /// blocked attempt has no node status of its own. Nothing auto-retries and
    /// no other account is spent.
    #[test]
    fn quota_block_seals_partial_and_does_not_retry() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.record_attempt_blocked(
            &spawned.agent_id,
            BlockedReason {
                quota: "quota_exceeded".to_owned(),
                account: "codex".to_owned(),
                resets_at: Some(1_800_000_000),
            },
        )
        .unwrap();
        tree.settle_for_test();

        let attempt = tree.attempt(&spawned.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Blocked);
        let blocked = attempt.blocked.clone().unwrap();
        assert_eq!(blocked.quota, "quota_exceeded");
        assert_eq!(blocked.account, "codex");
        assert_eq!(blocked.resets_at, Some(1_800_000_000));
        assert!(attempt.ended_at.is_some());

        let node = tree.node(&spawned.agent_id).unwrap();
        assert_eq!(node.status, NodeStatus::Failed);
        assert!(!node.active_turn);
        assert_eq!(
            tree.attempts()
                .values()
                .filter(|row| row.agent_id == spawned.agent_id)
                .count(),
            1,
            "nothing auto-retries"
        );
        assert!(
            tree.messages().values().any(|mail| {
                mail.purpose == MessagePurpose::Result && mail.body.contains("blocked")
            }),
            "the parent learns without polling"
        );
        assert!(
            tree.poll_wait("root", &wait_for(&spawned.agent_id))
                .unwrap()
                .is_some(),
            "a blocked attempt settles an open wait"
        );
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(listed.agents[0]["blocked"]["resets_at"], 1_800_000_000_u64);
    }

    /// The four termination paths all seal. Every `execute` and
    /// `mutate` copy produces a candidate; `read_only` produces none.
    #[test]
    fn termination_paths_all_seal_a_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let completed = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let closed = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let failed = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let reader = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        for id in [&completed.agent_id, &closed.agent_id, &failed.agent_id] {
            std::fs::write(tree.node(id).unwrap().path.join("H.txt"), format!("{id}\n")).unwrap();
        }
        tree.record_turn_completed(&completed.agent_id).unwrap();
        tree.settle_for_test();
        tree.close_agent("root", &close_req(&closed.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        tree.record_host_materialization(&failed.agent_id)
            .and_then(|value| tree.fold_due(&failed.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        for (id, status) in [
            (&completed.agent_id, NodeStatus::Failed),
            (&closed.agent_id, NodeStatus::Closed),
            (&failed.agent_id, NodeStatus::RecoveryRequired),
        ] {
            assert_ne!(tree.node(id).unwrap().status, NodeStatus::Sealing, "{id}");
            assert_eq!(tree.node(id).unwrap().status, status, "{id}");
            let candidate = tree
                .attempt(id)
                .unwrap()
                .candidate_id
                .clone()
                .unwrap_or_else(|| unreachable!("{id} sealed"));
            let record = tree.candidates().get(&candidate).unwrap();
            assert_eq!(record.producing_agent_id, *id);
            assert_eq!(record.commit.len(), 40, "{}", record.commit);
            assert!(record.adopted_by.is_none());
        }
        // A read_only node has nothing of its own to seal.
        assert!(!tree.begin_seal(&reader.agent_id));
        assert!(
            tree.attempt(&reader.agent_id)
                .unwrap()
                .candidate_id
                .is_none()
        );
    }

    /// the snapshot MUST run with the tree mutex released. The
    /// mutation marks the node and queues the ticket under the lock; the VCS
    /// half runs on the effects batch, which holds no tree reference.
    #[test]
    fn seal_runs_outside_the_tree_mutex() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&spawned.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Sealing
        );
        assert!(tree.candidates().is_empty());
        let pending = tree.take_effects();
        assert_eq!(pending.seals.len(), 1);
        // The VCS half needs nothing from the tree.
        let completed = pending.run(&SystemVcs);
        tree.complete(completed);
        assert_eq!(tree.candidates().len(), 1);
        // The turn ended without a submit, so the status the termination path
        // chose is restored — not `Completed` blindly.
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Failed
        );
        assert_eq!(tree.candidates().len(), 1);
    }

    /// A dirty parent (modified, untracked, deleted, and renamed paths) forks a
    /// child whose copy *is* the parent's state, seals it, and adopts it back
    /// without disturbing the parent's dirty files or its HEAD.
    #[test]
    fn dirty_parent_fork_seals_and_adopts() {
        let repo = git_repo();
        std::fs::write(repo.path().join("DEL.txt"), "del\n").unwrap();
        std::fs::write(repo.path().join("OLD.txt"), "old\n").unwrap();
        git_commit(repo.path(), &["DEL.txt", "OLD.txt"], "more");
        // Modified, untracked, deleted, renamed — the four shapes the deleted
        // `copy_dirty_files` parser mishandled.
        std::fs::write(repo.path().join("SEED.txt"), "changed\n").unwrap();
        std::fs::write(repo.path().join("F.txt"), "dirty-F\n").unwrap();
        std::fs::remove_file(repo.path().join("DEL.txt")).unwrap();
        std::fs::rename(repo.path().join("OLD.txt"), repo.path().join("NEW.txt")).unwrap();
        let parent_status = git_porcelain(repo.path());
        let parent_head = git_head(repo.path());

        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();

        // The copy is the parent's state, not the launch HEAD plus a file copy.
        assert_eq!(
            std::fs::read_to_string(child_path.join("SEED.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(child_path.join("F.txt")).unwrap(),
            "dirty-F\n"
        );
        assert!(!child_path.join("DEL.txt").exists(), "deletion travelled");
        assert!(!child_path.join("OLD.txt").exists(), "rename travelled");
        assert!(child_path.join("NEW.txt").is_file(), "rename travelled");
        let base = tree
            .workspace(&child.workspace.id)
            .unwrap()
            .base_commit
            .clone()
            .unwrap_or_else(|| unreachable!("mint records a base"));
        assert_eq!(base.len(), 40);
        // The parent is untouched by the mint.
        assert_eq!(git_porcelain(repo.path()), parent_status);
        assert_eq!(git_head(repo.path()), parent_head);

        std::fs::write(child_path.join("H.txt"), "child work\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);

        let outcome = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), Some("op-adopt"))
            .unwrap();
        assert_eq!(outcome.candidate_id, candidate_id);
        assert_eq!(outcome.files_changed, 1);
        assert_ne!(outcome.target_before, outcome.target_after);
        // The child's file landed; the human's dirty files are intact and the
        // human checkout received no commit.
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "child work\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("F.txt")).unwrap(),
            "dirty-F\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "changed\n"
        );
        assert!(!repo.path().join("DEL.txt").exists());
        assert_eq!(git_head(repo.path()), parent_head, "no commit in the root");
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Adopted
        );
        assert_eq!(
            tree.candidates()
                .get(&candidate_id)
                .unwrap()
                .adopted_by
                .as_deref(),
            Some("root")
        );
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::Adopted
        );
        // Replay is the recorded receipt, not a second landing.
        let replay = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), Some("op-adopt"))
            .unwrap();
        assert_eq!(replay, outcome);
    }

    /// A human edit made in the parent *after* the mint survives the
    /// adopt, and a stale `expected_target` refuses with nothing applied.
    #[test]
    fn adopt_refuses_changed_target_and_preserves_unrelated_edits() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        let stale = tree
            .workspace(&child.workspace.id)
            .unwrap()
            .base_commit
            .clone()
            .unwrap();
        // The human keeps working in the root while the child runs.
        std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);

        let mut stale_request = adopt_req(&candidate_id);
        stale_request.expected_target = Some(stale);
        let error = tree
            .adopt_candidate("root", &stale_request, None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::TargetChanged);
        assert!(error.message.contains("nothing was applied"), "{error:?}");
        assert!(!repo.path().join("H.txt").exists(), "nothing was applied");
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Active,
            "a refused adopt restores the row"
        );

        let outcome = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(outcome.files_changed, 1);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "child\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("HUMAN.txt")).unwrap(),
            "human\n",
            "the human edit made after the mint survives"
        );
    }

    /// A fresh `expected_target` is accepted; it is the snapshot token, the
    /// commit, or the tree id.
    #[test]
    fn adopt_accepts_a_current_expected_target() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let current = plakous_workspace::occupancy_vcs::snapshot(repo.path(), "probe").unwrap();
        let mut request = adopt_req(&candidate_id);
        // The tree id is the durable content identity; the commit wrapping it
        // also carries the snapshot's label, so quote the content.
        request.expected_target = Some(current.content.clone());
        assert_eq!(
            tree.adopt_candidate("root", &request, None)
                .unwrap()
                .files_changed,
            1
        );
    }

    /// A conflicting adopt is a failed result, never a success status: the
    /// parent keeps its own content and the child copy is retained.
    #[test]
    fn adopt_conflict_retains_child_copy() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let error = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::MergeConflict);
        assert!(error.message.contains("SEED.txt"), "{error:?}");
        assert!(error.message.contains("retained"), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "human side\n",
            "the parent is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(child_path.join("SEED.txt")).unwrap(),
            "child side\n",
            "the child copy is retained"
        );
        assert!(
            tree.candidates()
                .get(&candidate_id)
                .unwrap()
                .adopted_by
                .is_none()
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Active
        );
    }

    /// Everything the child wrote before its turn ended is in the
    /// candidate; nothing it writes after the seal is.
    #[test]
    fn candidate_holds_writes_until_the_turn_ends_and_no_later() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("BEFORE.txt"), "1\n").unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        // the child can still write until the turn ends.
        std::fs::write(child_path.join("AFTER_SUBMIT.txt"), "2\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        std::fs::write(child_path.join("AFTER_SEAL.txt"), "3\n").unwrap();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let content = tree
            .candidates()
            .get(&candidate_id)
            .unwrap()
            .content
            .clone();
        let paths = git_tree_paths(repo.path(), &content);
        assert!(paths.iter().any(|p| p == "BEFORE.txt"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "AFTER_SUBMIT.txt"), "{paths:?}");
        assert!(
            !paths.iter().any(|p| p == "AFTER_SEAL.txt"),
            "a write after the seal is not in the candidate: {paths:?}"
        );
    }

    /// L4: `close_agent` on a running child still seals, and the candidate
    /// survives the close.
    #[test]
    fn close_agent_keeps_candidate_until_explicit_discard() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let record = tree.candidates().get(&candidate_id).unwrap().clone();
        assert!(record.adopted_by.is_none());
        assert_eq!(record.commit.len(), 40);
        // Close retains the candidate until its manager explicitly gives it up.
        let request = WorkspaceIdRequest {
            workspace_id: child.workspace.id.clone(),
        };
        assert_eq!(
            tree.discard_workspace("root", &request, None)
                .unwrap()
                .status,
            WorkspaceStatus::Discarding
        );
        tree.settle_for_test();
    }

    /// The `refs/plak/<namespace>/candidates/<id>` pin outlives the adopt (a later adopt
    /// of the same copy needs it as its merge base) and is deleted with the
    /// copy.
    #[test]
    fn candidate_ref_is_deleted_with_its_copy() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let commit = tree.candidates().get(&candidate_id).unwrap().commit.clone();
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        let reference = plakous_workspace::occupancy_vcs::candidate_ref(
            &digest_text(&tree.tree_id),
            candidate_id.as_str(),
        );
        let resolve = || {
            Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", reference.as_str()])
                .current_dir(repo.path())
                .output()
                .unwrap()
        };
        let pinned = resolve();
        assert!(pinned.status.success(), "the pin is gone after adopt");
        assert_eq!(String::from_utf8_lossy(&pinned.stdout).trim(), commit);
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: child.workspace.id.clone(),
            },
            None,
        )
        .unwrap();
        tree.settle_for_test();
        assert!(!child_path.exists(), "the copy is gone");
        assert!(
            !resolve().status.success(),
            "the candidate pin outlived its copy"
        );
    }

    /// Adoption authority is subtree membership plus `may_adopt`, and evidence
    /// must name attempts that exist in that subtree.
    #[test]
    fn review_evidence_names_the_exact_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let reviewer = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        tree.record_turn_completed(&reviewer.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);

        // A sibling with no adopt grant cannot land it.
        assert_eq!(
            tree.adopt_candidate(&reviewer.agent_id, &adopt_req(&candidate_id), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::NotAuthorized
        );
        let mut bogus = adopt_req(&candidate_id);
        bogus.evidence = Some(vec!["nope-a9".into()]);
        assert_eq!(
            tree.adopt_candidate("root", &bogus, None).unwrap_err().code,
            TopologyErrorCode::EvidenceMismatch
        );
        let mut evidenced = adopt_req(&candidate_id);
        evidenced.evidence = Some(vec![reviewer.attempt_id.to_string()]);
        assert_eq!(
            tree.adopt_candidate("root", &evidenced, None)
                .unwrap()
                .candidate_id,
            candidate_id
        );
    }

    /// The jj mirror of base, seal, adopt, and conflict.
    #[test]
    fn jj_mint_seals_and_adopts_a_candidate() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        std::fs::write(repo.path().join("DIRTY.txt"), "dirty\n").unwrap();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        assert!(child_path.join("DIRTY.txt").is_file(), "the base travelled");
        let workspace = tree.workspace(&child.workspace.id).unwrap();
        assert!(
            workspace
                .base_commit
                .as_ref()
                .is_some_and(|id| !id.is_empty()),
            "the jj mint records a base"
        );
        // No commit: jj auto-snapshots the working copy into `@`, which is
        // exactly what the seal captures.
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let record = tree.candidates().get(&candidate_id).unwrap().clone();
        assert!(record.change_id.is_some(), "jj pins the change id");
        let outcome = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(outcome.candidate_id, candidate_id);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "child\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("DIRTY.txt")).unwrap(),
            "dirty\n",
            "the human's dirty file is intact"
        );
    }

    /// Gate 3: any jj command anywhere in the repo — here the parent's own
    /// `jj status`, but a sibling mint or an adopt does the same — leaves the
    /// child's copy stale, and a stale copy refuses `status` and `log`. The
    /// seal used to swallow that and report `candidate: null` while the work
    /// sat in the copy.
    #[test]
    fn jj_seal_after_parent_activity_still_produces_a_candidate() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        jj_in(repo.path(), &["status"]);
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        assert_ne!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired,
            "a stale copy is recovered, not a fault"
        );
        assert!(tree.fault_reason(&child.agent_id).is_none());
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "child\n"
        );
    }

    /// The other half of the same fix: recovering the stale copy must not
    /// invent divergence either. The child wrote nothing, so there is nothing
    /// to keep even though its `@` commit id moved under it.
    #[test]
    fn jj_untouched_copy_after_parent_activity_seals_nothing() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
        jj_in(repo.path(), &["status"]);
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert!(
            tree.attempt(&child.agent_id)
                .unwrap()
                .candidate_id
                .is_none(),
            "a copy that wrote nothing has nothing to keep"
        );
        assert!(tree.candidates().is_empty(), "{:?}", tree.candidates());
    }

    /// A seal that could not *run* is a fault, not "nothing diverged".
    /// Reported on git, where the copy is simply gone.
    #[test]
    fn a_seal_that_cannot_run_leaves_the_node_recovery_required() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        std::fs::remove_dir_all(&child_path).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert!(attempt.candidate_id.is_none());
        let failure = attempt.failure.as_ref().unwrap();
        assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
        assert!(
            failure.message.contains("could not be sealed"),
            "{}",
            failure.message
        );
        assert!(
            tree.fault_reason(&child.agent_id)
                .is_some_and(|reason| reason.contains("could not be sealed")),
            "the fault reason is the only account of a failed seal"
        );
        assert!(
            tree.candidates().is_empty(),
            "a failed seal records no candidate at all, null or otherwise"
        );
    }

    /// A `mutate` child minted over an *unadopted* candidate sits on top of
    /// that candidate's change. Adoption must land the child's own delta only:
    /// `parent@..child@` swept in the sibling's commits, landed the sibling's
    /// file here, and emptied the sibling's `@`.
    #[test]
    fn jj_child_minted_over_a_candidate_adopts_only_its_own_delta() {
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
        assert_eq!(
            std::fs::read_to_string(second_path.join("FIRST.txt")).unwrap(),
            "first\n",
            "the copy starts from the candidate"
        );
        std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &second.agent_id);
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.path().join("SECOND.txt")).unwrap(),
            "second\n"
        );
        assert!(
            !repo.path().join("FIRST.txt").exists(),
            "the unadopted input's delta is not this child's to land"
        );
        assert_eq!(
            std::fs::read_to_string(first_path.join("FIRST.txt")).unwrap(),
            "first\n",
            "the sibling's own working copy is intact"
        );
        assert!(
            tree.candidates().get(&input).unwrap().adopted_by.is_none(),
            "the input candidate was not adopted by this landing"
        );
    }

    /// Two `mutate` siblings minted from the same
    /// parent working copy, both sealed, adopted one after the other. The
    /// first adopt rewrites the parent's `@`, which rebases the second
    /// sibling's working-copy commit under it and leaves that copy stale; the
    /// adopt's base probe ran plain `jj log` *inside* that copy, got "The
    /// working copy is stale", read the base as gone, and refused the second
    /// adopt `mint_unavailable` — its file never landed.
    #[test]
    fn jj_second_sibling_still_adopts_after_the_first_landed() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let first_path = tree.node(&first.agent_id).unwrap().path.clone();
        let second_path = tree.node(&second.agent_id).unwrap().path.clone();
        std::fs::write(first_path.join("ONE.md"), "one\n").unwrap();
        std::fs::write(second_path.join("OTHER.md"), "other\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let first_candidate = candidate_of(&tree, &first.agent_id);
        let second_candidate = candidate_of(&tree, &second.agent_id);

        tree.adopt_candidate("root", &adopt_req(&first_candidate), None)
            .unwrap();
        let landed = tree
            .adopt_candidate("root", &adopt_req(&second_candidate), None)
            .unwrap_or_else(|error| panic!("the second sibling was refused: {error:?}"));
        assert_eq!(landed.candidate_id, second_candidate);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("OTHER.md")).unwrap(),
            "other\n",
            "the second sibling's delta landed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("ONE.md")).unwrap(),
            "one\n",
            "the first sibling's delta is still there"
        );
    }

    /// The git mirror of the same order: two sibling worktrees off one parent,
    /// adopted one after the other, both deltas in the parent.
    #[test]
    fn git_second_sibling_still_adopts_after_the_first_landed() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let second = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let first_path = tree.node(&first.agent_id).unwrap().path.clone();
        let second_path = tree.node(&second.agent_id).unwrap().path.clone();
        std::fs::write(first_path.join("ONE.md"), "one\n").unwrap();
        std::fs::write(second_path.join("OTHER.md"), "other\n").unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let first_candidate = candidate_of(&tree, &first.agent_id);
        let second_candidate = candidate_of(&tree, &second.agent_id);

        tree.adopt_candidate("root", &adopt_req(&first_candidate), None)
            .unwrap();
        tree.adopt_candidate("root", &adopt_req(&second_candidate), None)
            .unwrap_or_else(|error| panic!("the second sibling was refused: {error:?}"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("OTHER.md")).unwrap(),
            "other\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("ONE.md")).unwrap(),
            "one\n"
        );
    }

    /// `inputs:[<candidate>]` on jj after that candidate
    /// was adopted. The adopt's `jj squash` hides the candidate's change, so a
    /// mint that forks by change id alone was refused `mint_unavailable`; the
    /// recorded commit id still resolves and is the fallback.
    #[test]
    fn jj_mint_over_an_already_adopted_candidate_still_forks() {
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
        tree.adopt_candidate("root", &adopt_req(&input), None)
            .unwrap();

        // The reviewer that reads what was just landed.
        let mut over = mint_req();
        over.tier = Tier::Execute;
        over.inputs = Some(vec![input.to_string()]);
        let reviewer = tree.spawn_agent("root", &over, None).unwrap();
        let reviewer_path = tree.node(&reviewer.agent_id).unwrap().path.clone();
        assert_eq!(
            std::fs::read_to_string(reviewer_path.join("FIRST.txt")).unwrap(),
            "first\n",
            "the copy starts from the adopted candidate"
        );
        // An untouched execute copy still seals nothing and discards cleanly.
        tree.record_turn_completed(&reviewer.agent_id).unwrap();
        tree.settle_for_test();
        assert!(
            tree.attempt(&reviewer.agent_id)
                .unwrap()
                .candidate_id
                .is_none()
        );
    }

    /// The other half of D-J2, and the rule that now stands in front of it: a
    /// candidate that a *running* child was minted over is not adoptable —
    /// landing it rewrites that child's chain under it. Once the child is
    /// finished the landing is allowed, and the child's own candidate must
    /// still apply afterwards even though its base was rewritten by that
    /// landing (the seal's revset used to fail outright: `Revision ... doesn't
    /// exist`, `Change ID ... is divergent`).
    #[test]
    fn jj_adopt_of_a_base_candidate_waits_for_the_child_holding_it() {
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
        // The child commits, the way a long-running implementer does.
        std::fs::write(second_path.join("SECOND.txt"), "second\n").unwrap();
        jj_commit(&second_path, &["SECOND.txt"], "child commit");
        // Landing its input now would rewrite the copy under it.
        let refused = tree
            .adopt_candidate("root", &adopt_req(&input), None)
            .unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::AdoptNotApplicable);
        assert!(
            refused.message.contains(&second.agent_id)
                && refused.message.contains("wait for it or close it"),
            "{}",
            refused.message
        );
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        // A sibling copy forked from the parent's own state is not over this
        // candidate and must not block it: the rule names the copies that hold
        // it as their base, not every running child.
        let sibling = tree.spawn_agent("root", &mint_req(), None).unwrap();
        assert_eq!(
            tree.node(&sibling.agent_id).unwrap().status,
            NodeStatus::Running
        );
        // With the child finished, the input lands and rewrites the copy's
        // chain; the candidate the child already sealed must still apply.
        tree.adopt_candidate("root", &adopt_req(&input), None)
            .unwrap();
        assert_ne!(
            tree.node(&second.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired,
            "{:?}",
            tree.fault_reason(&second.agent_id)
        );
        let candidate_id = candidate_of(&tree, &second.agent_id);
        tree.adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SECOND.txt")).unwrap(),
            "second\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("FIRST.txt")).unwrap(),
            "first\n",
            "the earlier landing is still there"
        );
    }

    /// The copy stopped being a workspace of the launch
    /// repo — `.jj` moved aside and a standalone repo initialized in its place
    /// — so every id it reports is foreign. That is a typed seal fault naming
    /// the copy, never `candidate: null` and never a candidate built from a
    /// repo the parent cannot see.
    #[test]
    fn jj_copy_that_is_not_a_workspace_faults_the_seal_by_name() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        std::fs::rename(child_path.join(".jj"), child_path.join(".jj.bak")).unwrap();
        jj_in(&child_path, &["git", "init"]);
        assert!(child_path.join(".jj").join("repo").is_dir());

        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert!(attempt.candidate_id.is_none(), "nothing foreign was sealed");
        let failure = attempt.failure.as_ref().unwrap();
        assert_eq!(failure.kind, ProductAgentTree::SEAL_FAILED_KIND);
        assert!(
            failure
                .message
                .contains("not a workspace of the launch repo")
                && failure.message.contains("standalone jj repo"),
            "{}",
            failure.message
        );
        assert!(tree.candidates().is_empty());
        // The work is still in the copy; the fault is the account of it.
        assert_eq!(
            std::fs::read_to_string(child_path.join("H.txt")).unwrap(),
            "child\n"
        );
    }

    /// An `execute` copy on jj that changed nothing seals nothing, so it
    /// cannot pin its own workspace against `discard_workspace`. Regression
    /// for the live smoke's empty `cand-c2-a1`.
    #[test]
    fn jj_untouched_execute_copy_seals_nothing_and_discards() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let mut request = mint_req();
        request.tier = Tier::Execute;
        let child = tree.spawn_agent("root", &request, None).unwrap();
        assert_eq!(child.tier, Tier::Execute);
        let base = tree
            .workspace(&child.workspace.id)
            .unwrap()
            .base_commit
            .clone()
            .unwrap_or_else(|| unreachable!("the jj mint records a base"));
        assert_ne!(base, "");
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert!(
            tree.attempt(&child.agent_id)
                .unwrap()
                .candidate_id
                .is_none(),
            "an untouched copy has nothing to keep"
        );
        assert!(tree.candidates().is_empty());
        assert_eq!(
            tree.discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: child.workspace.id.clone(),
                },
                None,
            )
            .unwrap()
            .status,
            WorkspaceStatus::Discarding
        );
        tree.settle_for_test();
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
    }

    /// The same copy that writes a file does seal, so the rule is a real
    /// divergence check and not a blanket refusal.
    #[test]
    fn jj_execute_copy_that_writes_still_seals() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let mut request = mint_req();
        request.tier = Tier::Execute;
        let child = tree.spawn_agent("root", &request, None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        assert!(tree.candidates().contains_key(&candidate_id));
        assert_eq!(
            tree.discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: child.workspace.id.clone(),
                },
                None,
            )
            .unwrap()
            .status,
            WorkspaceStatus::Discarding
        );
        tree.settle_for_test();
    }

    /// A jj adopt that conflicts is undone before the squash, so the conflict
    /// never lands in the human's `@`.
    #[test]
    fn jj_adopt_conflict_leaves_the_parent_untouched() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let error = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert!(
            matches!(
                error.code,
                TopologyErrorCode::MergeConflict | TopologyErrorCode::TargetChanged
            ),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "human side\n",
            "the human's copy is untouched"
        );
        assert!(
            child_path.join("SEED.txt").is_file(),
            "the copy is retained"
        );
    }

    #[test]
    fn repeated_close_during_pending_terminal_seal_keeps_one_ticket_and_close_intent() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("WORK.txt"), "complete work\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Sealing
        );
        assert_eq!(tree.effects.seals.len(), 1);
        for _ in 0..2 {
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::Sealing
            );
            assert_eq!(tree.effects.seals.len(), 1);
        }
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::Closed
        );
        assert_eq!(tree.candidates().len(), 1);
        assert_eq!(
            result_mails_from(&tree, &child.agent_id),
            [] as [MailRecord; 0]
        );
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        assert!(tree.effects.seals.is_empty());
    }

    #[test]
    fn close_during_seal_then_retirement_fault_survives_seal_success() {
        close_during_seal_then_retirement_fault(false);
    }

    #[test]
    fn close_during_seal_then_retirement_fault_survives_seal_failure() {
        close_during_seal_then_retirement_fault(true);
    }

    fn close_during_seal_then_retirement_fault(seal_fails: bool) {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let id = &child.agent_id;
        tree.submit_result(
            id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(id).unwrap();
        let dispatched = tree.take_effects();
        assert_eq!(dispatched.seals.len(), 1);
        tree.close_agent("root", &close_req(id), None).unwrap();
        assert_eq!(tree.node(id).unwrap().status, NodeStatus::Sealing);
        assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
        tree.record_host_retirement(id, "release refused".into())
            .and_then(|value| tree.fold_due(id).map(|_| value))
            .unwrap();
        if seal_fails {
            vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
        }
        tree.complete(dispatched.run(vcs.as_ref()));
        assert_eq!(tree.node(id).unwrap().status, NodeStatus::RecoveryRequired);
        assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
        assert_eq!(
            tree.attempt(id).unwrap().output_json.as_deref(),
            Some(r#"{"ok":true}"#)
        );
        assert_eq!(result_mails_from(&tree, id), [] as [MailRecord; 0]);
        assert!(matches!(
            tree.recovery_need(id).unwrap(),
            RecoveryNeed::Session { .. }
        ));
        let attempt = tree.attempt(id).unwrap().attempt_id.clone();
        let receipt = tree
            .recover_host_decision(
                id,
                &attempt,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiescent".into(),
                },
            )
            .unwrap();
        assert!(receipt.recovered);
        tree.settle_for_test();
        assert_eq!(tree.node(id).unwrap().status, NodeStatus::Closed);
        assert_eq!(tree.attempt(id).unwrap().status, AttemptStatus::Closed);
        assert_eq!(result_mails_from(&tree, id), [] as [MailRecord; 0]);
    }

    #[test]
    fn retirement_fault_and_repeated_close_keep_dispatched_seal_and_failure() {
        for seal_fails in [false, true] {
            let (_root, vcs, mut tree) = fake_tree();
            let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            let dispatched = tree.take_effects();
            assert_eq!(dispatched.seals.len(), 1);
            assert!(tree.effects.seals.is_empty());
            tree.record_host_retirement(&child.agent_id, "release refused".into())
                .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
                .unwrap();
            assert!(tree.clone().seal_pending(&child.agent_id));
            let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
            assert!(!reopened.seal_pending(&child.agent_id));
            assert!(
                reopened
                    .finish_seal_with(dispatched.seals[0].clone(), Ok(None))
                    .is_none()
            );
            assert_eq!(
                reopened
                    .attempt(&child.agent_id)
                    .unwrap()
                    .failure
                    .as_ref()
                    .unwrap()
                    .kind,
                "session_retirement"
            );
            assert_eq!(
                reopened.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            for _ in 0..2 {
                tree.close_agent("root", &close_req(&child.agent_id), None)
                    .unwrap();
                tree.stop_all("root", None).unwrap();
                assert!(!tree.begin_seal(&child.agent_id));
                assert!(tree.take_effects().seals.is_empty());
                assert!(tree.seal_pending(&child.agent_id));
                assert_eq!(
                    tree.node(&child.agent_id).unwrap().status,
                    NodeStatus::RecoveryRequired
                );
            }
            if seal_fails {
                vcs.seal_next_with(Err(VcsError::seal_failed("snapshot refused")));
            }
            tree.complete(dispatched.run(vcs.as_ref()));
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
            assert_eq!(tree.candidates().len(), usize::from(!seal_fails));
            assert!(!tree.seal_pending(&child.agent_id));
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            let attempt = tree.attempt(&child.agent_id).unwrap();
            assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
            let failure = attempt.failure.as_ref().unwrap();
            assert_eq!(failure.kind, "session_retirement");
            assert!(failure.message.contains("release refused"));
            if seal_fails {
                assert!(failure.message.contains("snapshot refused"));
            }
            assert!(
                tree.fault_reason(&child.agent_id)
                    .unwrap()
                    .contains("release refused")
            );
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert!(tree.take_effects().seals.is_empty());
        }
    }

    #[test]
    fn unresolved_terminal_survives_seal_close_adopt_and_bound_replay() {
        use super::super::turn_outcome::FailedTurnDisposition;
        for seal_fails in [false, true] {
            let (_root, vcs, mut tree) = fake_tree();
            let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.submit_result(
                &child.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            assert!(tree.begin_seal(&child.agent_id));
            let outstanding = tree.take_effects();
            tree.record_turn_failure(
                &child.agent_id,
                FailedTurnDisposition::RecoveryRequired,
                "canonical outcome unknown".into(),
            )
            .unwrap();
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            tree.bind_session_locator(&child.agent_id, "bound-child-session")
                .unwrap();
            tree.record_session_bound_after_failure(&child.agent_id)
                .unwrap();
            tree.fold_due(&child.agent_id).unwrap();
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert!(!tree.begin_seal(&child.agent_id));
            if seal_fails {
                vcs.seal_next_with(Err(VcsError::seal_failed("snapshot failed")));
            }
            tree.complete(outstanding.run(vcs.as_ref()));
            if !seal_fails {
                let candidate = candidate_of(&tree, &child.agent_id);
                tree.adopt_candidate("root", &adopt_req(&candidate), None)
                    .unwrap();
                assert!(tree.candidates()[&candidate].adopted_by.is_some());
            }
            tree.stop_all("root", None).unwrap();
            tree.record_turn_failure(
                &child.agent_id,
                FailedTurnDisposition::Failed {
                    kind: "turn_failed",
                },
                "later terminal failure".into(),
            )
            .unwrap();
            tree.record_host_retirement(&child.agent_id, "release refused".into())
                .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
                .unwrap();
            tree.record_host_retirement(&child.agent_id, "release refused".into())
                .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
                .unwrap();
            let persisted = tree.to_persisted();
            let attempt = persisted.rows().attempt(&child.agent_id).unwrap();
            assert_eq!(attempt.status, AttemptStatus::RecoveryRequired);
            assert_eq!(attempt.output_json.as_deref(), Some(r#"{"ok":true}"#));
            let failure = attempt.failure.as_ref().unwrap();
            for reason in [
                "canonical outcome unknown",
                "later terminal failure",
                "release refused",
            ] {
                assert!(failure.message.contains(reason));
            }
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
            assert!(tree.take_effects().seals.is_empty());
        }
    }

    #[test]
    fn old_seal_completion_cannot_consume_same_attempt_reseal() {
        let (_root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        assert!(tree.begin_seal(&child.agent_id));
        let old = tree.take_effects().seals.pop().unwrap();
        let first = old.seal(vcs.as_ref()).unwrap();
        tree.finish_seal_with(old.clone(), Ok(first.clone()));
        // Force the existing early snapshot to predate the terminal timestamp.
        tree.candidates
            .get_mut(&old.candidate_id)
            .unwrap()
            .sealed_at = 0;
        tree.record_turn_completed(&child.agent_id).unwrap();
        let current = tree.take_effects().seals.pop().unwrap();
        assert_eq!(old.candidate_id, current.candidate_id);
        assert!(tree.finish_seal_with(old, Ok(first)).is_none());
        assert!(tree.seal_pending(&child.agent_id));
        assert!(!tree.begin_seal(&child.agent_id));
        let sealed = current.seal(vcs.as_ref());
        assert!(tree.finish_seal_with(current, sealed).is_some());
        assert!(!tree.seal_pending(&child.agent_id));
        assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 2);
    }

    /// B1: `close_agent` on a child whose turn is still running does not seal;
    /// the candidate is cut when the turn exits, so it holds everything the
    /// child wrote up to that point.
    #[test]
    fn close_while_running_defers_the_seal_to_the_cancelled_turn() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        std::fs::write(child_path.join("BEFORE_CLOSE.txt"), "1\n").unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert!(
            tree.candidates().is_empty(),
            "a running child is not sealed at close; its turn is still unwinding"
        );
        assert_eq!(
            result_mails_from(&tree, &child.agent_id),
            [] as [MailRecord; 0]
        );
        // The turn keeps writing until it exits.
        std::fs::write(child_path.join("AFTER_CLOSE.txt"), "2\n").unwrap();
        seal_after_turn_exit(&mut tree, &child.agent_id);
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let content = tree.candidates()[&candidate_id].content.clone();
        let paths = git_tree_paths(repo.path(), &content);
        assert!(paths.iter().any(|p| p == "BEFORE_CLOSE.txt"), "{paths:?}");
        assert!(
            paths.iter().any(|p| p == "AFTER_CLOSE.txt"),
            "the write between close and turn exit is in the candidate: {paths:?}"
        );
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
    }

    /// B2: an interrupt neither seals nor pushes a `result`; the resumed
    /// attempt's completion seals once, with the final content, and pushes
    /// exactly one `result`.
    #[test]
    fn interrupt_seals_nothing_and_completion_pushes_one_result() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        std::fs::write(child_path.join("FIRST.txt"), "1\n").unwrap();
        tree.interrupt_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Interrupted
        );
        // The turn driver's cancelled branch seals only a `Closed` node
        // (`finalize_of_an_interrupted_turn_seals_nothing`), so nothing has
        // sealed and `enqueue_result_mail` refuses the running attempt.
        assert!(tree.candidates().is_empty(), "an interrupt does not seal");
        assert_eq!(
            result_mails_from(&tree, &child.agent_id),
            [] as [MailRecord; 0]
        );
        // Resume: the parent's mail starts a new turn on the same attempt.
        tree.send_agent_message("root", &mail(&child.agent_id, "continue"), None)
            .unwrap();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        std::fs::write(child_path.join("SECOND.txt"), "2\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.candidates().len(), 1);
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let paths = git_tree_paths(repo.path(), &tree.candidates()[&candidate_id].content);
        assert!(paths.iter().any(|p| p == "FIRST.txt"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "SECOND.txt"), "{paths:?}");
        let mails = result_mails_from(&tree, &child.agent_id);
        assert_eq!(mails.len(), 1, "{mails:?}");
        assert!(
            mails[0]
                .body
                .contains(&format!("\"candidate\":\"{candidate_id}\""))
        );
        assert!(!mails[0].body.contains("\"status\":\"running\""));
    }

    /// B2, second half: a candidate sealed while the attempt was still
    /// running is replaced by the seal at termination rather than blocking it.
    #[test]
    fn a_candidate_sealed_before_the_attempt_ended_is_replaced_at_termination() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("EARLY.txt"), "1\n").unwrap();
        seal_after_turn_exit(&mut tree, &child.agent_id);
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let early = tree.candidates()[&candidate_id].clone();
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::Running
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(child_path.join("LATE.txt"), "2\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.candidates().len(), 1, "same id, replaced in place");
        let final_record = tree.candidates()[&candidate_id].clone();
        assert_ne!(final_record.content, early.content);
        let paths = git_tree_paths(repo.path(), &final_record.content);
        assert!(paths.iter().any(|p| p == "LATE.txt"), "{paths:?}");
        // And a terminal seal is final: a further seal attempt is refused.
        assert!(!tree.begin_seal(&child.agent_id));
    }

    /// For `close_agent`: an idle closed copy seals as an
    /// effect, so the snapshot runs with the tree mutex released.
    #[test]
    fn close_seal_runs_outside_the_tree_mutex() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Sealing
        );
        assert!(tree.candidates().is_empty());
        let pending = tree.take_effects();
        assert_eq!(pending.seals.len(), 1);
        tree.complete(pending.run(&SystemVcs));
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(tree.candidates().len(), 1);
    }

    /// the landing subprocess runs on a ticket that holds no tree
    /// reference; the row is `Adopting` in between and `Adopted` after.
    #[test]
    fn adopt_lands_outside_the_tree_mutex() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        tree.begin_adopt("root", &adopt_req(&candidate_id)).unwrap();
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Adopting
        );
        assert!(!repo.path().join("H.txt").exists());
        // The VCS half needs nothing from the tree.
        let pending = tree.take_effects();
        assert_eq!(pending.adopts.len(), 1);
        let mut settled = tree.complete(pending.run(&SystemVcs));
        let outcome = settled.adopts.pop().unwrap().unwrap();
        assert_eq!(outcome.files_changed, 1);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("H.txt")).unwrap(),
            "child\n"
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Adopted
        );
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::Adopted
        );
    }

    /// A hub with a mutate child it may spawn and adopt from: the hub id, the
    /// child id, the child's copy and its sealed candidate.
    fn hub_with_sealed_grandchild(
        tree: &mut ProductAgentTree,
    ) -> (String, String, String, CandidateId) {
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
        hub_binding.spawns = vec![TemplateName::parse("implement").unwrap()];
        hub_binding.may_adopt = true;
        tree.pin_template("orchestrate", hub_binding);
        let hub = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "hub", "coordinate"),
                None,
            )
            .unwrap();
        tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
        tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
        let child = tree.spawn_agent(&hub.agent_id, &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate = candidate_of(tree, &child.agent_id);
        (hub.agent_id, child.agent_id, child.workspace.id, candidate)
    }

    /// An adopt records its caller on the row while it lands, and a reopen
    /// that finds it in flight faults that adopter, never the copy's owner:
    /// the root's adopt of a grandchild's candidate leaves the hub between
    /// them alone and becomes the root's fault reason.
    #[test]
    fn an_interrupted_grandparent_adopt_faults_the_adopter_not_the_hub() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let (hub, _child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
        tree.begin_adopt("root", &adopt_req(&candidate)).unwrap();
        let row = tree.workspace(&workspace).unwrap();
        assert_eq!(row.status, WorkspaceStatus::Adopting);
        assert_eq!(
            row.owner_agent_id, hub,
            "the hub owns the copy it spawned into"
        );
        assert_eq!(row.adopter_agent_id.as_deref(), Some("root"));
        let _lost_with_the_process = tree.take_effects();
        tree.restore_paused();
        assert_ne!(
            tree.node(&hub).unwrap().status,
            NodeStatus::RecoveryRequired,
            "the hub's checkout was never written"
        );
        assert_ne!(
            tree.node("root").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            tree.fault_reason("root")
                .is_some_and(|reason| reason.contains("adopt into this checkout"))
        );
    }

    /// A non-root adopter is faulted along with the copy's occupant; the root
    /// is left alone.
    #[test]
    fn an_interrupted_adopt_faults_a_non_root_adopter() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let (hub, child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
        tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
        assert_eq!(
            tree.workspace(&workspace)
                .unwrap()
                .adopter_agent_id
                .as_deref(),
            Some(hub.as_str())
        );
        let _lost_with_the_process = tree.take_effects();
        tree.restore_paused();
        assert_eq!(
            tree.node(&hub).unwrap().status,
            NodeStatus::RecoveryRequired,
            "the adopter's checkout may hold a half-run landing"
        );
        assert_eq!(
            tree.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired,
            "the copy's occupant is faulted as before"
        );
        assert_ne!(
            tree.node("root").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(tree.fault_reason("root").is_none());
    }

    #[test]
    fn a_live_child_adopt_survives_a_projector_pass_without_a_vcs_fault() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let (hub, child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
        tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
        tree.fold_due(&hub).unwrap();
        tree.fold_due(&child).unwrap();
        assert_ne!(
            tree.node(&hub).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_ne!(
            tree.node(&child).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let pending = tree.take_effects();
        assert!(
            tree.complete(pending.run(&SystemVcs))
                .adopts
                .pop()
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            tree.workspace(&workspace).unwrap().status,
            WorkspaceStatus::Adopted
        );
        assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Adopted);
    }

    /// The conflict paths read back from the refusal, also when the message
    /// bound cut it inside the list.
    /// A landing clears the adopter record whichever way it ends.
    #[test]
    fn a_finished_landing_clears_the_adopter_record() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let (hub, _child, workspace, candidate) = hub_with_sealed_grandchild(&mut tree);
        tree.begin_adopt(&hub, &adopt_req(&candidate)).unwrap();
        let pending = tree.take_effects();
        let mut settled = tree.complete(pending.run(&SystemVcs));
        settled.adopts.pop().unwrap().unwrap();
        let row = tree.workspace(&workspace).unwrap();
        assert_eq!(row.status, WorkspaceStatus::Adopted);
        assert_eq!(row.adopter_agent_id, None);
    }

    /// B3: a child that commits inside its jj copy and then keeps writing has
    /// its whole range landed, not just its working-copy commit.
    #[test]
    fn jj_committed_work_in_the_child_is_adopted() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("COMMITTED.txt"), "committed\n").unwrap();
        jj_commit(&child_path, &["COMMITTED.txt"], "child commit");
        std::fs::write(child_path.join("WORKING.txt"), "working\n").unwrap();
        // Meanwhile the human keeps editing.
        std::fs::write(repo.path().join("HUMAN.txt"), "human\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let outcome = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap();
        assert_eq!(outcome.files_changed, 2, "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("COMMITTED.txt")).unwrap(),
            "committed\n",
            "the committed change landed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("WORKING.txt")).unwrap(),
            "working\n",
            "the working-copy change landed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("HUMAN.txt")).unwrap(),
            "human\n"
        );
    }

    /// B6: a jj conflict is rolled back with `op restore`, so neither the
    /// rebase nor the squash survives in the human's repo.
    #[test]
    fn jj_conflict_restores_the_operation_log() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("SEED.txt"), "child side\n").unwrap();
        jj_commit(&child_path, &[], "child edit");
        std::fs::write(child_path.join("MORE.txt"), "more\n").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "human side\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let error = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::MergeConflict, "{error:?}");
        assert!(error.message.contains("SEED.txt"), "{}", error.message);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("SEED.txt")).unwrap(),
            "human side\n"
        );
        assert!(
            !repo.path().join("MORE.txt").exists(),
            "nothing was squashed"
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Active,
            "the copy is retained"
        );
    }

    /// the copy's base advances to what was adopted, so once a
    /// hub has landed one of a child's candidates, a *different* node landing
    /// a later one would receive only the second delta. Refused, and the
    /// refusal names the node that holds the first.
    #[test]
    fn a_second_adopter_of_the_same_copy_is_refused_and_names_the_first() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
        hub_binding.spawns = vec![TemplateName::parse("implement").unwrap()];
        hub_binding.may_adopt = true;
        tree.pin_template("orchestrate", hub_binding);
        let hub = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "hub", "coordinate"),
                None,
            )
            .unwrap();
        tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
        tree.grants.get_mut(&hub.agent_id).unwrap().may_adopt = true;
        let child = tree.spawn_agent(&hub.agent_id, &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = candidate_of(&tree, &child.agent_id);
        tree.adopt_candidate(&hub.agent_id, &adopt_req(&first), None)
            .unwrap();

        let mut followup = mail(&child.agent_id, "one more");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message(&hub.agent_id, &followup, None)
            .unwrap();
        std::fs::write(child_path.join("TWO.txt"), "two\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let second = candidate_of(&tree, &child.agent_id);
        let error = tree
            .adopt_candidate("root", &adopt_req(&second), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
        assert!(error.message.contains(&hub.agent_id), "{}", error.message);
        assert!(error.message.contains(first.as_str()), "{}", error.message);
        // The hub itself may still land its own next candidate.
        assert_eq!(
            tree.adopt_candidate(&hub.agent_id, &adopt_req(&second), None)
                .unwrap()
                .candidate_id,
            second
        );
    }

    /// Adopting `a1` while `a2` runs is refused — but "the producing attempt
    /// has not finished" described an attempt that had in fact finished. The
    /// refusal now names the node and the attempt it is talking about.
    #[test]
    fn adopting_an_earlier_attempt_while_a_later_one_runs_names_the_node() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("ONE.txt"), "one\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = candidate_of(&tree, &child.agent_id);
        let mut followup = mail(&child.agent_id, "keep going");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &followup, None).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Running
        );
        let error = tree
            .adopt_candidate("root", &adopt_req(&first), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::NotSealed);
        assert!(
            error.message.contains(&child.agent_id)
                && error.message.contains("later attempt")
                && !error
                    .message
                    .contains("the producing attempt has not finished"),
            "{}",
            error.message
        );
    }

    /// adoption authority is subtree membership. A sibling is not
    /// in the caller's subtree however wide its own grant is.
    #[test]
    fn adopting_outside_the_callers_subtree_is_refused() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let producer_path = tree.node(&producer.agent_id).unwrap().path.clone();
        std::fs::write(producer_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&producer.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &producer.agent_id);
        let sibling = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.grants.get_mut(&sibling.agent_id).unwrap().may_adopt = true;
        let error = tree
            .adopt_candidate(&sibling.agent_id, &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::NotAuthorized);
        assert!(error.message.contains("subtree"), "{}", error.message);
        assert!(
            tree.candidates()
                .get(&candidate_id)
                .unwrap()
                .adopted_by
                .is_none(),
            "nothing landed"
        );
    }

    /// An execute copy that never diverged pinned
    /// `refs/plak/<namespace>/candidates/<id>` while the tree filtered the candidate out,
    /// so "a candidate ref is pinned" could pass on a phantom. No candidate,
    /// no ref.
    #[test]
    fn a_seal_with_no_divergence_leaves_no_candidate_ref() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert!(tree.candidates().is_empty());
        let expected_ref = plakous_workspace::occupancy_vcs::candidate_ref(
            &digest_text(&tree.tree_id),
            &format!("cand-{}", child.attempt_id),
        );
        let refs = Command::new("git")
            .args(["for-each-ref", "refs/plak/"])
            .current_dir(&child_path)
            .output()
            .unwrap();
        let refs = String::from_utf8_lossy(&refs.stdout);
        assert!(!refs.contains(&expected_ref), "orphan pin survived: {refs}");
        // The diverging case still pins.
        let mut followup = mail(&child.agent_id, "write something");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &followup, None).unwrap();
        std::fs::write(child_path.join("H.txt"), "child\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let refs = Command::new("git")
            .args(["for-each-ref", "refs/plak/"])
            .current_dir(&child_path)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&refs.stdout).contains(
            &plakous_workspace::occupancy_vcs::candidate_ref(
                &digest_text(&tree.tree_id),
                candidate_id.as_str()
            )
        ));
    }

    /// Regression: adopt -> followup -> adopt. The
    /// adopted copy hosts a second attempt; its candidate is the delta since
    /// the adoption, and the second landing integrates only that delta — both
    /// lines land once, uncommitted, HEAD unchanged.
    #[test]
    fn followup_after_adoption_seals_and_adopts_only_the_delta() {
        let repo = git_repo();
        let head = git_head(repo.path());
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("NOTES.md"), "delegated\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = candidate_of(&tree, &child.agent_id);
        let landed = tree
            .adopt_candidate("root", &adopt_req(&first), None)
            .unwrap();
        assert_eq!(landed.files_changed, 1);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
            "delegated\n"
        );
        let row = tree.workspace(&child.workspace.id).unwrap().clone();
        assert_eq!(row.status, WorkspaceStatus::Adopted);
        let adopted = tree.candidates().get(&first).unwrap().clone();
        assert_eq!(
            row.base_commit.as_deref(),
            Some(adopted.commit.as_str()),
            "the copy's base advanced to the adopted candidate"
        );
        assert_eq!(row.base_content.as_deref(), Some(adopted.content.as_str()));

        let mut followup = mail(&child.agent_id, "add a second line");
        followup.purpose = Some(MessagePurpose::Followup);
        let sent = tree.send_agent_message("root", &followup, None).unwrap();
        assert_eq!(sent.disposition, MailDisposition::Started);
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Active,
            "the adopted copy is live again for the new attempt"
        );
        let second_attempt = tree.attempt(&child.agent_id).unwrap().attempt_id.clone();
        assert_ne!(second_attempt, child.attempt_id);
        std::fs::write(child_path.join("NOTES.md"), "delegated\nsecond\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let second = candidate_of(&tree, &child.agent_id);
        assert_ne!(second, first);
        let record = tree.candidates().get(&second).unwrap();
        assert_eq!(
            record.base_commit, adopted.commit,
            "the second candidate's merge base is the adopted state"
        );

        let landed = tree
            .adopt_candidate("root", &adopt_req(&second), None)
            .unwrap();
        assert_eq!(landed.files_changed, 1, "{landed:?}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
            "delegated\nsecond\n",
            "both lines, each once"
        );
        assert_eq!(git_head(repo.path()), head, "no commit in the root");
        assert!(
            git_porcelain(repo.path()).contains("NOTES.md"),
            "the landing is uncommitted"
        );
        assert_eq!(
            tree.attempts.get(&child.attempt_id).unwrap().status,
            AttemptStatus::Adopted
        );
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::Adopted
        );
    }

    /// The jj shape of adopt -> followup -> adopt: the squash rewrote the
    /// child's `@`, so the copy's base is its post-landing working copy and a
    /// followup that changes nothing seals no candidate.
    #[test]
    fn jj_followup_after_adoption_adopts_only_the_delta() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("NOTES.md"), "delegated\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = candidate_of(&tree, &child.agent_id);
        tree.adopt_candidate("root", &adopt_req(&first), None)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
            "delegated\n"
        );
        let mut followup = mail(&child.agent_id, "add a second line");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &followup, None).unwrap();
        // Nothing changed in the copy: no candidate, no phantom divergence
        // from the rewritten `@`.
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.candidates().len(), 1, "{:?}", tree.candidates());

        let mut again = mail(&child.agent_id, "really add it");
        again.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &again, None).unwrap();
        std::fs::write(child_path.join("NOTES.md"), "delegated\nsecond\n").unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let second = candidate_of(&tree, &child.agent_id);
        assert_ne!(second, first);
        let landed = tree
            .adopt_candidate("root", &adopt_req(&second), None)
            .unwrap();
        assert_eq!(landed.files_changed, 1, "{landed:?}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("NOTES.md")).unwrap(),
            "delegated\nsecond\n"
        );
    }

    /// `require_review_evidence`: when the epoch demands it, an
    /// adopt must cite a subtree attempt whose output names the candidate.
    #[test]
    fn require_review_evidence_demands_a_review_that_names_the_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        tree.snapshot.require_review_evidence = true;
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let error = tree
            .adopt_candidate("root", &adopt_req(&candidate_id), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::EvidenceMismatch);
        assert!(
            error.message.contains(candidate_id.as_str()),
            "{}",
            error.message
        );
        // A reviewer whose output does not mention the candidate is not
        // evidence about it.
        let mut review = spawn_req();
        review.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"},"reviewed":{"type":"string"}},"required":["ok"],"additionalProperties":false}"#.into();
        let reviewer = tree.spawn_agent("root", &review, None).unwrap();
        tree.submit_result(
            &reviewer.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true,"reviewed":"nothing in particular"}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&reviewer.agent_id).unwrap();
        tree.settle_for_test();
        let mut with_evidence = adopt_req(&candidate_id);
        with_evidence.evidence = Some(vec![reviewer.attempt_id.to_string()]);
        assert_eq!(
            tree.adopt_candidate("root", &with_evidence, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::EvidenceMismatch
        );
        // One that does is.
        let second = tree.spawn_agent("root", &review, None).unwrap();
        tree.submit_result(
            &second.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: format!(r#"{{"ok":true,"reviewed":"{candidate_id}"}}"#),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        with_evidence.evidence = Some(vec![second.attempt_id.to_string()]);
        let outcome = tree.adopt_candidate("root", &with_evidence, None).unwrap();
        assert_eq!(outcome.candidate_id, candidate_id);
        assert_eq!(
            tree.candidates()[&candidate_id].adopted_evidence,
            vec![second.attempt_id.to_string()]
        );
    }
}
