//! Product child addresses and read-only child inspection.

use std::collections::VecDeque;
use std::io;

use plakous::sessions::{
    AgentSessions, SESSION_HISTORY_VISIBILITY_REVISION, SessionHistoryDirection,
    SessionHistoryItem, SessionHistoryPageOutcome, SessionHistoryPageRequest,
    SessionHistoryVisibility, SessionLocator,
};
use plakous_hosted::agent_tree::NodeRecord;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_recursive_subagents_store::TreeInspection;

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;

/// Splits `<id>[.cN]` and resolves `<id>` (a full id or an unambiguous prefix
/// among this workspace's sessions) without opening a writable store.
pub(crate) fn parse_read_address<'a>(
    paths: &PlakPaths,
    id: &'a str,
) -> CliResult<(String, Option<&'a str>)> {
    let Some((root, child)) = id.split_once('.') else {
        return Ok((super::resolve_read_only_session_id(paths, id)?, None));
    };
    let valid_child = child.strip_prefix('c').is_some_and(|ordinal| {
        ordinal
            .parse::<u32>()
            .is_ok_and(|number| number.to_string() == ordinal)
    });
    if !valid_child {
        return Err(CliFailure::new("unknown session id"));
    }
    Ok((
        super::resolve_read_only_session_id(paths, root)?,
        Some(child),
    ))
}

fn selected_child(
    paths: &PlakPaths,
    record: &SessionRecord,
    child: &str,
) -> CliResult<(NodeRecord, String)> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)?;
    let TreeInspection::Tree(tree) = observed else {
        return Err(CliFailure::new("unknown child: no persisted tree"));
    };
    let rows = tree.rows();
    let node = rows
        .nodes()
        .get(child)
        .filter(|node| node.parent_agent_id.is_some())
        .ok_or_else(|| CliFailure::new("unknown child in session tree"))?;
    let attempt = rows.attempt(child).map_or_else(
        || "unavailable".to_owned(),
        |row| format!("{:?}", row.status),
    );
    Ok((node.clone(), attempt))
}

fn child_locator(node: &NodeRecord) -> CliResult<SessionLocator> {
    node.session_locator
        .as_deref()
        .ok_or_else(|| CliFailure::new("child canonical locator unavailable"))?
        .parse()
        .map_err(|_| CliFailure::new("child canonical locator is invalid"))
}

pub(crate) fn show_selected_child(
    paths: &PlakPaths,
    record: &SessionRecord,
    child: &str,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let (node, attempt) = selected_child(paths, record, child)?;
    writeln!(stdout, "Session {}.{child}", record.id)
        .and_then(|()| {
            writeln!(
                stdout,
                "  Child          {child} node={:?} attempt={attempt}",
                node.status
            )
        })
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let recorded = match child_locator(&node) {
        Ok(locator) => locator,
        Err(error) => {
            writeln!(stdout, "  Canonical      {}", error.message())
                .map_err(|_| CliFailure::new("stdout failed"))?;
            return Err(CliFailure::recovery_required(error.message()));
        }
    };
    writeln!(stdout, "  Recorded head  {recorded}")
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let (current, recovery) = super::inspect_locator(paths, recorded)?;
    if current != recorded {
        writeln!(
            stdout,
            "  Current head   {current} (recorded locator is stale; unchanged)"
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    if let Some(report) = recovery {
        super::write_recovery_report(paths, stdout, &report)
    } else {
        writeln!(
            stdout,
            "  Canonical      quiescent at {current}; node retirement is separate"
        )
        .map_err(|_| CliFailure::new("stdout failed"))
    }
}

pub(crate) fn log_child_read_only(
    paths: &PlakPaths,
    id: &str,
    kind: Option<&str>,
    tail: usize,
    json: bool,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let (root, child) = parse_read_address(paths, id)?;
    let child = child.ok_or_else(|| CliFailure::new("child session address required"))?;
    let record = crate::store::ProductStore::inspect_session_read_only(paths, &root)
        .map_err(CliFailure::from)?;
    let (node, _) = selected_child(paths, &record, child)?;
    let recorded = child_locator(&node)?;
    let (current, recovery) = super::inspect_locator(paths, recorded)?;
    let entries = child_history_tail(paths, current, kind, tail)?;
    if let Some(report) = recovery {
        let disclaimer = "items after the last commit are not shown";
        if json {
            let header = serde_json::json!({
                "kind": "history_snapshot", "head": current.to_string(),
                "status": "Working", "recovery_phase": format!("{:?}", report.phase()),
                "note": disclaimer,
            });
            writeln!(stdout, "{header}").map_err(|_| CliFailure::new("stdout failed"))?;
        } else {
            writeln!(
                stdout,
                "Working head {current} phase={:?}; {disclaimer}",
                report.phase()
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
    }
    let mut table = crate::cli_output::Table::new(["ENTRY", "KIND", "CALLS", "RESULTS", "TEXT"]);
    for item in &entries {
        let text = item.display_text().unwrap_or_default();
        if json {
            let entry = serde_json::json!({
                "entry_id": item.entry_id(), "kind": item.projected_kind(), "text": text,
                "tool_calls": item.tool_calls(), "tool_results": item.tool_results(),
            });
            writeln!(stdout, "{entry}").map_err(|_| CliFailure::new("stdout failed"))?;
        } else {
            table
                .push([
                    item.entry_id().to_string(),
                    item.projected_kind().to_owned(),
                    item.tool_calls().to_string(),
                    item.tool_results().to_string(),
                    text.to_owned(),
                ])
                .map_err(CliFailure::new)?;
        }
    }
    if !json {
        if entries.is_empty() {
            writeln!(stdout, "No canonical history records for this child.")
                .map_err(|_| CliFailure::new("stdout failed"))?;
        } else {
            crate::cli_human_output()
                .write_table(stdout, &table)
                .map_err(|_| CliFailure::new("stdout failed"))?;
        }
    }
    Ok(())
}

fn child_history_tail(
    paths: &PlakPaths,
    locator: SessionLocator,
    kind: Option<&str>,
    tail: usize,
) -> CliResult<VecDeque<SessionHistoryItem>> {
    if tail == 0 {
        return Err(CliFailure::new("session log --tail must be >= 1"));
    }
    let mut retained = VecDeque::new();
    let mut cursor = None;
    loop {
        let request = SessionHistoryPageRequest::new(
            locator,
            locator.checkpoint_id(),
            SessionHistoryVisibility::Content,
            SESSION_HISTORY_VISIBILITY_REVISION,
            SessionHistoryDirection::Backward,
            256,
            65_536,
            cursor,
        );
        let page = AgentSessions::inspect_history_page_sqlite(paths.runtime_database(), &request)
            .map_err(|error| {
            CliFailure::new(format!("child history inspection failed: {error}"))
        })?;
        let SessionHistoryPageOutcome::Page { items, next, .. } = page else {
            return Err(CliFailure::new(
                "child history page requires a new snapshot",
            ));
        };
        for item in items.into_iter().rev() {
            if kind.is_none_or(|kind| item.projected_kind() == kind) {
                retained.push_front(item);
                if retained.len() == tail {
                    return Ok(retained);
                }
            }
        }
        if next.is_none() {
            return Ok(retained);
        }
        cursor = next;
    }
}
