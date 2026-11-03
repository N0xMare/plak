use crate::epoch::BindHints;

use super::{
    AdoptCandidateRequest, AdoptOutcome, Arc, AttemptRecord, ListAgentsOutput, ListAgentsRequest,
    NodeStatus, PathBuf, ProductAgentTree, SpawnAdmission, SpawnAgentOutput, SpawnAgentRequest,
    SubmitNudgeBudget, SystemVcs, TopologyError, TopologyErrorCode, VcsOps, WriterMode,
    topology_error_from_vcs,
};

/// Liveness that holds every holder gone: the stores' tests take over a
/// claim without a host liveness lock.
struct AssumeHolderGone;

impl super::custody::sealed::Sealed for AssumeHolderGone {}

impl super::CustodyLiveness for AssumeHolderGone {
    fn holder_gone(&self, _holder: &super::CustodyHost) -> bool {
        true
    }
}

impl ProductAgentTree {
    /// A takeover proof for `claim` as if its holder were proven gone, for
    /// store tests that have no host liveness lock.
    #[must_use]
    pub fn holder_gone_for_test(claim: super::CustodyClaim) -> super::HolderGone {
        super::HolderGone::prove(claim, &AssumeHolderGone)
            .unwrap_or_else(|| unreachable!("assumed liveness proves every holder gone"))
    }

    /// Spends one admitted submit-harness Start on the current attempt.
    /// The budget is persisted, so a crash-cut rejoin cannot
    /// reset it, and the harness counts against the template's turn ceiling
    /// like any other turn.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent and an attempt with no turns left.
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

    /// `bootstrap_root` plus [`crate::epoch::RosterInput::test_working_set`], so a test
    /// tree can spawn `SpawnAgentRequest::test_review()` /
    /// `test_orchestrate()` without installing a config-derived epoch.
    pub fn bootstrap_root_for_test(
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        Self::bootstrap_root_for_test_with(
            Arc::new(SystemVcs),
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple,
            writer_mode,
        )
    }

    /// [`Self::bootstrap_root_for_test`] over an explicit VCS port, for the
    /// tests that run on `FakeVcs`.
    pub fn bootstrap_root_for_test_with(
        vcs: Arc<dyn VcsOps>,
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        let selected_tuple = selected_tuple.into();
        let mut tree = super::block_on_for_test(Self::bootstrap_root_with(
            vcs,
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple.clone(),
            writer_mode,
        ))?;
        let working_set = crate::epoch::RosterInput::test_working_set(selected_tuple);
        for (name, binding) in working_set.templates {
            tree.pin_template(name.as_str(), binding);
        }
        tree.pin_bind_hints_for_test(working_set.bind_hints);
        Ok(tree)
    }

    /// Installs an account roster on the live epoch. Production epochs carry
    /// the product's roster in through `install_snapshot`; a tree bootstrapped
    /// straight into a test admits only its working set's `codex` cards until
    /// this runs, so the caller — the product, or a fixture — says which
    /// accounts its epoch admits.
    pub fn pin_bind_hints_for_test(&mut self, hints: BindHints) {
        self.snapshot.bind_hints = hints;
        self.snapshot.digest = self.snapshot.compute_digest();
    }

    /// Forces one node's status, so a test can stage a state the tree would
    /// otherwise only reach through a live turn.
    pub fn set_node_status_for_test(&mut self, agent_id: &str, status: NodeStatus) {
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = status;
        }
    }

    /// The node's current attempt, mutable, so a test can stage usage or a
    /// failure the runtime would have written.
    pub fn attempt_mut_for_test(&mut self, agent_id: &str) -> &mut AttemptRecord {
        self.current_attempt_mut(agent_id)
            .unwrap_or_else(|| unreachable!("test node {agent_id} has an attempt"))
    }

    /// The tree's own port, for the conveniences below and the tree's tests.
    pub(super) fn vcs(&self) -> &dyn VcsOps {
        self.vcs.0.as_ref()
    }

    /// Spawns a child inline: list the copy root, reserve, run the mint the
    /// reservation owes through the tree's own port, commit, blocking on
    /// each port answer. The convenience for tests.
    ///
    /// Production drives [`Self::spawn_reserve`] / [`Self::spawn_commit`]
    /// through the runtime so the port answers with the tree mutex released.
    pub fn spawn_agent(
        &mut self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        operation_id: Option<&str>,
    ) -> Result<SpawnAgentOutput, TopologyError> {
        let copies = super::block_on_for_test(self.vcs().jj_copy_dirs(&self.launch_checkout));
        match self.spawn_reserve(caller_id, request, operation_id, &copies)? {
            SpawnAdmission::Replayed(output) => Ok(output),
            SpawnAdmission::Reserved(reservation, None) => self.spawn_commit(*reservation, None),
            SpawnAdmission::Reserved(reservation, Some(ticket)) => {
                match super::block_on_for_test(ticket.mint(self.vcs())) {
                    Ok(minted) => self.spawn_commit(*reservation, Some(minted)),
                    Err(error) => {
                        Err(self.spawn_abort(*reservation, topology_error_from_vcs(error)))
                    }
                }
            }
        }
    }

    /// Renders `list_agents` through the tree's own port, blocking on each
    /// probe: the in-process convenience for tests. Production drives
    /// [`Self::occupancy_probe`] / [`Self::list_agents_with`] through the
    /// runtime so the probes run with the tree mutex released.
    pub fn list_agents(
        &self,
        caller_id: &str,
        request: &ListAgentsRequest,
    ) -> Result<ListAgentsOutput, TopologyError> {
        let occupancy =
            super::block_on_for_test(self.occupancy_probe(caller_id, request)?.run(self.vcs()));
        self.list_agents_with(caller_id, request, &occupancy)
    }

    /// `adopt_candidate` driven synchronously through the
    /// tree's own port, for tests: authority checks, the landing, and the
    /// receipt in one call. Production runs the landing as an effect with
    /// the mutex released (`begin_adopt` queues the ticket; the runtime
    /// completes it) and keeps the operation ledger itself.
    pub fn adopt_candidate(
        &mut self,
        caller_id: &str,
        request: &AdoptCandidateRequest,
        operation_id: Option<&str>,
    ) -> Result<AdoptOutcome, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "adopt_candidate", request)? {
            return Ok(prior);
        }
        self.begin_adopt_for_test(caller_id, request)?;
        let ticket = self
            .effects
            .adopts
            .pop()
            .unwrap_or_else(|| unreachable!("begin_adopt queued the ticket"));
        let landed = super::block_on_for_test(ticket.land(self.vcs()));
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

    /// [`Self::begin_adopt`] with its probe answered through the tree's own
    /// port, blocking on each answer: the convenience for tests.
    ///
    /// # Errors
    ///
    /// As [`Self::begin_adopt`].
    pub fn begin_adopt_for_test(
        &mut self,
        caller_id: &str,
        request: &AdoptCandidateRequest,
    ) -> Result<(), TopologyError> {
        let facts = super::block_on_for_test(self.adopt_probe(caller_id, request).run(self.vcs()));
        self.begin_adopt(caller_id, request, &facts)
    }
}
