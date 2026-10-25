//! Immutable conflict lineage, resolver ownership and source acceptance.

use super::{
    AdoptLanding, AdoptTicket, CandidateId, CandidateKind, CandidateRecord, ProductAgentTree,
    TopologyError, TopologyErrorCode, WorkspaceStatus, now_unix_millis,
};
use plakous_workspace::occupancy_vcs::{ConflictCandidate, ConflictPath, ConflictPin};

#[cfg(test)]
mod tests;

impl ProductAgentTree {
    pub(super) fn resolution_source(
        &self,
        caller: &str,
        candidate: &CandidateRecord,
    ) -> Result<Option<CandidateRecord>, TopologyError> {
        let resolution = candidate.kind == CandidateKind::Conflict || candidate.resolves.is_some();
        if resolution
            && (candidate.conflict_target.is_none()
                || candidate.conflict_paths.is_empty()
                || (candidate.kind == CandidateKind::Conflict
                    && candidate.source_candidate_id.is_none()))
        {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "conflict lineage is incomplete; nothing was adopted",
            ));
        }
        if let Some(target) = &candidate.conflict_target
            && target != caller
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!(
                    "{} belongs to {target}'s checkout; nothing was adopted",
                    candidate.candidate_id
                ),
            ));
        }
        let Some(id) = candidate
            .source_candidate_id
            .as_ref()
            .or(candidate.resolves.as_ref())
        else {
            return Ok(None);
        };
        let source = self.candidates.get(id).ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::NotSealed,
                format!("resolution source {id} is not retained"),
            )
        })?;
        if let Some(via) = &source.adopted_via {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!("{id} was already adopted via {via}; nothing was adopted"),
            ));
        }
        if let Some(adopter) = &source.adopted_by {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!("{id} was already adopted by {adopter}; nothing was adopted"),
            ));
        }
        if self.input_was_relinquished(source) {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                format!("{id} was explicitly discarded; nothing was adopted"),
            ));
        }
        self.check_adopt_available(&source.workspace_id)?;
        if self
            .nodes
            .get(&source.producing_agent_id)
            .is_some_and(|node| !node.status.is_finished())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::AdoptNotApplicable,
                "the resolution source is running a later attempt; wait for it or close it",
            ));
        }
        self.check_base_pin_recovery(&source.workspace_id)?;
        Ok(Some(source.clone()))
    }

    pub(super) fn check_adopt_available(&self, workspace: &str) -> Result<(), TopologyError> {
        if self.inflight_adopts.contains(workspace)
            || (self
                .workspaces
                .get(workspace)
                .is_some_and(|row| row.status == WorkspaceStatus::Adopting)
                && !self.workspace_has_vcs_fault(workspace))
        {
            return Err(TopologyError::new(
                TopologyErrorCode::WorkspaceRecoveryRequired,
                "an adoption is already in flight for that copy; wait for its outcome",
            ));
        }
        Ok(())
    }

    pub(super) fn reserve_conflict_pin(
        &self,
        candidate: &CandidateRecord,
        caller: &str,
    ) -> Option<ConflictPin> {
        if candidate.kind != CandidateKind::Work || candidate.resolves.is_some() {
            return None;
        }
        let ordinal = 1 + self
            .candidates
            .values()
            .filter(|row| row.source_candidate_id.as_ref() == Some(&candidate.candidate_id))
            .count();
        Some(ConflictPin {
            id: format!("{}-x{ordinal}", candidate.candidate_id),
            label: format!("plak conflict {} into {caller}", candidate.candidate_id),
        })
    }

    pub(super) fn record_conflict_candidate(
        &mut self,
        ticket: &AdoptTicket,
        paths: &[ConflictPath],
        merge: ConflictCandidate,
    ) -> Option<CandidateId> {
        let pin = ticket.conflict_pin.as_ref()?;
        let id = CandidateId::from_persisted(pin.id.clone());
        let mut record = ticket.candidate.clone();
        record.candidate_id = id.clone();
        record.kind = CandidateKind::Conflict;
        record.source_candidate_id = Some(ticket.candidate.candidate_id.clone());
        record.conflict_target = Some(ticket.caller_id.clone());
        record.conflict_paths = paths.to_vec();
        record.base_commit = merge.base.commit;
        record.base_content = merge.base.content;
        record.base_change_id = merge.base.change_id;
        record.commit = merge.snapshot.commit;
        record.content = merge.snapshot.content;
        record.change_id = merge.snapshot.change_id;
        record.sealed_at = now_unix_millis();
        self.candidates.insert(id.clone(), record);
        Some(id)
    }

    pub(super) fn resolver_input(&self, workspace: &str) -> Option<&CandidateRecord> {
        let row = self.workspaces.get(workspace)?;
        let id = row.input_candidate_id.as_ref()?;
        self.candidates.get(id).filter(|candidate| {
            candidate.kind == CandidateKind::Conflict
                && row.base_commit.as_deref() == Some(candidate.base_commit.as_str())
        })
    }

    pub(super) fn finish_resolution(
        &mut self,
        ticket: &AdoptTicket,
        landing: &AdoptLanding,
    ) -> Result<(), TopologyError> {
        let Some(source) = &ticket.source else {
            return Ok(());
        };
        if let Some(record) = self.candidates.get_mut(&source.candidate_id) {
            record.adopted_by = Some(ticket.caller_id.clone());
            record.adopted_via = Some(ticket.candidate.candidate_id.clone());
            record.target_before = Some(
                landing
                    .target_before
                    .split(':')
                    .next()
                    .unwrap_or(&landing.target_before)
                    .to_owned(),
            );
            record.merged_at = Some(
                landing
                    .target_after
                    .split(':')
                    .next()
                    .unwrap_or(&landing.target_after)
                    .to_owned(),
            );
            record.adopted_evidence.clone_from(&ticket.evidence);
        }
        let input_id = self
            .workspaces
            .get(&ticket.candidate.workspace_id)
            .and_then(|row| row.input_candidate_id.clone());
        if let Some(conflict) = input_id
            .and_then(|id| self.candidates.get_mut(&id))
            .filter(|row| {
                ticket.candidate.resolves.is_some()
                    && row.kind == CandidateKind::Conflict
                    && row.source_candidate_id.as_ref() == Some(&source.candidate_id)
            })
        {
            conflict.adopted_by = Some(ticket.caller_id.clone());
            conflict.adopted_via = Some(ticket.candidate.candidate_id.clone());
        }
        let reclaimed = self.reclaimed(&source.workspace_id);
        if let Some(row) = self.workspaces.get_mut(&source.workspace_id) {
            row.base_commit = Some(source.commit.clone());
            row.base_content = Some(source.content.clone());
            row.base_change_id.clone_from(&source.change_id);
            row.status = if reclaimed {
                WorkspaceStatus::CleanRemoved
            } else {
                WorkspaceStatus::Adopted
            };
            row.adopter_agent_id = None;
        }
        if !self.conversation_collected(&source.producing_agent_id) {
            self.record_effect_outcome(
                &source.producing_agent_id,
                Some(source.producing_attempt_id.clone()),
                super::effect_outcomes::Outcome::AttemptAdopted,
            )?;
        }
        Ok(())
    }

    pub(super) fn supersede_resolutions(&mut self, ticket: &AdoptTicket) {
        let source = ticket.source.as_ref().unwrap_or(&ticket.candidate);
        for row in self.candidates.values_mut() {
            if row.candidate_id != ticket.candidate.candidate_id
                && row.adopted_by.is_none()
                && (row.source_candidate_id.as_ref() == Some(&source.candidate_id)
                    || row.resolves.as_ref() == Some(&source.candidate_id))
            {
                row.adopted_via = Some(ticket.candidate.candidate_id.clone());
            }
        }
    }
}
