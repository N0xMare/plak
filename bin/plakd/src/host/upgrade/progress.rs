//! Categorical progress on the private host-upgrade connection.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use crate::CliResult;
use crate::carry::child_reopen::PreExistingChildFaults;

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
    /// Step-drain waiting, timed from the start of the drain.
    Waiting(&'a str),
    Qualification(Timing),
    Barrier(Barrier),
    ChildFaults(&'a str, PreExistingChildFaults),
}

/// The earlier admission-verdict barrier, timed by itself.
pub(super) enum Barrier {
    Started,
    Resolved(std::time::Duration),
}

impl Barrier {
    pub(super) fn detail(&self) -> String {
        match self {
            Self::Started => "admission barrier: resolving earlier admission verdicts".to_owned(),
            Self::Resolved(elapsed) => format!(
                "admission barrier: resolved earlier admission verdicts elapsed_ms={}",
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            ),
        }
    }
}

#[derive(Default)]
pub(super) struct ChildFaults(Mutex<BTreeMap<String, PreExistingChildFaults>>);

impl ChildFaults {
    pub(super) fn record(&self, root: &str, faults: PreExistingChildFaults) {
        let mut latest = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if faults.is_empty() {
            latest.remove(root);
        } else {
            latest.insert(root.to_owned(), faults);
        }
    }

    pub(super) fn finish(self) -> PreExistingChildFaults {
        let mut total = PreExistingChildFaults::default();
        for faults in self
            .0
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .into_values()
        {
            total.merge(faults);
        }
        total
    }
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
