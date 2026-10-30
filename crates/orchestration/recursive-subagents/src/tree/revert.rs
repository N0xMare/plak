//! Reverse one pinned landing without undoing unrelated, later target edits.

use super::{
    AdoptCandidateRequest, AdoptLanding, AdoptOutcome, AdoptTicket, CandidateId, CandidateRecord,
    ProductAgentTree, TopologyError, TopologyErrorCode, WorkspaceRecord, WorkspaceStatus,
};
use plakous_workspace::occupancy_vcs::{RevertRange, is_jj_copy_root};

#[derive(Clone, Debug)]
pub(super) struct RevertPlan {
    pub(super) requested: CandidateId,
    pub(super) range: RevertRange,
    records: Vec<CandidateId>,
    bases: Vec<WorkspaceRecord>,
}

fn refuse(message: impl Into<String>) -> TopologyError {
    TopologyError::new(TopologyErrorCode::AdoptNotApplicable, message)
}

impl ProductAgentTree {
    #[allow(
        clippy::too_many_lines,
        reason = "revert admission validates accepted candidate lineage and target identity before reserving the reversal"
    )]
    pub(super) fn begin_revert(
        &mut self,
        caller: &str,
        request: &AdoptCandidateRequest,
        requested: &CandidateRecord,
    ) -> Result<(), TopologyError> {
        if request.accept_conflicts {
            return Err(refuse(
                "revert cannot accept conflict markers; nothing was reverted",
            ));
        }
        let target = self.node(caller)?.path.clone();
        if is_jj_copy_root(&target) || target.join(".jj").exists() {
            return Err(refuse("revert is not yet available for jj"));
        }
        if requested.adopted_by.is_none()
            && requested.reverted_by.as_deref() == Some(caller)
            && let Some(at) = &requested.reverted_at
        {
            return Err(refuse(format!(
                "already reverted at {at}; adopt it again to restore it"
            )));
        }
        if requested.adopted_by.as_deref() != Some(caller) {
            return Err(refuse(
                "only the original adopter may revert this candidate",
            ));
        }
        let accepting = requested
            .adopted_via
            .as_ref()
            .unwrap_or(&requested.candidate_id);
        let candidate = self
            .candidates
            .get(accepting)
            .cloned()
            .ok_or_else(|| refuse("the accepting candidate is no longer retained"))?;
        if candidate.adopted_by.as_deref() != Some(caller) {
            return Err(refuse(
                "the accepting candidate is not adopted in this checkout",
            ));
        }
        let (Some(before), Some(after)) = (&candidate.target_before, &candidate.merged_at) else {
            return Err(refuse(
                "no adoption provenance to revert; nothing was reverted",
            ));
        };
        let group: Vec<_> = self
            .candidates
            .values()
            .filter(|row| {
                row.adopted_by.as_deref() == Some(caller)
                    && (&row.candidate_id == accepting
                        || row.adopted_via.as_ref() == Some(accepting))
            })
            .cloned()
            .collect();
        self.check_revert_stack(&group)?;
        let source = candidate
            .resolves
            .as_ref()
            .or(candidate.source_candidate_id.as_ref())
            .and_then(|id| self.candidates.get(id))
            .cloned();
        let mut bases = Vec::new();
        for record in &group {
            self.check_adopt_available(&record.workspace_id)?;
            self.check_base_pin_recovery(&record.workspace_id)?;
            if self
                .nodes
                .get(&record.producing_agent_id)
                .is_some_and(|node| !node.status.is_finished())
            {
                return Err(refuse(
                    "a producing copy is running; wait for it before reverting",
                ));
            }
            if let Some(row) = self.workspaces.get(&record.workspace_id)
                && !self.reclaimed(&row.workspace_id)
                && row.base_commit.as_deref() == Some(record.commit.as_str())
            {
                let mut restored = row.clone();
                restored.base_commit = Some(record.base_commit.clone());
                restored.base_content = Some(record.base_content.clone());
                restored.base_change_id.clone_from(&record.base_change_id);
                bases.push(restored);
            }
        }
        let plan = RevertPlan {
            requested: requested.candidate_id.clone(),
            range: RevertRange {
                before: before.clone(),
                after: after.clone(),
                restore_bases: bases
                    .iter()
                    .map(|row| -> Result<_, TopologyError> {
                        Ok((
                            self.workspace_producer(&row.workspace_id)?,
                            row.base_commit
                                .clone()
                                .ok_or_else(|| refuse("revert base is missing"))?,
                        ))
                    })
                    .collect::<Result<_, _>>()?,
            },
            records: group.iter().map(|row| row.candidate_id.clone()).collect(),
            bases,
        };
        self.queue_revert(caller, request, candidate, source, target, plan);
        Ok(())
    }

    fn check_revert_stack(&self, group: &[CandidateRecord]) -> Result<(), TopologyError> {
        for earlier in group {
            if let Some(later) = self.candidates.values().find(|later| {
                later.workspace_id == earlier.workspace_id
                    && later.adopted_by.is_some()
                    && !group
                        .iter()
                        .any(|row| row.candidate_id == later.candidate_id)
                    && (self
                        .candidate_seal_order(&later.candidate_id)
                        .zip(self.candidate_seal_order(&earlier.candidate_id))
                        .is_some_and(|(later, earlier)| later > earlier)
                        || later.sealed_at > earlier.sealed_at
                        || later.base_commit == earlier.commit)
            }) {
                return Err(refuse(format!(
                    "revert {} first; later work from this copy is adopted",
                    later.candidate_id
                )));
            }
        }
        Ok(())
    }

    fn queue_revert(
        &mut self,
        caller: &str,
        request: &AdoptCandidateRequest,
        candidate: CandidateRecord,
        source: Option<CandidateRecord>,
        target: std::path::PathBuf,
        plan: RevertPlan,
    ) {
        let child_path = self
            .workspaces
            .get(&candidate.workspace_id)
            .map_or_else(|| self.launch_checkout.clone(), |row| row.path.clone());
        let interrupted_adopt = self.workspace_has_vcs_fault(&candidate.workspace_id)
            || source
                .as_ref()
                .is_some_and(|row| self.workspace_has_vcs_fault(&row.workspace_id));
        for id in std::iter::once(&candidate.workspace_id)
            .chain(source.as_ref().map(|row| &row.workspace_id))
        {
            if let Some(row) = self.workspaces.get_mut(id) {
                row.status = WorkspaceStatus::Adopting;
                row.adopter_agent_id = Some(caller.to_owned());
            }
            self.inflight_adopts.insert(id.clone());
        }
        self.effects.adopts.push(AdoptTicket {
            resolution_refusal: None,
            resolver_paths: Vec::new(),
            revert: Some(plan),
            accept_conflicts: false,
            conflict_pin: None,
            source,
            source_copy_agent: None,
            interrupted_adopt,
            namespace: super::digest_text(&self.tree_id),
            advance_copy_base: false,
            caller_id: caller.to_owned(),
            candidate,
            target_path: target,
            child_path,
            expected_target: request.expected_target.clone(),
            evidence: Vec::new(),
        });
        self.bump();
    }

    pub(super) fn finish_revert(
        &mut self,
        ticket: &AdoptTicket,
        landing: AdoptLanding,
    ) -> Result<AdoptOutcome, TopologyError> {
        let Some(plan) = &ticket.revert else {
            return Err(refuse("no reversal was admitted"));
        };
        for id in &plan.records {
            if let Some(row) = self.candidates.get_mut(id) {
                row.adopted_by = None;
                row.adopted_via = None;
                row.reverted_at = Some(landing.target_after.clone());
                row.reverted_by = Some(ticket.caller_id.clone());
            }
        }
        for base in &plan.bases {
            if let Some(row) = self.workspaces.get_mut(&base.workspace_id) {
                row.base_commit.clone_from(&base.base_commit);
                row.base_content.clone_from(&base.base_content);
                row.base_change_id.clone_from(&base.base_change_id);
            }
        }
        for id in std::iter::once(&ticket.candidate.workspace_id)
            .chain(ticket.source.as_ref().map(|row| &row.workspace_id))
        {
            let reclaimed = self.reclaimed(id);
            if let Some(row) = self.workspaces.get_mut(id) {
                row.status = if reclaimed {
                    WorkspaceStatus::CleanRemoved
                } else {
                    WorkspaceStatus::Active
                };
                row.adopter_agent_id = None;
            }
        }
        self.bump();
        let (files, files_truncated) = landing.receipt_files();
        Ok(AdoptOutcome {
            reverted: true,
            candidate_id: plan.requested.clone(),
            target_before: landing.target_before,
            target_after: landing.target_after,
            files_changed: landing.files_changed,
            files,
            files_truncated,
        })
    }
}
