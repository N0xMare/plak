//! The candidate seam: sealing an attempt's copy and adopting it.
//!

use std::path::Path;

use plakous_workspace::occupancy_vcs::is_jj_copy_root as jj_layout_copy;

use plakous_workspace::occupancy_vcs::ConflictPath;

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
    history_left_base: bool,
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
        let resolver = self.resolver_input(&workspace_id);
        let sealed = match sealed.and_then(|sealed| {
            resolver.as_ref().map(|_| sealed).map_err(|error| {
                VcsError::new(
                    plakous_workspace::occupancy_vcs::VcsErrorKind::RecoveryRequired,
                    error.message.clone(),
                )
            })
        }) {
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
                    false,
                );
                return None;
            }
        };
        let mut recorded = None;
        // A copy that never diverged from its base has nothing to keep: no
        // candidate, and therefore nothing pinning its workspace against
        // `discard_workspace`. `SealTicket::seal` already
        // applies this; a hand-built snapshot is filtered here.
        let history_left_base = sealed
            .as_ref()
            .is_some_and(|snapshot| snapshot.history_left_base);
        let sealed = sealed.filter(|snapshot| diverged_from_base(snapshot, &base_content));
        if let Some(snapshot) = sealed {
            let resolver = resolver.ok().flatten();
            let producing_attempt_id = attempt_id
                .clone()
                .unwrap_or_else(|| AttemptId::new(&agent_id, 1));
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
            history_left_base,
        );
        recorded
    }

    pub(super) fn seal_history_notice(&self, agent_id: &str, attempt_id: &AttemptId) -> bool {
        self.operations.values().any(|operation| {
            operation.tool_name == SEAL_OUTCOME_KIND
                && operation.caller_agent_id == agent_id
                && serde_json::from_str::<SealOutcome>(&operation.canonical_request).is_ok_and(
                    |outcome| {
                        outcome.attempt_id.as_ref() == Some(attempt_id)
                            && outcome.history_left_base
                            && outcome.failure.is_none()
                    },
                )
        })
    }

    fn record_seal_outcome(
        &mut self,
        agent_id: &str,
        attempt_id: Option<&AttemptId>,
        completed_at_ms: u64,
        failure: Option<&TopologyError>,
        candidate_id: Option<CandidateId>,
        history_left_base: bool,
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
            history_left_base,
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

    #[allow(
        clippy::too_many_lines,
        reason = "seal replay checks workspace, attempt and candidate evidence before consuming exact ownership"
    )]
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

    /// A candidate whose copy was minted on another candidate this caller has
    /// not adopted. Landing it three-ways against that input's base and drops
    /// the input's delta. `None` when there is no such input, or the caller
    /// already adopted it.
    fn stacked_input_blocker(
        &self,
        caller_id: &str,
        candidate: &CandidateRecord,
    ) -> Option<TopologyError> {
        let input = self
            .workspaces
            .get(&candidate.workspace_id)?
            .input_candidate_id
            .as_ref()?;
        let blocker = self.candidates.get(input)?;
        if blocker.adopted_by.as_deref() == Some(caller_id)
            || self.input_was_relinquished(blocker)
            || self.caller_has_input(caller_id, blocker)
        {
            return None;
        }
        let (which, remedy) = match blocker.adopted_by.as_deref() {
            Some(adopter)
                if self
                    .nodes
                    .get(adopter)
                    .is_some_and(|node| self.node_inherits_checkout(node)) =>
            {
                (
                    format!("landed in {adopter}"),
                    format!(
                        "ask {adopter} to adopt {} into its checkout",
                        candidate.candidate_id
                    ),
                )
            }
            Some(adopter) => (
                format!("landed in {adopter}"),
                format!(
                    "adopt {} first (or {adopter}'s candidate)",
                    blocker.candidate_id
                ),
            ),
            None => (
                "is not adopted".to_owned(),
                format!("adopt {} first", blocker.candidate_id),
            ),
        };
        Some(TopologyError::new(
            TopologyErrorCode::AdoptNotApplicable,
            format!(
                "{} was built on {}, which {which}; {remedy}, or the landing would drop {}'s changes; nothing was adopted",
                candidate.candidate_id, blocker.candidate_id, blocker.candidate_id
            ),
        ))
    }

    /// Explicit discard gives the input up. Landing a copy that was minted
    /// over it no longer drops work the caller was still owed.
    pub(super) fn input_was_relinquished(&self, input: &CandidateRecord) -> bool {
        self.workspaces.get(&input.workspace_id).is_some_and(|row| {
            matches!(
                row.status,
                WorkspaceStatus::Discarding | WorkspaceStatus::Discarded
            )
        })
    }

    fn node_inherits_checkout(&self, node: &super::NodeRecord) -> bool {
        self.workspaces.get(&node.workspace_id).is_some_and(|row| {
            row.mode == WorkspaceMode::Inherit || row.owner_agent_id != node.agent_id
        })
    }

    /// Walk immutable carriers, not just one adopter hop. Inherit nodes land
    /// directly in their resolved checkout; a later seal of that checkout or
    /// of the source copy can carry the input onward. Fold orders avoid clocks.
    pub(super) fn candidate_seal_order(&self, candidate: &CandidateId) -> Option<u64> {
        self.operations
            .values()
            .filter(|op| op.tool_name == SEAL_OUTCOME_KIND)
            .filter_map(|op| serde_json::from_str::<SealOutcome>(&op.canonical_request).ok())
            .filter(|outcome| outcome.candidate_id.as_ref() == Some(candidate))
            .map(|outcome| outcome.reserved_at.order)
            .max()
    }

    fn caller_has_input(&self, caller_id: &str, input: &CandidateRecord) -> bool {
        let Some(caller) = self.nodes.get(caller_id) else {
            return false;
        };
        let seals: std::collections::BTreeMap<_, _> = self
            .operations
            .values()
            .filter(|op| op.tool_name == SEAL_OUTCOME_KIND)
            .filter_map(|op| serde_json::from_str::<SealOutcome>(&op.canonical_request).ok())
            .filter(|outcome| outcome.failure.is_none())
            .filter_map(|outcome| Some((outcome.candidate_id?, outcome.reserved_at.order)))
            .collect();
        let mut pending = vec![input];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !visited.insert(&current.candidate_id) {
                continue;
            }
            if current.adopted_by.as_deref() == Some(caller_id) {
                return true;
            }
            let adopter = current
                .adopted_by
                .as_ref()
                .and_then(|id| self.nodes.get(id));
            if adopter
                .is_some_and(|node| node.path == caller.path && self.node_inherits_checkout(node))
            {
                return true;
            }
            let adopted_at = self
                .attempt_adoption_order(&current.producing_agent_id, &current.producing_attempt_id);
            for later in self
                .candidates
                .values()
                .filter(|later| !later.commit.is_empty())
            {
                let Some(order) = seals.get(&later.candidate_id) else {
                    continue;
                };
                let from_source = later.workspace_id == current.workspace_id
                    && seals
                        .get(&current.candidate_id)
                        .is_some_and(|earlier| order > earlier);
                let from_adopter = adopter.is_some_and(|node| {
                    self.nodes
                        .get(&later.producing_agent_id)
                        .is_some_and(|producer| producer.path == node.path)
                        && adopted_at.is_some_and(|adopted| *order > adopted)
                });
                if from_source || from_adopter {
                    pending.push(later);
                }
            }
        }
        false
    }

    /// Authority, sealing, and freshness checks; marks the candidate's
    /// workspace row `Adopting` and queues the ticket the landing runs on.
    /// The receipt comes back from [`Self::complete`].
    #[allow(
        clippy::too_many_lines,
        reason = "adoption validates lineage, conflict authority, occupancy and provenance before reserving one landing"
    )]
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
        if request.revert {
            return self.begin_revert(caller_id, request, &candidate);
        }
        let target = &self.node(caller_id)?.path;
        if (candidate.kind == super::CandidateKind::Conflict || candidate.resolves.is_some())
            && (jj_layout_copy(target) || target.join(".jj").exists())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                "conflict candidates are not yet available for jj",
            ));
        }
        self.check_base_pin_recovery(&candidate.workspace_id)?;
        self.check_adopt_available(&candidate.workspace_id)?;
        if let Some(via) = &candidate.adopted_via
            && (candidate.resolves.is_none() || candidate.adopted_by.is_some())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                self.resolution_refusal(&candidate)
                    .unwrap_or_else(|| format!("that candidate was already adopted via {via}")),
            ));
        }
        if let Some(adopter) = &candidate.adopted_by {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!("that candidate was already adopted by {adopter}"),
            ));
        }
        let source = self.resolution_source(caller_id, &candidate)?;
        let resolution_refusal = self.resolution_refusal(&candidate);
        let resolver_paths = if candidate.resolves.is_some() {
            let workspace = self
                .workspaces
                .get(&candidate.workspace_id)
                .filter(|row| row.resolves == candidate.resolves && !row.conflict_paths.is_empty())
                .ok_or_else(|| {
                    TopologyError::new(
                        TopologyErrorCode::WorkspaceRecoveryRequired,
                        "persisted resolver lineage is missing; nothing was adopted",
                    )
                })?;
            workspace
                .conflict_paths
                .iter()
                .map(|path| path.path.clone())
                .collect()
        } else {
            candidate
                .conflict_paths
                .iter()
                .map(|path| path.path.clone())
                .collect()
        };
        if candidate.kind == super::CandidateKind::Conflict && !request.accept_conflicts {
            let path = self
                .workspaces
                .get(&candidate.workspace_id)
                .map_or(self.launch_checkout.as_path(), |row| row.path.as_path());
            let message = merge_conflict_message_extra(
                &candidate.conflict_paths,
                path,
                &format!(
                    "; conflict candidate {}: spawn a resolver with workspace=fork and inputs=[\"{}\"], or adopt with accept_conflicts=true",
                    candidate.candidate_id, candidate.candidate_id
                ),
            );
            self.record_adopt_conflict(
                candidate.candidate_id.as_str(),
                candidate
                    .conflict_paths
                    .iter()
                    .map(|path| path.path.clone())
                    .collect(),
                Some(candidate.candidate_id.to_string()),
            );
            return Err(TopologyError::new(
                TopologyErrorCode::MergeConflict,
                message,
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
            .filter(|record| {
                record.workspace_id == candidate.workspace_id
                    && record.commit == candidate.base_commit
                    && !self.caller_has_input(caller_id, record)
            })
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
                    "an earlier candidate from that copy ({earlier}) landed in {adopter}; adopt \
                     {adopter}'s candidate instead, or nothing carries both deltas"
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
        if source.is_none()
            && resolution_refusal.is_none()
            && let Some(blocked) = self.stacked_input_blocker(caller_id, &candidate)
        {
            return Err(blocked);
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
                    "{} sealed that candidate on {} but is running a later attempt; adopt after \
                     the node finishes, or the two attempts' copies diverge under you",
                    candidate.producing_agent_id, candidate.producing_attempt_id
                ),
            ));
        }
        // A jj copy minted with `inputs:[<this candidate>]` holds it as its
        // base. Landing the candidate rewrites that base under a child that
        // is still running, and the copy goes stale — recovery checks out a
        // fresh divergent working-copy commit and resets the files with it,
        // so whatever the child had written and not committed is gone before
        // any host code runs. Git does not rewrite the child copy, so a
        // worktree under `.plak/worktrees/` is not a dependent.
        let dependent = self.nodes.values().find(|node| {
            if node.status.is_finished() {
                return false;
            }
            let Some(row) = self.workspaces.get(&node.workspace_id) else {
                return false;
            };
            if row.mode == WorkspaceMode::Inherit || !jj_layout_copy(&row.path) {
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
                    history_left_base: false,
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
        let base = source
            .as_ref()
            .filter(|_| candidate.kind == super::CandidateKind::Conflict)
            .unwrap_or(&candidate);
        let advance_copy_base = !self.reclaimed(&candidate.workspace_id)
            && child.base_commit.as_deref() == Some(base.base_commit.as_str());
        let interrupted_adopt = self.workspace_has_vcs_fault(&candidate.workspace_id)
            || source
                .as_ref()
                .is_some_and(|source| self.workspace_has_vcs_fault(&source.workspace_id));
        let conflict_pin = self.reserve_conflict_pin(&candidate, caller_id);
        let source_copy_agent = source
            .as_ref()
            .filter(|source| {
                !self.reclaimed(&source.workspace_id)
                    && self
                        .workspaces
                        .get(&source.workspace_id)
                        .is_some_and(|row| {
                            row.base_commit.as_deref() == Some(source.base_commit.as_str())
                        })
            })
            .map(|source| source.producing_agent_id.clone());
        if let Some(row) = source
            .as_ref()
            .and_then(|source| self.workspaces.get_mut(&source.workspace_id))
        {
            row.status = WorkspaceStatus::Adopting;
            row.adopter_agent_id = Some(caller_id.to_owned());
        }
        if let Some(row) = self.workspaces.get_mut(&candidate.workspace_id) {
            row.status = WorkspaceStatus::Adopting;
            row.adopter_agent_id = Some(caller_id.to_owned());
        }
        self.inflight_adopts.insert(candidate.workspace_id.clone());
        if let Some(source) = &source {
            self.inflight_adopts.insert(source.workspace_id.clone());
        }
        self.bump();
        self.take_adopt_conflict(candidate.candidate_id.as_str());
        self.effects.adopts.push(AdoptTicket {
            resolution_refusal,
            resolver_paths,
            revert: None,
            accept_conflicts: request.accept_conflicts,
            conflict_pin,
            source,
            source_copy_agent,
            interrupted_adopt,
            advance_copy_base,
            namespace: super::digest_text(&self.tree_id),
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
    #[allow(
        clippy::too_many_lines,
        reason = "adoption completion records success or conflict evidence and releases the exact in-flight ownership"
    )]
    pub fn finish_adopt(
        &mut self,
        ticket: &AdoptTicket,
        landed: Result<AdoptLanding, AdoptFailure>,
    ) -> Result<AdoptOutcome, TopologyError> {
        let workspace_id = ticket.candidate.workspace_id.clone();
        self.inflight_adopts.remove(&workspace_id);
        if let Some(source) = &ticket.source {
            self.inflight_adopts.remove(&source.workspace_id);
        }
        let retain_hold = ticket.interrupted_adopt
            && landed.is_err()
            && !matches!(landed, Err(AdoptFailure::NotApplicable(_)));
        if !retain_hold {
            self.settle_workspace_faults(&workspace_id);
            if let Some(source) = &ticket.source {
                self.settle_workspace_faults(&source.workspace_id);
            }
        }
        let reclaimed = self.reclaimed(&workspace_id);
        match landed {
            Ok(landing) => {
                if ticket.revert.is_some() {
                    return self.finish_revert(ticket, landing);
                }
                self.finish_resolution(ticket, &landing)?;
                self.supersede_resolutions(ticket);
                let accepted_base = if ticket.candidate.kind == super::CandidateKind::Conflict {
                    ticket.source.as_ref().unwrap_or(&ticket.candidate)
                } else {
                    &ticket.candidate
                };
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
                            accepted_base.commit.clone(),
                            accepted_base.content.clone(),
                            accepted_base.change_id.clone(),
                        ),
                    };
                    if ticket.advance_copy_base {
                        row.base_commit = Some(commit);
                        row.base_content = Some(content);
                        // The jj squash abandoned the change the copy was minted
                        // on; the next seal must ask its divergence question
                        // against the change the copy sits on *now*.
                        row.base_change_id = change_id;
                    }
                }
                if let Some(record) = self.candidates.get_mut(&ticket.candidate.candidate_id) {
                    record.adopted_by = Some(ticket.caller_id.clone());
                    record.reverted_at = None;
                    record.reverted_by = None;
                    record.target_before = Some(
                        landing
                            .target_before
                            .split(':')
                            .next()
                            .unwrap_or(&landing.target_before)
                            .to_owned(),
                    );
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
                let (files, files_truncated) = landing.receipt_files();
                Ok(AdoptOutcome {
                    reverted: false,
                    candidate_id: ticket.candidate.candidate_id.clone(),
                    target_before: landing.target_before,
                    target_after: landing.target_after,
                    files_changed: landing.files_changed,
                    files,
                    files_truncated,
                })
            }
            Err(failure) => {
                if let Some(source) = &ticket.source
                    && !retain_hold
                {
                    let reclaimed = self.reclaimed(&source.workspace_id);
                    if let Some(row) = self.workspaces.get_mut(&source.workspace_id) {
                        row.status = if reclaimed {
                            WorkspaceStatus::CleanRemoved
                        } else {
                            WorkspaceStatus::Active
                        };
                        row.adopter_agent_id = None;
                    }
                }
                if let Some(row) = self
                    .workspaces
                    .get_mut(&workspace_id)
                    .filter(|_| !retain_hold)
                {
                    row.status = if reclaimed {
                        WorkspaceStatus::CleanRemoved
                    } else {
                        WorkspaceStatus::Active
                    };
                    row.adopter_agent_id = None;
                }
                self.bump();
                Err(match failure {
                    AdoptFailure::Conflict {
                        paths: conflicts,
                        candidate,
                    } => {
                        let recorded = candidate.and_then(|merge| {
                            self.record_conflict_candidate(ticket, &conflicts, *merge)
                        });
                        let notice = recorded.as_ref().map_or_else(String::new, |id| format!(
                            "; conflict candidate {id} pins the merge onto your current state. Spawn a resolver with workspace=fork and inputs=[\"{id}\"], or adopt with accept_conflicts=true"));
                        let error = TopologyError::new(
                            TopologyErrorCode::MergeConflict,
                            if ticket.revert.is_some() {
                                merge_conflict_message_tail(
                                    &conflicts,
                                    "",
                                    "; reversal conflicts with later edits; nothing was reverted",
                                )
                            } else {
                                merge_conflict_message_extra(
                                    &conflicts,
                                    &ticket.child_path,
                                    &notice,
                                )
                            },
                        );
                        self.record_adopt_conflict(
                            ticket.candidate.candidate_id.as_str(),
                            conflicts
                                .into_iter()
                                .map(|conflict| conflict.path)
                                .collect(),
                            recorded.map(|id| id.to_string()),
                        );
                        error
                    }
                    AdoptFailure::TargetChanged(current) => TopologyError::new(
                        TopologyErrorCode::TargetChanged,
                        format!("the target moved: current is {current}; nothing was applied"),
                    ),
                    AdoptFailure::RecoveryRequired(reason) => {
                        TopologyError::new(TopologyErrorCode::WorkspaceRecoveryRequired, reason)
                    }
                    AdoptFailure::NotApplicable(reason) => {
                        TopologyError::new(TopologyErrorCode::AdoptNotApplicable, reason)
                    }
                    AdoptFailure::Unavailable(reason) if retain_hold => TopologyError::new(
                        TopologyErrorCode::WorkspaceRecoveryRequired,
                        format!(
                            "interrupted adoption remains unresolved; inspect the target before retrying: {reason}"
                        ),
                    ),
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
const CONFLICT_TAIL: &str = " is retained untouched and nothing was applied";
const CONFLICT_MESSAGE_CAP: usize = 512;

/// The `merge_conflict` refusal of an adopt.
///
/// `N` is the full count. Entries are `path (kind)`, de-duplicated in order,
/// cut at whole entries inside the 512-byte wire cap, with `+N more` for what
/// was left out. A child path long enough to blow the cap on its own is
/// shortened; conflict paths are not. The adopting call's facts keep every
/// path, uncut.
#[must_use]
pub fn merge_conflict_message(paths: &[ConflictPath], child_path: &Path) -> String {
    merge_conflict_message_extra(paths, child_path, "")
}

fn merge_conflict_message_extra(paths: &[ConflictPath], child_path: &Path, notice: &str) -> String {
    let count = paths
        .iter()
        .map(|path| (&path.path, &path.kind))
        .collect::<std::collections::HashSet<_>>()
        .len();
    let head = format!("merge_conflict in {count}{CONFLICT_PATHS_LEAD}");
    let marker_room = format!("+{count} more").len();
    let child = capped_child_path(&head, child_path, marker_room + notice.len());
    merge_conflict_message_tail(paths, &child, &format!("{CONFLICT_TAIL}{notice}"))
}

fn merge_conflict_message_tail(paths: &[ConflictPath], child: &str, tail: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let entries: Vec<String> = paths
        .iter()
        .filter_map(|conflict| {
            let entry = format!("{} ({})", conflict.path, conflict.kind);
            seen.insert(entry.clone()).then_some(entry)
        })
        .collect();
    let head = format!("merge_conflict in {}{CONFLICT_PATHS_LEAD}", entries.len());
    let suffix = format!("{CONFLICT_PATHS_END}{child}{tail}");
    let budget = CONFLICT_MESSAGE_CAP.saturating_sub(head.len() + suffix.len());
    let take = entries_that_fit(&entries, budget);
    let omitted = entries.len() - take;
    let mut listed = entries[..take].join(", ");
    if omitted > 0 {
        let marker = format!("+{omitted} more");
        let with = if listed.is_empty() {
            marker
        } else {
            format!("{listed}, {marker}")
        };
        if head.len() + with.len() + suffix.len() <= CONFLICT_MESSAGE_CAP {
            listed = with;
        }
    }
    format!("{head}{listed}{suffix}")
}

/// Shortens only the child path, on a char boundary, when the head and the
/// fixed suffix already exceed the wire cap.
fn capped_child_path(head: &str, child_path: &Path, marker_room: usize) -> String {
    let raw = child_path.display().to_string();
    let fixed = CONFLICT_PATHS_END.len() + CONFLICT_TAIL.len();
    let room = CONFLICT_MESSAGE_CAP.saturating_sub(head.len() + fixed + marker_room);
    if raw.len() <= room {
        return raw;
    }
    let keep = room.saturating_sub(3);
    let mut end = keep.min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &raw[..end])
}

/// Largest prefix whose joined text, plus `+N more` when entries remain,
/// fits in `budget`. A full take can be shorter than the marker alone, so
/// the scan walks down from every entry rather than assuming a bigger
/// prefix costs more. Prefix lengths make each step constant work.
fn entries_that_fit(entries: &[String], budget: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let mut prefix = Vec::with_capacity(entries.len());
    let mut sum = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        sum = sum.saturating_add(entry.len());
        if index > 0 {
            sum = sum.saturating_add(2);
        }
        prefix.push(sum);
    }
    let mut take = entries.len();
    loop {
        let omitted = entries.len() - take;
        let used = if take == 0 {
            format!("+{} more", entries.len()).len()
        } else {
            let more = if omitted == 0 {
                0
            } else {
                format!(", +{omitted} more").len()
            };
            prefix[take - 1].saturating_add(more)
        };
        if used <= budget || take == 0 {
            return take;
        }
        take -= 1;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
