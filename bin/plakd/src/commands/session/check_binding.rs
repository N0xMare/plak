//! Read-only binding inspection beside a running owner.

use crate::command::BindingScope;
use crate::home::paths::PlakPaths;
use plakous::sessions::SessionLocator;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

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

impl Row {
    pub(crate) fn audit_failed(&self) -> bool {
        self.binding_change.is_some()
            || self.unavailable.is_some()
            || self
                .child_binding
                .as_ref()
                .is_some_and(|child| child.binding_change.is_some() || child.unavailable.is_some())
    }
}

/// No `Services` or config resolution: neither inspection path opens a writer,
/// a runtime, a runner, credential sources or diagnostics.
pub(crate) fn inspect(paths: &PlakPaths, scope: BindingScope) -> CliResult<Report> {
    let disposition = dispositions::inspect(paths)?;
    inspect_runtime(&paths.runtime_database(), &disposition, scope, &mut |_| {})
}

/// A head's archival, succession and child state, known before decoding.
struct Status {
    archived: bool,
    superseded: bool,
    child: bool,
    closed: bool,
}

impl Status {
    fn of(
        disposition: &dispositions::Dispositions,
        locator: SessionLocator,
        archived: bool,
        line_head: bool,
    ) -> Self {
        let key = dispositions::key(locator);
        let (product_archived, superseded) =
            disposition.rows.get(&key).copied().unwrap_or_default();
        let child_status = disposition.children_closed.get(&key).copied();
        Self {
            archived: product_archived || archived,
            superseded: superseded || !line_head,
            child: child_status.is_some(),
            closed: child_status == Some(true),
        }
    }

    /// Only these rows can refuse a same-key upgrade.
    const fn gates(&self) -> bool {
        !self.archived && !self.superseded && !self.closed
    }
}

/// `decoding` sees the `<session>/<branch>` key of each head selected for decoding.
fn inspect_runtime(
    runtime: &Path,
    disposition: &dispositions::Dispositions,
    scope: BindingScope,
    decoding: &mut dyn FnMut(String),
) -> CliResult<Report> {
    let selected = |status: &Status| scope == BindingScope::All || status.gates();
    let sources = plakous::sessions::rehearsal::inspect_selected_binding_presentations_sqlite(
        runtime,
        |locator, archived, line_head| {
            let decode = selected(&Status::of(disposition, locator, archived, line_head));
            if decode {
                decoding(dispositions::key(locator));
            }
            decode
        },
    )
    .map_err(CliFailure::from)?;
    let mut compatible = true;
    let mut rows = Vec::with_capacity(sources.len());
    for head in sources {
        let locator = head.locator;
        let status = Status::of(disposition, locator, head.archived, head.line_head);
        if !selected(&status) {
            continue;
        }
        let gates = status.gates();
        let Status {
            archived,
            superseded,
            child,
            closed,
        } = status;
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
        let decoded = head.presentation.as_ref().is_some_and(Result::is_ok);
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
        // Only the full audit can prove decode failures outside the binding
        // projection. Binding findings on decoded children go to the child probe.
        let child_binding = (child && decoded).then(|| ChildBinding {
            binding_change: binding_change.take(),
            unavailable: unavailable.take(),
        });
        let row = Row {
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
        };
        if gates {
            compatible &= !row.audit_failed();
        }
        rows.push(row);
    }
    Ok(Report {
        compatible,
        sessions: rows,
    })
}

/// Inspects a runtime store under given product and tree dispositions, keyed
/// `<session>/<branch>`, with the keys of the heads selected for decoding.
#[cfg(test)]
pub(crate) fn inspect_with_dispositions(
    runtime: &Path,
    rows: std::collections::HashMap<String, (bool, bool)>,
    children_closed: std::collections::HashMap<String, bool>,
    scope: BindingScope,
) -> CliResult<(Report, Vec<String>)> {
    let disposition = dispositions::Dispositions {
        rows,
        children_closed,
    };
    let mut decoded = Vec::new();
    let report = inspect_runtime(runtime, &disposition, scope, &mut |key| decoded.push(key))?;
    Ok((report, decoded))
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
    scope: BindingScope,
    summary: bool,
    output: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    #[derive(Serialize)]
    struct Summary {
        compatible: bool,
        count: usize,
    }
    let report = inspect(paths, scope)?;
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
