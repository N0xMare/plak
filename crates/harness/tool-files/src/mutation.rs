use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use plakous_tools::{
    ExecutionBackendError, ExecutionBackendErrorKind, ToolContext, ToolOutcome, ToolRuntimeError,
    ToolRuntimeErrorKind, WorkspaceExpectedFileState, WorkspaceMutationDisposition,
    WorkspaceMutationTransaction, WorkspaceMutationTransactionResult, WorkspaceReadSliceRequest,
    backend_runtime_error, operation_control,
};

use crate::text::{PLAKOUS_TOOL_WRITE_STALE, PLAKOUS_TOOL_WRITE_UNREAD};

pub(super) type WriteGuards = BTreeMap<PathBuf, WorkspaceExpectedFileState>;

pub(super) fn write_refusal(path: &Path, stale: bool) -> String {
    let template = if stale {
        PLAKOUS_TOOL_WRITE_STALE
    } else {
        PLAKOUS_TOOL_WRITE_UNREAD
    };
    template.replace("{path}", &quoted_path(path))
}

fn quoted_path(path: &Path) -> String {
    serde_json::Value::String(path.to_string_lossy().into_owned()).to_string()
}

pub(super) async fn blind_write_expected(
    context: &ToolContext,
    path: &Path,
) -> Result<WorkspaceExpectedFileState, ToolRuntimeError> {
    let read = context
        .environment()
        .read_workspace(
            WorkspaceReadSliceRequest::start(path, 1).map_err(backend_runtime_error)?,
            operation_control(context),
        )
        .await;
    let read = match read {
        Ok(read) => read,
        Err(error) if error.kind() == ExecutionBackendErrorKind::NotFound => {
            return Ok(WorkspaceExpectedFileState::Missing);
        }
        Err(error) => return Err(backend_runtime_error(error)),
    };
    require_read(context, path, read.content())
}

pub(super) fn require_read(
    context: &ToolContext,
    path: &Path,
    revision: plakous_tools::WorkspaceContentIdentity,
) -> Result<WorkspaceExpectedFileState, ToolRuntimeError> {
    let observed = context.environment().last_text_read(context.run_id(), path);
    match observed {
        Some(identity) if identity == revision => Ok(WorkspaceExpectedFileState::Content(identity)),
        identity => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::WorkspacePolicy,
            write_refusal(path, identity.is_some()),
        )),
    }
}

pub(super) fn guarded_conflict(
    error: &ExecutionBackendError,
    guards: &WriteGuards,
) -> Option<String> {
    if error.kind() != ExecutionBackendErrorKind::Conflict {
        return None;
    }
    let path = error.workspace_conflict_path()?;
    let expected = guards.get(path)?;
    Some(write_refusal(
        path,
        matches!(expected, WorkspaceExpectedFileState::Content(_)),
    ))
}

fn change_line(path: &Path, action: char) -> String {
    format!("\n{action} {}", quoted_path(path))
}

/// Budget every path as changed before any filesystem effect. Account for the
/// nested result wrapper too, so no presentation silently drops a path.
pub(super) fn preflight_result(
    transaction: &WorkspaceMutationTransaction,
) -> Result<(), ToolRuntimeError> {
    let mut worst = format!("changed_files={}", transaction.mutations().len());
    for mutation in transaction.mutations() {
        worst.push_str(&change_line(mutation.path(), 'M'));
    }
    let once = serde_json::Value::String(worst.clone()).to_string();
    let twice = serde_json::Value::String(once).to_string();
    if twice.len() > plakous_model::SemanticJsonV1::maximum_string_bytes() - 4_096 {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "file change result exceeds the text bound; split the transaction",
        ));
    }
    ToolOutcome::success_text(worst).map(|_| ())
}

pub(super) fn result_text(result: &WorkspaceMutationTransactionResult) -> String {
    let mut paths = result
        .items()
        .iter()
        .filter_map(|item| {
            let action = match item.disposition() {
                WorkspaceMutationDisposition::Created => 'A',
                WorkspaceMutationDisposition::Updated => 'M',
                WorkspaceMutationDisposition::Deleted => 'D',
                _ => return None,
            };
            Some((item.path(), action))
        })
        .collect::<Vec<_>>();
    paths.sort_unstable_by(|(left, _), (right, _)| left.as_os_str().cmp(right.as_os_str()));
    let mut text = format!("changed_files={}", paths.len());
    for (path, action) in paths {
        text.push_str(&change_line(path, action));
    }
    text
}

#[cfg(test)]
mod tests;
