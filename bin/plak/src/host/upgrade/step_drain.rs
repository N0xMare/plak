//! Same-key step freeze; cross-key live carry is not yet admitted.

use crate::command::HostDrain;
use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;
use crate::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{
    AdmissionPause, HostStepDrain, SessionHost, StepDrainMonitor,
};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

pub(super) struct Budget {
    pub(super) start: Instant,
    pub(super) barrier_deadline: Instant,
    deadline: Option<Instant>,
}

impl Budget {
    pub(super) fn new(mode: HostDrain) -> CliResult<Self> {
        let start = Instant::now();
        let deadline = mode
            .duration()
            .map(|duration| {
                start.checked_add(duration).ok_or_else(|| {
                    CliFailure::new("host drain deadline is outside the monotonic clock")
                })
            })
            .transpose()?;
        Ok(Self {
            start,
            barrier_deadline: deadline.map_or(start + Duration::from_secs(60), |deadline| {
                deadline.max(start + Duration::from_secs(10))
            }),
            deadline,
        })
    }
}

pub(super) fn prepare(
    host: &Arc<SessionHost<PlakExtension>>,
    services: &Services,
    pause: &AdmissionPause,
    budget: Budget,
    cancelled: &AtomicBool,
    runtime: &tokio::runtime::Handle,
    progress: &(dyn Fn(&str) -> CliResult<()> + Sync),
) -> CliResult<HostStepDrain> {
    while crate::test_hooks::upgrade_preparation_refused("step-drain-gate") {
        if cancelled.load(std::sync::atomic::Ordering::Acquire)
            || Instant::now() >= budget.barrier_deadline
        {
            return Err(CliFailure::new(
                "host step-drain test rendezvous was cancelled or expired",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let roots = crate::store::ProductStore::inspect_upgrade_roots(services.paths()?)
        .map_err(CliFailure::new)?;
    let trees = roots
        .into_iter()
        .filter_map(|record| plakous_recursive_subagents::hub::lookup(&record.node.tree_id))
        .collect();
    runtime.block_on(async {
        host.drain_steps(
            pause,
            budget.deadline,
            super::super::HANDOFF_DEADLINE.saturating_sub(super::super::STOP_MARGIN),
            &StepDrainMonitor {
                cancelled,
                progress,
            },
            trees,
        )
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_step_drain_verdict_floor_does_not_extend_natural_draining() {
        for milliseconds in [0, 5_000, 60_000] {
            let duration = Duration::from_millis(milliseconds);
            let budget = Budget::new(HostDrain::Bounded { milliseconds }).unwrap();
            assert_eq!(budget.deadline, Some(budget.start + duration));
            assert_eq!(
                budget.barrier_deadline,
                budget.start + duration.max(Duration::from_secs(10))
            );
        }
        let idle = Budget::new(HostDrain::Idle).unwrap();
        assert_eq!(idle.deadline, None);
        assert_eq!(idle.barrier_deadline, idle.start + Duration::from_secs(60));
    }
}
