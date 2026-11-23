//! One host-wide drain budget; no per-resident multiplication or authority move.

use super::SessionHost;
use crate::runtime::step_hold::WorkerStepPause;
use crate::topology_runtime::{ProductTopologyRuntime, TreeStepPause};
use crate::{CliFailure, CliResult};
use plakous::agent::Agent;
use plakous::agent::host_control::{
    BoundaryHold, BoundaryPhase, BoundaryProgress, hold_step_boundary,
};
use plakous::model::DriverInstanceId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

mod cutover;
#[cfg(test)]
mod tests;

/// Host-layer cancellation and categorical progress; neither grants runtime authority.
pub struct StepDrainMonitor<'a> {
    /// Requester loss or operator cancellation, checked throughout the rendezvous.
    pub cancelled: &'a AtomicBool,
    /// Bounded status text without model input, tool arguments or credentials.
    pub progress: &'a (dyn Fn(&str) -> CliResult<()> + Sync),
}

struct HeldDriver {
    agent: Agent,
    hold: BoundaryHold,
}

/// Reversible freeze retained until planned release or preparation rollback.
/// Field order reopens trees before drivers and host-owner admissions.
pub struct HostStepDrain {
    trees: BTreeMap<String, TreeStepPause>,
    drivers: BTreeMap<DriverInstanceId, HeldDriver>,
    owners: Vec<WorkerStepPause>,
}

impl HostStepDrain {
    /// Time reserved for the roots' cancel and release after recording children.
    pub const ROOT_RELEASE_RESERVE: Duration = crate::runtime::HOST_CANCEL_SETTLE_TIMEOUT
        .saturating_add(crate::runtime::HOST_RELEASE_DEADLINE);

    /// Records planned child release after the same-key handoff commits, while
    /// all tree admissions and drivers are still held. A root's own release
    /// need not exist or settle for its already-joined children to continue.
    ///
    /// Returns each tree whose record failed or had no remaining time to start.
    pub async fn record_child_handoffs(&self, remaining: Duration) -> Vec<(String, String)> {
        let deadline = tokio::time::Instant::now() + remaining;
        let mut failures = Vec::new();
        for (id, tree) in &self.trees {
            tree.begin_committed_handoff();
            let record = crate::runtime::record_tree_handoff(tree.runtime());
            let budget = "child handoff records exceeded the host release budget";
            let result = if tokio::time::Instant::now() >= deadline {
                Err(budget.to_owned())
            } else {
                tokio::time::timeout_at(deadline, record)
                    .await
                    .unwrap_or_else(|_| Err(budget.to_owned()))
            };
            if let Err(error) = result {
                failures.push((id.clone(), error));
            }
        }
        failures
    }

    async fn pause_tree(
        &mut self,
        tree: Arc<ProductTopologyRuntime>,
        deadline: Option<Instant>,
        monitor: &StepDrainMonitor<'_>,
    ) -> CliResult<()> {
        let identity = tree_id(&tree)?;
        if self.trees.contains_key(&identity) {
            return Ok(());
        }
        let mut pausing = Box::pin(tree.pause_step_admissions());
        loop {
            if monitor.cancelled.load(Ordering::Acquire) {
                return Err(CliFailure::new(
                    "host step drain cancelled while joining tree admissions",
                ));
            }
            if let Ok(result) =
                tokio::time::timeout(Duration::from_millis(20), pausing.as_mut()).await
            {
                self.trees
                    .insert(identity, result.map_err(CliFailure::new)?);
                return Ok(());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(CliFailure::new(
                    "host_step_drain_blocked: tree admissions did not join within the host-wide freeze budget",
                ));
            }
            (monitor.progress)("joining tree admissions")?;
        }
    }

    fn hold(&mut self, agent: Agent) -> CliResult<()> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.drivers.entry(agent.id()) {
            let hold = hold_step_boundary(&agent)
                .map_err(|error| CliFailure::new(format!("host_step_hold_refused: {error}")))?;
            entry.insert(HeldDriver { agent, hold });
        }
        Ok(())
    }

    async fn refresh(&mut self) -> CliResult<bool> {
        let mut joined = true;
        let mut wanted = BTreeSet::new();
        for index in 0..self.owners.len() {
            if let Some(agent) = self.owners[index].observed_agent()? {
                wanted.insert(agent.id());
                self.hold(agent)?;
            }
            let Some(boundary) = self.owners[index].boundary()? else {
                joined = false;
                continue;
            };
            let agent = boundary.agent.upgrade().ok_or_else(|| {
                CliFailure::new("the root runtime ended before its boundary hold")
            })?;
            joined &= boundary
                .turn
                .is_none_or(|turn| agent.turn_control(turn).is_some());
            wanted.insert(agent.id());
            self.hold(agent)?;
            if let Some(tree) = boundary.tree {
                let identity = tree_id(&tree)?;
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    self.trees.entry(identity)
                {
                    entry.insert(
                        tree.pause_step_admissions()
                            .await
                            .map_err(CliFailure::new)?,
                    );
                }
            }
        }
        let mut agents = Vec::new();
        for tree in self.trees.values() {
            let snapshot = tree.agents().await.map_err(CliFailure::new)?;
            joined &= !snapshot.transitioning && tree.effects_joined();
            agents.extend(snapshot.agents);
        }
        for agent in agents {
            wanted.insert(agent.id());
            self.hold(agent)?;
        }
        self.drivers.retain(|id, _| wanted.contains(id));
        Ok(joined)
    }

    fn pending_boundary(&self, model_bound: bool) -> CliResult<Option<&'static str>> {
        for driver in self.drivers.values() {
            let progress = driver.hold.progress().map_err(|error| {
                CliFailure::new(format!("host_step_owner_unavailable: {error}"))
            })?;
            if matches!(progress, BoundaryProgress::Held { turn: Some(turn), .. } if driver.agent.turn_control(turn).is_none())
            {
                return Ok(Some("turn_control_unavailable"));
            }
            match progress {
                BoundaryProgress::Held { .. } => {}
                BoundaryProgress::Draining {
                    phase: BoundaryPhase::Model,
                    ..
                } if model_bound => {}
                BoundaryProgress::Draining {
                    phase: BoundaryPhase::Compaction,
                    ..
                } => {
                    return Ok(Some("paid_compaction_in_flight"));
                }
                BoundaryProgress::Draining {
                    phase: BoundaryPhase::Effect,
                    ..
                } => {
                    return Ok(Some("effect_in_flight"));
                }
                BoundaryProgress::Draining {
                    phase: BoundaryPhase::Model,
                    ..
                } => {
                    return Ok(Some("model_in_flight"));
                }
                BoundaryProgress::Requested => return Ok(Some("boundary_requested")),
            }
        }
        Ok(None)
    }
}

fn tree_id(runtime: &ProductTopologyRuntime) -> CliResult<String> {
    runtime
        .read(|tree| tree.tree_id.clone())
        .map_err(|error| CliFailure::new(error.message))
}

impl<P> SessionHost<P>
where
    P: plakous_host_protocol::ProtocolExtension + 'static,
    P::Intent: Send,
    P::Observation: Send,
    plakous_host_protocol::wire::HostFrame<P>: serde::Serialize,
    plakous_host_protocol::wire::ClientFrame<P>: serde::de::DeserializeOwned,
{
    async fn pause_step_owners(
        &self,
        drain: &mut HostStepDrain,
        deadline: Option<Instant>,
        monitor: &StepDrainMonitor<'_>,
    ) -> CliResult<()> {
        let mut pending = self.slots();
        loop {
            if monitor.cancelled.load(Ordering::Acquire) || self.closed() {
                return Err(CliFailure::new(
                    "host step drain cancelled while joining session owners",
                ));
            }
            let mut busy = Vec::new();
            for slot in pending {
                match slot.pause_steps()? {
                    super::super::slot::StepOwnerHold::Busy => busy.push(slot),
                    super::super::slot::StepOwnerHold::Dormant => {}
                    super::super::slot::StepOwnerHold::Held(owner) => {
                        if let Some(agent) = owner.observed_agent()? {
                            drain.hold(agent)?;
                        }
                        drain.owners.push(owner);
                    }
                }
            }
            if busy.is_empty() {
                return Ok(());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(CliFailure::new(
                    "host_step_owner_busy: session owners did not join within the host-wide freeze budget",
                ));
            }
            pending = busy;
            (monitor.progress)("joining session owners")?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Freezes existing root/child steps under the caller's admission pause.
    /// The verdict barrier must succeed before calling this host-layer seam.
    ///
    /// # Errors
    /// Refuses without surrender on cancellation, a missing owner, or work
    /// that cannot join before the one host-wide freeze deadline.
    pub async fn drain_steps(
        &self,
        pause: &super::AdmissionPause,
        drain_until: Option<Instant>,
        freeze_budget: Duration,
        monitor: &StepDrainMonitor<'_>,
        trees: Vec<Arc<ProductTopologyRuntime>>,
    ) -> CliResult<HostStepDrain> {
        if !Arc::ptr_eq(&pause.gate, &self.upgrade_admission)
            || !*self
                .upgrade_admission
                .read()
                .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(CliFailure::new(
                "step drain requires paused host admissions",
            ));
        }
        let freeze_until = drain_until
            .map(|deadline| {
                deadline
                    .max(Instant::now())
                    .checked_add(freeze_budget)
                    .ok_or_else(|| {
                        CliFailure::new("host freeze deadline is outside the monotonic clock")
                    })
            })
            .transpose()?;
        let mut drain = HostStepDrain {
            drivers: BTreeMap::new(),
            trees: BTreeMap::new(),
            owners: Vec::new(),
        };
        self.pause_step_owners(&mut drain, freeze_until, monitor)
            .await?;
        for tree in trees {
            drain.pause_tree(tree, freeze_until, monitor).await?;
        }
        cutover::finish(self, drain, drain_until, freeze_until, monitor).await
    }
}
