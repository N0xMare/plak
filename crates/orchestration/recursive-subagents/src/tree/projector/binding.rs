use super::{
    CHILD_SESSION_KIND, FeedChange, FeedHeader, ProductAgentTree, TopologyError, TopologyErrorCode,
};

impl ProductAgentTree {
    pub(in crate::tree) fn join_child_session(
        &mut self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<(), TopologyError> {
        if self.node(agent_id)?.session_locator.is_some() {
            return Ok(());
        }
        let id = format!("{CHILD_SESSION_KIND}:{agent_id}");
        let Some(operation) = self.operations.get(&id) else {
            return Ok(());
        };
        let locator = operation.canonical_request.clone();
        let parsed = locator
            .parse::<plakous::sessions::SessionLocator>()
            .map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "child session allocation is invalid",
                )
            })?;
        if parsed.session_id().to_string() == session_id {
            self.bind_session_locator(agent_id, locator)?;
        }
        Ok(())
    }

    pub(super) fn project_locator(
        &mut self,
        agent_id: &str,
        header: &FeedHeader,
    ) -> Result<(), TopologyError> {
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        let Some(current) = self
            .node(agent_id)?
            .session_locator
            .as_deref()
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
        else {
            return Ok(());
        };
        if current.session_id() != header.session_id || current.branch_id() != header.branch_id {
            return Ok(());
        }
        let checkpoint = match header.change {
            FeedChange::Created { genesis, .. } => genesis,
            FeedChange::Advanced { checkpoint, .. } => checkpoint,
            FeedChange::Branched { target, .. } => target,
            FeedChange::LeaseReleased { head, .. } | FeedChange::Pruned { head, .. } => head,
            _ => return Ok(()),
        };
        let locator =
            plakous::sessions::SessionLocator::new(header.session_id, header.branch_id, checkpoint);
        if locator != current {
            self.advance_session_locator(agent_id, locator.to_string())?;
        }
        Ok(())
    }

    /// Seeds the attempt's binding and active-turn view from a retained head.
    /// This is a head observation, not an admission, so it spends no turn.
    ///
    /// # Errors
    /// Refuses an unknown agent.
    pub fn seed_head_turn(
        &mut self,
        agent_id: &str,
        turn_id: Option<String>,
    ) -> Result<(), TopologyError> {
        self.node(agent_id)?;
        if agent_id == self.root_agent_id {
            return Ok(());
        }
        let active = turn_id.is_some();
        if let Some(attempt) = self.current_attempt_mut(agent_id) {
            attempt.turn_id = turn_id;
        }
        // Observing an unfinished head is not starting another turn. An older
        // close or interrupt requests retirement without erasing that binding.
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.active_turn = active;
        }
        self.bump();
        Ok(())
    }
}
