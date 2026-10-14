//! Node lifecycle: spawn, followup, interrupt, close, and the per-attempt
//! contract.

use std::collections::BTreeSet;

use super::{
    AbandonedChildTurn, Arc, AttemptFailure, AttemptId, AttemptRecord, AttemptScopedRequest,
    AttemptStatus, BlockedReason, CandidateId, FollowupRollback, MAX_DIRECT_CHILDREN,
    MAX_PENDING_SPAWNS, MAX_RUNNING_CHILDREN, MAX_TREE_DEPTH, MAX_TREE_NODES, MintedCopy,
    NodeDetach, NodeRecord, NodeStatus, ProductAgentTree, SpawnAdmission, SpawnAgentOutput,
    SpawnAgentRequest, SpawnIdentity, SpawnReservation, SubmitNudgeBudget, SubmitResultOutput,
    SubmitResultRequest, TemplateName, TopologyError, TopologyErrorCode, TopologyGrant,
    TopologyObserver, TreeAdmissionState, UsageRow, VcsSnapshot, WorkspaceMode, WorkspacePlan,
    WorkspaceReceipt, WorkspaceStatus, admit_output_schema, next_child_id, now_unix_millis,
    topology_error_from_vcs, topology_submit_request_invalid_message, validate_output_json,
};

impl ProductAgentTree {
    /// Spawns a child inline: reserve, run the mint the reservation owes
    /// through the tree's own port, commit. The in-process convenience for
    /// the tree's own tests and for `inherit` spawns, which mint nothing.
    ///
    /// Production drives [`Self::spawn_reserve`] / [`Self::spawn_commit`]
    /// through the runtime so the mint runs with the tree mutex released.
    ///
    pub fn spawn_agent(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
    ) -> Result<SpawnAgentOutput, TopologyError> {
        match self.spawn_reserve(caller_id, request, operation_id)? {
            SpawnAdmission::Replayed(output) => Ok(output),
            SpawnAdmission::Reserved(reservation, None) => self.spawn_commit(*reservation, None),
            SpawnAdmission::Reserved(reservation, Some(ticket)) => match ticket.mint(self.vcs()) {
                Ok(minted) => self.spawn_commit(*reservation, Some(minted)),
                Err(error) => Err(self.spawn_abort(*reservation, topology_error_from_vcs(error))),
            },
        }
    }

    /// The admission half of a spawn: every budget,
    /// grant, template, and schema check, the child id, and the workspace
    /// plan — with no subprocess. A copying tier comes back as a
    /// [`MintTicket`](super::MintTicket) the caller runs with the mutex
    /// released; the reserved id is held in the tree's `minting` set until
    /// [`Self::spawn_commit`] or [`Self::spawn_abort`] gives it back, so a
    /// second spawn racing this one cannot mint over the same copy.
    pub fn spawn_reserve(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
    ) -> Result<SpawnAdmission, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "spawn_agent", request)? {
            return Ok(SpawnAdmission::Replayed(prior));
        }
        // the `call_id` digest above dedups retries of one call;
        // `request_key` dedups two independent calls, which it cannot.
        if let Some(receipt) = self.replay_request_key(caller_id, request)? {
            return Ok(SpawnAdmission::Replayed(receipt));
        }
        self.require_live()?;
        request.validate().map_err(|code| {
            TopologyError::new(
                code,
                "spawn request shape is invalid: prompt and output_schema_json are required, \
                 inputs holds at most 8 non-empty ids, and inherit takes no inputs",
            )
        })?;
        self.bound_text(&request.description)?;
        self.bound_text(&request.prompt)?;
        self.bound_text(&request.output_schema_json)?;
        let caller_depth = self.node(caller_id)?.depth;
        let grant = self.grant(caller_id)?.clone();
        if !grant.may_spawn {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller may not spawn",
            ));
        }
        if caller_depth >= grant.max_depth || caller_depth >= MAX_TREE_DEPTH {
            return Err(TopologyError::new(
                TopologyErrorCode::DepthExhausted,
                "spawn would exceed grant or tree depth",
            ));
        }
        // Dormancy derives from settlement and delivery. Unresolved recovery
        // still reserves tree capacity even though it cannot execute.
        let live_nodes = self
            .nodes
            .values()
            .filter(|node| self.holds_tree_slot(node))
            .count();
        let node_ceiling = MAX_TREE_NODES.min(usize::from(grant.max_nodes));
        if live_nodes >= node_ceiling {
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                format!(
                    "tree node budget is exhausted ({live_nodes} of {node_ceiling} live, root \
                     included); finished children release slots after result delivery"
                ),
            ));
        }
        // Settled, delivered children release direct slots without closing
        // their nodes or discarding their still-adoptable candidates.
        // Recovery-required nodes run nothing and likewise hold no direct slot.
        let children: Vec<&NodeRecord> = self
            .nodes
            .values()
            .filter(|node| {
                node.parent_agent_id.as_deref() == Some(caller_id)
                    && self.holds_direct_child_slot(node)
            })
            .collect();
        let child_ceiling = MAX_DIRECT_CHILDREN.min(usize::from(grant.max_children));
        if children.len() >= child_ceiling {
            // Checked before running capacity. Undelivered finished children
            // still hold slots. The refusal names the
            // set the same way `capacity` does so the caller can pick what to
            // close.
            let listed: Vec<String> = children
                .iter()
                .map(|node| {
                    format!(
                        "{} ({})",
                        node.agent_id,
                        serde_json::to_value(node.status)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    )
                })
                .collect();
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                format!(
                    "direct child budget is exhausted ({} of {child_ceiling}); settled children \
                     hold slots until their result is delivered (or they are closed). children: {}",
                    children.len(),
                    listed.join(", ")
                ),
            ));
        }
        // Saturation is a typed error carrying the running set, not a queue
        // and not a flat budget string. The caller decides what to interrupt
        // or wait for from the ids in the message. The bound counts executing
        // children only: the root's own turn never consumes a child slot.
        let running = self.running_child_ids();
        if running.len() >= MAX_RUNNING_CHILDREN {
            return Err(TopologyError::new(
                TopologyErrorCode::Capacity,
                format!(
                    "running capacity is full ({} of {MAX_RUNNING_CHILDREN} children); there is \
                     no queue. running: {}",
                    running.len(),
                    running.join(", ")
                ),
            ));
        }
        if usize::try_from(self.pending_spawns).unwrap_or(usize::MAX) >= MAX_PENDING_SPAWNS {
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                "pending spawn budget is exhausted",
            ));
        }
        if self.attempt_budget_exhausted() {
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                format!(
                    "attempt budget for this prompt is exhausted ({} of {})",
                    self.attempts_this_prompt(),
                    self.max_attempts
                ),
            ));
        }
        let (output_schema_json, schema_digest) = admit_output_schema(&request.output_schema_json)?;
        let caller_template = self
            .node(caller_id)?
            .template
            .as_ref()
            .map(TemplateName::as_str);
        // Menu before account and schema checks, so an off-menu card is
        // `template_not_spawnable` even when its account would not bind.
        // A name the epoch does not carry stays `template_unknown`.
        if self.snapshot.binding(&request.template).is_none() {
            return Err(TopologyError::new(
                TopologyErrorCode::TemplateUnknown,
                format!(
                    "template {} is not in this tree epoch's roster",
                    request.template
                ),
            ));
        }
        if !self
            .snapshot
            .menu_contains(caller_template, &request.template)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::TemplateNotSpawnable,
                format!(
                    "template {} is not on this node's spawn menu: the root spawns the working \
                     set, every other node its own template's spawns list minus itself",
                    request.template
                ),
            ));
        }
        let (binding, account, tuple_id, effort) =
            self.resolve_template_binding(&request.template)?;
        // above the ceiling lands at the ceiling; not an error.
        let tier = request.tier.clipped_to(binding.max_tier);
        // an `inputs` entry names the sealed candidate the child
        // starts from, instead of its parent's live state.
        let input_base = self.resolve_input_base(request.input_ids())?;

        self.pending_spawns = self.pending_spawns.saturating_add(1);
        let agent_id = next_child_id(
            self.collected_child_floor(),
            &self.tree_id,
            &self.nodes,
            &self.workspaces,
            &self.minting,
            &self.launch_checkout,
        );
        let plan = match self.plan_workspace(
            caller_id,
            tier,
            request.workspace,
            &agent_id,
            input_base.as_ref(),
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.pending_spawns = self.pending_spawns.saturating_sub(1);
                if let Some(id) = operation_id
                    && error.code == TopologyErrorCode::MintUnavailable
                {
                    self.remember_failure(id, caller_id, "spawn_agent", request, error.code)?;
                }
                return Err(error);
            }
        };
        let child_depth = caller_depth.saturating_add(1);
        let mut child_grant = TopologyGrant {
            visible_ids: vec![caller_id.to_owned(), agent_id.clone()],
            max_depth: grant.max_depth,
            max_nodes: grant.max_nodes,
            max_children: grant.max_children,
            ..TopologyGrant::default()
        };
        let mut parent_grant = grant.clone();
        TopologyGrant::apply_spawn(&mut parent_grant, &mut child_grant, caller_id, &agent_id);
        // Delegation authority is the child template's own menu, bounded by
        // depth. Adoption presumes a subtree, so it follows the grant's
        // may_spawn.
        child_grant.may_spawn = self
            .snapshot
            .spawnable_template_names(Some(request.template.as_str()))
            .next()
            .is_some()
            && child_depth < grant.max_depth;
        child_grant.may_adopt = binding.may_adopt && child_grant.may_spawn;
        child_grant.visible_ids = vec![caller_id.to_owned(), agent_id.clone()];
        let attempt_id = AttemptId::new(&agent_id, 1);
        let (bound, ticket, mode, caller_path) = match plan {
            WorkspacePlan::Inherit(bound) => {
                let mode = bound.mode;
                let caller_path = bound.path.clone();
                (Some(bound), None, mode, caller_path)
            }
            WorkspacePlan::Mint(ticket) => {
                let mode = ticket.mode();
                let caller_path = ticket.caller_path.clone();
                self.minting.insert(agent_id.clone());
                (None, Some(ticket), mode, caller_path)
            }
        };
        Ok(SpawnAdmission::Reserved(
            Box::new(SpawnReservation {
                caller_id: caller_id.to_owned(),
                caller_path,
                request: request.clone(),
                operation_id: operation_id.map(str::to_owned),
                agent_id,
                attempt_id,
                child_depth,
                tier,
                account,
                tuple_id,
                effort,
                output_schema_json,
                schema_digest,
                template: request.template.clone(),
                mode,
                parent_grant,
                child_grant,
                bound,
            }),
            ticket,
        ))
    }

    /// Writes the reserved spawn's rows once its mint has answered.
    /// `minted` is `None` exactly when the reservation
    /// carried no ticket — an `inherit` bind.
    pub fn spawn_commit(
        &mut self,
        reservation: SpawnReservation,
        minted: Option<MintedCopy>,
    ) -> Result<SpawnAgentOutput, TopologyError> {
        let SpawnReservation {
            caller_id,
            caller_path,
            request,
            operation_id,
            agent_id,
            attempt_id,
            child_depth,
            tier,
            account,
            tuple_id,
            effort,
            output_schema_json,
            schema_digest,
            template,
            mode,
            parent_grant,
            child_grant,
            bound,
        } = reservation;
        self.minting.remove(&agent_id);
        let bound = match (bound, minted) {
            (Some(bound), _) => bound,
            (None, Some(minted)) => self.record_minted(&caller_id, &agent_id, mode, minted),
            (None, None) => {
                self.pending_spawns = self.pending_spawns.saturating_sub(1);
                return Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "spawn reservation was committed without the copy its mint owed",
                ));
            }
        };
        let spawn_identity = request
            .request_key
            .as_ref()
            .map(|_| SpawnIdentity::admitted(&caller_id, &request, schema_digest.clone()));
        let child = NodeRecord {
            fold_order: Some(0),
            abandoned_turn: None,
            agent_id: agent_id.clone(),
            parent_agent_id: Some(caller_id.clone()),
            depth: child_depth,
            description: request.description.clone(),
            prompt: request.prompt.clone(),
            output_schema_json: output_schema_json.clone(),
            schema_digest: schema_digest.clone(),
            template: Some(template),
            tuple_id,
            account,
            effort,
            status: NodeStatus::Running,
            workspace_id: bound.workspace_id.clone(),
            tier,
            attempt_id: Some(attempt_id.clone()),
            session_locator: None,
            path: bound.path,
            branch: bound.branch,
            active_turn: false,
        };
        self.nodes.insert(agent_id.clone(), child);
        self.grants.insert(caller_id.clone(), parent_grant);
        self.grants.insert(agent_id.clone(), child_grant);
        self.attempts.insert(
            attempt_id.clone(),
            AttemptRecord {
                output_schema_json,
                schema_digest,
                attempt_id: attempt_id.clone(),
                agent_id: agent_id.clone(),
                retry_of: None,
                request_key: request.request_key.clone(),
                spawn_identity,
                started_at: now_unix_millis(),
                ended_at: None,
                blocked: None,
                failure: None,
                usage: UsageRow::default(),
                status: AttemptStatus::Running,
                turn_token: 1,
                turn_id: None,
                applied_terminal_read_seqs: BTreeSet::new(),
                output_json: None,
                submit_harness_nudge: SubmitNudgeBudget::NONE,
                candidate_id: None,
            },
        );
        self.inbound.entry(agent_id.clone()).or_default();
        self.bump();
        let output = SpawnAgentOutput {
            agent_id,
            attempt_id,
            status: NodeStatus::Running,
            tier,
            workspace: WorkspaceReceipt {
                id: bound.workspace_id,
                mode: bound.mode,
                base: bound.base,
            },
        };
        let remembered = if let Some(id) = &operation_id {
            self.remember_success(id, &caller_id, "spawn_agent", &request, &output)
        } else {
            Ok(())
        };
        self.pending_spawns = self.pending_spawns.saturating_sub(1);
        remembered?;
        // the template's `on_mint` runs once in the fresh copy,
        // with the mutex released, before any session binds to it.
        if let Some(hook) = self.mint_hook(
            &output.agent_id,
            &caller_id,
            &caller_path,
            &request,
            operation_id.as_deref(),
        ) {
            self.effects.hooks.push(hook);
        }
        Ok(output)
    }

    /// Gives a reservation back when its mint refused. No row
    /// was ever written, so the tree is exactly where `spawn_reserve` found
    /// it; the ledger records the refusal so a retry of the same call replays
    /// it rather than minting again, and a prepared operation is closed
    /// rather than left non-terminal for the next reopen to fault.
    pub fn spawn_abort(
        &mut self,
        reservation: SpawnReservation,
        error: TopologyError,
    ) -> TopologyError {
        self.minting.remove(&reservation.agent_id);
        self.pending_spawns = self.pending_spawns.saturating_sub(1);
        if let Some(id) = &reservation.operation_id
            && let Err(failed) = self.remember_failure(
                id,
                &reservation.caller_id,
                "spawn_agent",
                &reservation.request,
                error.code,
            )
        {
            return failed;
        }
        error
    }

    /// Children holding a running turn, in tree order. The root is never in
    /// the set: execution capacity is a children-only bound.
    pub(super) fn running_child_ids(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| node.parent_agent_id.is_some() && node.status == NodeStatus::Running)
            .map(|node| node.agent_id.clone())
            .collect()
    }

    pub(super) fn holds_direct_child_slot(&self, node: &NodeRecord) -> bool {
        if matches!(
            node.status,
            NodeStatus::Closed | NodeStatus::RecoveryRequired
        ) {
            return false;
        }
        if node.active_turn {
            return true;
        }
        !(node
            .parent_agent_id
            .as_deref()
            .is_some_and(|parent| self.delivered_to(parent, &node.agent_id))
            && self
                .wait_row(&node.agent_id)
                .is_ok_and(|row| self.row_settled(&row) && row.attempt.is_stopped()))
    }

    fn holds_tree_slot(&self, node: &NodeRecord) -> bool {
        node.parent_agent_id.is_none()
            || node.status == NodeStatus::RecoveryRequired
            || self.holds_direct_child_slot(node)
    }

    /// `request_key` dedup. A repeat with the same key and the
    /// same brief returns the first child's receipt and spawns nothing; a
    /// repeat with a different brief is refused rather than silently aliased,
    /// because the caller believes it asked for two different things.
    ///
    /// Identity is the brief the key was first spent on: caller, template,
    /// description, prompt, the admitted output schema, the checkout choice
    /// (`workspace`), and the candidate the copy was minted over (`inputs`).
    /// The requested tier is not part of it — the receipt reports the
    /// effective tier after the template clip, so a second ask above the
    /// ceiling is the same spawn.
    pub(super) fn replay_request_key(
        &self,
        caller_id: &str,
        request: &SpawnAgentRequest,
    ) -> Result<Option<SpawnAgentOutput>, TopologyError> {
        let Some(key) = request
            .request_key
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let Some(attempt) = self
            .attempts
            .values()
            .find(|attempt| attempt.request_key.as_deref() == Some(key))
        else {
            return Ok(None);
        };
        let agent_id = attempt.agent_id.clone();
        let Some(identity) = attempt.spawn_identity.as_ref() else {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "keyed spawn has no admitted request identity",
            ));
        };
        if identity.caller_id != caller_id {
            return Err(TopologyError::new(
                TopologyErrorCode::RequestKeyConflict,
                format!("request_key {key} is already spent by another caller; choose a new key"),
            ));
        }
        let same_brief =
            admit_output_schema(&request.output_schema_json).is_ok_and(|(_, digest)| {
                *identity == SpawnIdentity::admitted(caller_id, request, digest)
            });
        if !same_brief {
            return Err(TopologyError::new(
                TopologyErrorCode::RequestKeyConflict,
                format!(
                    "request_key {key} already spawned {agent_id} with a different brief; \
                     repeat that request or choose a new key"
                ),
            ));
        }
        self.spawn_receipt(&agent_id).map(Some)
    }

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
        // Removing a copy drops the pins of the (adopted) candidates it
        // sealed, so their commits are no longer guaranteed to exist.
        if self
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
                    "input {first} was adopted and its copy removed; fork the node that adopted it"
                ),
            ));
        }
        Ok(Some(VcsSnapshot {
            commit: candidate.commit.clone(),
            content: candidate.content.clone(),
            change_id: candidate.change_id.clone(),
        }))
    }

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

    /// Admits a `followup` and resolves the schema the new
    /// attempt will carry.
    ///
    /// `followup` re-tasks an **idle** child: finished, or interrupted with
    /// its turn already ended (an interrupt cancels a turn,
    /// not the node). A running child is not re-tasked — `priority=urgent`
    /// steers the turn it is already in, and a second brief on a live turn
    /// would silently replace the first. `Closed` and `RecoveryRequired` can
    /// never run again, and neither can a failed spawn that never produced a
    /// session: there is nothing to re-task.
    pub(super) fn admit_followup(
        &self,
        agent_id: &str,
        status: NodeStatus,
        active_turn: bool,
        output_schema_json: Option<&str>,
    ) -> Result<(String, String), TopologyError> {
        if self.seal_pending(agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::FollowupNotIdle,
                "recipient still owns an unfinished seal",
            ));
        }
        match status {
            NodeStatus::Failed
                if self
                    .current_attempt(agent_id)
                    .is_some_and(AttemptRecord::never_created) =>
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupClosed,
                    "this agent never produced a session and cannot be re-tasked; spawn a new \
                     agent",
                ));
            }
            NodeStatus::Completed | NodeStatus::Failed if !active_turn => {}
            NodeStatus::Interrupted
                if !active_turn
                    && self
                        .current_attempt(agent_id)
                        .is_none_or(|attempt| attempt.turn_id.is_none()) => {}
            NodeStatus::Closed => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupClosed,
                    if self.conversation_retired(agent_id) {
                        "conversation was retired and cannot accept follow-up; its sealed work remains usable"
                    } else {
                        "recipient is closed and cannot accept another attempt"
                    },
                ));
            }
            NodeStatus::RecoveryRequired => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupRecoveryRequired,
                    "recipient requires recovery before another attempt can be admitted",
                ));
            }
            _ => {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupNotIdle,
                    "recipient is not an idle finished child; steer a running child with \
                     priority=urgent instead",
                ));
            }
        }
        // A re-task runs in the child's copy. A discarded or adopting copy has
        // no live path to run in, so the node is idle but not re-taskable. An
        // adopted copy still exists: the followup's seal is a new candidate
        // that diverges from the adopted state and adopting it integrates
        // only the delta.
        let workspace_status = self
            .nodes
            .get(agent_id)
            .and_then(|node| self.workspaces.get(&node.workspace_id))
            .map(|row| row.status);
        if let Some(status) = workspace_status
            && !matches!(status, WorkspaceStatus::Active | WorkspaceStatus::Adopted)
            && !self
                .nodes
                .get(agent_id)
                .is_some_and(|node| self.reclaimed(&node.workspace_id))
        {
            return Err(TopologyError::new(
                if matches!(
                    status,
                    WorkspaceStatus::Discarded | WorkspaceStatus::CleanRemoved
                ) {
                    TopologyErrorCode::FollowupWorkspaceGone
                } else {
                    TopologyErrorCode::FollowupNotIdle
                },
                format!(
                    "recipient's workspace is {} and cannot host another attempt; spawn a new \
                     child instead",
                    serde_json::to_value(status)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default()
                ),
            ));
        }
        let node = self.node(agent_id)?;
        if !self.holds_tree_slot(node)
            && let Some(parent) = node.parent_agent_id.as_deref()
        {
            let ceiling = MAX_TREE_NODES.min(usize::from(self.grant(parent)?.max_nodes));
            let held = self
                .nodes
                .values()
                .filter(|node| self.holds_tree_slot(node))
                .count();
            if held >= ceiling {
                return Err(TopologyError::new(
                    TopologyErrorCode::SpawnBudget,
                    "tree node budget is exhausted; followup would reactivate a dormant child",
                ));
            }
        }
        if !self.holds_direct_child_slot(node)
            && let Some(parent) = node.parent_agent_id.as_deref()
        {
            let ceiling = MAX_DIRECT_CHILDREN.min(usize::from(self.grant(parent)?.max_children));
            let held = self
                .nodes
                .values()
                .filter(|child| {
                    child.parent_agent_id.as_deref() == Some(parent)
                        && self.holds_direct_child_slot(child)
                })
                .count();
            if held >= ceiling {
                return Err(TopologyError::new(
                    TopologyErrorCode::SpawnBudget,
                    "direct child budget is exhausted; followup would reactivate a released slot",
                ));
            }
        }
        if self.attempt_budget_exhausted() {
            return Err(TopologyError::new(
                TopologyErrorCode::SpawnBudget,
                format!(
                    "attempt budget for this prompt is exhausted ({} of {})",
                    self.attempts_this_prompt(),
                    self.max_attempts
                ),
            ));
        }
        if let Some(raw) = output_schema_json.filter(|value| !value.is_empty()) {
            self.bound_text(raw)?;
            admit_output_schema(raw)
        } else {
            let node = self.node(agent_id)?;
            Ok((node.output_schema_json.clone(), node.schema_digest.clone()))
        }
    }

    /// Opens the next attempt on an idle finished node.
    ///
    /// The previous attempt row keeps its own `output_json`, `usage`, and
    /// candidate: the node is re-tasked, its history is not overwritten. The
    /// turn token advances so a late submission against the old attempt is
    /// still refused as stale.
    pub(super) fn open_followup_attempt(
        &mut self,
        agent_id: &str,
        opener: &str,
        output_schema_json: String,
        schema_digest: String,
    ) -> Result<AttemptId, TopologyError> {
        self.require_fold_started(agent_id)?;
        let previous = self.node(agent_id)?.attempt_id.clone();
        let rollback = {
            let node = self.node(agent_id)?;
            FollowupRollback {
                opener: opener.to_owned(),
                // Filled in below, once the new attempt has an id.
                opened: AttemptId::new(agent_id, 0),
                previous: previous
                    .as_ref()
                    .and_then(|id| self.attempts.get(id))
                    .map(|attempt| (attempt.status, attempt.ended_at)),
                node_attempt_id: node.attempt_id.clone(),
                node_status: node.status,
                node_active_turn: node.active_turn,
                workspace_id: node.workspace_id.clone(),
                workspace_status: self
                    .workspaces
                    .get(&node.workspace_id)
                    .map(|row| row.status),
                workspace_generation: self
                    .workspaces
                    .get(&node.workspace_id)
                    .map(|row| row.generation),
                fault_reason: self.fault_reasons.get(agent_id).cloned(),
                node_fold_order: node.fold_order,
                prior_operations: self.operations.keys().cloned().collect(),
                prior_commands: self.commands.keys().cloned().collect(),
            }
        };
        let seq = self
            .attempts
            .values()
            .filter(|attempt| attempt.agent_id == agent_id)
            .count()
            .saturating_add(1);
        let attempt_id = AttemptId::new(agent_id, u32::try_from(seq).unwrap_or(u32::MAX));
        let turn_token = previous
            .as_ref()
            .and_then(|id| self.attempts.get(id))
            .map_or(1, |attempt| attempt.turn_token.saturating_add(1));
        self.attempts.insert(
            attempt_id.clone(),
            AttemptRecord {
                output_schema_json: output_schema_json.clone(),
                schema_digest: schema_digest.clone(),
                attempt_id: attempt_id.clone(),
                agent_id: agent_id.to_owned(),
                retry_of: previous,
                request_key: None,
                spawn_identity: None,
                started_at: now_unix_millis(),
                ended_at: None,
                blocked: None,
                failure: None,
                usage: UsageRow::default(),
                status: AttemptStatus::Running,
                turn_token,
                turn_id: None,
                applied_terminal_read_seqs: BTreeSet::new(),
                output_json: None,
                submit_harness_nudge: SubmitNudgeBudget::NONE,
                candidate_id: None,
            },
        );
        self.record_followup_opening(
            agent_id,
            rollback,
            attempt_id.clone(),
            output_schema_json,
            schema_digest,
        )?;
        Ok(attempt_id)
    }

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
                effective: owned || parked,
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

    /// caller with no open attempt.
    ///
    /// Remedy names stay the grant's slot names (`wait_agent`). Callers that
    /// present those slots under another spelling use
    /// [`Self::submit_result_presented`].
    pub fn submit_result(
        &mut self,
        caller_id: &str,
        request: &SubmitResultRequest,
        operation_id: Option<&str>,
    ) -> Result<SubmitResultOutput, TopologyError> {
        self.submit_result_presented(caller_id, request, operation_id, |name| name)
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

    /// The template's per-attempt turn ceiling (`max_turns`).
    pub(super) fn attempt_turn_ceiling(&self, agent_id: &str) -> Option<u32> {
        let template = self.nodes.get(agent_id)?.template.as_ref()?;
        self.snapshot.binding(template)?.max_turns
    }

    /// Whether the current attempt has spent that ceiling.
    #[must_use]
    pub fn attempt_turns_spent(&self, agent_id: &str) -> bool {
        let Some(ceiling) = self.attempt_turn_ceiling(agent_id) else {
            return false;
        };
        self.current_attempt(agent_id)
            .is_some_and(|attempt| attempt.usage.turns >= ceiling)
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

    /// Marks whether a turn is in flight on this node. A terminal node may
    /// retain an unwinding turn, but cannot start a new one.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent and an active turn on a terminal node.
    pub fn set_active_turn(&mut self, agent_id: &str, active: bool) -> Result<(), TopologyError> {
        if active {
            let node = self.node(agent_id)?;
            if node.status.is_terminal() {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientClosed,
                    "terminal node cannot hold an active turn",
                ));
            }
            if self
                .current_attempt(agent_id)
                .is_some_and(|attempt| attempt.status.is_stopped())
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientAttemptTerminal,
                    "terminal attempt cannot hold an active turn",
                ));
            }
        }
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(|| TopologyError::new(TopologyErrorCode::GrantDenied, "node is missing"))?;
        node.active_turn = active;
        // A turn starting on an interrupted node is its resume:
        // the node runs again.
        if active && node.status == NodeStatus::Interrupted {
            node.status = NodeStatus::Running;
        }
        self.bump();
        Ok(())
    }

    /// Records the child session a binder opened for this node, once.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent and a node that already carries a locator —
    /// a second bind would mean two sessions behind one node.
    pub fn bind_session_locator(
        &mut self,
        agent_id: &str,
        locator: impl Into<String>,
    ) -> Result<(), TopologyError> {
        {
            let node = self.nodes.get_mut(agent_id).ok_or_else(|| {
                TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
            })?;
            if node.session_locator.is_some() {
                return Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "session locator is already bound",
                ));
            }
            node.session_locator = Some(locator.into());
        }
        self.bump();
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

    /// Spends one admitted submit-harness Start on the current attempt.
    /// The budget is persisted, so a crash-cut rejoin cannot
    /// reset it, and the harness counts against the template's turn ceiling
    /// like any other turn.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent and an attempt with no turns left.
    #[cfg(any(test, feature = "test-support"))]
    pub fn record_submit_harness_round(
        &mut self,
        agent_id: &str,
        nudge: SubmitNudgeBudget,
    ) -> Result<(), TopologyError> {
        // the harness is another turn on the attempt, so it may
        // not run past the template's `max_turns`.
        if self.attempt_turns_spent(agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::QuotaBlocked,
                "attempt has spent its template max_turns; no further turn is admitted",
            ));
        }
        let attempt = self.current_attempt_mut(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "attempt is missing")
        })?;
        if attempt.output_json.is_some() || attempt.status.is_stopped() {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientAttemptTerminal,
                "terminal attempt cannot admit a submit harness round",
            ));
        }
        attempt.submit_harness_nudge = nudge;
        self.bump();
        Ok(())
    }

    /// Records what a reopen of this node's session had to abandon.
    ///
    /// A child session is opened inside the caller's tool work, away from any
    /// face, so the node carries the fact until a face reads it. Recording it
    /// twice for one reopen would say it twice, so the last one wins rather
    /// than accumulating.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent.
    pub fn record_abandoned_turn(
        &mut self,
        agent_id: &str,
        abandoned: AbandonedChildTurn,
    ) -> Result<(), TopologyError> {
        let node = self.nodes.get_mut(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
        })?;
        node.abandoned_turn = Some(abandoned);
        self.bump();
        Ok(())
    }

    /// Moves a bound node's locator forward as its session advances, so a
    /// reopen finds the head rather than the checkpoint the bind recorded.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent.
    pub fn advance_session_locator(
        &mut self,
        agent_id: &str,
        locator: impl Into<String>,
    ) -> Result<(), TopologyError> {
        let node = self.nodes.get_mut(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
        })?;
        node.session_locator = Some(locator.into());
        self.bump();
        Ok(())
    }

    /// Forgets the attempt's bound turn once that turn has ended by an
    /// interrupt. The attempt stays `running` and the node
    /// `Interrupted`; with no bound turn the next mail is `Started`, i.e. a
    /// fresh turn on the same attempt, rather than a rejoin of a turn that no
    /// longer exists.
    #[cfg(any(test, feature = "test-support"))]
    pub fn release_attempt_turn(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        let attempt = self.current_attempt_mut(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
        })?;
        if attempt.status.is_stopped() {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientAttemptTerminal,
                "terminal attempt has no turn to release",
            ));
        }
        attempt.turn_id = None;
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.active_turn = false;
        }
        // A cancelled turn closed its exchange in canonical history before
        // its terminal was published, so its awaits can never roll back.
        self.retire_wait_receipts(agent_id);
        self.release_turn_owner(agent_id);
        self.bump();
        Ok(())
    }

    /// Names the runtime turn now driving this attempt, so an interrupt can
    /// find the turn to cancel.
    ///
    /// # Errors
    ///
    /// Refuses an agent with no open attempt.
    #[cfg(any(test, feature = "test-support"))]
    pub fn bind_attempt_turn(
        &mut self,
        agent_id: &str,
        turn_id: impl Into<String>,
    ) -> Result<(), TopologyError> {
        let attempt = self.current_attempt_mut(agent_id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
        })?;
        let turn_id = turn_id.into();
        // one bound turn is one turn of this attempt's budget.
        // Rebinding the same turn after a reopen is not a second one.
        if attempt.turn_id.as_deref() != Some(turn_id.as_str()) {
            attempt.usage.turns = attempt.usage.turns.saturating_add(1);
        }
        attempt.turn_id = Some(turn_id);
        self.bump();
        Ok(())
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
                .ok()
                .is_some_and(|node| node.status.is_finished() && !node.active_turn)
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
        // stop-all is the operator's explicit "close every agent",
        // and the second half of the one confirmed compound action the Agents
        // tab offers when a roster save is refused. On a restored tree the
        // pause bit fences descendants that were in flight when the process
        // ended; once stop-all has closed every one of them there is nothing
        // left to resume. Leaving the bit up made the offered remedy a dead end:
        // the very save it exists to unblock was refused again with
        // the same text, for every press after it.
        if self.state == TreeAdmissionState::RestoredPaused && self.quiescence_refusal().is_none() {
            self.state = TreeAdmissionState::TreeTerminal;
            self.bump();
        }
        Ok(closed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::tree::test_support::*;
    use crate::tree::vcs_fixtures::*;
    use crate::tree::*;

    /// Turn waiter registrations count per child, change no revision, and
    /// end only when every waiter left.
    #[test]
    fn turn_waiters_are_counted_per_child_without_a_revision() {
        let (_root, _vcs, mut tree) = fake_tree();
        let revision = tree.revision;
        tree.enter_turn_waiter("c1");
        tree.enter_turn_waiter("c1");
        assert!(tree.has_turn_waiter("c1"));
        assert!(!tree.has_turn_waiter("c2"));
        tree.leave_turn_waiter("c1");
        assert!(tree.has_turn_waiter("c1"), "one waiter is still running");
        tree.leave_turn_waiter("c1");
        tree.leave_turn_waiter("c1");
        assert!(!tree.has_turn_waiter("c1"));
        assert_eq!(tree.revision, revision);
    }

    /// between `spawn_reserve` and its commit the mint is
    /// running with the tree mutex released, and no node, attempt, grant, or
    /// workspace row names the reserved id. A close or a wait that races the
    /// mint therefore gets exactly the answer the id gets before it is ever
    /// spawned, and a second spawn cannot be handed the same copy.
    #[test]
    fn a_tool_racing_the_mint_answers_as_for_an_unspawned_id() {
        let (_root, vcs, mut tree) = fake_tree();
        let SpawnAdmission::Reserved(reservation, Some(ticket)) = tree
            .spawn_reserve("root", &mint_req(), Some("op-mint"))
            .unwrap()
        else {
            unreachable!("a mutate spawn reserves a mint");
        };
        let reserved = reservation.agent_id.clone();
        let unknown = tree.node(&reserved).unwrap_err();
        assert_eq!(unknown.code, TopologyErrorCode::GrantDenied);
        assert_eq!(
            tree.close_agent("root", &close_req(&reserved), Some("op-close"))
                .unwrap_err()
                .code,
            unknown.code
        );
        assert_eq!(
            tree.poll_wait("root", &wait_for(&reserved))
                .unwrap_err()
                .code,
            unknown.code
        );
        let SpawnAdmission::Reserved(second, _) = tree
            .spawn_reserve("root", &mint_req(), Some("op-mint-2"))
            .unwrap()
        else {
            unreachable!("a mutate spawn reserves a mint");
        };
        assert_ne!(
            second.agent_id, reserved,
            "a reserved id is not offered twice"
        );
        let minted = ticket.mint(vcs.as_ref()).unwrap();
        let output = tree.spawn_commit(*reservation, Some(minted)).unwrap();
        assert_eq!(output.agent_id, reserved);
        assert_eq!(
            tree.node(&reserved).unwrap().status,
            NodeStatus::Running,
            "the rows land when the mint answers"
        );
    }

    /// a mint that refuses gives the reservation back. The
    /// ledger closes on the refusal, and a wait for the reserved id answers
    /// as for an id that was never spawned — there is nothing to wait for.
    #[test]
    fn a_wait_after_a_failed_mint_answers_as_for_an_unspawned_id() {
        // No git or jj identity under this launch checkout: the mint refuses.
        let (_tmp, mut tree) = tree();
        let SpawnAdmission::Reserved(reservation, Some(ticket)) = tree
            .spawn_reserve("root", &mint_req(), Some("op-mint"))
            .unwrap()
        else {
            unreachable!("a mutate spawn reserves a mint");
        };
        let reserved = reservation.agent_id.clone();
        let refused = ticket.mint(tree.vcs()).unwrap_err();
        let error = tree.spawn_abort(*reservation, topology_error_from_vcs(refused));
        assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
        assert_eq!(
            tree.operations().get("op-mint").unwrap().terminal_code,
            Some(TopologyErrorCode::MintUnavailable)
        );
        assert_eq!(
            tree.poll_wait("root", &wait_for(&reserved))
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        assert_eq!(tree.nodes().len(), 1, "no node is left behind");
        assert!(
            tree.epoch_allows_rebind(),
            "the pending counter is released"
        );
    }

    #[test]
    fn spawn_list_wait_submit_pairs_with_completed() {
        let (_tmp, mut tree) = tree();
        let spawned = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn-1"))
            .unwrap();
        assert_eq!(spawned.status, NodeStatus::Running);
        assert_eq!(spawned.tier, Tier::ReadOnly);
        assert_eq!(spawned.attempt_id, AttemptId::new(&spawned.agent_id, 1));
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(true),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        assert_eq!(listed.agents.len(), 2);
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            tree.record_turn_completed(&spawned.agent_id).unwrap(),
            AttemptStatus::Completed
        );
        tree.settle_for_test();
        let wait = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .unwrap();
        assert!(!wait.timed_out);
        assert_eq!(wait.agents[0].attempt, AttemptStatus::Completed);
        assert_eq!(
            wait.agents[0].attempt_id,
            Some(AttemptId::new(&spawned.agent_id, 1))
        );
        assert!(wait.agents[0].candidate.is_none());
        let replay = tree
            .spawn_agent("root", &spawn_req(), Some("op-spawn-1"))
            .unwrap();
        assert_eq!(replay.agent_id, spawned.agent_id);
    }

    /// The receipt is limited to the spawn admission facts: nothing about bind identity
    /// or paths reaches the model.
    #[test]
    fn spawn_receipt_carries_attempt_tier_and_workspace_only() {
        let (_tmp, mut tree) = tree();
        tree.set_node_account_for_test("root", "codex");
        pin(&mut tree, "review", "grok", "grok-4", Some("high"));
        let bound = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let node = tree.node(&bound.agent_id).unwrap();
        assert_eq!(node.account, "grok");
        assert_eq!(node.tuple_id, "grok-4");
        assert_eq!(node.effort.as_deref(), Some("high"));
        let wire = serde_json::to_value(&bound).unwrap();
        assert_eq!(
            wire.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["agent_id", "attempt_id", "status", "tier", "workspace"]
        );
        assert_eq!(wire["tier"], "read_only");
        assert_eq!(wire["workspace"]["mode"], "inherit");
        assert_eq!(wire["workspace"]["id"], node.workspace_id);
        assert!(wire["workspace"]["base"].is_null());
    }

    #[test]
    fn spawn_refuses_a_card_without_its_own_account_or_model() {
        let (_tmp, mut tree) = tree();
        tree.set_node_account_for_test("root", "codex");
        pin(&mut tree, "review", "", "selected-tuple", None);
        let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
        assert!(
            error.message.contains("names no account"),
            "{}",
            error.message
        );
        pin(&mut tree, "review", "codex", "", None);
        let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
        assert!(
            error.message.contains("names no model"),
            "{}",
            error.message
        );
        assert_eq!(tree.nodes.len(), 1, "a refused card spends no node");
    }

    #[test]
    fn spawn_admits_explicit_model_outside_allowed_tuples() {
        let (_tmp, mut tree) = tree();
        tree.set_node_account_for_test("root", "codex");
        pin(&mut tree, "review", "codex", "not-in-catalog-yet", None);
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().tuple_id,
            "not-in-catalog-yet"
        );
    }

    /// a tier above `max_tier` clips; the receipt and the node
    /// both carry the effective tier.
    #[test]
    fn tier_above_the_template_ceiling_clips_and_the_receipt_reports_it() {
        let (_tmp, mut tree) = tree();
        let mut ambitious = spawn_req();
        ambitious.tier = Tier::Mutate;
        let spawned = tree.spawn_agent("root", &ambitious, None).unwrap();
        assert_eq!(spawned.tier, Tier::ReadOnly);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
        assert_eq!(tree.node(&spawned.agent_id).unwrap().tier, Tier::ReadOnly);
        // Below the ceiling is honoured as asked.
        let modest = spawn_req();
        let spawned = tree.spawn_agent("root", &modest, None).unwrap();
        assert_eq!(spawned.tier, Tier::ReadOnly);
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
    }

    #[test]
    fn request_key_is_recorded_on_the_attempt() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("first-review".into());
        let spawned = tree.spawn_agent("root", &request, None).unwrap();
        let attempt = tree.attempt(&spawned.agent_id).unwrap();
        assert_eq!(attempt.request_key.as_deref(), Some("first-review"));
        assert_eq!(attempt.attempt_id, spawned.attempt_id);
        assert!(attempt.started_at > 0);
        assert_eq!(attempt.ended_at, None);
        assert_eq!(attempt.usage, UsageRow::default());
    }

    /// Two independent calls carrying one `request_key` are
    /// one spawn: the `call_id` digest cannot see across calls, so without the
    /// key a retried tool loop opens a second child on the same brief.
    #[test]
    fn request_key_dedups_two_independent_spawns() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("review-parser".into());
        let first = tree.spawn_agent("root", &request, Some("call-1")).unwrap();
        let nodes_after_first = tree.nodes().len();
        let second = tree.spawn_agent("root", &request, Some("call-2")).unwrap();
        assert_eq!(first, second, "the same key returns the same receipt");
        assert_eq!(
            tree.nodes().len(),
            nodes_after_first,
            "no second node is created"
        );
        assert_eq!(
            tree.attempts()
                .values()
                .filter(|attempt| attempt.request_key.as_deref() == Some("review-parser"))
                .count(),
            1
        );
    }

    #[test]
    fn request_key_identity_survives_changed_workspace_and_followup_brief() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("immutable-request".into());
        let first = tree.spawn_agent("root", &request, None).unwrap();
        let workspace_id = tree.node(&first.agent_id).unwrap().workspace_id.clone();
        tree.workspaces.get_mut(&workspace_id).unwrap().base_commit =
            Some("jj-working-copy-rewrite".into());
        let node = tree.nodes.get_mut(&first.agent_id).unwrap();
        node.prompt = "later followup".into();
        node.schema_digest = "later-schema".into();
        let before = tree.nodes().len();
        let replay = tree.spawn_agent("root", &request, None).unwrap();
        assert_eq!(replay.agent_id, first.agent_id);
        assert_eq!(tree.nodes().len(), before);
        let mut changed = request.clone();
        changed.prompt = "later followup".into();
        assert_eq!(
            tree.spawn_agent("root", &changed, None).unwrap_err().code,
            TopologyErrorCode::RequestKeyConflict
        );
    }

    #[test]
    fn keyed_spawn_without_admitted_identity_requires_recovery() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("incomplete-identity".into());
        let first = tree.spawn_agent("root", &request, None).unwrap();
        tree.attempts
            .get_mut(&first.attempt_id)
            .unwrap()
            .spawn_identity = None;
        assert_eq!(
            tree.spawn_agent("root", &request, None).unwrap_err().code,
            TopologyErrorCode::OperationRecoveryRequired
        );
        assert_eq!(tree.nodes().len(), 2);
    }

    /// The same key on a different brief is refused, not aliased — the
    /// caller asked for two different things and must learn that it did.
    #[test]
    fn request_key_conflict_is_typed() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("review-parser".into());
        tree.spawn_agent("root", &request, Some("call-1")).unwrap();
        let mut conflicting = request.clone();
        conflicting.prompt = "review something else entirely".into();
        let error = tree
            .spawn_agent("root", &conflicting, Some("call-2"))
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
        assert!(error.message.contains("review-parser"), "{}", error.message);
        assert_eq!(tree.nodes().len(), 2, "the conflict spawns nothing");
    }

    #[test]
    fn request_key_conflict_includes_workspace() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.request_key = Some("review-parser".into());
        tree.spawn_agent("root", &request, Some("call-1")).unwrap();
        let mut conflicting = request.clone();
        conflicting.workspace = SpawnWorkspace::Fork;
        let error = tree
            .spawn_agent("root", &conflicting, Some("call-2"))
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
        assert_eq!(tree.nodes().len(), 2, "the conflict spawns nothing");
    }

    /// Saturation is typed and names the running children, because a flat
    /// "budget exhausted" left the caller nothing to interrupt or wait on.
    /// The bound counts children only: a flat root's own running turn never
    /// consumes a slot, so reaching it takes children spawning children.
    #[test]
    fn capacity_error_carries_the_running_set() {
        let (_tmp, mut tree) = tree();
        tree.grants_mut_for_test("root").max_depth = 3;
        // Two supervising children with three leaves each: eight running
        // children, every parent's own direct-child budget still open.
        let mut spawned = Vec::new();
        for _ in 0..2 {
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap();
            spawned.push(parent.agent_id.clone());
            for _ in 0..3 {
                let leaf = tree
                    .spawn_agent(&parent.agent_id, &spawn_req(), None)
                    .unwrap();
                spawned.push(leaf.agent_id);
            }
        }
        let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::Capacity);
        assert!(
            error.message.contains("no queue"),
            "there is no queue in v1: {}",
            error.message
        );
        assert!(
            !error.message.contains("root"),
            "the root never consumes a child slot: {}",
            error.message
        );
        for id in &spawned {
            assert!(error.message.contains(id.as_str()), "{}", error.message);
        }
    }

    /// B1: `max_attempts` is a per-tree cap the tree may only
    /// lower. A raise is ignored rather than accepted and silently clipped.
    #[test]
    fn retries_consume_max_attempts() {
        let (_tmp, mut tree) = tree();
        tree.lower_max_attempts(2);
        assert_eq!(tree.max_attempts(), 2);
        tree.lower_max_attempts(MAX_TREE_ATTEMPTS * 2);
        assert_eq!(tree.max_attempts(), 2, "the ceiling only ever lowers");
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::SpawnBudget);
        assert!(
            error.message.contains("attempt budget"),
            "{}",
            error.message
        );
    }

    #[test]
    fn submit_does_not_adopt_and_only_copy_cannot_be_destroyed() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(tree.node("root").unwrap().path, tree.launch_checkout,);
        let inherit_ws = tree.node("root").unwrap().workspace_id.clone();
        assert_eq!(
            tree.discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: inherit_ws,
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::DiscardNotApplicable
        );
        tree.settle_for_test();
    }

    /// `followup` re-tasks an idle finished child on a **new**
    /// attempt: one assignment row per node could not hold two attempts'
    /// results without losing the first, which is why the attempt is the unit.
    #[test]
    fn followup_reruns_the_node_with_a_new_attempt_and_keeps_the_first_output() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        let first_attempt = spawned.attempt_id.clone();
        let first_token = tree.attempts().get(&first_attempt).unwrap().turn_token;

        let mut request = mail(&spawned.agent_id, "also cover the error paths");
        request.purpose = Some(MessagePurpose::Followup);
        request.output_schema_json = Some(
            r#"{"type":"object","properties":{"notes":{"type":"string"}},"required":["notes"]}"#
                .into(),
        );
        let sent = tree
            .send_agent_message("root", &request, None)
            .expect("followup is admitted on an idle finished child");
        assert_eq!(sent.disposition, MailDisposition::Started);

        let node = tree.node(&spawned.agent_id).unwrap();
        assert_eq!(node.status, NodeStatus::Running);
        let second_attempt = node.attempt_id.clone().unwrap();
        assert_eq!(
            second_attempt.as_str(),
            format!("{}-a2", spawned.agent_id),
            "the second attempt is a2 on the same node"
        );
        assert!(
            node.output_schema_json.contains("notes"),
            "the new attempt carries the new schema"
        );

        let second = tree.attempts().get(&second_attempt).unwrap();
        assert_eq!(second.status, AttemptStatus::Running);
        assert_eq!(second.retry_of.as_ref(), Some(&first_attempt));
        assert!(second.turn_token > first_token, "the turn token rotates");
        assert_eq!(second.output_json, None);

        let first = tree.attempts().get(&first_attempt).unwrap();
        assert_eq!(
            first.output_json.as_deref(),
            Some(r#"{"ok":true}"#),
            "attempt 1 keeps its own result"
        );
        assert_eq!(first.status, AttemptStatus::Completed);

        // The rotated token fences a late submission against the old attempt.
        assert_eq!(
            tree.submit_result(
                &spawned.agent_id,
                &SubmitResultRequest {
                    turn_token: first_token,
                    output_json: r#"{"notes":"late"}"#.into(),
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::SubmitStaleToken
        );
    }

    /// A running child is steered, not re-tasked; a closed one can never run
    /// again. Distinct codes identify a temporary busy state
    /// versus permanent closure without requiring prose parsing.
    #[test]
    fn followup_rejected_for_running_and_closed() {
        let (_tmp, mut tree) = tree();
        let running = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut request = mail(&running.agent_id, "again");
        request.purpose = Some(MessagePurpose::Followup);
        let error = tree.send_agent_message("root", &request, None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::FollowupNotIdle);
        assert!(error.message.contains("urgent"), "{}", error.message);

        let closed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&closed.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        request.agent_id = closed.agent_id;
        assert_eq!(
            tree.send_agent_message("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::FollowupClosed
        );

        // `result` is host-authored and never arrives from a model.
        let idle = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&idle.agent_id).unwrap();
        tree.settle_for_test();
        request.agent_id = idle.agent_id;
        request.purpose = Some(MessagePurpose::Result);
        assert_eq!(
            tree.send_agent_message("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    #[test]
    fn completed_without_submission_is_distinct() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::Running
        );
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            tree.record_turn_completed(&spawned.agent_id).unwrap(),
            AttemptStatus::Completed
        );
        tree.settle_for_test();
        assert!(tree.attempt(&spawned.agent_id).unwrap().ended_at.is_some());
        let missed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.record_turn_completed(&missed.agent_id).unwrap(),
            AttemptStatus::CompletedWithoutSubmission
        );
        tree.settle_for_test();
        assert_eq!(
            tree.submit_result(
                &missed.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::RecipientAttemptTerminal
        );
    }

    #[test]
    fn fail_session_materialization_terminals_attempt_and_frees_the_fork() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        assert_eq!(spawned.workspace.mode, WorkspaceMode::Fork);
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::RecoveryRequired
        );
        let discarded = tree
            .discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: spawned.workspace.id.clone(),
                },
                None,
            )
            .unwrap();
        assert_eq!(discarded.status, WorkspaceStatus::Discarding);
        tree.settle_for_test();
        assert_eq!(
            tree.workspace(&spawned.workspace.id).unwrap().status,
            WorkspaceStatus::Discarded
        );
    }

    #[test]
    fn fail_session_materialization_completes_submitted_attempt() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::Completed
        );
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Completed
        );
    }

    /// the 1+1 budget is a ceiling, so a count past it is
    /// refused rather than clamped into looking admitted. Clamping is what a
    /// `u8` field invited: `min(2)` turned a caller that had lost count into
    /// a caller that looked exhausted, which reads the same as a child that
    /// was actually asked twice.
    #[test]
    fn the_submit_harness_budget_refuses_a_count_past_its_ceiling() {
        assert_eq!(SubmitNudgeBudget::NONE.rounds(), 0);
        assert!(!SubmitNudgeBudget::NONE.is_exhausted());
        let one = one_round();
        assert_eq!(one.rounds(), 1);
        assert!(!one.is_exhausted());
        let two = one.next().expect("the second ask is admitted");
        assert_eq!(two, two_rounds());
        assert!(two.is_exhausted());
        assert!(two.next().is_none(), "there is no third ask");
        assert!(SubmitNudgeBudget::spent(SubmitNudgeBudget::CEILING + 1).is_none());
        assert!(SubmitNudgeBudget::spent(u8::MAX).is_none());
    }

    #[test]
    fn submit_harness_round_is_persisted_on_the_attempt() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_submit_harness_round(&spawned.agent_id, one_round())
            .unwrap();
        assert_eq!(
            tree.attempt(&spawned.agent_id)
                .unwrap()
                .submit_harness_nudge,
            one_round()
        );
        tree.record_submit_harness_round(&spawned.agent_id, two_rounds())
            .unwrap();
        assert_eq!(
            tree.attempt(&spawned.agent_id)
                .unwrap()
                .submit_harness_nudge,
            two_rounds()
        );
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.record_submit_harness_round(&spawned.agent_id, one_round())
                .unwrap_err()
                .code,
            TopologyErrorCode::RecipientAttemptTerminal
        );
    }

    #[test]
    fn restored_paused_blocks_spawn_and_mail() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.restore_paused();
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TreeRestoredPaused
        );
        assert_eq!(
            tree.send_agent_message("root", &mail(&spawned.agent_id, "hi"), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TreeRestoredPaused
        );
        assert_eq!(
            tree.discard_workspace(
                "root",
                &WorkspaceIdRequest {
                    workspace_id: spawned.workspace.id,
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::TreeRestoredPaused
        );
        tree.settle_for_test();
        tree.resume_tree().unwrap();
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
    }

    /// Spawn authority is the caller's own menu: the root's is the working
    /// set, a child's is its card's `spawns` list. A template frozen into the
    /// closure only for a nested card is not the root's to spawn, and a
    /// supervisor cannot reach a working-set template its card does not name.
    #[test]
    fn a_spawn_must_name_a_template_on_the_callers_own_menu() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_depth = 3;
        let name = |name: &str| TemplateName::parse(name).unwrap();
        let mut nested =
            RosterBinding::leaf(crate::epoch::RosterInput::TEST_ACCOUNT, "selected-tuple");
        nested.template = "nested".to_owned();
        tree.snapshot.roster.insert(name("nested"), nested);
        tree.snapshot
            .roster
            .get_mut(&name("orchestrate"))
            .unwrap()
            .spawns = vec![name("nested")];
        tree.snapshot.digest = tree.snapshot.compute_digest();
        let nested_request =
            SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do it");
        assert_eq!(
            tree.spawn_agent("root", &nested_request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TemplateNotSpawnable,
            "the root spawns the working set only"
        );
        let supervisor = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        assert_eq!(
            tree.spawn_agent(&supervisor.agent_id, &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TemplateNotSpawnable,
            "review is in the working set but not on the supervisor's card"
        );
        let child = tree
            .spawn_agent(&supervisor.agent_id, &nested_request, None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().template,
            Some(name("nested"))
        );
        let grant = tree.grants.get(&child.agent_id).unwrap();
        assert!(!grant.may_spawn && !grant.may_adopt, "nested is a leaf");
        assert!(
            !tree
                .compiled_tool_names(&child.agent_id)
                .unwrap()
                .contains(&"spawn_agent")
        );
    }

    /// An off-menu card is refused as not spawnable before its account is
    /// checked, so an unsupported account is not `schema_mismatch`.
    #[test]
    fn an_off_menu_template_with_an_unsupported_account_is_not_spawnable() {
        let (_tmp, mut tree) = tree();
        let name = |raw: &str| TemplateName::parse(raw).unwrap();
        let mut nested = RosterBinding::leaf("anthropic", "claude-sonnet-5");
        nested.template = "nested".to_owned();
        tree.snapshot.roster.insert(name("nested"), nested);
        tree.snapshot
            .roster
            .get_mut(&name("orchestrate"))
            .unwrap()
            .spawns = vec![name("nested")];
        tree.snapshot.digest = tree.snapshot.compute_digest();
        let error = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do it"),
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::TemplateNotSpawnable);
    }

    /// a node's menu excludes its own template.
    #[test]
    fn a_node_cannot_spawn_its_own_template() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let supervisor = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let error = tree
            .spawn_agent(
                &supervisor.agent_id,
                &SpawnAgentRequest::test_orchestrate(),
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::TemplateNotSpawnable);
        // The supervisor's advertised menu agrees with the refusal.
        assert!(
            tree.compiled_tool_names(&supervisor.agent_id)
                .unwrap()
                .contains(&"spawn_agent")
        );
        tree.snapshot
            .roster
            .retain(|name, _| name.as_str() == "orchestrate");
        tree.snapshot.digest = tree.snapshot.compute_digest();
        assert!(
            !tree
                .compiled_tool_names(&supervisor.agent_id)
                .unwrap()
                .contains(&"spawn_agent"),
            "a menu that is only the node's own template is empty"
        );
    }

    #[test]
    fn set_active_turn_refuses_terminal_node_and_attempt() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.bind_session_locator(&spawned.agent_id, "session:c1")
            .unwrap();
        tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.set_active_turn(&spawned.agent_id, true)
                .unwrap_err()
                .code,
            TopologyErrorCode::RecipientClosed
        );
        assert!(tree.inflight_join_targets().is_empty());
        tree.set_active_turn(&spawned.agent_id, false).unwrap();
        // A completed node is idle, not terminal: the refusal
        // there names the finished attempt, which a followup would replace.
        let done = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&done.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.set_active_turn(&done.agent_id, true).unwrap_err().code,
            TopologyErrorCode::RecipientAttemptTerminal
        );
    }

    #[test]
    fn luna_tuple_is_admitted_at_tree_layer_and_delegate_rejects() {
        let (_tmp, mut tree) = tree();
        pin(&mut tree, "review", "codex", "luna", None);
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(tree.node(&spawned.agent_id).unwrap().tuple_id, "luna");
        assert!(
            serde_json::from_str::<SendAgentMessageRequest>(
                r#"{"agent_id":"c1","message":"hi","purpose":"delegate"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn close_retains_rows_and_child_cannot_close_parent() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.close_agent(&spawned.agent_id, &close_req("root"), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        tree.settle_for_test();
        let closed = tree
            .close_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        assert_eq!(closed[0].1, NodeStatus::Closed);
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::Closed
        );
        assert!(tree.attempt(&spawned.agent_id).unwrap().ended_at.is_some());
    }

    /// `expected_attempt_id` fences interrupt and close to the
    /// attempt the caller saw.
    #[test]
    fn stale_expected_attempt_id_refuses_interrupt_and_close() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        let stale = AttemptScopedRequest {
            agent_id: spawned.agent_id.clone(),
            expected_attempt_id: Some(AttemptId::new(&spawned.agent_id, 7)),
        };
        assert_eq!(
            tree.interrupt_agent("root", &stale, None).unwrap_err().code,
            TopologyErrorCode::AttemptMismatch
        );
        assert_eq!(
            tree.close_agent("root", &stale, None).unwrap_err().code,
            TopologyErrorCode::AttemptMismatch
        );
        tree.settle_for_test();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Running
        );
        let exact = AttemptScopedRequest {
            agent_id: spawned.agent_id.clone(),
            expected_attempt_id: Some(spawned.attempt_id.clone()),
        };
        assert_eq!(
            tree.interrupt_agent("root", &exact, None).unwrap(),
            NodeStatus::Interrupted
        );
        tree.close_agent("root", &exact, None).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Closed
        );
    }

    /// The submission outcomes stay distinct. A schema-invalid payload, a
    /// stale turn token, an identical resubmit, and a conflicting one are four
    /// different answers, not one "submit failed".
    #[test]
    fn invalid_stale_duplicate_and_conflicting_submissions_are_distinct() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let id = spawned.agent_id.as_str();
        let submit = |output: &str, token: u64| SubmitResultRequest {
            turn_token: token,
            output_json: output.to_owned(),
        };
        assert_eq!(
            tree.submit_result(id, &submit("{ not json", 1), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
        assert_eq!(
            tree.submit_result(id, &submit(r#"{"ok":true}"#, 9), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SubmitStaleToken
        );
        let first = tree
            .submit_result(id, &submit(r#"{"ok":true}"#, 1), None)
            .unwrap();
        assert_eq!(first.attempt, AttemptStatus::Submitted);
        assert_eq!(first.candidate, None, "a submission is not a seal");
        let repeat = tree
            .submit_result(id, &submit(r#"{"ok":true}"#, 1), None)
            .unwrap();
        assert_eq!(repeat, first, "an identical resubmit is idempotent");
        assert_eq!(
            tree.submit_result(id, &submit(r#"{"ok":false}"#, 1), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SubmitDuplicateConflict
        );
    }

    #[test]
    fn submit_refuses_a_running_child_attempt() {
        let (_tmp, mut tree) = tree();
        tree.grants_mut_for_test("root").max_depth = 3;
        let hub = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
        let result = tree.submit_result(
            &hub.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        );
        let error = result.expect_err("running child must refuse submission");
        assert!(error.message.contains(&child.agent_id));
        assert_eq!(serde_json::to_value(error.code).unwrap(), "live_work");
    }

    #[test]
    fn completion_closes_the_live_subtree_without_result_mail() {
        let (_tmp, mut tree) = tree();
        tree.grants_mut_for_test("root").max_depth = 3;
        let hub = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
        tree.record_turn_completed(&hub.agent_id).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert!(tree.pending_mail(&hub.agent_id).is_empty());
    }

    /// the template's `max_turns` ends the attempt `blocked` with
    /// whatever it produced, rather than letting the submit harness hand it
    /// another round.
    #[test]
    fn every_attempt_end_closes_descendants_including_work_started_after_submit() {
        for ending in [
            "completed",
            "missing_submission",
            "failed",
            "blocked",
            "cancelled",
        ] {
            let (_tmp, mut tree) = tree();
            tree.grants_mut_for_test("root").max_depth = 3;
            let hub = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap();
            if ending == "completed" {
                tree.submit_result(
                    &hub.agent_id,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
            }
            let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
            match ending {
                "completed" | "missing_submission" => {
                    tree.record_turn_completed(&hub.agent_id).unwrap();
                }
                "failed" => {
                    tree.record_turn_failure(
                        &hub.agent_id,
                        turn_outcome::FailedTurnDisposition::Failed { kind: "test" },
                        "failed fixture".into(),
                    )
                    .unwrap();
                }
                "blocked" => {
                    tree.record_attempt_blocked(
                        &hub.agent_id,
                        BlockedReason {
                            quota: "test".into(),
                            account: "test".into(),
                            resets_at: None,
                        },
                    )
                    .unwrap();
                }
                "cancelled" => {
                    tree.record_host_cancellation(&hub.agent_id)
                        .and_then(|value| tree.fold_due(&hub.agent_id).map(|_| value))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            tree.fold_reserved_closes().unwrap();
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::Closed,
                "{ending}"
            );
            assert!(tree.pending_mail(&hub.agent_id).is_empty(), "{ending}");
        }
    }

    #[test]
    fn max_turns_ends_the_attempt_blocked_with_partial_output() {
        let (_tmp, mut tree) = tree();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::ReadOnly;
        binding.max_turns = Some(1);
        tree.pin_template("review", binding);
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(!tree.attempt_turns_spent(&spawned.agent_id));
        tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
        // Rebinding the same turn after a reopen is not a second turn.
        tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
        assert_eq!(tree.attempt(&spawned.agent_id).unwrap().usage.turns, 1);
        assert!(tree.attempt_turns_spent(&spawned.agent_id));
        assert_eq!(
            tree.record_submit_harness_round(&spawned.agent_id, one_round())
                .unwrap_err()
                .code,
            TopologyErrorCode::QuotaBlocked
        );
        assert_eq!(
            tree.record_turn_completed(&spawned.agent_id).unwrap(),
            AttemptStatus::Blocked
        );
        tree.settle_for_test();
        let attempt = tree.attempt(&spawned.agent_id).unwrap();
        assert_eq!(attempt.blocked.as_ref().unwrap().quota, "max_turns");
        assert_eq!(attempt.output_json, None, "there was no partial output");
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Failed
        );
    }

    /// model-visible rows carry per-attempt usage, and only that.
    #[test]
    fn usage_row_fields_are_populated_per_attempt() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
        // What the tree projector folds from the turn's `TurnTerminal`.
        let row = &mut tree.current_attempt_mut(&spawned.agent_id).unwrap().usage;
        row.elapsed_ms = 1_500;
        row.tool_calls = 4;
        row.input_tokens = 900;
        row.output_tokens = 120;
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();

        let usage = tree.attempt(&spawned.agent_id).unwrap().usage;
        assert_eq!(usage.turns, 1);
        assert_eq!(usage.tool_calls, 4);
        assert_eq!(usage.input_tokens, 900);
        assert_eq!(usage.output_tokens, 120);
        assert!(usage.elapsed_ms >= 1_500);

        let settled = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .unwrap();
        assert_eq!(settled.agents[0].usage, Some(usage));
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
        assert_eq!(listed.agents[0]["usage"]["tool_calls"], 4);
        assert!(
            listed.agents[0].get("cost").is_none(),
            "cost never reaches a model-visible row"
        );

        // A second attempt starts its own row; usage is never rolled up.
        let mut followup = mail(&spawned.agent_id, "one more pass");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &followup, None).unwrap();
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().usage,
            UsageRow::default()
        );
    }

    #[test]
    fn leftover_workspace_or_worktree_from_another_tree_does_not_steal_child_id() {
        let (_tmp, mut tree) = tree();
        std::fs::create_dir_all(tree.launch_checkout.join(".plak/worktrees/c1")).unwrap();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(spawned.agent_id, "c1");
        tree.workspaces.insert(
            "ws-c2".into(),
            WorkspaceRecord {
                workspace_id: "ws-c2".into(),
                owner_agent_id: "root".into(),
                path: tree.launch_checkout.join(".plak/worktrees/old-tree-c2"),
                mode: WorkspaceMode::Fork,
                status: WorkspaceStatus::Active,
                base_commit: None,
                base_content: None,
                base_change_id: None,
                generation: 0,
                adopter_agent_id: None,
            },
        );
        let next = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(next.agent_id, "c3");
        tree.rollback_minted_workspace("ws-c2");
        tree.settle_for_test();
        assert!(tree.workspace("ws-c2").is_none());
    }

    #[test]
    fn grant_max_nodes_and_pending_spawn_budgets() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_nodes = 1;
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SpawnBudget
        );
        tree.grants.get_mut("root").unwrap().max_nodes = 32;
        tree.pending_spawns = u32::try_from(MAX_PENDING_SPAWNS).unwrap();
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SpawnBudget
        );
    }

    /// Closed children give their node slot back, so a session can keep
    /// delegating past the old lifetime budget of 31 children.
    #[test]
    fn closed_children_free_node_slots() {
        let (_tmp, mut tree) = tree();
        for _ in 0..40 {
            tree.mark_root_live_for_prompt();
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            tree.settle_for_test();
        }
        tree.grants.get_mut("root").unwrap().max_nodes = 2;
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
        assert!(
            refused.message.contains("2 of 2 live"),
            "{}",
            refused.message
        );
    }

    /// The attempt budget counts one root prompt; the next prompt starts a
    /// fresh count.
    #[test]
    fn the_attempt_budget_resets_with_each_root_prompt() {
        let (_tmp, mut tree) = tree();
        tree.lower_max_attempts(3);
        tree.mark_root_live_for_prompt();
        for _ in 0..3 {
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.close_agent("root", &close_req(&child.agent_id), None)
                .unwrap();
            tree.settle_for_test();
        }
        let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
        assert!(
            refused.message.contains("this prompt"),
            "{}",
            refused.message
        );
        tree.mark_root_live_for_prompt();
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
    }

    #[test]
    fn depth_exhaustion_is_its_own_code() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_depth = 0;
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::DepthExhausted
        );
    }

    #[test]
    fn spawn_and_submit_admit_portable_schema_v2() {
        let (_tmp, mut tree) = tree();
        let mut req = spawn_req();
        req.output_schema_json = "{}".into();
        assert_eq!(
            tree.spawn_agent("root", &req, None).unwrap_err().code,
            TopologyErrorCode::SchemaMismatch
        );
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.submit_result(
                &spawned.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":1}"#.into(),
                },
                None,
            )
            .unwrap_err()
            .code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    /// `output_schema_json` arguments of the shape `codex · gpt-5.6-luna`
    /// (native Code Mode) emits: four consecutive `spawn_agent` calls that the
    /// old admission refused as `schema_mismatch`.
    const LUNA_LIVE_ARGUMENTS: [&str; 4] = [
        r#"{"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}},"raw_output":{"type":"string"}},"required":["files","raw_output"]}"#,
        r#"{"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}}},"required":["files"]}"#,
        r#"{"version":2,"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}}},"required":["files"]}"#,
        "{}",
    ];

    #[test]
    fn admits_the_live_luna_output_schema_shapes() {
        // The two benign shapes (a correct Draft 2020-12 object schema with
        // `$schema` and `additionalProperties` omitted) now admit and freeze
        // closed; the two guesses the model made after the content-free
        // refusal still fail closed.
        for raw in [LUNA_LIVE_ARGUMENTS[0], LUNA_LIVE_ARGUMENTS[1]] {
            let (frozen, digest) = admit_output_schema(raw).expect(raw);
            assert!(frozen.contains(OUTPUT_SCHEMA_DIALECT), "{frozen}");
            assert!(
                frozen.contains(r#""additionalProperties":false"#),
                "{frozen}"
            );
            assert!(!digest.is_empty());
            // The frozen schema is the contract the child submits against.
            validate_output_json(&frozen, r#"{"files":["a"],"raw_output":"x"}"#)
                .or_else(|_| validate_output_json(&frozen, r#"{"files":["a"]}"#))
                .expect(raw);
        }
        for raw in [LUNA_LIVE_ARGUMENTS[2], LUNA_LIVE_ARGUMENTS[3]] {
            let error = admit_output_schema(raw).unwrap_err();
            assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
            // The refusal carries the closed-dialect repair, not just a verdict.
            assert!(
                error.message.contains("PortableToolSchemaV2")
                    && error.message.contains("additionalProperties=false"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn output_schema_normalisation_is_bounded_and_truthful() {
        // An explicitly open object is the one shape that cannot be filled in
        // truthfully: a submitted result is validated closed.
        let open = r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":true}"#;
        assert!(
            admit_output_schema(open)
                .unwrap_err()
                .message
                .contains("additionalProperties")
        );
        // A foreign dialect is refused by name rather than silently rewritten.
        let dialect = r#"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"a":{"type":"string"}},"required":["a"]}"#;
        assert!(
            admit_output_schema(dialect)
                .unwrap_err()
                .message
                .contains("Draft 2020-12")
        );
        // Nested objects, including inside array items, normalise too.
        let nested = r#"{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"}}}}}}"#;
        let (frozen, _) = admit_output_schema(nested).unwrap();
        assert_eq!(frozen.matches(r#""additionalProperties":false"#).count(), 2);
        assert_eq!(frozen.matches(r#""required":[]"#).count(), 2);
        // One level of double encoding is unwrapped.
        let encoded = serde_json::to_string(LUNA_LIVE_ARGUMENTS[1]).unwrap();
        assert_eq!(
            admit_output_schema(&encoded).unwrap(),
            admit_output_schema(LUNA_LIVE_ARGUMENTS[1]).unwrap()
        );
    }

    #[test]
    fn interrupt_cancels_turn_without_terminalizing_attempt() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.interrupt_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        // The node reads `interrupted`, not `running` — but it is still
        // non-terminal, and its attempt is untouched.
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Interrupted
        );
        assert!(!NodeStatus::Interrupted.is_terminal());
        assert!(tree.node(&spawned.agent_id).unwrap().active_turn);
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::Running
        );
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Failed
        );
        let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&other.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        tree.record_turn_completed(&other.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&other.agent_id).unwrap().status,
            NodeStatus::Closed
        );
    }

    #[test]
    fn nested_children_bind_their_own_cards_not_their_callers() {
        let (_tmp, mut tree) = tree();
        tree.set_node_account_for_test("root", "grok");
        pin(
            &mut tree,
            "orchestrate",
            "claude",
            "claude-sonnet-5",
            Some("high"),
        );
        pin(&mut tree, "review", "codex", "gpt-5.6-luna", None);
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().tuple_id,
            "claude-sonnet-5"
        );
        assert_eq!(tree.node(&child.agent_id).unwrap().account, "claude");
        assert!(tree.grants.get(&child.agent_id).unwrap().may_spawn);
        assert!(tree.grants.get(&child.agent_id).unwrap().may_adopt);
        let grandchild = tree
            .spawn_agent(&child.agent_id, &spawn_req(), None)
            .unwrap();
        let grandchild = tree.node(&grandchild.agent_id).unwrap();
        assert_eq!(
            (grandchild.account.as_str(), grandchild.tuple_id.as_str()),
            ("codex", "gpt-5.6-luna")
        );
        assert_eq!(grandchild.effort, None, "the caller's effort is not copied");
        assert!(!tree.grants.get(&grandchild.agent_id).unwrap().may_spawn);
    }

    #[test]
    fn stop_all_closes_non_root_and_is_root_only() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.stop_all(&spawned.agent_id, None).unwrap_err().code,
            TopologyErrorCode::GrantDenied
        );
        tree.settle_for_test();
        let closed = tree.stop_all("root", None).unwrap();
        tree.settle_for_test();
        assert!(closed.iter().any(|(id, _)| id == &spawned.agent_id));
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
    }

    #[test]
    fn stop_all_with_shared_host_operation_closes_every_sibling() {
        let (_tmp, mut tree) = tree();
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let closed = tree.stop_all("root", Some("op-stop-all")).unwrap();
        tree.settle_for_test();
        assert!(closed.iter().any(|(id, _)| id == &first.agent_id));
        assert!(closed.iter().any(|(id, _)| id == &second.agent_id));
        assert_eq!(
            tree.node(&first.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(
            tree.node(&second.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
    }

    /// After `interrupt_agent` the
    /// child read `recovery_required` / `session_materialization` and its
    /// parent's inbox got a `result` row, so it could be neither steered nor
    /// re-tasked. An interrupt cancels *this
    /// turn*; the node stays reusable, seals nothing and pushes nothing. Once
    /// the cancelled turn has unwound (`release_attempt_turn`), the next mail
    /// starts a fresh turn on the same attempt and the node runs again.
    #[test]
    fn interrupted_child_keeps_its_attempt_pushes_no_result_and_resumes_on_the_next_mail() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "partial\n",
        )
        .unwrap();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        tree.bind_attempt_turn(&child.agent_id, "turn-1").unwrap();
        tree.interrupt_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        // The host waiter observes the cancelled turn and releases it.
        tree.release_attempt_turn(&child.agent_id).unwrap();

        let node = tree.node(&child.agent_id).unwrap();
        assert_eq!(node.status, NodeStatus::Interrupted);
        assert!(!node.active_turn);
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.attempt_id, child.attempt_id);
        assert_eq!(attempt.status, AttemptStatus::Running);
        assert!(attempt.turn_id.is_none(), "the cancelled turn is released");
        assert!(attempt.failure.is_none(), "no failure kind on an interrupt");
        assert!(tree.candidates().is_empty(), "an interrupt seals nothing");
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(
            listed
                .inbox
                .iter()
                .filter(|mail| mail.purpose == MessagePurpose::Result)
                .count(),
            0,
            "an interrupt pushes no result: {:?}",
            listed.inbox
        );

        // An urgent steer is the resume: with no bound turn it starts one.
        let mut steer = mail(&child.agent_id, "stop sleeping, finish and submit");
        steer.priority = Some(MessagePriority::Urgent);
        let sent = tree.send_agent_message("root", &steer, None).unwrap();
        assert_eq!(sent.disposition, MailDisposition::Started);
        // ...which the tool layer records exactly like a spawn start.
        tree.set_active_turn(&child.agent_id, true).unwrap();
        tree.bind_attempt_turn(&child.agent_id, "turn-2").unwrap();
        let node = tree.node(&child.agent_id).unwrap();
        assert_eq!(node.status, NodeStatus::Running, "the node runs again");
        assert!(node.active_turn);
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.attempt_id, child.attempt_id, "same attempt");
        assert_eq!(attempt.turn_id.as_deref(), Some("turn-2"));
        assert_eq!(attempt.usage.turns, 2, "the resume is a second turn");
        // The wait parks again on the running child.
        assert!(
            tree.poll_wait("root", &wait_for(&child.agent_id))
                .unwrap()
                .is_none()
        );
    }

    /// `followup` is refused only for `Closed`/`RecoveryRequired`
    /// nodes and discarded copies. An interrupted child whose turn has ended
    /// can be re-tasked: the new attempt supersedes the interrupted one, which
    /// ends `cancelled` without a result. While the cancelled turn is still
    /// unwinding (bound turn present) the followup is refused as not idle.
    #[test]
    fn followup_after_interrupt_opens_a_new_attempt_and_cancels_the_interrupted_one() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        tree.bind_attempt_turn(&child.agent_id, "turn-1").unwrap();
        tree.interrupt_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        let mut followup = mail(&child.agent_id, "create LATER.md and submit");
        followup.purpose = Some(MessagePurpose::Followup);
        let error = tree
            .send_agent_message("root", &followup, None)
            .unwrap_err();
        assert_eq!(
            error.code,
            TopologyErrorCode::FollowupNotIdle,
            "the turn has not unwound yet: {error:?}"
        );

        tree.release_attempt_turn(&child.agent_id).unwrap();
        let sent = tree.send_agent_message("root", &followup, None).unwrap();
        assert_eq!(sent.disposition, MailDisposition::Started);
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_ne!(attempt.attempt_id, child.attempt_id, "a new attempt");
        assert_eq!(attempt.status, AttemptStatus::Running);
        assert_eq!(attempt.retry_of.as_ref(), Some(&child.attempt_id));
        let interrupted = tree.attempts.get(&child.attempt_id).unwrap();
        assert_eq!(interrupted.status, AttemptStatus::Cancelled);
        assert!(interrupted.failure.is_none());
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Running
        );
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert!(
            listed
                .inbox
                .iter()
                .all(|mail| mail.purpose != MessagePurpose::Result),
            "{:?}",
            listed.inbox
        );
    }

    #[test]
    fn bind_after_materialization_failure_restores_running() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        tree.bind_session_locator(&spawned.agent_id, "session:c1")
            .unwrap();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        tree.record_session_bound_after_failure(&spawned.agent_id)
            .unwrap();
        tree.fold_due(&spawned.agent_id).unwrap();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Running
        );
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::Running
        );
        assert_eq!(
            tree.node(&spawned.agent_id)
                .unwrap()
                .session_locator
                .as_deref(),
            Some("session:c1")
        );
    }

    #[test]
    fn prepared_spawn_replays_and_restore_refuses_unfinished_operation() {
        let (_tmp, mut tree) = tree();
        let req = spawn_req();
        assert!(
            tree.prepare_operation("op-spawn-live", "root", "spawn_agent", &req)
                .unwrap()
                .is_none()
        );
        let first = tree
            .spawn_agent("root", &req, Some("op-spawn-live"))
            .unwrap();
        let replay = tree
            .spawn_agent("root", &req, Some("op-spawn-live"))
            .unwrap();
        assert_eq!(replay.agent_id, first.agent_id);
        assert_eq!(replay.attempt_id, first.attempt_id);
        tree.prepare_operation("op-spawn-crash", "root", "spawn_agent", &req)
            .unwrap();
        tree.restore_paused();
        assert_eq!(
            tree.spawn_agent("root", &req, Some("op-spawn-crash"))
                .unwrap_err()
                .code,
            TopologyErrorCode::OperationRecoveryRequired
        );
    }

    /// B9: a child that needs recovery is not idle, and a followup would open
    /// a second attempt over a copy whose state nobody has accounted for.
    #[test]
    fn followup_to_a_recovery_required_child_is_refused() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_fault(&child.agent_id, "the session went away");
        tree.record_host_materialization(&child.agent_id)
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let mut followup = mail(&child.agent_id, "carry on");
        followup.purpose = Some(MessagePurpose::Followup);
        let error = tree
            .send_agent_message("root", &followup, None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::FollowupRecoveryRequired);
        assert!(error.message.contains("recovery"), "{}", error.message);
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().attempt_id,
            child.attempt_id,
            "no second attempt was opened"
        );
    }

    /// Gate 3: `AgentErrorKind::Cancelled` also arrives from a provider, on a
    /// node the host never interrupted. Ending the attempt is what lets the
    /// parent's `wait_agent` settle; leaving it `running` stranded the node.
    #[test]
    fn a_cancel_the_host_did_not_ask_for_ends_the_attempt() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "partial\n").unwrap();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Running
        );
        tree.record_host_cancellation(&child.agent_id)
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        let node = tree.node(&child.agent_id).unwrap();
        assert_eq!(node.status, NodeStatus::Failed);
        assert!(node.status.settles_a_wait(), "the parent's wait settles");
        let attempt = tree.attempt(&child.agent_id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.kind.as_str()),
            Some(CANCELLED_KIND)
        );
        assert!(attempt.candidate_id.is_some(), "partial work still seals");
    }
}
