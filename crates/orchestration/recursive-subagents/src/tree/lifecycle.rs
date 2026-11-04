//! Node lifecycle: spawn, followup, interrupt, close, and the per-attempt
//! contract.

mod followup;
mod spawn;
mod turns;

use std::collections::BTreeSet;

use super::{
    AbandonedChildTurn, Arc, AttemptFailure, AttemptId, AttemptRecord, AttemptScopedRequest,
    AttemptStatus, BlockedReason, CandidateId, FollowupRollback, MAX_DIRECT_CHILDREN,
    MAX_PENDING_SPAWNS, MAX_TREE_DEPTH, MAX_TREE_NODES, MintedCopy, NodeDetach, NodePlacement,
    NodeRecord, NodeStatus, ProductAgentTree, SpawnAdmission, SpawnAgentOutput, SpawnAgentRequest,
    SpawnIdentity, SpawnReservation, SubmitNudgeBudget, SubmitResultOutput, SubmitResultRequest,
    TemplateName, TopologyError, TopologyErrorCode, TopologyGrant, TopologyObserver,
    TreeAdmissionState, UsageRow, VcsError, VcsSnapshot, WorkspaceMode, WorkspacePlan,
    WorkspaceReceipt, WorkspaceStatus, admit_output_schema, next_child_id, now_unix_millis,
    topology_error_from_vcs, topology_submit_request_invalid_message, validate_output_json,
};

impl ProductAgentTree {
    /// Interrupts one descendant's running turn. The node stays
    /// alive and re-steerable; only the turn ends.
    ///
    /// # Errors
    ///
    /// Refuses a caller that does not manage the target, an unknown agent,
    /// and a stale attempt id.
    pub fn interrupt_agent(
        &mut self,
        caller_id: &str,
        request: &AttemptScopedRequest,
        operation_id: Option<&str>,
    ) -> Result<NodeStatus, TopologyError> {
        self.interrupt_agent_for(caller_id, request, operation_id, None)
    }

    /// [`Self::interrupt_agent`], recording why: an operator's interrupt
    /// from a face attached to the node records
    /// [`super::InterruptCause::Operator`].
    ///
    /// # Errors
    ///
    /// As [`Self::interrupt_agent`].
    pub fn interrupt_agent_for(
        &mut self,
        caller_id: &str,
        request: &AttemptScopedRequest,
        operation_id: Option<&str>,
        cause: Option<super::InterruptCause>,
    ) -> Result<NodeStatus, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "interrupt_agent", request)? {
            return Ok(prior);
        }
        self.require_expected_attempt(&request.agent_id, request.expected_attempt_id.as_ref())?;
        let managed = {
            let grant = self.grant(caller_id)?;
            grant.managed_ids.iter().any(|id| id == &request.agent_id)
                || grant.interrupt_ids.iter().any(|id| id == &request.agent_id)
        };
        if !managed {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller may not interrupt that node",
            ));
        }
        // A parked child has no turn, but the park is its turn's work:
        // interrupting it ends the park exactly like a running turn. A child
        // ending its attempt after a wake that was not admitted has neither.
        let parked = self.is_parked(&request.agent_id) && !self.is_park_ending(&request.agent_id);
        let owned = self.has_turn_owner(&request.agent_id);
        // A child still owed its recover_node is the lost owner's: its next
        // turn waits on that recovery, which an interrupt ends.
        let awaiting_recovery = self.recovery_owed(&request.agent_id);
        // The terminal observation clears ownership; requesting an
        // interrupt cannot make a still-unwinding copy safe to seal.
        // An interrupt that only cleared `active_turn` left the
        // node reading `running`. The canvas painted `◐ … running`
        // indefinitely, the epoch still counted it non-terminal, and a
        // root inside `wait_agent` was stranded for the whole timeout.
        // `Interrupted` is still non-terminal, so mail and a resume
        // remain legal.
        let attempt_id = self.node(&request.agent_id)?.attempt_id.clone();
        self.record_operator_intent(
            &request.agent_id,
            attempt_id,
            super::operator_intents::Decision::Interrupt {
                effective: owned || parked || awaiting_recovery,
                cause,
            },
        )?;
        let status = self.node(&request.agent_id)?.status;
        if let Some(id) = operation_id {
            self.remember_success(id, caller_id, "interrupt_agent", request, &status)?;
        }
        Ok(status)
    }

    /// Closes one descendant and its subtree, returning every
    /// node the close moved and the status it left it in.
    ///
    /// A close seals what the copy had: closing is not discarding, so work
    /// that diverged survives as a candidate the caller may still adopt.
    ///
    /// # Errors
    ///
    /// Refuses a caller that does not manage the target, an unknown agent,
    /// and a stale attempt id.
    pub fn close_agent(
        &mut self,
        caller_id: &str,
        request: &AttemptScopedRequest,
        operation_id: Option<&str>,
    ) -> Result<Vec<(String, NodeStatus)>, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "close_agent", request)? {
            return Ok(prior);
        }
        self.require_expected_attempt(&request.agent_id, request.expected_attempt_id.as_ref())?;
        let grant = self.grant(caller_id)?;
        let allowed = grant.managed_ids.iter().any(|id| id == &request.agent_id)
            || grant.close_ids.iter().any(|id| id == &request.agent_id);
        if !allowed {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller may not close that node",
            ));
        }
        let mut subtree = Vec::new();
        self.collect_subtree(&request.agent_id, &mut subtree);
        let mut closed = Vec::new();
        let closed_at_ms = now_unix_millis();
        for id in subtree.into_iter().rev() {
            self.fold_due(&id)?;
            if self.node(&id)?.status != NodeStatus::Closed {
                self.record_close_target(&id, closed_at_ms)?;
                self.fold_due(&id)?;
            }
            let held = self
                .current_attempt(&id)
                .is_some_and(AttemptRecord::has_unresolved_session_failure);
            closed.push((
                id,
                if held {
                    NodeStatus::RecoveryRequired
                } else {
                    NodeStatus::Closed
                },
            ));
        }
        if let Some(id) = operation_id {
            self.remember_success(id, caller_id, "close_agent", request, &closed)?;
        }
        Ok(closed)
    }

    /// Records the child's own result against its current attempt.
    /// The payload must satisfy the spawn's output schema, and
    /// the turn token must be the live one.
    ///
    /// A submission is not a seal: the candidate appears when the copy is
    /// sealed, which is why the output may name no candidate.
    ///
    /// # Errors
    ///
    /// Refuses a stale turn token, a payload that fails the schema, and a
    /// Direct child attempts that still own live work, including submitted turns.
    #[must_use]
    pub fn live_child_ids(&self, caller: &str) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| {
                node.parent_agent_id.as_deref() == Some(caller)
                    && self
                        .current_attempt(&node.agent_id)
                        .is_some_and(|attempt| !attempt.status.is_stopped())
            })
            .map(|node| node.agent_id.clone())
            .collect()
    }

    /// Closes live descendants at the owning attempt's terminal boundary.
    /// The host retires their live turns before settling the queued seals.
    ///
    /// # Errors
    /// Refuses if descendant closure cannot be admitted.
    pub fn close_live_descendants(&mut self, agent_id: &str) -> Result<Vec<String>, TopologyError> {
        let closed = self.record_close_live_descendants(agent_id, now_unix_millis())?;
        for target in &closed {
            self.fold_due(target)?;
        }
        Ok(closed)
    }

    pub(super) fn record_close_live_descendants(
        &mut self,
        agent_id: &str,
        closed_at_ms: u64,
    ) -> Result<Vec<String>, TopologyError> {
        let effect_id = format!(
            "plak:descendant-close:{agent_id}:{}",
            self.node(agent_id)?
                .attempt_id
                .as_ref()
                .map_or_else(|| format!("root:{}", self.revision), ToString::to_string,),
        );
        if let Some(effect) = self.operations.get(&effect_id) {
            return serde_json::from_str(&effect.canonical_request).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "descendant close reservation is invalid",
                )
            });
        }
        let mut descendants = Vec::new();
        self.collect_subtree(agent_id, &mut descendants);
        if descendants.into_iter().skip(1).any(|id| {
            self.nodes
                .get(&id)
                .is_some_and(|node| node.fold_order.is_none())
        }) {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "descendant folds must be rebuilt before reserving the parent close cascade; retry recovery in post-order",
            ));
        }
        let mut closed = Vec::new();
        for child in self.live_child_ids(agent_id) {
            let mut subtree = Vec::new();
            self.collect_subtree(&child, &mut subtree);
            for target in subtree.into_iter().rev() {
                self.record_close_target(&target, closed_at_ms)?;
                closed.push(target);
            }
        }
        let canonical = super::canonical_json(&closed)?;
        self.operations.insert(
            effect_id.clone(),
            super::OperationRecord {
                fold: None,
                operation_id: effect_id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: "descendant_close_effect".to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some("effects_reserved".to_owned()),
            },
        );
        self.bump();
        self.pending_close_folds.extend(closed.iter().cloned());
        Ok(closed)
    }

    /// Records a host turn waiter for `agent_id`: the waiter releases or
    /// settles the turns it watches. Process-local; it changes no row and
    /// no revision.
    pub fn enter_turn_waiter(&mut self, agent_id: &str) {
        *self.turn_waiters.entry(agent_id.to_owned()).or_default() += 1;
    }

    /// Reserves live mutation ownership without changing a view or revision.
    pub fn reserve_turn_owner(&mut self, agent_id: &str) {
        let attempt = self
            .current_attempt(agent_id)
            .map(|attempt| attempt.attempt_id.clone());
        self.turn_owners
            .entry(agent_id.to_owned())
            .or_insert((attempt, None));
    }

    /// Transfers a live reservation to the exact turn watched by its owner.
    pub fn bind_turn_owner(&mut self, agent_id: &str, turn_id: String) {
        let attempt = self
            .current_attempt(agent_id)
            .map(|attempt| attempt.attempt_id.clone());
        self.turn_owners
            .insert(agent_id.to_owned(), (attempt, Some(turn_id)));
    }

    /// The live turn, independent of the possibly newer projected binding.
    #[must_use]
    pub fn owned_turn_id(&self, agent_id: &str) -> Option<&str> {
        self.turn_owners
            .get(agent_id)
            .filter(|(attempt, _)| {
                attempt.as_ref()
                    == self
                        .current_attempt(agent_id)
                        .map(|attempt| &attempt.attempt_id)
            })
            .and_then(|(_, turn)| turn.as_deref())
    }

    /// A prior attempt's retirement fences close, but cannot receive mail
    /// addressed to a newly admitted followup attempt.
    #[must_use]
    pub fn has_current_turn_owner(&self, agent_id: &str) -> bool {
        self.turn_owners.get(agent_id).is_some_and(|(attempt, _)| {
            attempt.as_ref()
                == self
                    .current_attempt(agent_id)
                    .map(|attempt| &attempt.attempt_id)
        }) || self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.active_turn)
    }

    /// Releases a startup or waiter reservation once mutation has stopped.
    pub fn release_turn_owner(&mut self, agent_id: &str) {
        self.turn_owners.remove(agent_id);
    }

    /// Whether this child's bound or active turn still waits for its
    /// `recover_node`: until then it takes no new turn.
    #[must_use]
    pub fn recovery_owed(&self, agent_id: &str) -> bool {
        self.recovery_owed.contains(agent_id)
    }

    /// The children still owed a `recover_node`, descendants first.
    #[must_use]
    pub fn recovery_owed_targets(&self) -> Vec<String> {
        let mut ordered = Vec::new();
        self.preorder(self.root_agent_id.as_str(), &mut ordered);
        ordered
            .into_iter()
            .rev()
            .filter(|id| self.recovery_owed.contains(id))
            .collect()
    }

    /// `recover_node` ran for this child: it takes turns again.
    pub fn recovery_settled(&mut self, agent_id: &str) {
        if self.recovery_owed.remove(agent_id) {
            self.notify.notify_waiters();
        }
    }

    /// Live ownership fences interrupt and close; after restart the projected
    /// active-turn view is the only remaining evidence.
    #[must_use]
    pub fn has_turn_owner(&self, agent_id: &str) -> bool {
        self.turn_owners.contains_key(agent_id)
            || self
                .nodes
                .get(agent_id)
                .is_some_and(|node| node.active_turn)
    }

    /// Ends one [`Self::enter_turn_waiter`] registration.
    pub fn leave_turn_waiter(&mut self, agent_id: &str) {
        if let Some(count) = self.turn_waiters.get_mut(agent_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.turn_waiters.remove(agent_id);
            }
        }
    }

    /// Whether a host turn waiter for `agent_id` is still running, so a turn
    /// bound to its attempt will still be released or settled by it.
    #[must_use]
    pub fn has_turn_waiter(&self, agent_id: &str) -> bool {
        self.turn_waiters.contains_key(agent_id)
    }

    /// Children that had a started turn when the host crashed (`load_tree` pauses).
    pub fn inflight_join_targets(&self) -> Vec<(String, String, String, String)> {
        self.nodes
            .values()
            .filter(|node| node.agent_id != self.root_agent_id)
            .filter(|node| !node.status.is_finished())
            .filter_map(|node| {
                let locator = node.session_locator.clone()?;
                let attempt = self.current_attempt(&node.agent_id)?;
                if attempt.status.is_stopped() {
                    return None;
                }
                Some((
                    node.agent_id.clone(),
                    locator,
                    node.path.display().to_string(),
                    attempt.turn_id.clone()?,
                ))
            })
            .collect()
    }

    /// Reports one node detach per bound non-root node lifetime.
    pub fn try_emit_tree_node_detached(
        &mut self,
        observer: Option<&Arc<dyn TopologyObserver>>,
        agent_id: &str,
        status: NodeDetach,
    ) {
        if agent_id == self.root_agent_id {
            return;
        }
        if self.detach_emitted.contains(agent_id) {
            return;
        }
        let node = match self.nodes.get(agent_id) {
            Some(node) => node,
            None => return,
        };
        if node.session_locator.is_none() && status != NodeDetach::RecoveryRequired {
            return;
        }
        let parent_node_id = node.parent_agent_id.clone();
        // Dedupe even when there is no observer: the runtime-scoped observer is
        // fixed for the tree's life.
        if !self.detach_emitted.insert(agent_id.to_owned()) {
            return;
        }
        if let Some(observer) = observer {
            observer.node_detached(&self.tree_id, agent_id, parent_node_id.as_deref(), status);
        }
    }

    /// Emits detach for bound non-root nodes that are still non-terminal when the hub shuts down.
    pub fn emit_shutdown_detaches(&mut self, observer: Option<&Arc<dyn TopologyObserver>>) {
        let ids: Vec<String> = self
            .nodes
            .values()
            .filter(|node| {
                node.agent_id != self.root_agent_id
                    && node.session_locator.is_some()
                    && !node.status.is_finished()
            })
            .map(|node| node.agent_id.clone())
            .collect();
        for agent_id in ids {
            self.try_emit_tree_node_detached(observer, &agent_id, NodeDetach::Shutdown);
        }
    }

    /// Closes every descendant at once (the root's own stop), returning each
    /// node and the status it was left in.
    ///
    /// # Errors
    ///
    /// Refuses any caller but the root.
    pub fn stop_all(
        &mut self,
        caller_id: &str,
        operation_id: Option<&str>,
    ) -> Result<Vec<(String, NodeStatus)>, TopologyError> {
        if caller_id != self.root_agent_id {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "stop-all is a root host command",
            ));
        }
        let targets: Vec<String> = self
            .nodes
            .values()
            .filter(|node| {
                node.agent_id != self.root_agent_id
                    && (!node.status.is_finished() || node.active_turn)
            })
            .map(|node| node.agent_id.clone())
            .collect();
        let mut closed: Vec<(String, NodeStatus)> = Vec::new();
        for agent_id in targets {
            if closed.iter().any(|(id, _)| id == &agent_id) {
                continue;
            }
            if self
                .node(&agent_id)
                .is_ok_and(|node| node.status.is_finished() && !node.active_turn)
            {
                continue;
            }
            let close_operation = operation_id.map(|id| format!("{id}:{agent_id}"));
            let batch = self.close_agent(
                caller_id,
                &AttemptScopedRequest {
                    agent_id: agent_id.clone(),
                    expected_attempt_id: None,
                },
                close_operation.as_deref(),
            )?;
            closed.extend(batch);
        }
        self.refresh_epoch_after_close();
        if self.state == TreeAdmissionState::Live && self.quiescence_refusal().is_none() {
            self.state = TreeAdmissionState::TreeTerminal;
            self.bump();
        }
        Ok(closed)
    }
}

impl ProductAgentTree {
    /// The spawn receipt as the tree holds it now.
    pub(super) fn spawn_receipt(&self, agent_id: &str) -> Result<SpawnAgentOutput, TopologyError> {
        let node = self.node(agent_id)?;
        let workspace = self.workspaces.get(&node.workspace_id);
        Ok(SpawnAgentOutput {
            agent_id: node.agent_id.clone(),
            attempt_id: node
                .attempt_id
                .clone()
                .unwrap_or_else(|| AttemptId::new(agent_id, 1)),
            status: node.status,
            tier: node.tier,
            workspace: WorkspaceReceipt {
                id: node.workspace_id.clone(),
                mode: workspace.map_or(WorkspaceMode::Inherit, |row| row.mode),
                base: workspace.and_then(|row| row.base_commit.clone()),
            },
        })
    }
}

impl ProductAgentTree {
    /// Resolves a spawn's `inputs` to the base its copy is minted over.
    ///
    ///
    /// One entry is admitted: a copy has one merge base, and two candidates
    /// would be a merge the spawn is not authorized to perform — that is what
    /// `adopt_candidate` is for. Inherit has already been refused by
    /// `validate`, since a candidate can only be given a minted copy. An id
    /// that is not a sealed candidate is `input_not_sealed`, not
    /// `candidate_unknown`: an attempt id is a legal thing to name and simply
    /// has not sealed.
    pub(super) fn resolve_input_base(
        &self,
        inputs: &[String],
    ) -> Result<Option<VcsSnapshot>, TopologyError> {
        let Some(first) = inputs.first() else {
            return Ok(None);
        };
        if inputs.len() > 1 {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "a copy is minted over one base: name a single candidate input and adopt the \
                 rest into it",
            ));
        }
        let candidate = self
            .candidates
            .get(&CandidateId::from_persisted(first.clone()))
            .ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::InputNotSealed,
                format!(
                    "input {first} is not a sealed candidate; wait for the producing attempt to \
                     seal and pass the candidate id from its wait row"
                ),
            )
        })?;
        if candidate.kind == super::CandidateKind::Conflict
            && let Some(reason) = self.resolution_refusal(candidate)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                reason,
            ));
        }
        // Adopted records retain their pins after copy removal. Only an
        // explicit discard of unadopted work relinquishes this input.
        if candidate.adopted_by.is_none()
            && self
                .workspaces
                .get(&candidate.workspace_id)
                .is_some_and(|row| {
                    !self.reclaimed(&row.workspace_id)
                        && matches!(
                            row.status,
                            WorkspaceStatus::Discarding
                                | WorkspaceStatus::Discarded
                                | WorkspaceStatus::CleanRemoved
                        )
                })
        {
            return Err(TopologyError::new(
                TopologyErrorCode::InputNotSealed,
                format!(
                    "input {first} was explicitly discarded; its snapshot is no longer retained"
                ),
            ));
        }
        Ok(Some(VcsSnapshot {
            history_left_base: false,
            commit: candidate.commit.clone(),
            content: candidate.content.clone(),
            change_id: candidate.change_id.clone(),
        }))
    }

    /// [`Self::submit_result`] with remedy names mapped through the caller's
    /// vocabulary. `present` is the identity for the Plak names.
    pub fn submit_result_presented(
        &mut self,
        caller_id: &str,
        request: &SubmitResultRequest,
        operation_id: Option<&str>,
        present: impl Fn(&'static str) -> &'static str,
    ) -> Result<SubmitResultOutput, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "submit_result", request)? {
            return Ok(prior);
        }
        request
            .validate(caller_id == self.root_agent_id)
            .map_err(|code| {
                TopologyError::new(code, topology_submit_request_invalid_message(code, request))
            })?;
        let running = self.live_child_ids(caller_id);
        if !running.is_empty() {
            let remedies: Vec<_> = self
                .compiled_tool_names(caller_id)?
                .into_iter()
                .filter(|name| matches!(*name, "wait_agent" | "close_agent" | "interrupt_agent"))
                .map(present)
                .collect();
            return Err(TopologyError::new(
                TopologyErrorCode::LiveWork,
                format!(
                    "live work: children {}; finish them with {} before submitting",
                    running.join(", "),
                    remedies.join(", ")
                ),
            ));
        }
        let attempt = self.current_attempt(caller_id).ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller has no delegated attempt",
            )
        })?;
        if request.turn_token != attempt.turn_token {
            return Err(TopologyError::new(
                TopologyErrorCode::SubmitStaleToken,
                format!(
                    "turn token is not the current attempt token; expected {}",
                    attempt.turn_token
                ),
            ));
        }
        if attempt.status.is_stopped() {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientAttemptTerminal,
                "attempt is already terminal",
            ));
        }
        if let Some(prior) = attempt.output_json.as_ref() {
            if prior == &request.output_json {
                let output = SubmitResultOutput {
                    agent_id: caller_id.to_owned(),
                    attempt: attempt.status,
                    candidate: attempt.candidate_id.clone(),
                };
                if let Some(id) = operation_id {
                    self.remember_success(id, caller_id, "submit_result", request, &output)?;
                }
                return Ok(output);
            }
            return Err(TopologyError::new(
                TopologyErrorCode::SubmitDuplicateConflict,
                "attempt already submitted a different output",
            ));
        }
        let schema_json = self.node(caller_id)?.output_schema_json.clone();
        validate_output_json(&schema_json, &request.output_json)?;
        let attempt = self.current_attempt(caller_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
        })?;
        let candidate = attempt.candidate_id.clone();
        self.record_operator_intent(
            caller_id,
            Some(attempt.attempt_id.clone()),
            super::operator_intents::Decision::Submitted {
                output_json: request.output_json.clone(),
            },
        )?;
        let output = SubmitResultOutput {
            agent_id: caller_id.to_owned(),
            attempt: AttemptStatus::Submitted,
            candidate,
        };
        if let Some(id) = operation_id {
            self.remember_success(id, caller_id, "submit_result", request, &output)?;
        }
        Ok(output)
    }

    pub(super) fn record_attempt_completed_at(
        &mut self,
        agent_id: &str,
        attempt_id: &AttemptId,
        ended_at: u64,
        reserve_effects: bool,
        recorded_turns_spent: Option<bool>,
    ) -> Result<AttemptStatus, TopologyError> {
        let current = self.node(agent_id)?.attempt_id.as_ref() == Some(attempt_id);
        let reserve_effects = reserve_effects && current;
        let node_status = self.node(agent_id)?.status;
        if current && matches!(node_status, NodeStatus::Closed) {
            return Ok(self
                .current_attempt(agent_id)
                .map(|attempt| attempt.status)
                .unwrap_or(AttemptStatus::Cancelled));
        }
        if current && node_status == NodeStatus::RecoveryRequired {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientRecoveryRequired,
                "completed turn cannot clobber recovery required",
            ));
        }
        if current {
            self.retire_wait_receipts(agent_id);
        }
        // Completion is idempotent even after acknowledged result history
        // has been pruned: terminal attempts cannot push another result.
        if let Some(attempt) = self.attempts.get(attempt_id)
            && attempt.status.is_stopped()
        {
            return Ok(attempt.status);
        }
        // the template's `max_turns` is spent on this attempt.
        // It ends `blocked` with whatever partial output it has rather than
        // being handed another round by the submit harness or a steer.
        if reserve_effects {
            self.record_close_live_descendants(agent_id, ended_at)?;
        }
        let turns_spent = recorded_turns_spent.unwrap_or_else(|| {
            self.attempt_turn_ceiling(agent_id).is_some_and(|ceiling| {
                self.attempts
                    .get(attempt_id)
                    .is_some_and(|attempt| attempt.usage.turns >= ceiling)
            })
        });
        let account = self.node(agent_id)?.account.clone();
        let status = {
            let attempt = self.attempts.get_mut(attempt_id).ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
            })?;
            attempt.usage.elapsed_ms = ended_at
                .saturating_sub(attempt.started_at)
                .max(attempt.usage.elapsed_ms);
            attempt.status = if attempt.output_json.is_some() {
                AttemptStatus::Completed
            } else if turns_spent {
                attempt.blocked = Some(BlockedReason {
                    quota: "max_turns".to_owned(),
                    account,
                    resets_at: None,
                });
                AttemptStatus::Blocked
            } else {
                AttemptStatus::CompletedWithoutSubmission
            };
            attempt.ended_at = Some(ended_at);
            attempt.status
        };
        let mutate_workspace = self.nodes.get(agent_id).and_then(|node| {
            node.tier
                .allows_file_writes()
                .then(|| node.workspace_id.clone())
        });
        if current && let Some(node) = self.nodes.get_mut(agent_id) {
            node.active_turn = false;
            if reserve_effects || node_status != NodeStatus::Sealing {
                node.status = if status == AttemptStatus::Completed {
                    NodeStatus::Completed
                } else {
                    NodeStatus::Failed
                };
            }
        }
        if reserve_effects && let Some(workspace_id) = mutate_workspace {
            self.bump_workspace_generation(&workspace_id);
        }
        // Forked copies seal at turn completion so the parent's `result` mail
        // can name the candidate. Inherit seals nothing; completion pushes
        // from here instead.
        if reserve_effects && !self.begin_seal(agent_id) {
            self.push_result_unless_sealing(agent_id);
        }
        self.bump();
        Ok(status)
    }

    /// A provider quota refusal ends this attempt.
    ///
    /// The **attempt** carries `blocked { quota, account, resets_at? }` and the
    /// **node** goes `Failed` — a blocked attempt has no node status of its own,
    /// and the TUI reads the attempt for `resets_at`. Partial work still seals
    /// through the slice-0 seam. Nothing auto-retries, and no other account's
    /// quota is spent without the user saying so.
    #[cfg(any(test, feature = "test-support"))]
    pub fn record_attempt_blocked(
        &mut self,
        agent_id: &str,
        reason: BlockedReason,
    ) -> Result<(), TopologyError> {
        if self.node(agent_id)?.status == NodeStatus::Closed {
            return Ok(());
        }
        if self
            .current_attempt(agent_id)
            .is_some_and(|attempt| attempt.status.is_stopped())
        {
            return Ok(());
        }
        self.record_host_turn_failure(
            agent_id,
            super::turn_outcome::FailedTurnDisposition::Blocked(reason),
            String::new(),
            Vec::new(),
        )?;
        self.fold_due(agent_id)?;
        self.fold_reserved_closes()?;
        Ok(())
    }

    /// Records the authoritative terminal failure without discarding submitted
    /// output or an existing candidate. An unresolved outcome forbids a new seal.
    pub fn record_turn_failure(
        &mut self,
        agent_id: &str,
        disposition: super::turn_outcome::FailedTurnDisposition,
        message: String,
    ) -> Result<(), TopologyError> {
        let attempt_id = self.node(agent_id)?.attempt_id.clone();
        self.record_attempt_failure(
            agent_id,
            attempt_id.as_ref(),
            disposition,
            message,
            true,
            now_unix_millis(),
        )?;
        self.fold_reserved_closes()?;
        Ok(())
    }

    pub(super) fn record_attempt_failure(
        &mut self,
        agent_id: &str,
        attempt_id: Option<&AttemptId>,
        disposition: super::turn_outcome::FailedTurnDisposition,
        message: String,
        reserve_effects: bool,
        observed_at_ms: u64,
    ) -> Result<(), TopologyError> {
        use super::turn_outcome::FailedTurnDisposition;
        let current = self.node(agent_id)?.attempt_id.as_ref() == attempt_id;
        if attempt_id
            .and_then(|id| self.attempts.get(id))
            .is_some_and(AttemptRecord::settled_by_abandonment)
        {
            return Ok(());
        }
        let closed = (current && self.node(agent_id)?.status == NodeStatus::Closed)
            || attempt_id
                .and_then(|id| self.attempts.get(id))
                .is_some_and(|attempt| attempt.status == AttemptStatus::Closed);
        if let Some(prior) = attempt_id
            .and_then(|id| self.attempts.get(id))
            .filter(|attempt| attempt.has_unresolved_session_failure())
            .and_then(|attempt| attempt.failure.clone())
        {
            if current && let Some(node) = self.nodes.get_mut(agent_id) {
                node.active_turn = false;
                node.status = NodeStatus::RecoveryRequired;
            }
            let combined = format!("{}; {message}", prior.message);
            if let Some(attempt) = attempt_id.and_then(|id| self.attempts.get_mut(id)) {
                attempt.status = if closed {
                    AttemptStatus::Closed
                } else {
                    AttemptStatus::RecoveryRequired
                };
                attempt.failure = Some(AttemptFailure {
                    kind: "turn_recovery_required".into(),
                    message: combined.clone(),
                });
            }
            if current {
                self.record_fault(agent_id, combined);
            }
            if current && !closed && reserve_effects {
                self.push_result_unless_sealing(agent_id);
            }
            return Ok(());
        }
        let unresolved = matches!(disposition, FailedTurnDisposition::RecoveryRequired);
        let (node_status, attempt_status, kind) = match &disposition {
            FailedTurnDisposition::Failed { kind } => {
                (NodeStatus::Failed, AttemptStatus::Failed, *kind)
            }
            FailedTurnDisposition::Blocked(_) => {
                (NodeStatus::Failed, AttemptStatus::Blocked, "quota")
            }
            FailedTurnDisposition::RecoveryRequired => (
                NodeStatus::RecoveryRequired,
                AttemptStatus::RecoveryRequired,
                "turn_recovery_required",
            ),
        };
        if current {
            let node = self.nodes.get_mut(agent_id).ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
            })?;
            node.active_turn = false;
            if unresolved || node.status != NodeStatus::Closed {
                node.status = node_status;
            }
        }
        if let Some(attempt) = attempt_id.and_then(|id| self.attempts.get_mut(id)) {
            attempt.status = if closed && unresolved {
                AttemptStatus::Closed
            } else {
                attempt_status
            };
            let ended_at = *attempt.ended_at.get_or_insert(observed_at_ms);
            attempt.usage.elapsed_ms = ended_at
                .saturating_sub(attempt.started_at)
                .max(attempt.usage.elapsed_ms);
            if let FailedTurnDisposition::Blocked(reason) = disposition {
                attempt.blocked = Some(reason);
                attempt.failure = None;
            } else {
                attempt.failure = Some(AttemptFailure {
                    kind: kind.into(),
                    message: message.clone(),
                });
            }
        }
        if current {
            self.record_fault(agent_id, message);
        }
        if current && !unresolved && reserve_effects {
            self.record_close_live_descendants(agent_id, observed_at_ms)?;
            self.begin_seal(agent_id);
        }
        if current && (!unresolved || !closed) && reserve_effects {
            self.push_result_unless_sealing(agent_id);
        }
        Ok(())
    }
}

impl ProductAgentTree {
    /// Records that one node's turn ended, moving its attempt to the terminal
    /// status the turn earned — completed, or completed without a submission.
    ///
    /// Queues the copy's seal, so a node that diverged leaves a candidate
    /// whether or not it remembered to submit.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent.
    #[cfg(any(test, feature = "test-support"))]
    pub fn record_turn_completed(
        &mut self,
        agent_id: &str,
    ) -> Result<AttemptStatus, TopologyError> {
        if self.node(agent_id)?.status == NodeStatus::Closed {
            return Ok(self
                .current_attempt(agent_id)
                .map_or(AttemptStatus::Cancelled, |attempt| attempt.status));
        }
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
        })?;
        if self.node(agent_id)?.status == NodeStatus::RecoveryRequired {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientRecoveryRequired,
                "completed turn cannot clobber recovery required",
            ));
        }
        if attempt.status.is_stopped() {
            return Ok(attempt.status);
        }
        let turn_id = attempt
            .turn_id
            .clone()
            .unwrap_or_else(|| "test-only-terminal".to_owned());
        let terminal = super::ChildTerminal {
            attempt_id: attempt.attempt_id.clone(),
            turn_id: turn_id.clone(),
            committed_at_ms: now_unix_millis(),
            seq: self.fold_anchor_now(agent_id)?.seq,
            outcome: plakous::sessions::feed::FeedTurnOutcome::Completed {
                finish: plakous::model::FinishReason::Stop,
            },
        };
        self.record_child_finalization(
            agent_id,
            &turn_id,
            super::ChildFinalizationDecision::Finalize,
            true,
            Some(terminal),
        )?;
        self.fold_due(agent_id)?;
        self.fold_reserved_closes()?;
        Ok(self
            .current_attempt(agent_id)
            .map_or(AttemptStatus::Cancelled, |attempt| attempt.status))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
