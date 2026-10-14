//! The tree's own projection: `TreeView` and `NodeView`.
//!
//! Rows, not presentation. Every field is the protocol's own enum, and the
//! only strings are ids, briefs, and paths the tree already holds. What a
//! label reads like, which glyph it takes, and how a transcript pages are the
//! host's; none of that is here.

use plakous_host_protocol::views::{AttemptUsage, BlockedObservation, NodeView, TreeView};

use super::{AttemptId, MailDurableState, ProductAgentTree, WorkspaceMode};

/// What the tree projects: enums, not labels.
pub fn project_tree_view(tree: &ProductAgentTree) -> TreeView {
    TreeView {
        tree_id: tree.tree_id.clone(),
        revision: tree.revision,
        state: tree.state,
        solicited: false,
        nodes: tree
            .preorder_ids()
            .into_iter()
            .filter_map(|id| project_node(tree, &id))
            .collect(),
    }
}

fn project_node(tree: &ProductAgentTree, id: &str) -> Option<NodeView> {
    let node = tree.node(id).ok()?;
    let workspace = tree.workspace(&node.workspace_id);
    let attempt = tree.attempt(id);
    Some(NodeView {
        agent_id: node.agent_id.clone(),
        parent_agent_id: node.parent_agent_id.clone(),
        depth: node.depth,
        status: node.presentation_status(tree.state),
        attempt_status: attempt.map(|attempt| attempt.status),
        // A `followup` opens `a2` on the same node, so the sequence is what the
        // canvas shows; the full id is durable but far too wide for a box.
        attempt_seq: node
            .attempt_id
            .as_ref()
            .and_then(|attempt_id| attempt_sequence(&node.agent_id, attempt_id)),
        attempt_id: node
            .attempt_id
            .as_ref()
            .map(|attempt_id| attempt_id.as_str().to_owned()),
        // Earlier attempts are not deleted by a re-task, so the row can say
        // "third try" instead of implying a first one.
        attempts: node_attempt_sequences(tree, &node.agent_id),
        tier: Some(node.tier),
        description: node.description.clone(),
        prompt: node.prompt.clone(),
        workspace_path: workspace
            .map(|workspace| workspace.path.display().to_string())
            .unwrap_or_else(|| node.path.display().to_string()),
        workspace_mode: workspace.map_or(WorkspaceMode::Inherit, |workspace| workspace.mode),
        // `adopting` and `adopted` are workspace-row statuses, not node
        // statuses (one in-flight status per row), so the canvas
        // reads them from here rather than from `status`.
        workspace_status: workspace.map(|workspace| workspace.status),
        // A candidate may exist without a submission.
        candidate_id: attempt.and_then(|attempt| attempt.candidate_id.clone()),
        // A quota refusal marks the *attempt* blocked and the node `Failed`, so
        // `resets_at` is only readable from the attempt.
        blocked: attempt
            .and_then(|attempt| attempt.blocked.as_ref())
            .map(|blocked| BlockedObservation {
                quota: blocked.quota.clone(),
                account: blocked.account.clone(),
                resets_at_unix: blocked.resets_at,
            }),
        usage: attempt.map(|attempt| AttemptUsage {
            elapsed_ms: attempt.usage.elapsed_ms,
            turns: attempt.usage.turns,
            tool_calls: attempt.usage.tool_calls,
            input_tokens: attempt.usage.input_tokens,
            output_tokens: attempt.usage.output_tokens,
            compaction: tree.attempt_compaction(&attempt.attempt_id),
        }),
        active_turn: node.active_turn,
        live_progress: None,
        unread_mail: unread_inbound(tree, id),
        has_session: node.session_locator.is_some(),
        template: node
            .template
            .as_ref()
            .map(|template| template.as_str().to_owned()),
        model_account: node.account.clone(),
        model_tuple: node.tuple_id.clone(),
        model_effort: node.effort.clone(),
        // `turn_token` counts admitted turns on the attempt. Without an
        // attempt row the count is unknown, never zero.
        turns: attempt.map(|attempt| u32::try_from(attempt.turn_token).unwrap_or(u32::MAX)),
        // The attempt's open stamp is Unix milliseconds. The canvas reads seconds.
        started_at_unix: attempt
            .map(|attempt| attempt.started_at / 1_000)
            .filter(|seconds| *seconds > 0),
        ended_at_unix: attempt
            .and_then(|attempt| attempt.ended_at)
            .map(|ended_at| ended_at / 1_000)
            .filter(|seconds| *seconds > 0),
        fault: attempt
            .filter(|attempt| attempt.candidate_handoff().is_some())
            .and_then(super::AttemptRecord::presented_failure)
            .map(|failure| failure.message)
            .or_else(|| tree.fault_reason(id).map(str::to_owned)),
        waiting_on: tree.waiting_on(id).map(|waiting| {
            waiting
                .processes
                .iter()
                .chain(&waiting.agents)
                .cloned()
                .chain(waiting.network.then(|| "network".to_owned()))
                .collect()
        }),
    })
}

fn unread_inbound(tree: &ProductAgentTree, agent_id: &str) -> u32 {
    u32::try_from(
        tree.messages()
            .values()
            .filter(|mail| {
                mail.to == agent_id
                    && matches!(
                        mail.durable,
                        MailDurableState::Accepted | MailDurableState::Acknowledged
                    )
            })
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// Every attempt sequence recorded for one node, ascending.
fn node_attempt_sequences(tree: &ProductAgentTree, agent_id: &str) -> Vec<u32> {
    let mut sequences: Vec<u32> = tree
        .attempts()
        .values()
        .filter(|attempt| attempt.agent_id == agent_id)
        .filter_map(|attempt| attempt_sequence(agent_id, &attempt.attempt_id))
        .collect();
    sequences.sort_unstable();
    sequences
}

/// `agent-7-a2` on node `agent-7` is attempt `2`. The id is `<agent_id>-a<seq>`
/// by construction, so the sequence is read back rather than stored twice; an
/// id that does not match the node returns `None` instead of a guessed number.
fn attempt_sequence(agent_id: &str, attempt_id: &AttemptId) -> Option<u32> {
    attempt_id
        .as_str()
        .strip_prefix(agent_id)?
        .strip_prefix("-a")?
        .parse()
        .ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    /// `<agent_id>-a<seq>` is read back rather than stored twice, and an id
    /// that does not belong to the node yields nothing rather than a number.
    #[test]
    fn the_attempt_sequence_is_read_from_the_attempt_id() {
        assert_eq!(
            attempt_sequence("agent-7", &AttemptId::new("agent-7", 2)),
            Some(2)
        );
        assert_eq!(
            attempt_sequence("agent-7", &AttemptId::from_persisted("agent-8-a2".into())),
            None
        );
        assert_eq!(
            attempt_sequence("agent-7", &AttemptId::from_persisted("agent-7-aX".into())),
            None
        );
    }
}
