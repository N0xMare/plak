//! A bounded feed read carried across the neutral host/tree boundary.

use super::{TopologyError, TreeProjectorError};

/// Ordered feed records and their bounded reads, prepared without the tree lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeFeed {
    /// Runtime session whose records were read.
    pub session_id: String,
    /// First sequence whose facts are retained by the runtime.
    pub retained_from: u64,
    /// Sequence immediately below the retained window, used to seed replay.
    pub below_retained: Option<u64>,
    /// Branch heads authenticated by a retention page or an exact facade head read.
    pub heads: Vec<plakous::sessions::SessionLocator>,
    /// Exact head running-turn observations for retention seeds.
    pub head_turns: std::collections::BTreeMap<
        plakous::sessions::SessionLocator,
        Option<plakous::model::TurnId>,
    >,
    /// Ordered feed records.
    pub records: Vec<plakous::sessions::feed::FeedRecord>,
    /// Bounded terminal reads named by the records.
    pub reads: super::TreeFeedReads,
}

impl TreeFeed {
    /// Applies the run to `tree` for `agent_id`, whose session it is.
    ///
    /// # Errors
    ///
    /// Returns the projector's refusal; nothing is applied.
    pub fn apply(
        &self,
        tree: &mut super::ProductAgentTree,
        agent_id: &str,
    ) -> Result<super::TreeProjection, TopologyError> {
        self.apply_checked(tree, agent_id)
            .map_err(TopologyError::from)
    }

    /// Applies a page without erasing internal retryable projection refusals.
    ///
    /// # Errors
    /// Returns a typed projection refusal with no state change.
    pub fn apply_checked(
        &self,
        tree: &mut super::ProductAgentTree,
        agent_id: &str,
    ) -> Result<super::TreeProjection, TreeProjectorError> {
        let mut next = tree.clone();
        next.join_child_session(agent_id, &self.session_id)?;
        let mut refused = Vec::new();
        if let Some(below) = self.below_retained {
            next.mark_commands_behind_retention(agent_id)?;
            if next.node(agent_id)?.fold_order.is_none()
                || next.feed_cursor(&self.session_id) < below
            {
                refused.extend(next.fold_before_head_seed(agent_id, &self.session_id, below)?);
                self.seed_head(&mut next, agent_id)?;
            }
            refused.extend(next.fold_onto_seed(agent_id, below)?);
            next.seed_feed_cursor(&self.session_id, below);
        }
        // Roots have no node locator view. A freshly authenticated exact head
        // supplies wait evidence even when no new feed record names it.
        if agent_id == next.root_agent_id {
            next.project_retained_head_waits(agent_id, &self.heads, &self.reads)?;
        }
        let mut projection =
            match next.project_feed(agent_id, &self.session_id, &self.records, &self.reads) {
                Err(TreeProjectorError::RefoldRequired)
                    if self.below_retained.is_some()
                        && self.records.first().is_some_and(|record| {
                            record.header().seq.get() == self.retained_from
                        })
                        && self.records.last().is_some_and(|record| {
                            record.header().seq.get() >= tree.feed_cursor(&self.session_id)
                        }) =>
                {
                    next = tree.clone();
                    next.clear_agent_feed_views(agent_id, &self.session_id);
                    let mut projection = self.apply_checked(&mut next, agent_id)?;
                    if projection.cursor < tree.feed_cursor(&self.session_id) {
                        return Err(TreeProjectorError::RefoldRequired);
                    }
                    projection.retain_new_usage(&self.records, tree.feed_cursor(&self.session_id));
                    next.pending_turn_usages
                        .clone_from(&tree.pending_turn_usages);
                    projection
                }
                result => result?,
            };
        projection.refused_intents.extend(refused);
        for terminal in &projection.terminal_usages {
            next.pending_turn_usages.insert(terminal.turn_id, *terminal);
        }
        *tree = next;
        Ok(projection)
    }
}

impl TreeFeed {
    /// Whether the run resumed above the feed's retention: nothing below
    /// `feed_retained_from` was derived (spec 03 section 14.1).
    pub const fn went_behind_retention(&self) -> bool {
        self.below_retained.is_some()
    }

    /// Seeds a child's session locator from the current head of its branch
    /// (spec 03 section 14.3): the records that would have advanced it are
    /// below retention. A node on another session or branch is left.
    fn seed_head(
        &self,
        tree: &mut super::ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        if agent_id == tree.root_agent_id {
            return Ok(());
        }
        let Some(current) = tree
            .node(agent_id)
            .ok()
            .and_then(|node| node.session_locator.as_deref())
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
        else {
            return Ok(());
        };
        let Some(head) = self.heads.iter().find(|head| {
            head.session_id() == current.session_id() && head.branch_id() == current.branch_id()
        }) else {
            return Ok(());
        };
        if *head != current {
            tree.advance_session_locator(agent_id, head.to_string())?;
        }
        let has_accepted_command = self
            .records
            .iter()
            .flat_map(plakous::sessions::feed::FeedRecord::facts)
            .any(|fact| {
                matches!(fact,
                plakous::sessions::feed::FeedFact::CommandAccepted { command_id, .. }
                    if tree.command(command_id.as_str()).is_some_and(|command| {
                        command.recipient == agent_id && command.attempt_id.is_some()
                    }))
            });
        if !has_accepted_command && let Some(turn) = self.head_turns.get(head) {
            tree.seed_head_turn(agent_id, turn.map(|turn| turn.to_string()))?;
        }
        Ok(())
    }
}
