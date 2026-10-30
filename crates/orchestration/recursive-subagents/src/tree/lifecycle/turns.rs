use super::{
    AbandonedChildTurn, NodeStatus, ProductAgentTree, SubmitResultOutput, SubmitResultRequest,
    TopologyError, TopologyErrorCode,
};

impl ProductAgentTree {
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

    /// The template's per-attempt turn ceiling (`max_turns`).
    pub(in crate::tree) fn attempt_turn_ceiling(&self, agent_id: &str) -> Option<u32> {
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
}
