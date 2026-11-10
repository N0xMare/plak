//! Reconstruct proven-unstarted runner seal debt without replaying a stage.

use super::super as tree;
use super::{CandidateId, NodeStatus, ProductAgentTree, SealEffectRequest, SealTicket};

impl ProductAgentTree {
    pub(in crate::tree) fn restore_unstarted_seal(
        &mut self,
        agent_id: &str,
        canonical: &str,
    ) -> bool {
        let Ok(reservation) = serde_json::from_str::<SealEffectRequest>(canonical) else {
            return false;
        };
        let Some(node) = self.nodes.get(agent_id) else {
            return false;
        };
        // Only managed ports publish a stage before dispatch. Absence of a
        // stage proves an unsent reservation there, never on a local port.
        if (self.vcs.0.placement_ready(&node.path).is_none() && node.placement.runner_id.is_none())
            || node.attempt_id != reservation.attempt_id
            || self.operations.values().any(|op| {
                op.tool_name == tree::VcsStageIntent::KIND && op.caller_agent_id == agent_id
            })
        {
            return false;
        }
        if self.seal_pending(agent_id) {
            return true;
        }
        let Some(workspace) = self.workspaces.get(&node.workspace_id) else {
            return false;
        };
        let Some(base_commit) = workspace
            .base_commit
            .clone()
            .filter(|base| !base.is_empty())
        else {
            return false;
        };
        let ownership = std::sync::Arc::new(());
        let ticket = SealTicket {
            ownership: ownership.clone(),
            agent_id: agent_id.to_owned(),
            attempt_id: reservation.attempt_id.clone(),
            workspace_id: node.workspace_id.clone(),
            candidate_id: reservation.attempt_id.as_ref().map_or_else(
                || CandidateId::for_agent(agent_id),
                CandidateId::for_attempt,
            ),
            namespace: tree::digest_text(&self.tree_id),
            path: node.path.clone(),
            base_commit,
            base_content: workspace.base_content.clone().unwrap_or_default(),
            base_change_id: workspace.base_change_id.clone(),
            prior_status: reservation.resume_status,
            carry: self.seal_carry(agent_id),
        };
        self.outstanding_seals
            .insert(agent_id.to_owned(), ownership);
        self.effects.seals.push(ticket);
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = NodeStatus::Sealing;
        }
        true
    }
}
