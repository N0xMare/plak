use crate::epoch::BindHints;

use super::{
    Arc, AttemptRecord, NodeStatus, PathBuf, ProductAgentTree, SubmitNudgeBudget, SystemVcs,
    TopologyError, TopologyErrorCode, VcsOps, WriterMode,
};

impl ProductAgentTree {
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
        let mut tree = Self::bootstrap_root_with(
            vcs,
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple.clone(),
            writer_mode,
        )?;
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
}
