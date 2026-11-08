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
            .caller_epoch(caller_id)?
            .spawnable_template_names(caller_template)
            .next()
            .is_none()
        {
            names.retain(|name| *name != "spawn_agent");
        }
        Ok(names)
    }

    /// The current epoch, mutable, so a test can shape a roster the epoch
    /// would otherwise have frozen. A real roster change is an install or an
    /// append.
    #[cfg(test)]
    pub(crate) fn current_epoch_mut_for_test(&mut self) -> &mut OrchestrationSnapshot {
        self.current_epoch_mut()
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

    /// The card a spawn binds, from the caller's epoch. A child binds
    /// exactly what its card names — account, model, effort and tier — and
    /// never its caller's.
    pub(super) fn resolve_template_binding(
        &self,
        caller_id: &str,
        template: &TemplateName,
    ) -> Result<(RosterBinding, String, String, Option<String>), TopologyError> {
        let epoch = self.caller_epoch(caller_id)?;
        let Some(binding) = epoch.binding(template) else {
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
        let hints = &epoch.bind_hints;
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

    /// Makes `snapshot` this tree's current epoch in place: bounds are
    /// re-clamped, the per-cell budget is rebuilt, and the working set
    /// replaced. Every tree is on epoch 0 today, so this replaces the
    /// header snapshot.
    ///
    /// Only ever called on a quiescent tree — genesis, or a successor epoch
    /// past `epoch_allows_rebind` — so no cell is mid-loop when its counter
    /// is replaced.
    pub fn install_snapshot(&mut self, snapshot: OrchestrationSnapshot) {
        self.clamp_root_ceilings(&snapshot);
        // the per-cell agent bound is epoch state too. An
        // install only happens on a quiescent tree (genesis, or a successor
        // epoch after `epoch_allows_rebind`), so no cell is mid-loop when its
        // counter is replaced.
        self.cell_budgets.insert(
            self.epochs().current,
            Arc::new(CellAgentBudget::for_snapshot(&snapshot)),
        );
        *self.current_epoch_mut() = snapshot;
        self.bump();
    }

    /// The root's epoch state from `snapshot`: the writer mode, the root
    /// grant's spawn ceilings and the per-prompt attempt bound.
    pub(super) fn clamp_root_ceilings(&mut self, snapshot: &OrchestrationSnapshot) {
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
    }

    /// Makes `snapshot` the root's epoch on a quiescent tree (a roster
    /// apply) and lifts the tree to live. A finished agent spawned under the
    /// current epoch keeps it, because a followup can revive it on that
    /// epoch's card, so then the snapshot is appended as the next epoch;
    /// otherwise nothing references the current epoch but the root and it is
    /// replaced in place.
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
        if self.current_epoch_referenced() {
            self.append_epoch(snapshot)?;
            self.state = TreeAdmissionState::Live;
            return Ok(());
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
        let epoch = self.current_epoch_mut();
        epoch.working_set.insert(name.clone());
        epoch.roster.insert(name, binding);
        epoch.digest = epoch.compute_digest();
    }

    /// Refusal shown after stop-all until the next admitted root prompt.
    pub const TREE_STOPPED_REFUSAL: &str = "delegation is stopped for this turn; send a new prompt after all agents \
         have retired, or resolve any recovery-required agents first";

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

    /// Distinct operator strings: pending spawns vs live descendants.
    #[must_use]
    pub fn epoch_rebind_refusal(&self) -> Option<&'static str> {
        self.epoch_rebind_refusal_kind()
            .map(EpochRebindRefusal::message)
    }

    /// Typed form of [`Self::epoch_rebind_refusal`] for the roster apply.
    #[must_use]
    pub fn epoch_rebind_refusal_kind(&self) -> Option<EpochRebindRefusal> {
        self.quiescence_refusal()
    }

    /// Whether the tree's own roster is quiescent: no pending spawn and no
    /// non-terminal descendant.
    ///
    /// This is the judgement a headless `plak config set` needs.
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
    /// Each interrupt records cause `cascade`.
    ///
    /// # Errors
    /// Refuses malformed recorded intent data.
    pub fn interrupt_running_descendants(&mut self) -> Result<Vec<String>, TopologyError> {
        self.interrupt_running_descendants_for(super::InterruptCause::Cascade)
    }

    /// [`Self::interrupt_running_descendants`], recording `cause`: `stopped`
    /// when an embedded run's exit stops the tree. A child held after a loss
    /// is not running, but the stop still records an interrupt on it, which
    /// ends its hold: stops stay stopped.
    ///
    /// # Errors
    /// Refuses malformed recorded intent data.
    pub fn interrupt_running_descendants_for(
        &mut self,
        cause: super::InterruptCause,
    ) -> Result<Vec<String>, TopologyError> {
        let interrupted = self.interrupt_working_children(cause, true)?;
        if !interrupted.is_empty() {
            self.bump();
        }
        Ok(interrupted)
    }

    /// A planned handoff (spec 03 section 14.1): before the releasing host
    /// lets go, each child whose assignment started (a turn of its attempt
    /// was admitted) and that is running or parked is interrupted
    /// with cause `handoff`, its attempt kept, so the next host continues it
    /// once by itself. A child reserved for startup whose Start was never
    /// accepted is not: the continuation would reach it without its
    /// assignment, and its startup is recovered instead. A child held after
    /// a loss keeps its hold. Returns the children handed off.
    ///
    /// # Errors
    /// Refuses malformed recorded intent data.
    pub fn record_handoff(&mut self) -> Result<Vec<String>, TopologyError> {
        let handed_off = self.interrupt_working_children(super::InterruptCause::Handoff, false)?;
        if !handed_off.is_empty() {
            self.bump();
        }
        Ok(handed_off)
    }

    fn interrupt_working_children(
        &mut self,
        cause: super::InterruptCause,
        end_holds: bool,
    ) -> Result<Vec<String>, TopologyError> {
        let root = self.root_agent_id.clone();
        let mut interrupted = Vec::new();
        let ids: Vec<String> = self.nodes.keys().cloned().collect();
        for id in ids {
            // A stop is final for the turn it interrupted. A later cascade
            // or a planned handoff skips that child only while no turn of
            // the attempt began after the stop; a resumed turn is
            // interrupted again.
            if matches!(
                cause,
                super::InterruptCause::Cascade | super::InterruptCause::Handoff
            ) && self.interrupt_already_stopped(&id)
            {
                continue;
            }
            let parked = self.is_parked(&id) && !self.is_park_ending(&id);
            let owned = self.has_turn_owner(&id)
                && (cause != super::InterruptCause::Handoff || self.assignment_started(&id));
            if let Some(node) = self.nodes.get(&id)
                && id != root
                && (owned || parked || cause == super::InterruptCause::Stopped)
                && node.status == NodeStatus::Running
            {
                self.record_operator_intent(
                    &id,
                    node.attempt_id.clone(),
                    super::operator_intents::Decision::Interrupt {
                        effective: true,
                        cause: Some(cause),
                    },
                )?;
                interrupted.push(id);
            } else if end_holds && id != root && self.loss_continuation(&id).is_some() {
                let attempt = self.nodes.get(&id).and_then(|node| node.attempt_id.clone());
                self.record_operator_intent(
                    &id,
                    attempt,
                    super::operator_intents::Decision::Interrupt {
                        effective: false,
                        cause: Some(cause),
                    },
                )?;
            }
        }
        Ok(interrupted)
    }

    /// Whether `agent_id`'s current attempt has a turn the runtime admitted:
    /// one bound to it (a turn is bound only once its command was accepted),
    /// or an accepted command on record.
    fn assignment_started(&self, agent_id: &str) -> bool {
        if self
            .attempt(agent_id)
            .is_some_and(|attempt| attempt.turn_id.is_some())
        {
            return true;
        }
        let attempt = self
            .nodes
            .get(agent_id)
            .and_then(|node| node.attempt_id.as_ref());
        attempt.is_some_and(|attempt| {
            self.commands.values().any(|command| {
                command.recipient == agent_id
                    && command.attempt_id.as_ref() == Some(attempt)
                    && command.fact == Some(super::CommandFact::Accepted)
            })
        })
    }

    /// Whether `agent_id`'s latest interrupt is a stop, and no turn of that
    /// attempt was admitted or bound after it.
    fn interrupt_already_stopped(&self, agent_id: &str) -> bool {
        let Some(attempt) = self
            .nodes
            .get(agent_id)
            .and_then(|node| node.attempt_id.clone())
        else {
            return false;
        };
        let Some((interrupt, cause)) = self.latest_interrupt(agent_id, &attempt) else {
            return false;
        };
        cause == Some(super::InterruptCause::Stopped)
            && !self.turn_began_after_interrupt(agent_id, &attempt, interrupt)
    }

    /// Records that the root's turn was interrupted while work was still
    /// live, so the tree reads as interrupted rather than idle. A tree
    /// already interrupted is left as it is: a repeated stop writes nothing.
    pub fn mark_root_interrupted(&mut self) {
        if !self.epoch_allows_rebind() && self.state != TreeAdmissionState::Interrupted {
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
        self.root_pending_spawns = pending;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
