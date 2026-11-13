//! One host-wide drain budget; no per-resident multiplication or authority move.

use super::SessionHost;
use crate::runtime::step_hold::WorkerStepPause;
use crate::topology_runtime::TreeStepPause;
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

struct HeldDriver {
    agent: Agent,
    hold: BoundaryHold,
}

/// Reversible freeze retained until planned release or preparation rollback.
/// Field order reopens trees before drivers and host-owner admissions.
pub struct HostStepDrain {
    trees: BTreeMap<usize, TreeStepPause>,
    drivers: BTreeMap<DriverInstanceId, HeldDriver>,
    owners: Vec<WorkerStepPause>,
}

impl HostStepDrain {
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
                let identity = Arc::as_ptr(&tree) as usize;
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

    fn at_boundaries(&self, model_bound: bool) -> CliResult<bool> {
        for driver in self.drivers.values() {
            let progress = driver.hold.progress().map_err(|error| {
                CliFailure::new(format!("host_step_owner_unavailable: {error}"))
            })?;
            if matches!(progress, BoundaryProgress::Held { turn: Some(turn), .. } if driver.agent.turn_control(turn).is_none())
            {
                return Ok(false);
            }
            if !(matches!(progress, BoundaryProgress::Held { .. })
                || (model_bound
                    && matches!(
                        progress,
                        BoundaryProgress::Draining {
                            phase: BoundaryPhase::Model,
                            ..
                        }
                    )))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn blocked_at_bound(&self) -> bool {
        self.drivers.values().any(|driver| {
            matches!(
                driver.hold.progress(),
                Ok(BoundaryProgress::Draining {
                    phase: BoundaryPhase::Compaction | BoundaryPhase::Effect,
                    ..
                })
            )
        })
    }
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
        cancelled: &AtomicBool,
    ) -> CliResult<()> {
        let mut pending = self.slots();
        loop {
            if cancelled.load(Ordering::Acquire) || self.closed() {
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
        cancelled: &AtomicBool,
        trees: Vec<Arc<crate::topology_runtime::ProductTopologyRuntime>>,
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
        self.pause_step_owners(&mut drain, freeze_until, cancelled)
            .await?;
        for tree in trees {
            let identity = Arc::as_ptr(&tree) as usize;
            if let std::collections::btree_map::Entry::Vacant(entry) = drain.trees.entry(identity) {
                entry.insert(
                    tree.pause_step_admissions()
                        .await
                        .map_err(CliFailure::new)?,
                );
            }
        }
        loop {
            if cancelled.load(Ordering::Acquire) || self.closed() {
                return Err(CliFailure::new(
                    "host step drain was cancelled before surrender",
                ));
            }
            let at_bound = drain_until.is_some_and(|deadline| Instant::now() >= deadline);
            if at_bound && drain.blocked_at_bound() {
                return Err(CliFailure::new(
                    "host_step_drain_blocked: compaction or effects have not joined at the drain bound; source remains authoritative",
                ));
            }
            let effects_joined = match freeze_until {
                Some(deadline) => tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), drain.refresh())
                    .await.map_err(|_| CliFailure::new("host_step_drain_blocked: the freeze rendezvous did not join within its host-wide bound"))??,
                None => drain.refresh().await?,
            };
            if effects_joined && drain.at_boundaries(at_bound)? {
                return Ok(drain);
            }
            if freeze_until.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(CliFailure::new(
                    "host_step_drain_blocked: compaction, effects or tree transitions remain in flight; source remains authoritative",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
