//! The SDK-only `edit_files` capability: an ordered list of typed file ops,
//! every op and precondition validated before anything is written, then one
//! workspace mutation transaction.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_model::{ToolConcurrency, ToolEffect};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionValue, FileChange, FileChangeAction, FileChangesValue,
    MAX_STANDARD_PATCH_CHANGES, MAX_STANDARD_TEXT_BYTES, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind,
    WorkspaceContentIdentity, WorkspaceExpectedFileState, WorkspaceMutation,
    WorkspaceMutationTransaction, WorkspaceReadSliceRequest, WorkspaceReplacement,
    backend_runtime_error, decode_arguments, operation_control, standard_definition,
};
use serde::Deserialize;
use serde_json::json;

use crate::codex_patch::{CodexPatchOp, apply_codex_update_chunks, parse_codex_patch_with_moves};
use crate::relative_workspace_path;
use crate::text::PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT;

/// Canonical name of the SDK-only typed edit.
pub const EDIT_FILES_NAME: &str = "edit_files";
/// Registered tool and executor id of the SDK-only typed edit.
pub const EDIT_FILES_REGISTERED_TOOL_ID: &str = "plakous.edit-files.v1";
/// The `expect` value that requires a path to be absent.
const EXPECT_ABSENT: &str = "absent";

/// The SDK-only `edit_files` registration. It is nested-eligible and belongs
/// to no standard pack; the composition that presents it registers it.
///
/// # Errors
///
/// Returns an invalid-definition error if the registration cannot be constructed.
pub fn edit_files_registration() -> Result<ToolRegistration, ToolRuntimeError> {
    let definition = standard_definition(
        EDIT_FILES_NAME,
        "Apply ordered typed file ops atomically: every op and every expected revision is checked before anything is written.",
        plakous_tools::object_schema(
            json!({
                "ops": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_STANDARD_PATCH_CHANGES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["patch", "replace", "write", "delete", "move"]},
                            "text": {"type": "string"},
                            "path": {"type": "string"},
                            "old": {"type": "string"},
                            "new": {"type": "string"},
                            "count": {"type": "integer", "minimum": 1},
                            "content": {"type": "string"},
                            "src": {"type": "string"},
                            "dst": {"type": "string"},
                            "expect": {"type": "string"}
                        },
                        "required": ["op"],
                        "additionalProperties": false
                    }
                }
            }),
            &["ops"],
        ),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new(EDIT_FILES_REGISTERED_TOOL_ID)?,
        ToolExecutorId::new(EDIT_FILES_REGISTERED_TOOL_ID)?,
        ToolExecutorBindingIdentity::new(1, &[b"edit-files", b"one-transaction"])?,
        definition,
        Arc::new(EditFilesTool),
    )
    .nested_eligible())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFilesArgs {
    ops: Vec<EditOp>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum EditOp {
    Patch {
        text: String,
    },
    Replace {
        path: PathBuf,
        old: String,
        new: String,
        count: u64,
        expect: Option<String>,
    },
    Write {
        path: PathBuf,
        content: String,
        expect: Option<String>,
    },
    Delete {
        path: PathBuf,
        expect: Option<String>,
    },
    Move {
        src: PathBuf,
        dst: PathBuf,
        expect: Option<String>,
    },
}

struct EditFilesTool;

impl ToolExecutor for EditFilesTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: EditFilesArgs = decode_arguments(&invocation)?;
            let mut plan = EditPlan::default();
            for (index, op) in args.ops.into_iter().enumerate() {
                context.check_cancelled()?;
                if let Err(message) = plan.apply(&context, op).await? {
                    return ToolOutcome::failure_text(format!("op {}: {message}", index + 1));
                }
            }
            plan.commit(&context).await
        })
    }
}

/// One path's state before the call and after the ops applied so far.
struct PathState {
    original: Option<WorkspaceContentIdentity>,
    current: Option<Vec<u8>>,
    moved_from: Option<String>,
}

/// The in-memory result of the ops so far. Nothing touches the workspace
/// until [`EditPlan::commit`].
#[derive(Default)]
struct EditPlan {
    paths: BTreeMap<PathBuf, PathState>,
}

/// A definite, model-visible validation failure; nothing has been written.
type Refusal = String;

impl EditPlan {
    async fn apply(
        &mut self,
        context: &ToolContext,
        op: EditOp,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        match op {
            EditOp::Patch { text } => self.patch(context, &text).await,
            EditOp::Replace {
                path,
                old,
                new,
                count,
                expect,
            } => {
                let path = relative_workspace_path(context, path)?;
                if old.is_empty() {
                    return Ok(Err("replace needs a non-empty old text".to_owned()));
                }
                self.edit_text(context, &path, expect.as_deref(), |text| {
                    let found = text.matches(old.as_str()).count() as u64;
                    if found != count {
                        return Err(format!(
                            "{}: old text occurs {found} times, expected {count}",
                            path.display()
                        ));
                    }
                    Ok(text.replace(old.as_str(), &new))
                })
                .await
            }
            EditOp::Write {
                path,
                content,
                expect,
            } => {
                let path = relative_workspace_path(context, path)?;
                self.write(
                    context,
                    &path,
                    expect.as_deref(),
                    content.into_bytes(),
                    false,
                )
                .await
            }
            EditOp::Delete { path, expect } => {
                let path = relative_workspace_path(context, path)?;
                self.delete(context, &path, expect.as_deref()).await
            }
            EditOp::Move { src, dst, expect } => {
                let src = relative_workspace_path(context, src)?;
                let dst = relative_workspace_path(context, dst)?;
                self.relocate(context, &src, &dst, expect.as_deref()).await
            }
        }
    }

    /// Applies one patch envelope's ops, moves included, to the plan.
    async fn patch(
        &mut self,
        context: &ToolContext,
        text: &str,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        let ops = match parse_codex_patch_with_moves(text) {
            Ok(ops) => ops,
            Err(error) => return Ok(Err(error.message().to_string())),
        };
        for op in ops {
            let step = match op {
                CodexPatchOp::Add { path, contents } => {
                    let path = relative_workspace_path(context, path)?;
                    self.write(context, &path, None, contents.into_bytes(), true)
                        .await?
                }
                CodexPatchOp::Delete { path } => {
                    let path = relative_workspace_path(context, path)?;
                    self.delete(context, &path, None).await?
                }
                CodexPatchOp::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let path = relative_workspace_path(context, path)?;
                    let step = self
                        .edit_text(context, &path, None, |text| {
                            if chunks.is_empty() {
                                return Ok(text.to_owned());
                            }
                            apply_codex_update_chunks(&path.to_string_lossy(), text, &chunks)
                                .map_err(|error| error.message().to_string())
                        })
                        .await?;
                    match (step, move_to) {
                        (Ok(()), Some(destination)) => {
                            let destination = relative_workspace_path(context, destination)?;
                            self.relocate(context, &path, &destination, None).await?
                        }
                        (step, _) => step,
                    }
                }
            };
            if step.is_err() {
                return Ok(step);
            }
        }
        Ok(Ok(()))
    }

    /// Loads a path's state once and checks `expect` against the file as it
    /// was before the call.
    async fn state(
        &mut self,
        context: &ToolContext,
        path: &Path,
        expect: Option<&str>,
    ) -> Result<Result<&mut PathState, Refusal>, ToolRuntimeError> {
        if !self.paths.contains_key(path) {
            let (original, current) = read_original(context, path)
                .await?
                .map_or((None, None), |(identity, bytes)| {
                    (Some(identity), Some(bytes))
                });
            self.paths.insert(
                path.to_path_buf(),
                PathState {
                    original,
                    current,
                    moved_from: None,
                },
            );
        }
        let Some(state) = self.paths.get_mut(path) else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "edit plan lost a path it loaded",
            ));
        };
        let display = path.display();
        match expect {
            None => {}
            Some(EXPECT_ABSENT) => {
                if state.original.is_some() {
                    return Ok(Err(format!("{display} exists, expected absent")));
                }
            }
            Some(token) => {
                let Ok(expected) = WorkspaceContentIdentity::from_revision_token(token) else {
                    return Ok(Err(format!(
                        "{display}: expect is not a revision or \"absent\""
                    )));
                };
                if state.original != Some(expected) {
                    return Ok(Err(format!(
                        "{display} changed since that revision; read it again"
                    )));
                }
            }
        }
        Ok(Ok(state))
    }

    async fn write(
        &mut self,
        context: &ToolContext,
        path: &Path,
        expect: Option<&str>,
        bytes: Vec<u8>,
        must_be_absent: bool,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        let state = match self.state(context, path, expect).await? {
            Ok(state) => state,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if must_be_absent && state.current.is_some() {
            return Ok(Err(format!("{} already exists", path.display())));
        }
        state.current = Some(bytes);
        Ok(Ok(()))
    }

    async fn delete(
        &mut self,
        context: &ToolContext,
        path: &Path,
        expect: Option<&str>,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        let state = match self.state(context, path, expect).await? {
            Ok(state) => state,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if state.current.take().is_none() {
            return Ok(Err(format!("{} does not exist", path.display())));
        }
        Ok(Ok(()))
    }

    async fn edit_text(
        &mut self,
        context: &ToolContext,
        path: &Path,
        expect: Option<&str>,
        edit: impl FnOnce(&str) -> Result<String, Refusal>,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        let state = match self.state(context, path, expect).await? {
            Ok(state) => state,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let Some(current) = state.current.as_deref() else {
            return Ok(Err(format!("{} does not exist", path.display())));
        };
        let Ok(text) = std::str::from_utf8(current) else {
            return Ok(Err(format!("{} is not valid UTF-8", path.display())));
        };
        match edit(text) {
            Ok(next) => {
                state.current = Some(next.into_bytes());
                Ok(Ok(()))
            }
            Err(refusal) => Ok(Err(refusal)),
        }
    }

    async fn relocate(
        &mut self,
        context: &ToolContext,
        src: &Path,
        dst: &Path,
        expect: Option<&str>,
    ) -> Result<Result<(), Refusal>, ToolRuntimeError> {
        if src == dst {
            return Ok(Err(format!("{} cannot move onto itself", src.display())));
        }
        let bytes = match self.state(context, src, expect).await? {
            Ok(state) => match state.current.take() {
                Some(bytes) => bytes,
                None => return Ok(Err(format!("{} does not exist", src.display()))),
            },
            Err(refusal) => return Ok(Err(refusal)),
        };
        let destination = match self.state(context, dst, None).await? {
            Ok(state) => state,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if destination.current.is_some() {
            return Ok(Err(format!("{} already exists", dst.display())));
        }
        destination.current = Some(bytes);
        destination.moved_from = Some(src.to_string_lossy().into_owned());
        Ok(Ok(()))
    }

    async fn commit(self, context: &ToolContext) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut mutations = Vec::new();
        let mut changes = Vec::new();
        for (path, state) in self.paths {
            let unchanged = match (&state.original, &state.current) {
                (None, None) => true,
                (Some(original), Some(bytes)) => {
                    WorkspaceContentIdentity::from_bytes(bytes).ok().as_ref() == Some(original)
                }
                _ => false,
            };
            if unchanged {
                continue;
            }
            let expected = state.original.map_or(
                WorkspaceExpectedFileState::Missing,
                WorkspaceExpectedFileState::Content,
            );
            let (replacement, revision) = match &state.current {
                Some(bytes) => (
                    WorkspaceReplacement::content(bytes.clone()),
                    Some(
                        WorkspaceContentIdentity::from_bytes(bytes)
                            .map_err(backend_runtime_error)?,
                    ),
                ),
                None => (WorkspaceReplacement::Missing, None),
            };
            let action = match (state.original.is_some(), state.current.is_some()) {
                (_, true) if state.moved_from.is_some() => FileChangeAction::Move,
                (false, true) => FileChangeAction::Add,
                (true, false) => FileChangeAction::Delete,
                _ => FileChangeAction::Update,
            };
            mutations.push(
                WorkspaceMutation::new(&path, expected, replacement)
                    .map_err(backend_runtime_error)?,
            );
            changes.push(FileChange {
                path: path.to_string_lossy().into_owned(),
                action,
                revision,
            });
        }
        if mutations.is_empty() {
            return Ok(ToolOutcome::success_text("changed_files=0")?
                .with_execution_value(ExecutionValue::FileChanges(FileChangesValue { changes })));
        }
        let transaction = match WorkspaceMutationTransaction::new(mutations) {
            Ok(transaction) => transaction,
            Err(error) => return ToolOutcome::failure_text(error.message().as_str()),
        };
        match context
            .environment()
            .mutate_workspace(transaction, operation_control(context))
            .await
        {
            Ok(_) => {}
            Err(error) if error.kind() == ExecutionBackendErrorKind::Conflict => {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ExecutionBackendErrorKind::RecoveryRequired
                        | ExecutionBackendErrorKind::Cancelled
                        | ExecutionBackendErrorKind::CleanupUnresolved
                        | ExecutionBackendErrorKind::TimedOut
                ) =>
            {
                return Err(backend_runtime_error(error));
            }
            Err(error) => return ToolOutcome::failure_text(error.message().as_str()),
        }
        let mut text = format!("changed_files={}", changes.len());
        for change in &changes {
            let action = match change.action {
                FileChangeAction::Add => "add",
                FileChangeAction::Update => "update",
                FileChangeAction::Delete => "delete",
                FileChangeAction::Move => "move",
            };
            let _ = write!(text, "\n{action} {}", change.path);
        }
        Ok(ToolOutcome::success_text(text)?
            .with_execution_value(ExecutionValue::FileChanges(FileChangesValue { changes })))
    }
}

/// The file's identity and bytes before the call, or `None` when absent.
async fn read_original(
    context: &ToolContext,
    path: &Path,
) -> Result<Option<(WorkspaceContentIdentity, Vec<u8>)>, ToolRuntimeError> {
    let read = context
        .environment()
        .read_workspace(
            WorkspaceReadSliceRequest::start(path, MAX_STANDARD_TEXT_BYTES)
                .map_err(backend_runtime_error)?,
            operation_control(context),
        )
        .await;
    match read {
        Ok(read) if read.next().is_none() => Ok(Some((read.content(), read.bytes().to_vec()))),
        Ok(_) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            crate::text::PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
        )
        .with_proven_no_effect(true)),
        Err(error) if error.kind() == ExecutionBackendErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == ExecutionBackendErrorKind::Cancelled => {
            Err(backend_runtime_error(error))
        }
        Err(error) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            error.message().as_str(),
        )
        .with_proven_no_effect(true)),
    }
}
