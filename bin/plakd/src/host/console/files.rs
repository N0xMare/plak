//! Saved-workspace inspection, separate from session admission and tool effects.

mod changes;
mod cursors;
mod quota;
mod resolve;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated workspace inspection fixtures")]
mod tests;

use super::{Console, HostPrincipal, HostRequestContext, refused};
use base64::Engine as _;
use plakd_wire_types::console::{
    FileContent, FileEntry, FileKind, FileMatch, FileQuerySummary, FileStopReason, HostRequest,
    HostResponse, ReadRefusal, ValidationCode, WorkspaceReadFailure, WorkspaceReadLimits,
    WorkspaceTarget,
};
use plakous::tools::{
    ExecutionBackendError, ExecutionBackendErrorKind, OperationContext, RepositoryMatchCase,
    RepositoryQuery, RepositoryQueryLimits, RepositoryQueryResult, RepositoryQueryStopReason,
    WorkspaceReadSliceRequest,
};
use plakous_host_protocol::wire::{ConsoleCapability, RequestRefusal};
use plakous_hosted::host_service::wire::HostReadTask;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

pub(super) const PAGE: u64 = LIMITS.page_bytes;
const INPUT: u64 = LIMITS.file_bytes;

pub(super) fn welcome_limits() -> WorkspaceReadLimits {
    LIMITS
}

pub(super) const LIMITS: WorkspaceReadLimits = WorkspaceReadLimits {
    page_bytes: 256 * 1024,
    file_bytes: 16 * 1024 * 1024,
    reads_inflight_per_principal: 2,
    requests_per_second: 10,
    request_burst: 20,
    output_bytes_per_second: 4 * 1024 * 1024,
    output_burst_bytes: 8 * 1024 * 1024,
    hash_bytes_per_second: 32 * 1024 * 1024,
    hash_burst_bytes: 64 * 1024 * 1024,
    inflight_host: 16,
    search_starts_per_minute: 6,
    search_inflight_per_principal: 1,
    file_deadline_ms: 2_000,
    changes_deadline_ms: 5_000,
    changes_entries: 2_000,
    changes_targets_per_principal: 4,
    changes_token_ttl_ms: 60_000,
};

fn request_budget(request: &HostRequest) -> Duration {
    Duration::from_millis(
        if matches!(
            request,
            HostRequest::ChangesStatus { .. } | HostRequest::ChangesDiff { .. }
        ) {
            LIMITS.changes_deadline_ms
        } else {
            LIMITS.file_deadline_ms
        },
    )
}

#[derive(Default)]
pub(super) struct State {
    quota: quota::State,
    cursors: cursors::State,
    changes: changes::State,
}

pub(in super::super) fn target(request: &HostRequest) -> Option<&WorkspaceTarget> {
    match request {
        HostRequest::FilesList { target, .. }
        | HostRequest::FilesRead { target, .. }
        | HostRequest::FilesSearch { target, .. }
        | HostRequest::ChangesStatus { target }
        | HostRequest::ChangesDiff { target, .. } => Some(target),
        _ => None,
    }
}

pub(super) fn failure(cause: WorkspaceReadFailure) -> ReadRefusal {
    ReadRefusal::WorkspaceReadFailed { cause }
}

impl Console {
    pub(in super::super) fn workspace_request(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        services: crate::shell::services::Services,
        tree: Arc<plakous_recursive_subagents_store::SqliteTreeStore>,
    ) -> HostReadTask<plakd_wire_types::PlakExtension> {
        let mut audit = super::audit::Guard::new(Arc::clone(self), context.clone(), &request);
        let task = self.workspace_request_inner(context, request, services, tree);
        Box::pin(async move {
            let result = task.await;
            audit.finish(&result);
            result
        })
    }

    fn workspace_request_inner(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        services: crate::shell::services::Services,
        tree: Arc<plakous_recursive_subagents_store::SqliteTreeStore>,
    ) -> HostReadTask<plakd_wire_types::PlakExtension> {
        let this = Arc::clone(self);
        Box::pin(async move {
            if !matches!(context.principal, HostPrincipal::Socket(_))
                && !this.switches.web_files_read
            {
                return Err(RequestRefusal::CapabilityDisabled {
                    capability: ConsoleCapability::FilesRead,
                });
            }
            if let Err(reason) = validate(&request) {
                return Ok(refused(reason));
            }
            let guard = quota::Guard::admit(Arc::clone(&this), &context.principal, &request)?;
            let Some(runtime) = &this.runtime else {
                return Ok(refused(ReadRefusal::WorkspaceUnavailable {}));
            };
            let this = Arc::clone(&this);
            let task = runtime.spawn(async move {
                let budget = request_budget(&request);
                let expires = std::time::Instant::now() + budget;
                let cancellation = plakous::model::CancellationToken::new();
                let _cancel = Cancel(cancellation.clone());
                let Some(target) = target(&request).cloned() else {
                    unreachable!()
                };
                let resolved = tokio::task::spawn_blocking(move || {
                    (
                        resolve::saved(&services, &tree, &target, expires, cancellation),
                        guard,
                    )
                })
                .await;
                let Ok((workspace, _guard)) = resolved else {
                    return Ok(refused(ReadRefusal::WorkspaceUnavailable {}));
                };
                if std::time::Instant::now() >= expires {
                    return Ok(refused(failure(WorkspaceReadFailure::Deadline)));
                }
                let workspace = match workspace {
                    Ok(workspace) => workspace,
                    Err(resolve::Failure::Product(reason)) => return Ok(refused(reason)),
                    Err(resolve::Failure::Placement(reason)) => return Err(reason),
                };
                let _result = if matches!(request, HostRequest::ChangesStatus { .. }) {
                    Some(changes::Reservation::admit(
                        Arc::clone(&this),
                        &context,
                        &workspace.target,
                    )?)
                } else {
                    None
                };
                this.inspect_until(&context, request, workspace, expires)
                    .await
            });
            let _cancel = super::Cancel {
                abort: task.abort_handle(),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            task.await
                .unwrap_or_else(|_| Ok(refused(failure(WorkspaceReadFailure::Io))))
        })
    }

    #[cfg(test)]
    async fn inspect(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
        workspace: resolve::Workspace,
    ) -> Result<HostResponse, RequestRefusal> {
        let expires = std::time::Instant::now() + request_budget(&request);
        self.inspect_until(context, request, workspace, expires)
            .await
    }

    async fn inspect_until(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
        workspace: resolve::Workspace,
        expires: std::time::Instant,
    ) -> Result<HostResponse, RequestRefusal> {
        let placement = workspace.placement.clone();
        let cancellation = plakous::model::CancellationToken::new();
        let _cancel = Cancel(cancellation.clone());
        let budget_ms = u64::try_from(
            expires
                .saturating_duration_since(std::time::Instant::now())
                .as_millis(),
        )
        .unwrap_or(0);
        let deadline = u64::try_from(super::clock::now_ms())
            .unwrap_or(0)
            .saturating_add(budget_ms);
        let control = OperationContext::new(cancellation);
        let control = if let Ok(deadline) = plakous::model::Timestamp::new(
            deadline / 1_000,
            u32::try_from(deadline % 1_000 * 1_000_000).unwrap_or(0),
        ) {
            control.with_budget(deadline)
        } else {
            control
        };
        let result = tokio::time::timeout(
            Duration::from_millis(budget_ms),
            self.execute(context, request, workspace, control),
        )
        .await;
        if !matches!(result, Ok(Ok(_)))
            && let Some(refusal) = placement.and_then(|placement| placement.refusal())
        {
            return Err(refusal);
        }
        Ok(match result {
            Ok(Ok(response)) => response,
            Ok(Err(reason)) => refused(reason),
            Err(_) => refused(failure(WorkspaceReadFailure::Deadline)),
        })
    }

    async fn execute(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
        workspace: resolve::Workspace,
        control: OperationContext,
    ) -> Result<HostResponse, ReadRefusal> {
        match request {
            request @ HostRequest::FilesRead { .. } => {
                self.read_file(context, request, workspace, control).await
            }
            HostRequest::FilesList { path, .. } => {
                let query =
                    RepositoryQuery::list(path, None, 1, query_limits(1_000)?).map_err(backend)?;
                let result = workspace
                    .environment
                    .query_repository(query, control)
                    .await
                    .map_err(backend)?;
                list_reply(&result)
            }
            HostRequest::FilesSearch {
                path,
                literal,
                match_case,
                ..
            } => {
                let case = if match_case.unwrap_or(true) {
                    RepositoryMatchCase::Sensitive
                } else {
                    RepositoryMatchCase::AsciiInsensitive
                };
                let query =
                    RepositoryQuery::new(path, literal.into_bytes(), case, query_limits(200)?)
                        .map_err(backend)?;
                let result = workspace
                    .environment
                    .query_repository(query, control)
                    .await
                    .map_err(backend)?;
                search_reply(&result)
            }
            request @ (HostRequest::ChangesStatus { .. } | HostRequest::ChangesDiff { .. }) => {
                self.changes(context, request, workspace, control).await
            }
            _ => unreachable!(),
        }
    }

    async fn read_file(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
        workspace: resolve::Workspace,
        control: OperationContext,
    ) -> Result<HostResponse, ReadRefusal> {
        let HostRequest::FilesRead {
            path,
            cursor,
            max_bytes,
            ..
        } = request
        else {
            unreachable!()
        };
        let target = workspace.target.clone();
        let bytes = max_bytes.unwrap_or(PAGE);
        let start = WorkspaceReadSliceRequest::start(&path, bytes).map_err(backend)?;
        let normalized = utf8(start.path())?;
        let read = if let Some(ref cursor) = cursor {
            let cursor = self
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cursors
                .get(cursor, context, &target, &workspace.identity, &normalized)?;
            WorkspaceReadSliceRequest::resume(cursor, bytes)
        } else {
            Ok(start)
        }
        .map_err(backend)?;
        let reservation = cursors::Reservation::admit(self, context, cursor.as_deref())?;
        let slice = workspace
            .environment
            .read_workspace(read, control)
            .await
            .map_err(backend)?;
        reservation.consume();
        let next = slice
            .next()
            .map(|cursor| {
                self.files
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .cursors
                    .insert(context, target, workspace.identity, cursor.clone())
            })
            .transpose()?;
        Ok(HostResponse::FilesRead {
            content: FileContent {
                revision: slice.content().digest().to_string(),
                size: slice.content().byte_len(),
                offset: slice.offset(),
                bytes_b64: b64(slice.bytes()),
                next,
            },
        })
    }
}

fn list_reply(result: &RepositoryQueryResult) -> Result<HostResponse, ReadRefusal> {
    let mut summary = summary(result);
    let entries = result
        .matches()
        .iter()
        .filter_map(|row| {
            let Some(path) = row.path().to_str() else {
                summary.skipped_entries += 1;
                return None;
            };
            Some(FileEntry {
                path: path.into(),
                kind: if row.is_directory() {
                    FileKind::Directory
                } else {
                    FileKind::File
                },
                size: row.entry_size(),
            })
        })
        .collect::<Vec<_>>();
    let entries = bounded_rows(entries, &mut summary)?;
    Ok(HostResponse::FilesList { entries, summary })
}

fn search_reply(result: &RepositoryQueryResult) -> Result<HostResponse, ReadRefusal> {
    let mut summary = summary(result);
    let mut skipped = std::collections::BTreeSet::new();
    let matches = result
        .matches()
        .iter()
        .filter_map(|row| {
            let Some(path) = row.path().to_str() else {
                skipped.insert(row.path());
                return None;
            };
            Some(FileMatch {
                path: path.into(),
                line: row.line(),
                column: row.column(),
                bytes_b64: b64(row.bytes()),
                truncated: row.truncated(),
            })
        })
        .collect::<Vec<_>>();
    summary.skipped_entries += skipped.len() as u64;
    let matches = bounded_rows(matches, &mut summary)?;
    Ok(HostResponse::FilesSearch { matches, summary })
}

struct Cancel(plakous::model::CancellationToken);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn query_limits(matches: usize) -> Result<RepositoryQueryLimits, ReadRefusal> {
    RepositoryQueryLimits::new(
        1_000,
        10_000,
        1024 * 1024,
        INPUT,
        matches,
        2048,
        PAGE as usize,
    )
    .map_err(backend)
}

pub(super) fn validate(request: &HostRequest) -> Result<(), ReadRefusal> {
    let selected = target(request).ok_or(ReadRefusal::WorkspaceUnavailable {})?;
    if selected.session.is_empty()
        || selected.session.len() > 256
        || selected
            .node
            .as_ref()
            .is_some_and(|node| node.is_empty() || node.len() > 256)
    {
        return Err(ReadRefusal::WorkspaceUnavailable {});
    }
    match request {
        HostRequest::FilesRead {
            path,
            max_bytes,
            cursor,
            ..
        } => {
            valid_path(path, false)?;
            if !(1..=PAGE).contains(&max_bytes.unwrap_or(PAGE)) {
                return Err(super::validation("max_bytes", ValidationCode::InvalidLimit));
            }
            if cursor.as_ref().is_some_and(|c| c.len() > 128) {
                return Err(failure(WorkspaceReadFailure::InvalidCursor));
            }
        }
        HostRequest::FilesList { path, .. } => valid_path(path, true)?,
        HostRequest::FilesSearch {
            path,
            literal,
            match_case,
            ..
        } => {
            valid_path(path, true)?;
            if literal.is_empty()
                || literal.len() > 1024
                || literal.contains('\0')
                || (*match_case == Some(false) && !literal.is_ascii())
            {
                return Err(super::validation("literal", ValidationCode::InvalidValue));
            }
        }
        HostRequest::ChangesDiff { entry, .. } if entry.len() > 128 => {
            return Err(failure(WorkspaceReadFailure::ChangesConflict));
        }
        _ => {}
    }
    Ok(())
}

fn valid_path(value: &str, root: bool) -> Result<(), ReadRefusal> {
    use std::path::Component;
    let path = std::path::Path::new(value);
    if value.len() > 4096
        || value.contains('\0')
        || (!root && (value.is_empty() || value == "."))
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(failure(WorkspaceReadFailure::InvalidPath));
    }
    if path.components().any(|part| {
        part.as_os_str().to_str().is_some_and(|part| {
            part.eq_ignore_ascii_case(".git") || part.eq_ignore_ascii_case(".jj")
        })
    }) {
        return Err(failure(WorkspaceReadFailure::ProtectedPath));
    }
    Ok(())
}

fn utf8(path: &std::path::Path) -> Result<String, ReadRefusal> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(WorkspaceReadFailure::InvalidPath))
}
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn bounded_rows<T: serde::Serialize>(
    mut rows: Vec<T>,
    summary: &mut FileQuerySummary,
) -> Result<Vec<T>, ReadRefusal> {
    let mut bytes = 512_u64;
    for (index, row) in rows.iter().enumerate() {
        let size = serde_json::to_vec(row)
            .map_err(|_| failure(WorkspaceReadFailure::Io))?
            .len() as u64
            + 1;
        if bytes.saturating_add(size) > PAGE {
            rows.truncate(index);
            summary.truncated = true;
            summary.stop_reason = Some(FileStopReason::ResultBytes);
            break;
        }
        bytes += size;
    }
    Ok(rows)
}
fn summary(result: &RepositoryQueryResult) -> FileQuerySummary {
    FileQuerySummary {
        skipped_entries: result.entries_skipped() as u64,
        skipped_binary: result.skipped_binary() as u64,
        skipped_oversize: result.skipped_oversize() as u64,
        skipped_changed: result.skipped_changed() as u64,
        truncated: result.incomplete(),
        stop_reason: result.stop_reason().map(|reason| match reason {
            RepositoryQueryStopReason::TreeEntries => FileStopReason::TreeEntries,
            RepositoryQueryStopReason::Files => FileStopReason::Files,
            RepositoryQueryStopReason::Matches => FileStopReason::Matches,
            RepositoryQueryStopReason::ScannedBytes => FileStopReason::ScannedBytes,
            RepositoryQueryStopReason::ResultBytes => FileStopReason::ResultBytes,
        }),
    }
}
fn backend(error: ExecutionBackendError) -> ReadRefusal {
    failure(match error.kind() {
        ExecutionBackendErrorKind::Denied => WorkspaceReadFailure::PermissionDenied,
        ExecutionBackendErrorKind::ResourceLimit => WorkspaceReadFailure::FileTooLarge,
        ExecutionBackendErrorKind::Conflict => WorkspaceReadFailure::ContentChanged,
        ExecutionBackendErrorKind::WorkspacePolicy
        | ExecutionBackendErrorKind::SymlinkEscape
        | ExecutionBackendErrorKind::OutsideWorkspace => WorkspaceReadFailure::ProtectedPath,
        ExecutionBackendErrorKind::NotFound | ExecutionBackendErrorKind::NotADirectory => {
            WorkspaceReadFailure::NotFound
        }
        ExecutionBackendErrorKind::IsADirectory | ExecutionBackendErrorKind::NotARegularFile => {
            WorkspaceReadFailure::NotRegularFile
        }
        ExecutionBackendErrorKind::TimedOut | ExecutionBackendErrorKind::Cancelled => {
            WorkspaceReadFailure::Deadline
        }
        ExecutionBackendErrorKind::InvalidRequest => WorkspaceReadFailure::InvalidPath,
        _ => WorkspaceReadFailure::Io,
    })
}
