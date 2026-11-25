//! Read-only binding inspection beside a running owner.

use crate::home::paths::PlakPaths;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use serde::{Deserialize, Serialize};
use std::io::Write;

mod dispositions;

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
    pub(crate) archived: bool,
    pub(crate) superseded: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) child: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) child_binding: Option<ChildBinding>,
    pub(crate) unaffected: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ChildBinding {
    binding_change: Option<String>,
    unavailable: Option<String>,
}

/// No `Services` or config resolution: neither inspection path opens a writer,
/// a runtime, a runner, credential sources or diagnostics.
pub(crate) fn inspect(paths: &PlakPaths) -> CliResult<Report> {
    let disposition = dispositions::inspect(paths)?;
    let sources = plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(
        paths.runtime_database(),
    )
    .map_err(CliFailure::from)?;
    let mut compatible = true;
    let mut rows = Vec::with_capacity(sources.len());
    for head in sources {
        let locator = head.locator;
        let key = dispositions::key(locator);
        let (archived, superseded) = disposition.rows.get(&key).copied().unwrap_or_default();
        let archived = archived || head.archived;
        let superseded = superseded || !head.line_head;
        let child_status = disposition.children_closed.get(&key).copied();
        let child = child_status.is_some();
        let closed = child_status == Some(true);
        let unaffected = head
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.as_ref().ok())
            .and_then(|frozen| frozen.executor_requirements().ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, id, _)| id.as_str().starts_with("plakous.mcp."))
            .map(|(name, _, _)| format!("MCP external contract unaffected: {name}"))
            .collect();
        let (mut binding_change, mut unavailable) = match head.presentation {
            None => (None, None),
            Some(Err(error)) => (
                error.binding_change().map(ToString::to_string),
                Some(error.to_string()),
            ),
            Some(Ok(frozen)) => match plakous_hosted::agent::check_frozen_binding(&frozen) {
                Ok(change) => (
                    qualified_change(change, child).map(|change| change.to_string()),
                    None,
                ),
                Err(error) => (None, Some(error.message().to_owned())),
            },
        };
        if !archived && !superseded && !closed {
            compatible &= binding_change.is_none() && unavailable.is_none();
        }
        // A predecessor compares the flat fields without checking child
        // reopen. Keep the audit details, but let the child probe qualify them.
        let child_binding = child.then(|| ChildBinding {
            binding_change: binding_change.take(),
            unavailable: unavailable.take(),
        });
        rows.push(Row {
            session: locator.session_id().to_string(),
            branch: locator.branch_id().to_string(),
            checkpoint: locator.checkpoint_id().to_string(),
            binding_change,
            unavailable,
            archived,
            superseded,
            child,
            closed,
            child_binding,
            unaffected,
        });
    }
    Ok(Report {
        compatible,
        sessions: rows,
    })
}

fn qualified_change(
    change: Option<plakous::agent::BindingChange>,
    child: bool,
) -> Option<plakous::agent::BindingChange> {
    change.or_else(|| {
        (crate::test_hooks::binding_executor_cut()
            || child && crate::test_hooks::child_binding_cut())
        .then(|| plakous::agent::BindingChange::ToolBinding {
            tool: "fixture.revised-executor".into(),
        })
    })
}

pub(crate) fn run(
    paths: &PlakPaths,
    summary: bool,
    output: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    #[derive(Serialize)]
    struct Summary {
        compatible: bool,
        count: usize,
    }
    let report = inspect(paths)?;
    if !summary {
        return write_report(report, output);
    }
    serde_json::to_writer(
        &mut *output,
        &Summary {
            compatible: report.compatible,
            count: report.sessions.len(),
        },
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(output).map_err(|_| CliFailure::new("stdout failed"))?;
    verdict(report.compatible)
}

fn write_report(report: Report, output: &mut dyn Write) -> CliResult<ProcessOutcome> {
    serde_json::to_writer(&mut *output, &report).map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(output).map_err(|_| CliFailure::new("stdout failed"))?;
    verdict(report.compatible)
}

fn verdict(compatible: bool) -> CliResult<ProcessOutcome> {
    if compatible {
        Ok(ProcessOutcome::Success)
    } else {
        Err(CliFailure::new(
            "binding preflight refused; no runtime or state was changed",
        ))
    }
}
