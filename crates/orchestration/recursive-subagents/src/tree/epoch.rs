//! Orchestration snapshot install, template pinning, successor epochs, and
//! grant compilation.

use super::{
    Arc, CellAgentBudget, EpochRebindRefusal, MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS,
    MAX_TREE_DEPTH, MAX_TREE_NODES, NodeStatus, OrchestrationSnapshot, ProductAgentTree,
    RosterBinding, TemplateName, TopologyError, TopologyErrorCode, TreeAdmissionState,
};

#[cfg(any(test, feature = "test-support"))]
use super::TopologyGrant;

impl ProductAgentTree {
    /// The topology tools this caller is granted right now, in wire order.
    ///
    /// `spawn_agent` is withheld when the caller's spawn menu is empty: an
    /// advertised tool that can only fail closed is worse than an absent one.
    ///
    /// # Errors
    ///
    /// Refuses when `caller_id` is not a node of this tree.
    pub fn compiled_tool_names(&self, caller_id: &str) -> Result<Vec<&'static str>, TopologyError> {
        let grant = self.grant(caller_id)?;
        let node = self.node(caller_id)?;
        let remaining_depth = node.depth < grant.max_depth;
        let mut names = grant.compiled_tool_names(caller_id == self.root_agent_id, remaining_depth);
        // An empty menu must not advertise spawn_agent. The tree API still
        // fails closed with template_not_spawnable or template_unknown.
        let caller_template = node.template.as_ref().map(TemplateName::as_str);
        if self
            .snapshot
            .spawnable_template_names(caller_template)
            .next()
            .is_none()
        {
            names.retain(|name| *name != "spawn_agent");
        }
        Ok(names)
    }

    /// Names a node's account when it has none, so a test can stand in for
    /// the bind the session would have done.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_node_account_for_test(&mut self, id: &str, account: impl Into<String>) {
        self.set_node_account_if_empty(id, account);
    }

    /// One node's grant, mutable, so a test can narrow authority the epoch
    /// would otherwise have compiled.
    #[cfg(any(test, feature = "test-support"))]
    pub fn grants_mut_for_test(&mut self, agent_id: &str) -> &mut TopologyGrant {
        self.grants
            .get_mut(agent_id)
            .unwrap_or_else(|| unreachable!("test node {agent_id} has a grant"))
    }

    /// Records the account a node actually bound, but only when it has none:
    /// the epoch's card is authority, and a later observation must not move a
    /// bind the roster already made.
    pub fn set_node_account_if_empty(&mut self, id: &str, account: impl Into<String>) {
        if let Some(node) = self.nodes.get_mut(id)
            && node.account.is_empty()
        {
            node.account = account.into();
        }
    }

    /// The card a spawn binds. A child binds exactly what its card names —
    /// account, model, effort and tier — and never its caller's.
    pub(super) fn resolve_template_binding(
        &self,
        template: &TemplateName,
    ) -> Result<(RosterBinding, String, String, Option<String>), TopologyError> {
        let Some(binding) = self.snapshot.binding(template) else {
            return Err(TopologyError::new(
                TopologyErrorCode::TemplateUnknown,
                format!("template {template} is not in this tree epoch's roster"),
            ));
        };
        if binding.account.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "template names no account",
            ));
        }
        if binding.tuple_id.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "template names no model",
            ));
        }
        // The epoch's frozen `bind_hints` is the roster; the tree only asks
        // whether this account is in it. Every spawn path — a product binder
        // or the binder-less JSONL `invoke_tool` on the hub — reaches these
        // checks, so the fence is the same on all of them.
        let hints = &self.snapshot.bind_hints;
        if !hints.admits_account(&binding.account) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "template account is not a supported tree account",
            ));
        }
        // `native-ptc` is the vertical's own PTC system; an account the
        // product lists as enrolling none cannot carry it. Per-model admission is the
        // product's, at the child pin.
        if hints.refuses_native_protocol(&binding.account)
            && binding.composition
                == Some(plakous_host_protocol::selection::CompositionSelection::NativePtc)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "template composition must be direct|js-ptc|py-ptc for this account",
            ));
        }
        Ok((
            binding.clone(),
            binding.account.clone(),
            binding.tuple_id.clone(),
            binding.effort.clone(),
        ))
    }

    /// Makes `snapshot` this tree's epoch: bounds are re-clamped, the
    /// per-cell budget is rebuilt, and the working set replaced.
    ///
    /// Only ever called on a quiescent tree — genesis, or a successor epoch
    /// past `epoch_allows_rebind` — so no cell is mid-loop when its counter
    /// is replaced.
    pub fn install_snapshot(&mut self, snapshot: OrchestrationSnapshot) {
        self.writer_mode = snapshot.writer_mode;
        if let Some(root_grant) = self.grants.get_mut(&self.root_agent_id) {
            root_grant.max_depth = snapshot.max_depth.clamp(1, MAX_TREE_DEPTH);
            root_grant.max_nodes = snapshot.max_nodes.clamp(1, MAX_TREE_NODES as u8);
            root_grant.max_children = snapshot.max_children.clamp(1, MAX_DIRECT_CHILDREN as u8);
        }
        // the epoch's `[orchestration] max_attempts` is a
        // lower-only clamp of the host constant, re-read on every install so a
        // successor epoch can raise it back up to the constant.
        self.max_attempts = MAX_TREE_ATTEMPTS;
        self.lower_max_attempts(usize::from(snapshot.max_attempts));
        // the per-cell agent bound is epoch state too. An
        // install only happens on a quiescent tree (genesis, or a successor
        // epoch after `epoch_allows_rebind`), so no cell is mid-loop when its
        // counter is replaced.
        self.cell_budget = Arc::new(CellAgentBudget::for_snapshot(&snapshot));
        self.snapshot = snapshot;
        self.bump();
    }

    /// Start a successor epoch after TreeTerminal rebind. Same tree rows stay
    /// inspectable; bind authority is the new snapshot.
    pub fn begin_successor_epoch(
        &mut self,
        snapshot: OrchestrationSnapshot,
    ) -> Result<(), TopologyError> {
        if !self.epoch_allows_rebind() {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "successor epoch requires a quiescent tree",
            ));
        }
        self.selected_tuple = snapshot.root_tuple.clone();
        let root_id = self.root_agent_id.clone();
        if let Some(root) = self.nodes.get_mut(&root_id) {
            if !snapshot.root_account.is_empty() {
                root.account = snapshot.root_account.clone();
            }
            root.tuple_id = snapshot.root_tuple.clone();
            root.effort = snapshot.root_effort.clone();
        }
        self.install_snapshot(snapshot);
        self.state = TreeAdmissionState::Live;
        Ok(())
    }

    /// Pins one template into the live epoch's working set. Production
    /// epochs come from `install_snapshot`; this is the unit-test and
    /// bootstrap path for a single binding.
    pub fn pin_template(&mut self, name: &str, mut binding: RosterBinding) {
        let name = TemplateName::parse(name).unwrap_or_else(|error| {
            unreachable!("pin_template takes a valid template name: {error}")
        });
        if binding.template.is_empty() {
            binding.template = name.as_str().to_owned();
        }
        self.snapshot.working_set.insert(name.clone());
        self.snapshot.roster.insert(name, binding);
        self.snapshot.digest = self.snapshot.compute_digest();
    }

    /// Every refusal that a paused tree produces names the state in prose and
    /// the exact keystrokes that clear it.
    ///
    /// `ResumeTree` is the name of a request type and `restored_paused`
    /// is a wire value. The refusals state the condition and the operator's own
    /// way out of it instead.
    pub const TREE_PAUSED_REFUSAL: &str = "the agent tree is paused after an interrupt; open /tree and press r to \
         resume the tree, or relaunch with `plak run --resume-tree`";

    /// Refusal shown after stop-all until the next admitted root prompt.
    pub const TREE_STOPPED_REFUSAL: &str = "delegation is stopped for this turn; send a new prompt after all agents \
         have retired, or resolve any recovery-required agents first";

    /// Refusal shown when a roster apply arrives on a paused tree.
    pub const TREE_EPOCH_REBIND_PAUSED: &str =
        "cannot apply a roster while the agent tree is paused · /tree then r resumes it";

    /// Refusal shown when a rebind arrives while a spawn is mid-mint.
    pub const TREE_EPOCH_REBIND_PENDING_SPAWNS: &str =
        "cannot rebind while this tree epoch still has pending spawns";

    /// Refusal shown when a rebind arrives while descendants are still live.
    pub const TREE_EPOCH_REBIND_LIVE_DESCENDANTS: &str =
        "cannot rebind while this tree epoch still has live descendants";

    /// Whether the tree is quiescent enough for a successor epoch: no spawn
    /// mid-mint and no live descendant.
    #[must_use]
    pub fn epoch_allows_rebind(&self) -> bool {
        if self.pending_spawns > 0 {
            return false;
        }
        self.nodes
            .values()
            .all(|node| node.agent_id == self.root_agent_id || node.status.is_finished())
    }

    /// Distinct operator strings: paused vs pending spawns vs live descendants.
    /// RestoredPaused is reported even when the roster is otherwise quiescent.
    #[must_use]
    pub fn epoch_rebind_refusal(&self) -> Option<&'static str> {
        self.epoch_rebind_refusal_kind()
            .map(EpochRebindRefusal::message)
    }

    /// Typed form of [`Self::epoch_rebind_refusal`] for the roster apply.
    #[must_use]
    pub fn epoch_rebind_refusal_kind(&self) -> Option<EpochRebindRefusal> {
        if self.state == TreeAdmissionState::RestoredPaused {
            return Some(EpochRebindRefusal::RestoredPaused);
        }
        self.quiescence_refusal()
    }

    /// Whether the tree's own roster is quiescent: no pending spawn and no
    /// non-terminal descendant.
    ///
    /// This is the judgement a headless `plak config set` needs.
    /// `RestoredPaused` is an admission state for the *next* root turn on a
    /// tree that reopened with descendants in flight, not the quiescence
    /// judgement itself; a session-less apply consults only the roster.
    #[must_use]
    pub fn quiescence_refusal(&self) -> Option<EpochRebindRefusal> {
        if self.pending_spawns > 0 {
            return Some(EpochRebindRefusal::PendingSpawns);
        }
        if !self.epoch_allows_rebind() {
            return Some(EpochRebindRefusal::LiveDescendants);
        }
        None
    }

    /// A root interrupt reaches the children that are running or parked: each becomes
    /// `interrupted` with its attempt left open, exactly as `interrupt_agent`
    /// leaves it, so later mail resumes it and nothing it did is lost. The
    /// caller cancels the returned agents' live turns with the mutex released.
    ///
    /// # Errors
    /// Refuses malformed recorded intent data.
    pub fn interrupt_running_descendants(&mut self) -> Result<Vec<String>, TopologyError> {
        let root = self.root_agent_id.clone();
        let mut interrupted = Vec::new();
        let ids: Vec<String> = self.nodes.keys().cloned().collect();
        for id in ids {
            let parked = self.is_parked(&id) && !self.is_park_ending(&id);
            let owned = self.has_turn_owner(&id);
            if let Some(node) = self.nodes.get(&id)
                && id != root
                && (owned || parked)
                && node.status == NodeStatus::Running
            {
                self.record_operator_intent(
                    &id,
                    node.attempt_id.clone(),
                    super::operator_intents::Decision::Interrupt { effective: true },
                )?;
                interrupted.push(id);
            }
        }
        if !interrupted.is_empty() {
            self.bump();
        }
        Ok(interrupted)
    }

    /// Records that the root's turn was interrupted while work was still
    /// live, so the tree reads as interrupted rather than idle.
    pub fn mark_root_interrupted(&mut self) {
        if !self.epoch_allows_rebind() {
            self.state = TreeAdmissionState::Interrupted;
            self.bump();
        }
    }

    /// A new root prompt lifts a settled stop, without replacing the frozen
    /// roster or clearing any child recovery obligation, and starts the
    /// prompt's attempt budget. A recovery-required node runs nothing, so it
    /// cannot race the new epoch. It retires no await receipt: the previous
    /// terminal, open or the host's admission settlement already did.
    pub fn mark_root_live_for_prompt(&mut self) {
        self.prompt_attempt_base = self.attempts_spent();
        let stopped_and_settled = self.state == TreeAdmissionState::TreeTerminal
            && self.epoch_allows_rebind()
            && self
                .nodes
                .values()
                .all(|node| node.agent_id == self.root_agent_id || !node.active_turn);
        if self.state == TreeAdmissionState::Interrupted || stopped_and_settled {
            self.state = TreeAdmissionState::Live;
            self.bump();
        }
    }

    /// Re-reads the epoch state after a close, so a tree whose last live
    /// descendant just went away stops reading as interrupted.
    ///
    /// An idle tree is not a stopped one: closing the last child leaves the
    /// epoch `Live`, so the root can delegate another wave. Only the
    /// operator's explicit stop-all stops this turn (`TreeTerminal`).
    pub fn refresh_epoch_after_close(&mut self) {
        if self.epoch_allows_rebind() && self.state == TreeAdmissionState::Interrupted {
            self.state = TreeAdmissionState::Live;
            self.bump();
        }
    }

    /// Test-only: raise the pending-spawn counter so the epoch fence can be
    /// exercised without racing a real spawn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_pending_spawns_for_test(&mut self, pending: u32) {
        self.pending_spawns = pending;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::epoch::RosterInput;
    use crate::tree::test_support::*;
    use crate::tree::*;

    #[test]
    fn grant_compiler_hides_submit_on_root_and_spawn_on_child() {
        let (_tmp, mut tree) = tree();
        let root = tree.compiled_tool_names("root").unwrap();
        assert!(root.contains(&"spawn_agent"));
        assert!(root.contains(&"adopt_candidate"));
        assert!(!root.contains(&"submit_result"));
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let child = tree.compiled_tool_names(&spawned.agent_id).unwrap();
        assert!(child.contains(&"submit_result"));
        assert!(child.contains(&"send_agent_message"));
        assert!(!child.contains(&"spawn_agent"));
        assert!(!child.contains(&"adopt_candidate"));
        tree.bind_session_locator(&spawned.agent_id, "session:c1")
            .unwrap();
        assert_eq!(
            tree.node(&spawned.agent_id)
                .unwrap()
                .session_locator
                .as_deref(),
            Some("session:c1")
        );
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.attempt(&spawned.agent_id).unwrap().status,
            AttemptStatus::RecoveryRequired
        );
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert_eq!(
            tree.record_turn_completed(&spawned.agent_id)
                .unwrap_err()
                .code,
            TopologyErrorCode::RecipientRecoveryRequired
        );
        tree.settle_for_test();
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    #[test]
    fn unknown_template_fails_closed() {
        let (_tmp, mut tree) = tree();
        tree.snapshot.roster.clear();
        tree.snapshot.digest = tree.snapshot.compute_digest();
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::TemplateUnknown
        );
    }

    #[test]
    fn epoch_freeze_blocks_rebind_until_descendants_are_finished() {
        let (_tmp, mut tree) = tree();
        assert!(tree.epoch_allows_rebind());
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(!tree.epoch_allows_rebind());
        tree.close_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert!(tree.epoch_allows_rebind());
        assert_eq!(tree.state, TreeAdmissionState::Live);
        let mut input = RosterInput::unpinned("grok-4.6");
        input.root.account = "grok".to_owned();
        input.root.effort = Some("low".to_owned());
        let successor = OrchestrationSnapshot::freeze(input);
        tree.begin_successor_epoch(successor).unwrap();
        assert_eq!(tree.state, TreeAdmissionState::Live);
        assert_eq!(tree.selected_tuple, "grok-4.6");
        assert_eq!(tree.snapshot.root_tuple, "grok-4.6");
    }

    /// Closing the last child leaves an idle tree, not a stopped one: the
    /// root delegates wave after wave, including after failed children.
    #[test]
    fn closing_every_child_keeps_delegation_open_for_the_next_wave() {
        let (_tmp, mut tree) = tree();
        for _wave in 0..3 {
            let done = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            let failed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.record_host_materialization(&failed.agent_id)
                .and_then(|value| tree.fold_due(&failed.agent_id).map(|_| value))
                .unwrap();
            tree.settle_for_test();
            for id in [&done.agent_id, &failed.agent_id] {
                tree.close_agent("root", &close_req(id), None).unwrap();
                tree.settle_for_test();
            }
            assert!(tree.epoch_allows_rebind());
            assert_eq!(tree.state, TreeAdmissionState::Live);
        }
        tree.state = TreeAdmissionState::Interrupted;
        let last = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&last.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.state, TreeAdmissionState::Live);

        tree.stop_all("root", None).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
        let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::GrantDenied);
        assert_eq!(refused.message, ProductAgentTree::TREE_STOPPED_REFUSAL);
    }

    #[test]
    fn next_root_prompt_resumes_a_settled_stop_without_resetting_nodes() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.settle_for_test();
        tree.stop_all("root", None).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
        assert!(tree.spawn_agent("root", &spawn_req(), None).is_err());
        let count = tree.nodes.len();
        tree.mark_root_live_for_prompt();
        assert_eq!(tree.state, TreeAdmissionState::Live);
        assert_eq!(tree.nodes.len(), count);
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed
        );
        assert!(tree.spawn_agent("root", &spawn_req(), None).is_ok());
    }

    /// A recovery-required child runs nothing, so it does not hold a settled
    /// stop; the next root prompt lifts the stop and leaves the child's
    /// recovery obligation exactly where it was.
    #[test]
    fn next_root_prompt_lifts_a_stop_over_a_recovery_required_child_without_clearing_it() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_host_materialization(&child.agent_id)
            .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        tree.state = TreeAdmissionState::TreeTerminal;
        tree.mark_root_live_for_prompt();
        assert_eq!(tree.state, TreeAdmissionState::Live);
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// A child whose session outcome is unresolved holds no child slot, and
    /// closing it is accepted without authorizing a retry: the attempt keeps
    /// its recovery-required failure.
    #[test]
    fn an_unresolved_child_holds_no_slot_and_closing_it_keeps_its_failure() {
        let (_tmp, mut tree) = tree();
        tree.grants
            .get_mut("root")
            .expect("root grant")
            .max_children = 1;
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_failure(
            &child.agent_id,
            turn_outcome::FailedTurnDisposition::RecoveryRequired,
            "child turn outcome unknown".to_owned(),
        )
        .unwrap();
        tree.settle_for_test();
        let closed = tree
            .close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert_eq!(
            closed,
            [(child.agent_id.clone(), NodeStatus::RecoveryRequired)]
        );
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().status,
            AttemptStatus::RecoveryRequired
        );
        assert!(
            tree.spawn_agent("root", &spawn_req(), None).is_ok(),
            "the unresolved child does not hold the only slot"
        );
    }

    /// An attempt whose turn a reopen abandoned leaves its node failed and
    /// re-taskable, and is refunded: it does not count against the budget.
    #[test]
    fn an_abandoned_attempt_is_refunded_and_leaves_the_node_retaskable() {
        let (_tmp, mut tree) = tree();
        tree.max_attempts = 1;
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_failure(
            &child.agent_id,
            turn_outcome::FailedTurnDisposition::Failed {
                kind: TURN_ABANDONED,
            },
            "the process was lost while the child was running".to_owned(),
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Failed
        );
        assert!(tree.attempt(&child.agent_id).unwrap().turn_was_abandoned());
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
        assert!(
            tree.spawn_agent("root", &spawn_req(), None).is_ok(),
            "an abandoned attempt did not spend the only attempt"
        );
    }

    /// a completed child is idle, not terminal, yet the epoch is
    /// quiescent: rebind and roster apply judge finished work, not re-taskability.
    #[test]
    fn a_completed_child_is_idle_for_followup_but_quiescent_for_the_epoch() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        let status = tree.node(&spawned.agent_id).unwrap().status;
        assert_eq!(status, NodeStatus::Failed);
        assert!(!status.is_terminal());
        assert!(status.is_finished());
        assert!(tree.epoch_allows_rebind());
        assert_eq!(tree.epoch_rebind_refusal(), None);
        // Listing hides finished rows by default, as before.
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        assert!(listed.agents.is_empty());
        assert!(tree.stop_all("root", None).unwrap().is_empty());
    }

    /// A singleton (or a tree whose descendants are all finished) has nothing
    /// to resume: reopening it never paints a pause, never gates rebind or the
    /// roster apply, and lifts a stale pause persisted by an earlier process.
    #[test]
    fn reopening_a_quiescent_tree_never_pauses_it() {
        let (_tmp, mut singleton) = tree();
        singleton.restore_paused();
        assert_eq!(singleton.state, TreeAdmissionState::Live);
        assert_eq!(singleton.epoch_rebind_refusal(), None);
        assert!(singleton.spawn_agent("root", &spawn_req(), None).is_ok());

        let (_tmp, mut persisted_paused) = tree();
        persisted_paused.state = TreeAdmissionState::RestoredPaused;
        persisted_paused.restore_paused();
        assert_eq!(persisted_paused.state, TreeAdmissionState::Live);
        assert_eq!(persisted_paused.epoch_rebind_refusal(), None);

        let (_tmp, mut all_terminal) = tree();
        let spawned = all_terminal
            .spawn_agent("root", &spawn_req(), None)
            .unwrap();
        all_terminal
            .close_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        all_terminal.state = TreeAdmissionState::Interrupted;
        all_terminal.restore_paused();
        assert_eq!(all_terminal.state, TreeAdmissionState::Live);
        assert_eq!(all_terminal.epoch_rebind_refusal(), None);

        let (_tmp, mut terminal_epoch) = tree();
        terminal_epoch.state = TreeAdmissionState::TreeTerminal;
        terminal_epoch.restore_paused();
        assert_eq!(terminal_epoch.state, TreeAdmissionState::TreeTerminal);
    }

    /// Only a tree with a live descendant (or a pending spawn) reopens paused.
    #[test]
    fn reopening_a_tree_with_live_descendants_pauses_it() {
        let (_tmp, mut live) = tree();
        let spawned = live.spawn_agent("root", &spawn_req(), None).unwrap();
        live.restore_paused();
        assert_eq!(live.state, TreeAdmissionState::RestoredPaused);
        assert_eq!(
            live.node(&spawned.agent_id)
                .unwrap()
                .presentation_status(live.state),
            NodeStatus::Paused
        );
        assert_eq!(
            live.epoch_rebind_refusal(),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
        );

        let (_tmp, mut pending) = tree();
        pending.pending_spawns = 1;
        pending.restore_paused();
        assert_eq!(pending.state, TreeAdmissionState::RestoredPaused);
    }

    #[test]
    fn epoch_rebind_refusal_splits_paused_spawns_and_descendants() {
        let (_tmp, mut tree) = tree();
        assert_eq!(tree.epoch_rebind_refusal(), None);

        tree.state = TreeAdmissionState::RestoredPaused;
        assert_eq!(
            tree.epoch_rebind_refusal(),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
        );
        assert!(tree.epoch_allows_rebind());

        tree.state = TreeAdmissionState::Live;
        tree.pending_spawns = 1;
        assert_eq!(
            tree.epoch_rebind_refusal(),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS)
        );

        tree.pending_spawns = 0;
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.epoch_rebind_refusal(),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS)
        );
        let _ = spawned;
    }

    #[test]
    fn template_authority_and_depth_govern_nesting() {
        let (_tmp, mut tree) = tree();
        tree.set_node_account_for_test("root", "grok");
        let mut snapshot = OrchestrationSnapshot::unpinned("grok-4.6");
        snapshot.bind_hints = test_bind_hints();
        snapshot.max_depth = MAX_TREE_DEPTH;
        let mut supervisor = RosterBinding::leaf("claude", "claude-sonnet-5");
        supervisor.template = "supervisor".into();
        supervisor.effort = Some("low".into());
        supervisor.max_tier = Tier::ReadOnly;
        supervisor.spawns = vec![TemplateName::parse("explore").unwrap()];
        snapshot
            .roster
            .insert(TemplateName::parse("orchestrate").unwrap(), supervisor);
        snapshot
            .working_set
            .insert(TemplateName::parse("orchestrate").unwrap());
        let mut explore = RosterBinding::leaf("grok", "grok-4.6");
        explore.max_tier = Tier::ReadOnly;
        snapshot
            .roster
            .insert(TemplateName::parse("explore").unwrap(), explore);
        snapshot.digest = snapshot.compute_digest();
        tree.install_snapshot(snapshot);
        assert_eq!(tree.grants.get("root").unwrap().max_depth, MAX_TREE_DEPTH);
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        assert_eq!(tree.node(&child.agent_id).unwrap().account, "claude");
        assert!(tree.grants.get(&child.agent_id).unwrap().may_spawn);
        let explore = SpawnAgentRequest::test_named("explore", Tier::ReadOnly, "scout", "look");
        let grandchild = tree.spawn_agent(&child.agent_id, &explore, None).unwrap();
        let grandchild = tree.node(&grandchild.agent_id).unwrap();
        assert_eq!(
            (grandchild.account.as_str(), grandchild.tuple_id.as_str()),
            ("grok", "grok-4.6"),
            "a card binds its own account and model, never its caller's"
        );
        assert_eq!(grandchild.effort, None, "nor its caller's effort");
        assert!(!tree.grants.get(&grandchild.agent_id).unwrap().may_spawn);
    }

    #[test]
    fn only_spawn_capable_templates_compile_spawn_agent() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        assert!(
            tree.compiled_tool_names("root")
                .unwrap()
                .contains(&"spawn_agent")
        );
        let review = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(
            !tree
                .compiled_tool_names(&review.agent_id)
                .unwrap()
                .contains(&"spawn_agent")
        );
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        assert!(
            tree.compiled_tool_names(&child.agent_id)
                .unwrap()
                .contains(&"spawn_agent")
        );
        // A spawn-capable template at the depth ceiling cannot nest.
        tree.grants.get_mut("root").unwrap().max_depth = 1;
        let capped = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        assert!(!tree.grants.get(&capped.agent_id).unwrap().may_spawn);
        assert!(!tree.grants.get(&capped.agent_id).unwrap().may_adopt);
    }

    #[test]
    fn openrouter_template_admits_spawn_and_openai_stays_mismatch() {
        let (_tmp, mut tree) = tree();
        pin(&mut tree, "review", "openrouter", "vendor/model", None);
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(tree.node(&spawned.agent_id).unwrap().account, "openrouter");
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().tuple_id,
            "vendor/model"
        );
        pin(&mut tree, "review", "openai", "gpt-4o", None);
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
        pin(&mut tree, "review", "openrouter", "", None);
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
        let mut router = RosterBinding::leaf("openrouter", "vendor/model");
        router.composition =
            Some(plakous_host_protocol::selection::CompositionSelection::NativePtc);
        tree.pin_template("review", router);
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    #[test]
    fn oauth_alias_templates_admit_spawn_and_anthropic_stays_mismatch() {
        for account in ["chatgpt", "supergrok", "xai-oauth"] {
            let (_tmp, mut tree) = tree();
            pin(&mut tree, "review", account, "tuple", None);
            let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            assert_eq!(tree.node(&spawned.agent_id).unwrap().account, account);
        }
        let (_tmp, mut tree) = tree();
        pin(&mut tree, "review", "anthropic", "claude-sonnet-5", None);
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SchemaMismatch
        );
    }

    #[test]
    fn snapshot_digest_mismatch_is_detectable() {
        let (_tmp, mut tree) = tree();
        assert!(tree.snapshot.digest_matches());
        tree.snapshot.roster.clear();
        assert!(!tree.snapshot.digest_matches());
    }

    /// the epoch snapshot's `max_attempts` is a lower-only clamp
    /// re-read on install, and attempt stamps are milliseconds.
    #[test]
    fn install_snapshot_wires_max_attempts_and_stamps_are_milliseconds() {
        let (_tmp, mut tree) = tree();
        let mut snapshot = OrchestrationSnapshot::unpinned("selected-tuple");
        snapshot.bind_hints = test_bind_hints();
        snapshot.max_attempts = 3;
        tree.install_snapshot(snapshot.clone());
        assert_eq!(tree.max_attempts(), 3);
        snapshot.max_attempts = u8::MAX;
        tree.install_snapshot(snapshot);
        assert_eq!(tree.max_attempts(), MAX_TREE_ATTEMPTS);
        tree.pin_template(
            "review",
            RosterBinding::leaf("test-account", "selected-tuple"),
        );
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let started_at = tree.attempt(&child.agent_id).unwrap().started_at;
        assert!(
            started_at > 1_600_000_000_000,
            "started_at is Unix milliseconds, got {started_at}"
        );
    }
}
