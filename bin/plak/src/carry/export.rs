//! Export a session into a carry document.

#[cfg(any(test, feature = "tui"))]
use super::document::write_document;
use super::document::{
    CarryBinding, CarryDocument, CarrySource, FORMAT, PendingWorkEntry, ProducerBuild, VERSION,
};
use super::history;
use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind};
use plakous_host_protocol::topology::{
    NodeDisposition, NodeStatus, WorkspaceMode, WorkspaceStatus,
};
use plakous_recursive_subagents_store::TreeInspection;
use std::fmt;
#[cfg(any(test, feature = "tui"))]
use std::fs;
use std::path::PathBuf;

/// Why a session cannot be exported now. Each names its cause; none is
/// transient enough to retry blindly.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ExportRefusal {
    /// The record never bound a canonical runtime.
    NoRuntime,
    /// A command submission or workflow request has no feed fact yet: it may
    /// still reach the runtime, and an open settles or withdraws it.
    UnsettledIntent,
    /// A sub-agent is live or sealing: it may still change the tree.
    SubagentRunning {
        agent_id: String,
        status: NodeStatus,
    },
    /// The agent tree could not be read.
    TreeUnreadable(String),
    /// A turn is running or parked with an unresolved operation.
    TurnUnfinished,
    /// A job row has no durable terminal evidence.
    BackgroundProcesses,
    /// The runtime advanced past the product record; resuming once settles it.
    RecordBehindHead,
    /// The runtime store could not be read.
    Runtime(String),
}

impl fmt::Display for ExportRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRuntime => formatter.write_str("the session has no canonical runtime yet"),
            Self::UnsettledIntent => formatter.write_str(
                "a command submission or workflow request has no feed fact yet; open the session once to settle it",
            ),
            Self::SubagentRunning { agent_id, status } => write!(
                formatter,
                "sub-agent {agent_id} is not settled ({}); let it finish or close it first", node_status_name(*status)
            ),
            Self::TreeUnreadable(error) => {
                write!(formatter, "the agent tree could not be read: {error}")
            }
            Self::TurnUnfinished => formatter.write_str(
                "a turn is running or parked with an unresolved operation; let it finish or run `plak session recover` first",
            ),
            Self::BackgroundProcesses => formatter.write_str(
                "background process outcomes are not settled; resume the session and send a prompt to deliver their notices before exporting",
            ),
            Self::RecordBehindHead => formatter.write_str(
                "the runtime advanced past the session record; resume the session once first",
            ),
            Self::Runtime(error) => write!(formatter, "the runtime store could not be read: {error}"),
        }
    }
}

/// Exports `record` from this build's state, read-only.
///
/// First applies the session's projector to its feed tail, a lease-free
/// product write (spec 02 §2.5.1), then exports the reloaded record at the
/// head its views name.
pub(crate) fn export_session(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<CarryDocument, ExportRefusal> {
    if record.plakous_locator.is_none() {
        return Err(ExportRefusal::NoRuntime);
    }
    if let Some(projected) = current_projection(paths, record)? {
        return export_projected(paths, record, &projected);
    }
    let runtime = |error: String| ExportRefusal::Runtime(error);
    let records: std::sync::Arc<dyn crate::store::session_records::SessionRecordStore> =
        std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        ));
    let sessions = plakous::sessions::SessionBackend::sqlite(paths.runtime_database())
        .and_then(AgentSessions::new)
        .map_err(|error| runtime(error.to_string()))?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| runtime("application executor unavailable".to_owned()))?;
    let projected = executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &records, record,
        ))
        .map_err(|_| runtime("application execution failed".to_owned()))?
        .map_err(runtime)?
        .ok_or(ExportRefusal::NoRuntime)?;
    let record = records.load(&record.id).map_err(runtime)?;
    export_projected(paths, &record, &projected)
}

/// A settled projection needs no writable runtime (whose maintenance can
/// change SQLite bytes even when export has no feed records to apply).
fn current_projection(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<Option<plakous_hosted::runtime::ProjectedSession>, ExportRefusal> {
    use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
    use plakous_hosted::sessions::projector::{
        pending_command_ids, pending_workflow_operation_ids,
    };

    let (_, recorded, intents, views) =
        crate::store::ProductStore::inspect_projection_read_only(paths, &record.id)
            .map_err(ExportRefusal::Runtime)?;
    let (Some(recorded), Some(views)) = (recorded, views) else {
        return Ok(None);
    };
    let cursor = FeedCursor::new(recorded.session_id(), views.cursor);
    match AgentSessions::inspect_feed_sqlite(
        paths.runtime_database(),
        FeedPageRequest::after(cursor),
    )
    .map_err(|error| ExportRefusal::Runtime(error.to_string()))?
    {
        FeedPage::Records { records, tail, .. } if records.is_empty() && tail == views.cursor => {
            Ok(Some(plakous_hosted::runtime::ProjectedSession {
                head: plakous_hosted::runtime::projected_head(recorded, &views),
                pending_commands: pending_command_ids(&views, &intents),
                pending_workflows: pending_workflow_operation_ids(&views, &intents),
            }))
        }
        _ => Ok(None),
    }
}

/// Exports `record`, whose views a projector pass just brought to `projected`.
fn export_projected(
    paths: &PlakPaths,
    record: &SessionRecord,
    projected: &plakous_hosted::runtime::ProjectedSession,
) -> Result<CarryDocument, ExportRefusal> {
    if !projected.pending_commands.is_empty() || !projected.pending_workflows.is_empty() {
        return Err(ExportRefusal::UnsettledIntent);
    }
    let locator = projected.head;
    let (source_subagents, source_resolved_count, pending_work) =
        tree_reference_facts(paths, record)?;
    match AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), locator) {
        Ok(None) => {}
        Ok(Some(_)) => return Err(ExportRefusal::TurnUnfinished),
        Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
            return Err(ExportRefusal::RecordBehindHead);
        }
        Err(error) => return Err(ExportRefusal::Runtime(error.to_string())),
    }
    let history = AgentSessions::export_history_sqlite(paths.runtime_database(), locator).map_err(
        |error| match error.kind() {
            AgentSessionsErrorKind::UnsettledProcesses => ExportRefusal::BackgroundProcesses,
            AgentSessionsErrorKind::InvalidInput => ExportRefusal::TurnUnfinished,
            _ => ExportRefusal::Runtime(error.to_string()),
        },
    )?;
    let source = CarrySource {
        session_id: record.id.clone(),
        state_key: crate::home::state::state_key(),
        producer: match crate::build_identity::binary_id() {
            "unavailable" => ProducerBuild::Unavailable,
            binary_id => ProducerBuild::Verified {
                binary_id: binary_id.to_owned(),
            },
        },
    };
    Ok(CarryDocument {
        local_digest: None,
        format: FORMAT.to_owned(),
        version: VERSION,
        source,
        workspace_root: record.workspace_root.clone(),
        title: record.title.clone(),
        binding: CarryBinding {
            account: record.account.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            composition: record.composition,
            target_selection: record.target_selection.clone(),
        },
        named_state: history
            .named_state()
            .map_err(|error| ExportRefusal::Runtime(error.to_string()))?,
        history: Some(
            history::History::export(&history)
                .map_err(|e| ExportRefusal::Runtime(e.to_string()))?,
        ),
        pending_work,
        queue: record
            .pending_queue
            .iter()
            .map(|item| item.text.clone())
            .collect(),
        prompt_history: record.prompt_history.clone(),
        transcript: record.transcript.to_vec(),
        source_subagents,
        source_resolved_count: Some(source_resolved_count),
    })
}

/// Captures inert facts needed to find unfinished source work without
/// recreating its topology or transferring live authority. Live or sealing
/// children still refuse export; interrupted/recovery-required rows are safe
/// to reference because export is read-only and import never opens them.
fn tree_reference_facts(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<(usize, usize, Vec<PendingWorkEntry>), ExportRefusal> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record)
        .map_err(|error| ExportRefusal::TreeUnreadable(error.message().to_owned()))?;
    let TreeInspection::Tree(tree) = observed else {
        return Ok((0, 0, Vec::new()));
    };
    let tree = tree.rows();
    let child_count = count_archivable_children(
        tree.nodes()
            .values()
            .filter(|node| node.parent_agent_id.is_some())
            .map(|node| (node.agent_id.as_str(), node.status)),
    )?;
    let mut facts = tree
        .nodes()
        .values()
        .filter(|node| node.parent_agent_id.is_some() && source_node_unresolved(node.status))
        .map(|node| PendingWorkEntry {
            operation: node.agent_id.clone(),
            kind: "source_child".to_owned(),
            state: node_status_name(node.status).to_ascii_lowercase(),
            detail: Some(format!(
                "workspace={}; locator={}",
                node.workspace_id,
                if node.session_locator.is_some() {
                    "recorded"
                } else {
                    "unavailable"
                }
            )),
        })
        .collect::<Vec<_>>();
    facts.extend(
        tree.candidates()
            .values()
            .filter(|candidate| {
                candidate.adopted_by.is_none()
                    && tree
                        .workspaces()
                        .get(&candidate.workspace_id)
                        .is_some_and(|workspace| {
                            matches!(
                                workspace.status,
                                WorkspaceStatus::Active | WorkspaceStatus::Adopted
                            )
                        })
            })
            .map(|candidate| PendingWorkEntry {
                operation: candidate.candidate_id.to_string(),
                kind: "source_candidate".to_owned(),
                state: "unadopted".to_owned(),
                detail: Some(format!(
                    "workspace={}; producer={}; commit={}",
                    candidate.workspace_id, candidate.producing_agent_id, candidate.commit
                )),
            }),
    );
    facts.extend(
        tree.workspaces()
            .values()
            .filter(|workspace| {
                workspace.status.is_in_flight()
                    || (matches!(
                        workspace.status,
                        WorkspaceStatus::Active | WorkspaceStatus::Adopted
                    ) && plakous_hosted::workspace_maintenance::dirty(&workspace.path)
                        .unwrap_or(true))
            })
            .map(|workspace| PendingWorkEntry {
                operation: workspace.workspace_id.clone(),
                kind: "source_workspace".to_owned(),
                state: workspace_status_name(workspace.status).to_ascii_lowercase(),
                detail: Some(format!(
                    "owner={}; mode={}; generation={}; path={}",
                    workspace.owner_agent_id,
                    workspace_mode_name(workspace.mode),
                    workspace.generation,
                    workspace.path.display()
                )),
            }),
    );
    let resolved = child_count + tree.candidates().len() + tree.workspaces().len() - facts.len();
    Ok((child_count, resolved, facts))
}

fn source_node_unresolved(status: NodeStatus) -> bool {
    !status.is_finished()
        || matches!(
            status,
            NodeStatus::Interrupted | NodeStatus::RecoveryRequired
        )
}

const fn node_status_name(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "Pending",
        NodeStatus::Running => "Running",
        NodeStatus::Paused => "Paused",
        NodeStatus::Sealing => "Sealing",
        NodeStatus::Completed => "Completed",
        NodeStatus::Interrupted => "Interrupted",
        NodeStatus::Failed => "Failed",
        NodeStatus::Closed => "Closed",
        NodeStatus::RecoveryRequired => "RecoveryRequired",
    }
}

const fn workspace_status_name(status: WorkspaceStatus) -> &'static str {
    match status {
        WorkspaceStatus::Active => "Active",
        WorkspaceStatus::Adopting => "Adopting",
        WorkspaceStatus::Discarding => "Discarding",
        WorkspaceStatus::CleanRemoved => "CleanRemoved",
        WorkspaceStatus::Adopted => "Adopted",
        WorkspaceStatus::Discarded => "Discarded",
    }
}

const fn workspace_mode_name(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Inherit => "Inherit",
        WorkspaceMode::Snapshot => "Snapshot",
        WorkspaceMode::Fork => "Fork",
    }
}

fn count_archivable_children<'a>(
    children: impl Iterator<Item = (&'a str, NodeStatus)>,
) -> Result<usize, ExportRefusal> {
    let mut count = 0;
    for (agent_id, status) in children {
        match status.disposition() {
            NodeDisposition::Idle | NodeDisposition::Dead | NodeDisposition::Interrupted => {
                count += 1;
            }
            NodeDisposition::Live | NodeDisposition::Sealing => {
                return Err(ExportRefusal::SubagentRunning {
                    agent_id: agent_id.to_owned(),
                    status,
                });
            }
        }
    }
    Ok(count)
}

/// The private directory carry files live in, created on first use. It sits
/// beside `state/`, so no build mistakes it for another build's state.
fn carry_dir(paths: &PlakPaths) -> Result<PathBuf, String> {
    super::document::private_dir(&paths.home().join("carry"))
}

/// A fresh carry file path for `session_id`; the file does not exist yet.
pub(crate) fn new_carry_file(paths: &PlakPaths, session_id: &str) -> Result<PathBuf, String> {
    Ok(carry_dir(paths)?.join(format!("{session_id}.{}.json", uuid::Uuid::new_v4())))
}

/// Exports `record` into a fresh private carry file and returns its path.
#[cfg(any(test, feature = "tui"))]
pub(crate) fn carry_out(
    paths: &PlakPaths,
    record: &SessionRecord,
) -> Result<(PathBuf, String), String> {
    let document = export_session(paths, record)
        .map_err(|refusal| format!("session {} cannot be exported: {refusal}", record.id))?;
    let path = new_carry_file(paths, &record.id)?;
    write_document(&path, &document)?;
    let digest = plakous::model::sha256_hex(&[&fs::read(&path).map_err(|e| e.to_string())?]);
    if let Err(error) =
        super::document::write_private_new(&path.with_extension("handoff"), digest.as_bytes())
    {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok((path, document.carry_summary()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
