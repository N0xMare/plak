//! Reconstruct runner seal debt only when unsent or verifiable.

use super::super as tree;
use super::{CandidateId, NodeStatus, ProductAgentTree, SealEffectRequest, SealTicket};
use tree::{VcsStageClass, VcsStageIntent, VcsStageKind};

impl ProductAgentTree {
    pub(in crate::tree) fn restore_runner_seal(&mut self, agent_id: &str, canonical: &str) -> bool {
        let Ok(reservation) = serde_json::from_str::<SealEffectRequest>(canonical) else {
            return false;
        };
        let Some(node) = self.nodes.get(agent_id) else {
            return false;
        };
        // Only managed ports publish a stage before dispatch. Absence of a
        // stage proves an unsent reservation there, never on a local port.
        if node.placement.runner_id.is_none() || node.attempt_id != reservation.attempt_id {
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
        let candidate_id = reservation.attempt_id.as_ref().map_or_else(
            || CandidateId::for_agent(agent_id),
            CandidateId::for_attempt,
        );
        let namespace = tree::digest_text(&self.tree_id);
        let carry = self.seal_carry(agent_id);
        let carried_commit =
            match self.seal_stage_proof(agent_id, &candidate_id, &namespace, carry.as_ref()) {
                Ok(proof) => proof,
                Err(()) => return false,
            };
        let ownership = std::sync::Arc::new(());
        let ticket = SealTicket {
            ownership: ownership.clone(),
            agent_id: agent_id.to_owned(),
            attempt_id: reservation.attempt_id.clone(),
            workspace_id: node.workspace_id.clone(),
            candidate_id,
            namespace,
            path: node.path.clone(),
            base_commit,
            base_content: workspace.base_content.clone().unwrap_or_default(),
            base_change_id: workspace.base_change_id.clone(),
            prior_status: reservation.resume_status,
            carry,
            carried_commit,
        };
        self.outstanding_seals
            .insert(agent_id.to_owned(), ownership);
        self.effects.seals.push(ticket);
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = NodeStatus::Sealing;
        }
        true
    }

    fn seal_stage_proof(
        &self,
        agent: &str,
        candidate: &CandidateId,
        namespace: &str,
        carry: Option<&tree::cross_runner::SealCarry>,
    ) -> Result<Option<String>, ()> {
        let mut staged = false;
        let mut proved = None;
        for row in self
            .operations
            .values()
            .filter(|row| row.tool_name == VcsStageIntent::KIND && row.caller_agent_id == agent)
        {
            staged = true;
            let intent = VcsStageIntent::from_operation(row)
                .map_err(|_| ())?
                .ok_or(())?;
            if intent.class == VcsStageClass::Ambiguous
                || intent.candidate_id.as_ref() != Some(candidate)
                || intent.attempt_id != self.nodes[agent].attempt_id
            {
                return Err(());
            }
            if intent.class == VcsStageClass::Verifiable {
                let carry = carry.ok_or(())?;
                let expected = intent.expected.ok_or(())?;
                if intent.stage != VcsStageKind::BundleApply
                    || intent.source_runner.as_deref() != Some(&carry.source)
                    || intent.target_runner.as_deref() != Some(&carry.target)
                    || intent.tip_commit.as_deref() != Some(&expected.commit)
                    || expected.reference
                        != plakous_workspace::occupancy_vcs::candidate_ref(
                            namespace,
                            candidate.as_str(),
                        )
                    || proved.replace(expected.commit).is_some()
                {
                    return Err(());
                }
            }
        }
        if staged && proved.is_none() {
            return Err(());
        }
        Ok(proved)
    }
}
