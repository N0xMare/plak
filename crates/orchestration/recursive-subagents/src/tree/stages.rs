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
    AttemptId, Effects, MintTicket, ProductAgentTree, TopologyError, VcsStageClass, VcsStageIntent,
    VcsStageKind,
};

/// What settles a stage found unfinished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StageOutcome {
    /// Run it again under the same key: the effect's own workflow records it
    /// failed, and its rerun takes the stage's operation id again.
    Rerun,
    /// Check the recorded outcome. No stage of a single runner records one
    /// today, so this settles as a fault until node recovery checks it.
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
        // A reproducible stage stays for its rerun while its node can still
        // run it; a node that is gone or closed reruns nothing.
        if outcome == Some(StageOutcome::Rerun)
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
        if outcome != Some(StageOutcome::Rerun) {
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
        for ticket in &effects.base_pins {
            let mut intent = local_stage(
                VcsStageKind::Refresh,
                reproducible,
                ticket.agent_id(),
                runner,
            );
            intent.base_commit = Some(ticket.base().to_owned());
            stages.base_pins.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.refreshes {
            stages
                .refreshes
                .push(self.record_vcs_stage(&ticket.stage(runner))?);
        }
        for ticket in &effects.seals {
            let mut intent =
                local_stage(VcsStageKind::Seal, reproducible, &ticket.agent_id, runner);
            intent.attempt_id.clone_from(&ticket.attempt_id);
            intent.candidate_id = Some(ticket.candidate_id.clone());
            intent.base_commit = Some(ticket.base_commit.clone());
            stages.seals.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.dormancy {
            let intent = local_stage(
                VcsStageKind::Unmint,
                reproducible,
                ticket.agent_id(),
                runner,
            );
            stages.dormancy.push(self.record_vcs_stage(&intent)?);
        }
        for ticket in &effects.adopts {
            let (agent_id, candidate_id) = ticket.stage_identity();
            let mut intent = local_stage(
                VcsStageKind::Adopt,
                VcsStageClass::Ambiguous,
                agent_id,
                runner,
            );
            intent.candidate_id = Some(candidate_id.clone());
            stages.adopts.push(self.record_vcs_stage(&intent)?);
        }
        for hook in &effects.hooks {
            let intent = local_stage(
                VcsStageKind::Mint,
                VcsStageClass::Ambiguous,
                &hook.agent_id,
                runner,
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
            let intent = local_stage(VcsStageKind::Unmint, reproducible, &agent_id, runner);
            stages.unmints.push(self.record_vcs_stage(&intent)?);
        }
        Ok(stages)
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
            runner,
        );
        intent.attempt_id = attempt_id.cloned();
        intent.base_commit = ticket.input_commit().map(str::to_owned);
        self.record_vcs_stage(&intent)
    }
}

#[cfg(test)]
mod tests;
