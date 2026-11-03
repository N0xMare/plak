use super::{
    Arc, AttemptId, AttemptRecord, BTreeMap, BTreeSet, CandidateId, CandidateRecord,
    CellAgentBudget, DEFAULT_DIRECT_CHILDREN, DEFAULT_GRANT_DEPTH, DEFAULT_TREE_ATTEMPTS,
    DEFAULT_TREE_NODES, Effects, HashSet, LiveGeneration, MAX_TREE_ATTEMPTS, MailRecord,
    NodeRecord, NodeStatus, OperationRecord, OrchestrationSnapshot, PathBuf, ProductAgentTree,
    SystemVcs, Tier, TopologyError, TopologyErrorCode, TopologyGrant, TreeAdmissionState, TreeVcs,
    VcsOps, WorkspaceMode, WorkspaceRecord, WorkspaceStatus, WriterMode, valid_id,
};
use super::{NodePlacement, TreeLineage};

/// The VCS port of this process's sessions: the system `git`/`jj` port on
/// the local runner, and on another runner a port that refuses every effect,
/// since version control there is not carried yet.
fn session_vcs() -> Arc<dyn VcsOps> {
    if plakous::tools::runner::process_runner().is_some() {
        Arc::new(plakous_workspace::occupancy_vcs::UnavailableVcs)
    } else {
        Arc::new(SystemVcs)
    }
}

impl ProductAgentTree {
    /// Bootstraps a root over the system `git`/`jj` port, or the selected
    /// runner's refusing port.
    ///
    /// # Errors
    ///
    /// As [`Self::bootstrap_root_with`].
    pub async fn bootstrap_root(
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        Self::bootstrap_root_with(
            session_vcs(),
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple,
            writer_mode,
        )
        .await
    }

    /// [`Self::bootstrap_root`] over an explicit VCS port. The root's branch
    /// label is the first thing the port answers, before the tree exists.
    ///
    /// # Errors
    ///
    /// Refuses a tree or root id that is not a portable agent id.
    pub async fn bootstrap_root_with(
        vcs: Arc<dyn VcsOps>,
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        let tree_id = tree_id.into();
        let root_agent_id = root_agent_id.into();
        let selected_tuple = selected_tuple.into();
        if !valid_id(&tree_id) || !valid_id(&root_agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "tree or root id is not a portable agent id",
            ));
        }
        let workspace_id = format!("ws-{root_agent_id}");
        if !valid_id(&workspace_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "root workspace id is not portable",
            ));
        }
        let branch = vcs.label(&launch_checkout).await;
        let root = NodeRecord {
            fold_order: Some(0),
            abandoned_turn: None,
            agent_id: root_agent_id.clone(),
            parent_agent_id: None,
            depth: 0,
            description: root_role.into(),
            prompt: String::new(),
            output_schema_json: String::new(),
            schema_digest: String::new(),
            template: None,
            tuple_id: selected_tuple.clone(),
            account: String::new(),
            effort: None,
            status: NodeStatus::Running,
            workspace_id: workspace_id.clone(),
            tier: Tier::Mutate,
            attempt_id: None,
            session_locator: None,
            path: launch_checkout.clone(),
            branch,
            active_turn: false,
            line_id: None,
            epoch_no: 0,
            placement: NodePlacement::default(),
            parent_composition: None,
        };
        let grant = TopologyGrant {
            visible_ids: vec![root_agent_id.clone()],
            message_peers: Vec::new(),
            managed_ids: Vec::new(),
            may_spawn: true,
            may_adopt: true,
            max_depth: DEFAULT_GRANT_DEPTH,
            max_nodes: DEFAULT_TREE_NODES as u8,
            max_children: DEFAULT_DIRECT_CHILDREN as u8,
            interrupt_ids: Vec::new(),
            close_ids: Vec::new(),
        };
        let workspace = WorkspaceRecord {
            conflict_base: None,
            resolves: None,
            conflict_target: None,
            conflict_paths: Vec::new(),
            input_candidate_id: None,
            workspace_id: workspace_id.clone(),
            owner_agent_id: root_agent_id.clone(),
            path: launch_checkout.clone(),
            mode: WorkspaceMode::Inherit,
            status: WorkspaceStatus::Active,
            base_commit: None,
            base_content: None,
            base_change_id: None,
            generation: 0,
            adopter_agent_id: None,
        };
        Ok(Self::bootstrap_root_records(
            vcs,
            tree_id,
            selected_tuple,
            writer_mode,
            root,
            grant,
            workspace,
        ))
    }

    fn bootstrap_root_records(
        vcs: Arc<dyn VcsOps>,
        tree_id: String,
        selected_tuple: String,
        writer_mode: WriterMode,
        root: NodeRecord,
        grant: TopologyGrant,
        workspace: WorkspaceRecord,
    ) -> Self {
        let root_agent_id = root.agent_id.clone();
        let launch_checkout = root.path.clone();
        let workspace_id = workspace.workspace_id.clone();
        let mut nodes = BTreeMap::new();
        nodes.insert(root_agent_id.clone(), root);
        let mut grants = BTreeMap::new();
        grants.insert(root_agent_id.clone(), grant);
        let mut workspaces = BTreeMap::new();
        workspaces.insert(workspace_id.clone(), workspace);
        let mut allowed = BTreeSet::new();
        allowed.insert(selected_tuple.clone());
        Self {
            tree_id,
            root_agent_id,
            revision: 1,
            stored_revision: 0,
            host_generation: 1,
            state: TreeAdmissionState::Live,
            writer_mode,
            allowed_tuples: allowed,
            selected_tuple: selected_tuple.clone(),
            launch_checkout,
            snapshot: OrchestrationSnapshot::unpinned(selected_tuple),
            lineage: TreeLineage::default(),
            nodes,
            grants,
            attempts: BTreeMap::new(),
            candidates: BTreeMap::new(),
            workspaces,
            messages: BTreeMap::new(),
            inbound: BTreeMap::new(),
            next_message_sequence: 1,
            parked_waits: BTreeMap::new(),
            delivery_claims: BTreeSet::new(),
            start_reservations: BTreeMap::new(),
            turn_waiters: BTreeMap::new(),
            pending_close_folds: BTreeSet::new(),
            turn_owners: BTreeMap::new(),
            parked: BTreeMap::new(),
            operations: BTreeMap::new(),
            pending_spawns: 0,
            root_pending_spawns: 0,
            minting: BTreeSet::new(),
            live_generations: BTreeMap::from([(workspace_id.clone(), LiveGeneration::new(0))]),
            effects: Effects::default(),
            base_pin_recovery: BTreeMap::new(),
            #[cfg(test)]
            fold_scratch_clones: 0,
            pending_turn_usages: BTreeMap::new(),
            compaction_views: BTreeMap::new(),
            compaction_cursors: BTreeMap::new(),
            outstanding_seals: BTreeMap::new(),
            inflight_adopts: BTreeSet::new(),
            inflight_refreshes: BTreeSet::new(),
            detach_emitted: HashSet::new(),
            delivered: BTreeMap::new(),
            admission_marks: BTreeSet::new(),
            commands: BTreeMap::new(),
            feed_cursors: BTreeMap::new(),
            prune_failed: BTreeSet::new(),
            removals_in_flight: BTreeSet::new(),
            fault_reasons: BTreeMap::new(),
            reclaim_backoff: BTreeMap::new(),
            reclaim_trim_epochs: BTreeMap::new(),
            adopt_conflicts: BTreeMap::new(),
            terminal_recovery: BTreeMap::new(),
            max_attempts: DEFAULT_TREE_ATTEMPTS,
            prompt_attempt_base: 0,
            notify: Arc::new(tokio::sync::Notify::new()),
            vcs: TreeVcs(vcs),
            cell_budgets: BTreeMap::from([(
                0,
                Arc::new(CellAgentBudget::for_snapshot(
                    &OrchestrationSnapshot::unpinned(String::new()),
                )),
            )]),
        }
    }

    /// The tree's VCS port, shared: what a runtime runs the tree's effects
    /// on when it has not installed a port of its own.
    #[must_use]
    pub fn vcs_port(&self) -> Arc<dyn VcsOps> {
        Arc::clone(&self.vcs.0)
    }

    /// Installs the runtime's VCS port, so the mint inside `spawn_agent` and
    /// the effects the runtime runs unlocked drive one implementation.
    pub fn set_vcs(&mut self, vcs: Arc<dyn VcsOps>) {
        self.vcs = TreeVcs(vcs);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "exact persisted tree reconstruction"
    )]
    /// Rebuilds a tree from the rows a store handed back, exactly as they
    /// were written. Applying the reopen law — paused admission, no restored
    /// turn — is [`Self::reopen`]'s job, not this one's.
    pub fn from_persisted(
        tree_id: String,
        root_agent_id: String,
        revision: u64,
        host_generation: u64,
        state: TreeAdmissionState,
        writer_mode: WriterMode,
        selected_tuple: String,
        launch_checkout: PathBuf,
        snapshot: OrchestrationSnapshot,
        nodes: BTreeMap<String, NodeRecord>,
        grants: BTreeMap<String, TopologyGrant>,
        attempts: BTreeMap<AttemptId, AttemptRecord>,
        candidates: BTreeMap<CandidateId, CandidateRecord>,
        workspaces: BTreeMap<String, WorkspaceRecord>,
        messages: BTreeMap<String, MailRecord>,
        operations: BTreeMap<String, OperationRecord>,
    ) -> Self {
        let mut allowed_tuples = BTreeSet::new();
        allowed_tuples.insert(selected_tuple.clone());
        for node in nodes.values() {
            allowed_tuples.insert(node.tuple_id.clone());
        }
        let mut inbound = BTreeMap::<String, Vec<String>>::new();
        // Each recipient retains its newest acknowledged records, including
        // the high-water id, so pruning never loses the minting sequence.
        let next_message_sequence = messages
            .keys()
            .filter_map(|id| id.strip_prefix('m')?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut ordered_messages: Vec<_> = messages.values().collect();
        // Generated ids are canonical m<positive decimal sequence>. Length then
        // lexical order preserves their insertion order across the digit boundary.
        ordered_messages.sort_by(|left, right| {
            left.message_id
                .len()
                .cmp(&right.message_id.len())
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        for mail in ordered_messages {
            inbound
                .entry(mail.to.clone())
                .or_default()
                .push(mail.message_id.clone());
        }
        let (pending_spawns, root_pending_spawns) = pending_spawn_counts(&nodes, &root_agent_id);
        let live_generations = workspaces
            .iter()
            .map(|(id, row)| (id.clone(), LiveGeneration::new(row.generation)))
            .collect();
        let cell_budgets =
            BTreeMap::from([(0, Arc::new(CellAgentBudget::for_snapshot(&snapshot)))]);
        Self {
            tree_id,
            root_agent_id,
            revision,
            stored_revision: revision,
            host_generation,
            state,
            writer_mode,
            allowed_tuples,
            selected_tuple,
            launch_checkout,
            snapshot,
            lineage: TreeLineage::default(),
            nodes,
            grants,
            attempts,
            candidates,
            workspaces,
            messages,
            inbound,
            next_message_sequence,
            parked_waits: BTreeMap::new(),
            delivery_claims: BTreeSet::new(),
            start_reservations: BTreeMap::new(),
            turn_waiters: BTreeMap::new(),
            pending_close_folds: BTreeSet::new(),
            turn_owners: BTreeMap::new(),
            parked: BTreeMap::new(),
            operations,
            pending_spawns,
            root_pending_spawns,
            minting: BTreeSet::new(),
            live_generations,
            effects: Effects::default(),
            base_pin_recovery: BTreeMap::new(),
            #[cfg(test)]
            fold_scratch_clones: 0,
            pending_turn_usages: BTreeMap::new(),
            compaction_views: BTreeMap::new(),
            compaction_cursors: BTreeMap::new(),
            outstanding_seals: BTreeMap::new(),
            inflight_adopts: BTreeSet::new(),
            inflight_refreshes: BTreeSet::new(),
            detach_emitted: HashSet::new(),
            delivered: BTreeMap::new(),
            admission_marks: BTreeSet::new(),
            commands: BTreeMap::new(),
            feed_cursors: BTreeMap::new(),
            prune_failed: BTreeSet::new(),
            removals_in_flight: BTreeSet::new(),
            fault_reasons: BTreeMap::new(),
            reclaim_backoff: BTreeMap::new(),
            reclaim_trim_epochs: BTreeMap::new(),
            adopt_conflicts: BTreeMap::new(),
            terminal_recovery: BTreeMap::new(),
            max_attempts: MAX_TREE_ATTEMPTS,
            prompt_attempt_base: 0,
            notify: Arc::new(tokio::sync::Notify::new()),
            vcs: TreeVcs(session_vcs()),
            cell_budgets,
        }
    }
}

/// The pending spawns persisted rows hold, all and the root's own: a node
/// still `pending` is a spawn that has not settled.
fn pending_spawn_counts(nodes: &BTreeMap<String, NodeRecord>, root_agent_id: &str) -> (u32, u32) {
    let pending = || {
        nodes
            .values()
            .filter(|node| node.status == NodeStatus::Pending)
    };
    let count = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
    (
        count(pending().count()),
        count(
            pending()
                .filter(|node| node.parent_agent_id.as_deref() == Some(root_agent_id))
                .count(),
        ),
    )
}
