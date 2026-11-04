//! The reopen law: what a lost process left in flight was its lost owner's
//! (spec 03 section 14.1).

use super::workspaces::VCS_FAULT_KIND;
use super::{NodeStatus, ProductAgentTree, TreeAdmissionState};

impl ProductAgentTree {
    /// The reopen law for a persisted tree (design section 6.4): what the
    /// last process left in flight was its lost owner's. In-flight VCS is
    /// faulted as before. A running child the rows show parked between
    /// turns is interrupted with cause `owner_lost`; its attempt, subtree
    /// and surviving jobs are kept. A running child with a bound or active
    /// turn is owed a `recover_node`, which marks it from its feed (or
    /// exempts and rejoins it) and takes no new turn before. The tree
    /// reopens live: a stored `restored_paused` or `interrupted` admission
    /// reads as live plus that marking, and neither is written again. The
    /// marking reads durable rows only, so twin reopens agree.
    pub fn apply_reopen_law(&mut self) {
        // Nothing recovers the root (`recovery_need` is `NotRequired` for it),
        // so a root an earlier build faulted for a child copy's VCS step
        // reopens running instead of refusing its own waits for good.
        if let Some(root) = self.nodes.get_mut(&self.root_agent_id)
            && root.status == NodeStatus::RecoveryRequired
        {
            root.status = NodeStatus::Running;
        }
        self.fault_in_flight_vcs();
        // A stage a lost process left recorded settles by its class: a
        // rerunnable one stays for its node's recovery, the rest fault.
        self.settle_unfinished_vcs_stages();
        self.queue_recovery_base_pins();
        self.queue_refresh_recovery();
        let agents: std::collections::BTreeSet<_> = self
            .operations
            .values()
            .filter(|record| record.tool_name == VCS_FAULT_KIND)
            .map(|record| record.caller_agent_id.clone())
            .collect();
        for agent in agents {
            if let Err(error) = self.fold_due(&agent) {
                tracing::error!(agent_id = %agent, refusal = %error.message, "tree restore fold refused");
            }
        }
        self.mark_owners_lost();
        if matches!(
            self.state,
            TreeAdmissionState::RestoredPaused | TreeAdmissionState::Interrupted
        ) {
            self.state = TreeAdmissionState::Live;
        }
        self.bump();
    }

    /// Interrupts, with cause `owner_lost`, every running child the tree rows
    /// prove the lost owner left parked between turns, and owes a
    /// `recover_node` to every running or interrupted child with a bound or
    /// active turn.
    fn mark_owners_lost(&mut self) {
        type Lost = Vec<(String, Option<super::AttemptId>)>;
        let parked: std::collections::BTreeSet<String> = self
            .parked_reopen_targets()
            .into_iter()
            .map(|(agent_id, ..)| agent_id)
            .collect();
        let running = |node: &&super::NodeRecord| {
            node.agent_id != self.root_agent_id && node.status == NodeStatus::Running
        };
        // A parked child whose start the lost process prepared but no fact
        // names may have been admitted: its recover_node rejoins it or marks it.
        let (rejoinable, lost): (Lost, Lost) = self
            .nodes
            .values()
            .filter(running)
            .filter(|node| parked.contains(&node.agent_id))
            .map(|node| (node.agent_id.clone(), node.attempt_id.clone()))
            .partition(|(agent_id, attempt_id)| {
                self.has_unfacted_start(agent_id, attempt_id.as_ref())
            });
        // An interrupted child still bound to a turn is owed too: an
        // interrupt (or an earlier owner-lost mark) written before the
        // turn's terminal was projected leaves the binding to its feed.
        let bound = self
            .nodes
            .values()
            .filter(|node| {
                node.agent_id != self.root_agent_id
                    && matches!(node.status, NodeStatus::Running | NodeStatus::Interrupted)
            })
            .filter(|node| self.has_turn_owner(&node.agent_id))
            .map(|node| node.agent_id.clone())
            .collect::<Vec<String>>();
        for (agent_id, attempt_id) in lost {
            self.interrupt_owner_lost(&agent_id, attempt_id);
        }
        // An interrupted child with a command the lost process prepared and
        // no fact names is owed the rest of its mark, so the command is
        // withdrawn and its mail returns to unread. This runs after the
        // parked marks above, so a child this reopen just marked is judged
        // the same way a later reopen of these rows judges it.
        let unwithdrawn = self
            .nodes
            .values()
            .filter(|node| {
                node.agent_id != self.root_agent_id && node.status == NodeStatus::Interrupted
            })
            .filter(|node| {
                self.commands.values().any(|command| {
                    command.recipient == node.agent_id
                        && command.kind != super::AgentCommandKind::RootBatch
                        && command.unresolved()
                })
            })
            .map(|node| node.agent_id.clone())
            .collect::<Vec<String>>();
        self.recovery_owed = bound
            .into_iter()
            .chain(unwithdrawn)
            .chain(rejoinable.into_iter().map(|(agent_id, _)| agent_id))
            .collect();
    }

    /// A Wake or MailStart prepared for this attempt that no fact names
    /// and the tree has not withdrawn.
    fn has_unfacted_start(&self, agent_id: &str, attempt_id: Option<&super::AttemptId>) -> bool {
        self.commands.values().any(|command| {
            command.recipient == agent_id
                && matches!(
                    command.kind,
                    super::AgentCommandKind::Wake | super::AgentCommandKind::MailStart
                )
                && command.fact.is_none()
                && !command.withdrawn
                && command.attempt_id.as_ref() == attempt_id
        })
    }

    fn interrupt_owner_lost(&mut self, agent_id: &str, attempt_id: Option<super::AttemptId>) {
        if let Err(error) = self.record_operator_intent(
            agent_id,
            attempt_id,
            super::operator_intents::Decision::Interrupt {
                effective: true,
                cause: Some(super::operator_intents::InterruptCause::OwnerLost),
            },
        ) {
            tracing::error!(agent_id = %agent_id, refusal = %error.message,
                "owner-lost marking refused");
        }
    }

    /// Law L: the lost owner's turn of this child is gone. A retryable
    /// recovery state the old law left on it is lifted, the running child is
    /// interrupted with cause `owner_lost` and its attempt kept, and it
    /// takes turns again.
    pub fn lose_owner(&mut self, agent_id: &str, attempt_id: Option<super::AttemptId>) {
        if self.lift_lost_owner(agent_id) {
            self.interrupt_owner_lost(agent_id, attempt_id);
        }
        self.recovery_settled(agent_id);
        self.bump();
    }

    /// Law L inside a feed fold, where an abandoned turn's terminal is the
    /// evidence: the interrupt is applied in place rather than recorded as
    /// an intent, so a rebuild of the same feed applies it again.
    pub(super) fn lose_owner_in_fold(&mut self, agent_id: &str) {
        if self.lift_lost_owner(agent_id) {
            if let Some(node) = self.nodes.get_mut(agent_id) {
                node.status = NodeStatus::Interrupted;
            }
            self.interrupt_park(agent_id);
        }
        self.recovery_settled(agent_id);
        self.bump();
    }

    /// Lifts a retryable recovery state the old law left on a child whose
    /// owner is lost. Returns whether the child runs and so is interrupted.
    /// A VCS step whose outcome is unknown is not retryable: its session part
    /// settles, but the node stays `recovery_required` and only
    /// `close_agent` ends it. A submitted result is left to complete.
    fn lift_lost_owner(&mut self, agent_id: &str) -> bool {
        if self
            .current_attempt(agent_id)
            .is_some_and(|attempt| attempt.output_json.is_some())
        {
            return false;
        }
        if let Some(attempt) = self.current_attempt_mut(agent_id)
            && attempt.status == super::AttemptStatus::RecoveryRequired
        {
            attempt.status = super::AttemptStatus::Running;
            attempt.failure = None;
            attempt.ended_at = None;
        }
        if self.workspace_blocker(agent_id).is_some() {
            return false;
        }
        let Some(node) = self.nodes.get_mut(agent_id) else {
            return false;
        };
        if node.status == NodeStatus::RecoveryRequired {
            node.status = NodeStatus::Running;
        }
        node.status == NodeStatus::Running
    }

    /// recover_node's mark (design section 6.3, step 5): this owed child's
    /// exact open did not restore its turn, so the lost owner's turn is
    /// gone. It is interrupted with cause `owner_lost`, attempt and subtree
    /// kept. It stays owed until the caller has projected the feed the open
    /// left ([`Self::recovery_settled`]), so no delivery sees the mark
    /// before the lost turn's terminal is folded.
    pub fn mark_owner_lost(&mut self, agent_id: &str) {
        if let Some(node) = self.nodes.get(agent_id)
            && node.status == NodeStatus::Running
        {
            let attempt_id = node.attempt_id.clone();
            self.interrupt_owner_lost(agent_id, attempt_id);
            self.bump();
        }
    }
}
