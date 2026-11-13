//! Read-only binding inspection beside a running owner.

use crate::home::paths::PlakPaths;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Deserialize, Serialize)]
pub(crate) struct Report {
    pub(crate) compatible: bool,
    pub(crate) sessions: Vec<Row>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Row {
    pub(crate) session: String,
    pub(crate) branch: String,
    pub(crate) checkpoint: String,
    pub(crate) binding_change: Option<String>,
    pub(crate) unavailable: Option<String>,
}

/// No `Services` or config resolution: neither inspection path opens a writer,
/// a runtime, a runner, credential sources or diagnostics.
pub(crate) fn inspect(paths: &PlakPaths) -> CliResult<Report> {
    let sources = plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(
        paths.runtime_database(),
    )
    .map_err(CliFailure::from)?;
    let mut compatible = true;
    let mut rows = Vec::with_capacity(sources.len());
    for (locator, frozen) in sources {
        let (binding_change, unavailable) =
            match plakous_hosted::agent::check_frozen_binding(&frozen) {
                Ok(change) => (change.map(|change| change.to_string()), None),
                Err(error) => (None, Some(error.message().to_owned())),
            };
        compatible &= binding_change.is_none() && unavailable.is_none();
        rows.push(Row {
            session: locator.session_id().to_string(),
            branch: locator.branch_id().to_string(),
            checkpoint: locator.checkpoint_id().to_string(),
            binding_change,
            unavailable,
        });
    }
    Ok(Report {
        compatible,
        sessions: rows,
    })
}

pub(crate) fn run(paths: &PlakPaths, output: &mut dyn Write) -> CliResult<ProcessOutcome> {
    let report = inspect(paths)?;
    serde_json::to_writer(&mut *output, &report).map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(output).map_err(|_| CliFailure::new("stdout failed"))?;
    if report.compatible {
        Ok(ProcessOutcome::Success)
    } else {
        Err(CliFailure::new(
            "binding preflight refused; no runtime or state was changed",
        ))
    }
}
