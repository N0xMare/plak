use plakous::sessions::feed::{FeedFact, FeedRecord, FeedWorkflowKind, FeedWorkflowOutcome};
use plakous_host_protocol::views::AttemptCompaction;

use super::super::ProductAgentTree;

impl ProductAgentTree {
    /// Read cursor including disposable projections that must replay after open.
    /// This is not the durable command/terminal authority cursor.
    pub fn feed_read_cursor(&self, session_id: &str) -> u64 {
        let child_session = self.nodes.values().any(|node| {
            node.agent_id != self.root_agent_id
                && node
                    .session_locator
                    .as_deref()
                    .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
                    .is_some_and(|locator| locator.session_id().to_string() == session_id)
        }) || self.operations.values().any(|operation| {
            operation.tool_name == super::CHILD_SESSION_KIND
                && operation
                    .canonical_request
                    .parse::<plakous::sessions::SessionLocator>()
                    .is_ok_and(|locator| locator.session_id().to_string() == session_id)
        });
        if !child_session {
            return self.feed_cursor(session_id);
        }
        self.feed_cursor(session_id).min(
            self.compaction_cursors
                .get(session_id)
                .map_or(0, |(cursor, _, _)| *cursor),
        )
    }

    pub(super) fn project_compactions(
        &mut self,
        agent_id: &str,
        session_id: &str,
        record: &FeedRecord,
    ) {
        let seq = record.header().seq.get();
        let state = self
            .compaction_cursors
            .entry(session_id.to_owned())
            .or_insert((0, None, true));
        if seq <= state.0 {
            return;
        }
        state.2 &= seq == state.0 + 1;
        state.0 = seq;
        if agent_id == self.root_agent_id {
            return;
        }
        if self
            .nodes
            .get(agent_id)
            .and_then(|node| node.session_locator.as_deref())
            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
            .is_some_and(|locator| locator.branch_id() != record.header().branch_id)
        {
            return;
        }
        for fact in record.facts() {
            if let FeedFact::CommandAccepted { command_id, .. } = fact
                && let Some(command) = self.commands.get(command_id.as_str())
                && command.recipient == agent_id
                && let Some(attempt_id) = &command.attempt_id
            {
                state.1 = Some(attempt_id.clone());
                self.compaction_views
                    .entry(attempt_id.clone())
                    .or_insert(AttemptCompaction {
                        complete: state.2,
                        ..AttemptCompaction::default()
                    });
            }
            let FeedFact::WorkflowSettled {
                kind: FeedWorkflowKind::Compact { .. },
                outcome,
                ..
            } = fact
            else {
                continue;
            };
            let Some(attempt_id) = &state.1 else {
                // Retention may hide which attempt admitted the workflow.
                state.2 = false;
                continue;
            };
            let counters = self.compaction_views.entry(attempt_id.clone()).or_default();
            counters.complete &= state.2;
            match outcome {
                FeedWorkflowOutcome::Compacted {
                    tokens_before,
                    tokens_after,
                    ..
                } => {
                    counters.committed = counters.committed.saturating_add(1);
                    counters.tokens_before = Some(*tokens_before);
                    counters.tokens_after = Some(*tokens_after);
                }
                FeedWorkflowOutcome::Failed { .. } => {
                    counters.failed = counters.failed.saturating_add(1);
                }
                FeedWorkflowOutcome::NoEligibleRange => {
                    counters.no_eligible = counters.no_eligible.saturating_add(1);
                }
                _ => {}
            }
        }
    }
}
