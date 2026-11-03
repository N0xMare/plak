use super::{
    AttemptId, AttemptRecord, AttemptStatus, BTreeSet, CandidateId, MAX_DIRECT_CHILDREN,
    MAX_PENDING_SPAWNS, MAX_TREE_DEPTH, MAX_TREE_NODES, MintedCopy, NodeRecord, NodeStatus,
    ProductAgentTree, SpawnAdmission, SpawnAgentOutput, SpawnAgentRequest, SpawnIdentity,
    SpawnReservation, SubmitNudgeBudget, TemplateName, TopologyError, TopologyErrorCode,
    TopologyGrant, UsageRow, VcsError, WorkspacePlan, WorkspaceReceipt, admit_output_schema,
    next_child_id, now_unix_millis, topology_error_from_vcs,
};

impl ProductAgentTree {
    /// The admission half of a spawn: every budget,
    /// grant, template, and schema check, the child id, and the workspace
    /// plan — with no subprocess. A copying tier comes back as a
    /// [`MintTicket`](super::super::MintTicket) the caller runs with the mutex
    /// released; the reserved id is held in the tree's `minting` set until
    /// [`Self::spawn_commit`] or [`Self::spawn_abort`] gives it back, so a
    /// second spawn racing this one cannot mint over the same copy.
    ///
    /// `jj_copy_dirs` is the port's listing of the JJ copy root, asked before
    /// the lock: a child id whose copy directory exists without a row (a
    /// crashed mint) is skipped. A port that could not list it refuses the
    /// spawn after the replay checks.
    #[allow(
        clippy::too_many_lines,
        reason = "spawn admission validates grants, roster, schema and capacity before reserving a single workspace identity"
    )]
    pub fn spawn_reserve(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
        jj_copy_dirs: &Result<BTreeSet<String>, VcsError>,
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
        let jj_copy_dirs = jj_copy_dirs
            .as_ref()
            .map_err(|error| topology_error_from_vcs(error.clone()))?;

        self.pending_spawns = self.pending_spawns.saturating_add(1);
        let agent_id = next_child_id(
            self.collected_child_floor(),
            &self.tree_id,
            &self.nodes,
            &self.workspaces,
            &self.minting,
            jj_copy_dirs,
        );
        let plan = match self.plan_workspace(
            caller_id,
            tier,
            request.workspace,
            &agent_id,
            input_base.as_ref(),
            request.input_ids().first().map(String::as_str),
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
    #[allow(
        clippy::too_many_lines,
        reason = "spawn commit installs the reserved node, attempt, grant and workspace as one acknowledged transition"
    )]
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
            (None, Some(minted)) => self.record_minted(
                &caller_id,
                &agent_id,
                mode,
                minted,
                request
                    .input_ids()
                    .first()
                    .cloned()
                    .map(CandidateId::from_persisted),
            ),
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

    pub(in crate::tree) fn holds_direct_child_slot(&self, node: &NodeRecord) -> bool {
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

    pub(super) fn holds_tree_slot(&self, node: &NodeRecord) -> bool {
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
    pub(in crate::tree) fn replay_request_key(
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
}
