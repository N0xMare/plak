//! Immutable allocation associations are resolved identity, never owed effects.
use super::super::projector::CHILD_SESSION_KIND;
use super::{OperationRecord, ProductAgentTree, digest_text};

pub(super) fn resolved(tree: &ProductAgentTree, row: &OperationRecord) -> Result<bool, String> {
    if row.tool_name != CHILD_SESSION_KIND {
        return Ok(false);
    }
    let agent = &row.caller_agent_id;
    if agent == &tree.root_agent_id
        || row.operation_id != format!("{CHILD_SESSION_KIND}:{agent}")
        || row.fold.is_some()
        || row.terminal_code.is_some()
        || row.terminal_response.is_some()
        || row.request_digest != digest_text(&row.canonical_request)
    {
        return Err("invalid immutable child allocation marker".into());
    }
    let marker: plakous::sessions::SessionLocator = row
        .canonical_request
        .parse()
        .map_err(|_| "invalid child allocation locator")?;
    let current: plakous::sessions::SessionLocator = tree
        .nodes
        .get(agent)
        .and_then(|node| node.session_locator.as_deref())
        .ok_or("child allocation has no bound node")?
        .parse()
        .map_err(|_| "invalid bound child locator")?;
    if marker.session_id() != current.session_id() || marker.branch_id() != current.branch_id() {
        return Err("child allocation disagrees with its bound node".into());
    }
    Ok(true)
}

pub(super) fn record(agent: &str, locator: &str) -> OperationRecord {
    OperationRecord {
        operation_id: format!("{CHILD_SESSION_KIND}:{agent}"),
        caller_agent_id: agent.into(),
        tool_name: CHILD_SESSION_KIND.into(),
        request_digest: digest_text(locator),
        canonical_request: locator.into(),
        fold: None,
        terminal_code: None,
        terminal_response: None,
    }
}
