//! Categorical progress on the private host-upgrade connection.

use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::CliResult;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Image,
    Capability,
    BindingSource,
    BindingDestination,
    ExactReopen,
    RunnerProcess,
    CarryCapacity,
    StandaloneAdmission,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Timing {
    phase: Phase,
    elapsed_ms: u64,
}

impl Timing {
    pub(super) fn detail(&self) -> String {
        let phase = match self.phase {
            Phase::Image => "image",
            Phase::Capability => "capability",
            Phase::BindingSource => "binding_source",
            Phase::BindingDestination => "binding_destination",
            Phase::ExactReopen => "exact_reopen",
            Phase::RunnerProcess => "runner_process",
            Phase::CarryCapacity => "carry_capacity",
            Phase::StandaloneAdmission => "standalone_admission",
        };
        format!("qualification phase={phase} elapsed_ms={}", self.elapsed_ms)
    }
}

pub(super) enum Event<'a> {
    Waiting(&'a str),
    Qualification(Timing),
}

pub(super) type Report<'a> = dyn Fn(Event<'_>) -> CliResult<()> + Sync + 'a;

pub(super) fn timed<T>(
    report: &Report<'_>,
    phase: Phase,
    operation: impl FnOnce() -> CliResult<T>,
) -> CliResult<T> {
    let start = Instant::now();
    let result = operation();
    report(Event::Qualification(Timing {
        phase,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))?;
    result
}
