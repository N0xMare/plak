//! VCS stages on a runner (spec 03 §14): every effect a runner-backed port
//! runs is recorded as a `vcs_stage` intent, and published, before it runs.
//! Its steps are keyed by the intent's operation id, so the runner runs each
//! at most once and a host that lost the link replays it by key. A stage is
//! removed in the revision that records the effect's answer. A stage left
//! behind, because the link could not learn the outcome or the process ended
//! with the effect in flight, is settled by [`stage_outcome`]: a reproducible
//! one stays recorded, and the rerun of the same effect takes its operation
//! id again, so each step reaches the runner under the key it was first sent
//! with and the runner answers it (in progress, or its stored answer) instead
//! of running it a second time beside a step the lost host may still run.

use std::sync::Arc;

use plakous_workspace::occupancy_vcs::stage::VcsStage;

use super::{
    AttemptId, CandidateId, Effects, MintTicket, ProductAgentTree, TopologyError, VcsStageClass,
    VcsStageIntent, VcsStageKind,
};

/// What settles a stage found unfinished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StageOutcome {
    /// Run it again under the same key: the effect's own workflow records it
    /// failed, and its rerun takes the stage's operation id again.
    Rerun,
    /// Check the recorded outcome: its rerun asks whether the expected
    /// reference already names the expected commit, and runs only if not.
    Check,
    /// Not provable either way: a `vcs_fault` for the stage's node.
    Fault,
}

/// How an unfinished stage settles. The one place a stage's outcome is read;
/// node recovery replaces it with its own reading of the stage.
pub(crate) fn stage_outcome(intent: &VcsStageIntent) -> StageOutcome {
    match intent.class {
        VcsStageClass::Reproducible => StageOutcome::Rerun,
        VcsStageClass::Verifiable => StageOutcome::Check,
        VcsStageClass::Ambiguous => StageOutcome::Fault,
    }
}

/// The most ancestors [`ProductAgentTree::tree_runner_ancestors`] names.
pub const MAX_RUNNER_ANCESTORS: usize = 8;

/// The `agent_operations` key of a new stage.
fn stage_operation_id() -> String {
    format!("plak:vcs_stage:{}", uuid::Uuid::new_v4().simple())
}

/// One local stage of `kind` for `agent_id` on `runner`.
pub(super) fn local_stage(
    kind: VcsStageKind,
    class: VcsStageClass,
    agent_id: &str,
    runner: &str,
) -> VcsStageIntent {
    VcsStageIntent {
        attempt_id: None,
        stage: kind,
        class,
        agent_id: agent_id.to_owned(),
        candidate_id: None,
        source_runner: None,
        target_runner: Some(runner.to_owned()),
        base_commit: None,
        tip_commit: None,
        bundle_digest: None,
        expected: None,
    }
}

/// The stages one effects batch runs as, one per ticket, in ticket order.
/// Empty lists run unstaged: a port that works in this process records none.
#[derive(Debug, Default)]
pub(crate) struct EffectStages {
    pub(crate) base_pins: Vec<Arc<VcsStage>>,
    pub(crate) refreshes: Vec<Arc<VcsStage>>,
    pub(crate) seals: Vec<Arc<VcsStage>>,
    pub(crate) dormancy: Vec<Arc<VcsStage>>,
    pub(crate) adopts: Vec<Arc<VcsStage>>,
    pub(crate) hooks: Vec<Arc<VcsStage>>,
    pub(crate) unmints: Vec<Arc<VcsStage>>,
}

impl EffectStages {
    /// The stage of ticket `index` in `list`, if it was staged.
    pub(crate) fn at(list: &[Arc<VcsStage>], index: usize) -> Option<Arc<VcsStage>> {
        list.get(index).cloned()
    }
}

impl ProductAgentTree {
    /// Records `intent` as a stage about to run and returns its key. The
    /// caller publishes the revision before the stage runs.
    ///
    /// A recorded stage with exactly these inputs, left unfinished, is taken
    /// again under its own operation id: the rerun's steps carry the keys
    /// the first run's did.
    pub(crate) fn record_vcs_stage(
        &mut self,
        intent: &VcsStageIntent,
    ) -> Result<Arc<VcsStage>, TopologyError> {
        let row = intent.to_operation(String::new(), &intent.agent_id, None)?;
        if let Some(left) = self.operations.values().find(|recorded| {
            recorded.tool_name == VcsStageIntent::KIND
                && recorded.canonical_request == row.canonical_request
        }) {
            return Ok(VcsStage::new(left.operation_id.clone()));
        }
        // A verifiable stage left unfinished checked its expected outcome
        // before this one was recorded, so this one replaces it.
        if intent.class == VcsStageClass::Verifiable {
            let replaced: Vec<String> = self
                .operations
                .values()
                .filter(|recorded| {
                    VcsStageIntent::from_operation(recorded)
                        .ok()
                        .flatten()
                        .is_some_and(|left| {
                            left.stage == intent.stage
                                && left.agent_id == intent.agent_id
                                && left.candidate_id == intent.candidate_id
                                && left.expected == intent.expected
                        })
                })
                .map(|recorded| recorded.operation_id.clone())
                .collect();
            for id in replaced {
                self.operations.remove(&id);
            }
        }
        let id = stage_operation_id();
        let row = intent.to_operation(id.clone(), &intent.agent_id, None)?;
        self.operations.insert(id.clone(), row);
        self.bump();
        Ok(VcsStage::new(id))
    }

    /// Settles `stage` once its effect's answer is recorded: removes it, or,
    /// when a step's outcome was unknown, settles it by [`stage_outcome`].
    pub(crate) fn settle_vcs_stage(&mut self, stage: &VcsStage) {
        if stage.outcome_unknown() {
            self.settle_unfinished_vcs_stage(stage.operation_id());
        } else if self.operations.remove(stage.operation_id()).is_some() {
            self.bump();
        }
    }

    /// Settles the verifiable carry back of `candidate_id` once the seal it
    /// served is recorded: a lost run's check is moot then.
    pub(crate) fn settle_carried_seal(&mut self, agent_id: &str, candidate_id: &CandidateId) {
        let carried: Vec<String> = self
            .operations
            .values()
            .filter(|row| {
                VcsStageIntent::from_operation(row)
                    .ok()
                    .flatten()
                    .is_some_and(|intent| {
                        intent.class == VcsStageClass::Verifiable
                            && intent.agent_id == agent_id
                            && intent.candidate_id.as_ref() == Some(candidate_id)
                    })
            })
            .map(|row| row.operation_id.clone())
            .collect();
        if carried.is_empty() {
            return;
        }
        for id in carried {
            self.operations.remove(&id);
        }
        self.bump();
    }

    /// Settles every stage a crashed or lost process left recorded.
    pub(crate) fn settle_unfinished_vcs_stages(&mut self) {
        let ids: Vec<String> = self
            .operations
            .values()
            .filter(|row| row.tool_name == VcsStageIntent::KIND)
            .map(|row| row.operation_id.clone())
            .collect();
        for id in ids {
            self.settle_unfinished_vcs_stage(&id);
        }
    }

    fn settle_unfinished_vcs_stage(&mut self, id: &str) {
        let Some(row) = self.operations.get(id) else {
            return;
        };
        let intent = VcsStageIntent::from_operation(row).ok().flatten();
        let outcome = intent.as_ref().map(stage_outcome);
        // A reproducible or verifiable stage stays for its rerun while its
        // node can still run it; a node that is gone or closed reruns nothing.
        let reruns = matches!(outcome, Some(StageOutcome::Rerun | StageOutcome::Check));
        if reruns
            && intent.as_ref().is_some_and(|intent| {
                self.nodes
                    .get(&intent.agent_id)
                    .is_some_and(|node| node.status != super::NodeStatus::Closed)
            })
        {
            return;
        }
        let caller = row.caller_agent_id.clone();
        self.operations.remove(id);
        self.bump();
        if !reruns {
            let (agent_id, attempt_id) = intent.map_or_else(
                || (caller, None),
                |intent| (intent.agent_id, intent.attempt_id),
            );
            self.fault_vcs_stage(&agent_id, id, attempt_id);
        }
    }

    /// The stages `effects` runs as on `runner`, recorded. Seals, base pins,
    /// refresh applications, reclamations and removals re-run; adopts and
    /// `on_mint` hooks are ambiguous.
    pub(crate) fn record_effect_stages(
        &mut self,
        effects: &Effects,
        runner: &str,
    ) -> Result<EffectStages, TopologyError> {
        let reproducible = VcsStageClass::Reproducible;
        let mut stages = EffectStages::default();
        // Each effect runs on the runner of the node it serves.
        let placed = |tree: &Self, agent: &str| {
            tree.nodes
                .get(agent)
                .and_then(|node| node.placement.runner_id.clone())
                .unwrap_or_else(|| runner.to_owned())
        };
        for ticket in &effects.base_pins {
            let on = placed(self, ticket.agent_id());
            let mut intent =
                local_stage(VcsStageKind::Refresh, reproducible, ticket.agent_id(), &on);
            intent.base_commit = Some(ticket.base().to_owned());
            stages.base_pins.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.refreshes {
            stages
                .refreshes
                .push(self.record_vcs_stage(&ticket.stage(runner))?);
        }
        for ticket in &effects.seals {
            let on = placed(self, &ticket.agent_id);
            let mut intent = local_stage(VcsStageKind::Seal, reproducible, &ticket.agent_id, &on);
            intent.attempt_id.clone_from(&ticket.attempt_id);
            intent.candidate_id = Some(ticket.candidate_id.clone());
            intent.base_commit = Some(ticket.base_commit.clone());
            stages.seals.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.dormancy {
            let on = placed(self, ticket.agent_id());
            let intent = local_stage(VcsStageKind::Unmint, reproducible, ticket.agent_id(), &on);
            stages.dormancy.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.adopts {
            let (agent_id, candidate_id) = ticket.stage_identity();
            let on = placed(self, agent_id);
            let mut intent =
                local_stage(VcsStageKind::Adopt, VcsStageClass::Ambiguous, agent_id, &on);
            intent.candidate_id = Some(candidate_id.clone());
            stages.adopts.push(self.record_vcs_stage(&intent)?);
        }
        for hook in &effects.hooks {
            let on = placed(self, &hook.agent_id);
            let intent = local_stage(
                VcsStageKind::Mint,
                VcsStageClass::Ambiguous,
                &hook.agent_id,
                &on,
            );
            stages.hooks.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.unmints {
            let agent_id = ticket
                .workspace_id
                .as_deref()
                .and_then(|id| self.workspace(id))
                .map_or_else(
                    || self.root_agent_id.clone(),
                    |row| row.owner_agent_id.clone(),
                );
            let on = placed(self, &agent_id);
            let intent = local_stage(VcsStageKind::Unmint, reproducible, &agent_id, &on);
            stages.unmints.push(self.record_vcs_stage(&intent)?);
        }
        Ok(stages)
    }

    /// The commits runner `runner` already holds for this tree, at most
    /// [`MAX_RUNNER_ANCESTORS`]: each was the tip of a bundle applied
    /// there. A bundle to it leaves out each one the runner still holds.
    #[must_use]
    pub fn tree_runner_ancestors(&self, runner: &str) -> Vec<String> {
        self.runner_ancestors()
            .get(runner)
            .map(|commits| commits.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Records that `runner` holds `commit`, the tip of a bundle applied
    /// there. A full set gives up its first commit for it.
    pub(crate) fn record_runner_ancestor(&mut self, runner: &str, commit: &str) {
        let commits = self
            .lineage
            .runner_ancestors
            .entry(runner.to_owned())
            .or_default();
        if !commits.insert(commit.to_owned()) {
            return;
        }
        while commits.len() > MAX_RUNNER_ANCESTORS {
            let Some(first) = commits.iter().find(|held| *held != commit).cloned() else {
                break;
            };
            commits.remove(&first);
        }
        self.bump();
    }

    /// Records the stage `ticket`'s mint runs as on `runner`: a fresh copy
    /// before the child's first turn, re-run from its spawn.
    pub(crate) fn record_mint_stage(
        &mut self,
        ticket: &MintTicket,
        attempt_id: Option<&AttemptId>,
        runner: &str,
    ) -> Result<Arc<VcsStage>, TopologyError> {
        let mut intent = local_stage(
            VcsStageKind::Mint,
            VcsStageClass::Reproducible,
            ticket.agent_id(),
            ticket.target_runner().unwrap_or(runner),
        );
        intent.attempt_id = attempt_id.cloned();
        intent.base_commit = ticket.input_commit().map(str::to_owned);
        self.record_vcs_stage(&intent)
    }
}

#[cfg(test)]
mod tests;
