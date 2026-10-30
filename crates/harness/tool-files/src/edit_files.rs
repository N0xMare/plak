use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_model::{ToolDefinition, ToolEffect};
use plakous_tools::{
    ExecutionBackendErrorKind, MAX_STANDARD_PATCH_CHANGES, MAX_STANDARD_TEXT_BYTES, ToolContext,
    ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind,
    WorkspaceContentIdentity, WorkspaceExpectedFileState, WorkspaceMutation,
    WorkspaceMutationTransaction, WorkspaceReadSliceRequest, WorkspaceReplacement,
    backend_runtime_error, decode_arguments, object_schema, operation_control, standard_definition,
    workspace_concurrency,
};
use serde::Deserialize;
use serde_json::json;

use crate::codex_patch::{CodexPatchOp, apply_codex_update_chunks, parse_codex_patch};
use crate::relative_workspace_path;

pub(super) struct EditFilesTool;

impl EditFilesTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                "edit_files",
                crate::text::PLAKOUS_TOOL_EDIT_FILES_DESCRIPTION,
                object_schema(
                    json!({"ops": {
                            "type":"array", "minItems":1, "maxItems":MAX_STANDARD_PATCH_CHANGES,
                    "items":{"type":"object", "properties":{
                                "op":{"type":"string","enum":["patch","replace","write","delete","move"]},
                                "path":{"type":"string"}, "to":{"type":"string"},
                                "patch":{"type":"string"}, "content":{"type":"string"},
                                "old":{"type":"string"}, "new":{"type":"string"},
                                "all":{"type":"boolean"}, "expect":{"type":"string"}
                    }, "required":["op"], "additionalProperties":false}
                        }}),
                    &["ops"],
                ),
                workspace_concurrency(&["workspace.filesystem"])?,
                ToolEffect::Mutating,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    ops: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum EditOp {
    Patch {
        patch: String,
        expect: Option<String>,
    },
    Replace {
        path: PathBuf,
        old: String,
        new: String,
        #[serde(default)]
        all: bool,
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
        path: PathBuf,
        to: PathBuf,
        expect: Option<String>,
    },
}

struct FileState {
    original: WorkspaceExpectedFileState,
    original_is_binary: bool,
    bytes: Option<Vec<u8>>,
}

type Overlay = BTreeMap<PathBuf, FileState>;

fn invalid(message: impl Into<String>) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message.into())
}

enum EditOpFailure {
    Guard(ToolRuntimeError),
    Runtime(ToolRuntimeError),
}

impl From<ToolRuntimeError> for EditOpFailure {
    fn from(error: ToolRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl EditOpFailure {
    fn outcome(self, index: usize) -> Result<ToolOutcome, ToolRuntimeError> {
        match self {
            Self::Guard(error) => ToolOutcome::failure_text(error.message().as_str()),
            Self::Runtime(error) if error.kind() == ToolRuntimeErrorKind::Cancelled => Err(error),
            Self::Runtime(error) => {
                ToolOutcome::failure_text(format!("op {index}: {}", error.message().as_str()))
            }
        }
    }
}

async fn load(
    context: &ToolContext,
    files: &mut Overlay,
    path: &Path,
) -> Result<PathBuf, ToolRuntimeError> {
    let path = relative_workspace_path(context, path)?;
    if path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".jj")
        })
    }) {
        return Err(invalid("edit_files refuses version-control metadata paths"));
    }
    if files.contains_key(&path) {
        return Ok(path);
    }
    if files.len() == MAX_STANDARD_PATCH_CHANGES {
        return Err(invalid("edit_files touches too many files"));
    }
    let read = context
        .environment()
        .read_workspace(
            WorkspaceReadSliceRequest::start(path.clone(), MAX_STANDARD_TEXT_BYTES)
                .map_err(backend_runtime_error)?,
            operation_control(context),
        )
        .await;
    let state = match read {
        Ok(read) if read.next().is_none() => FileState {
            original: WorkspaceExpectedFileState::Content(read.content()),
            original_is_binary: crate::mutation::is_binary(read.bytes()),
            bytes: Some(read.bytes().to_vec()),
        },
        Ok(_) => return Err(invalid("edit_files target exceeds the text bound")),
        Err(error) if error.kind() == ExecutionBackendErrorKind::NotFound => FileState {
            original: WorkspaceExpectedFileState::Missing,
            original_is_binary: false,
            bytes: None,
        },
        Err(error) => return Err(crate::mutation::write_read_error(&path, error)),
    };
    files.insert(path.clone(), state);
    Ok(path)
}

fn check_expect(state: &FileState, expect: Option<&str>) -> Result<(), ToolRuntimeError> {
    let Some(expect) = expect else {
        return Ok(());
    };
    let revision = match state.bytes.as_deref() {
        Some(bytes) => WorkspaceContentIdentity::from_bytes(bytes)
            .map_err(backend_runtime_error)?
            .digest()
            .to_string(),
        None => String::from("absent"),
    };
    if expect != revision {
        return Err(invalid("edit_files revision conflict"));
    }
    Ok(())
}

fn text(state: &FileState) -> Result<&str, ToolRuntimeError> {
    let bytes = state
        .bytes
        .as_deref()
        .ok_or_else(|| invalid("edit_files target is absent"))?;
    std::str::from_utf8(bytes).map_err(|_| invalid("edit_files target is not UTF-8"))
}

async fn move_file(
    context: &ToolContext,
    files: &mut Overlay,
    source: &Path,
    destination: &Path,
    expect: Option<&str>,
) -> Result<(), ToolRuntimeError> {
    let source = load(context, files, source).await?;
    let destination = load(context, files, destination).await?;
    let state = &files[&source];
    check_expect(state, expect)?;
    let bytes = state
        .bytes
        .clone()
        .ok_or_else(|| invalid("edit_files move source is absent"))?;
    if files[&destination].bytes.is_some() {
        return Err(invalid("edit_files move destination must be absent"));
    }
    files
        .get_mut(&source)
        .ok_or_else(|| invalid("missing source"))?
        .bytes = None;
    files
        .get_mut(&destination)
        .ok_or_else(|| invalid("missing destination"))?
        .bytes = Some(bytes);
    Ok(())
}

async fn apply_patch(
    context: &ToolContext,
    files: &mut Overlay,
    envelope: &str,
    expect: Option<&str>,
) -> Result<(), ToolRuntimeError> {
    for op in parse_codex_patch(envelope)? {
        match op {
            CodexPatchOp::Add { path, contents } => {
                let path = load(context, files, &path).await?;
                let state = files
                    .get_mut(&path)
                    .ok_or_else(|| invalid("missing patch target"))?;
                check_expect(state, expect)?;
                if state.bytes.is_some() {
                    return Err(invalid("edit_files patch add target exists"));
                }
                state.bytes = Some(contents.into_bytes());
            }
            CodexPatchOp::Delete { path } => {
                let path = load(context, files, &path).await?;
                let state = files
                    .get_mut(&path)
                    .ok_or_else(|| invalid("missing patch target"))?;
                check_expect(state, expect)?;
                if state.bytes.take().is_none() {
                    return Err(invalid("edit_files patch delete target is absent"));
                }
            }
            CodexPatchOp::Update {
                path,
                move_to,
                chunks,
            } => {
                let path = load(context, files, &path).await?;
                let state = files
                    .get_mut(&path)
                    .ok_or_else(|| invalid("missing patch target"))?;
                check_expect(state, expect)?;
                let next =
                    apply_codex_update_chunks(&path.to_string_lossy(), text(state)?, &chunks)?;
                state.bytes = Some(next.into_bytes());
                if let Some(destination) = move_to {
                    move_file(context, files, &path, &destination, None).await?;
                }
            }
        }
    }
    Ok(())
}

async fn apply_op(
    context: &ToolContext,
    files: &mut Overlay,
    op: EditOp,
    guards: &mut crate::mutation::WriteGuards,
) -> Result<(), EditOpFailure> {
    let (path, expect) = match &op {
        EditOp::Patch { patch, expect } => {
            return apply_patch(context, files, patch, expect.as_deref())
                .await
                .map_err(Into::into);
        }
        EditOp::Move { path, to, expect } => {
            return move_file(context, files, path, to, expect.as_deref())
                .await
                .map_err(Into::into);
        }
        EditOp::Write { path, expect, .. }
        | EditOp::Replace { path, expect, .. }
        | EditOp::Delete { path, expect } => (path, expect),
    };
    let path = load(context, files, path).await?;
    let state = files
        .get_mut(&path)
        .ok_or_else(|| invalid("missing edit target"))?;
    check_expect(state, expect.as_deref())?;
    match op {
        EditOp::Write { content, .. } => {
            if let WorkspaceExpectedFileState::Content(identity) = state.original {
                crate::mutation::require_read(context, &path, identity, state.original_is_binary)
                    .map_err(EditOpFailure::Guard)?;
            }
            guards.insert(path.clone(), state.original);
            state.bytes = Some(content.into_bytes());
        }
        EditOp::Delete { .. } => {
            if state.bytes.take().is_none() {
                return Err(invalid("edit_files delete target is absent").into());
            }
        }
        EditOp::Replace { old, new, all, .. } => {
            let original = text(state)?;
            let count = original.matches(&old).count();
            if old.is_empty() || count == 0 || (!all && count != 1) {
                return Err(invalid(
                    "edit_files replacement must match uniquely unless all is true",
                )
                .into());
            }
            let next_len = original
                .len()
                .checked_sub(old.len().saturating_mul(count))
                .and_then(|size| {
                    new.len()
                        .checked_mul(count)
                        .and_then(|extra| size.checked_add(extra))
                });
            if next_len.is_none_or(|size| {
                u64::try_from(size).unwrap_or(u64::MAX) > MAX_STANDARD_TEXT_BYTES
            }) {
                return Err(invalid("edit_files replacement exceeds the text bound").into());
            }
            state.bytes = Some(original.replace(&old, &new).into_bytes());
        }
        EditOp::Patch { .. } | EditOp::Move { .. } => {
            return Err(invalid("edit_files operation was not preflighted").into());
        }
    }
    Ok(())
}

impl ToolExecutor for EditFilesTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: EditArgs = decode_arguments(&invocation)?;
            if args.ops.is_empty() || args.ops.len() > MAX_STANDARD_PATCH_CHANGES {
                return ToolOutcome::failure_text(
                    "edit_files requires a bounded nonempty ops list",
                );
            }
            let mut files = Overlay::new();
            let mut guards = crate::mutation::WriteGuards::new();
            for (index, op) in args.ops.into_iter().enumerate() {
                let parsed = serde_json::from_value(op)
                    .map_err(|error| invalid(format!("invalid operation: {error}")));
                let result = match parsed {
                    Ok(op) => apply_op(&context, &mut files, op, &mut guards).await,
                    Err(error) => Err(error.into()),
                };
                if let Err(error) = result {
                    return error.outcome(index + 1);
                }
                if files.values().any(|state| {
                    state.bytes.as_ref().is_some_and(|bytes| {
                        u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STANDARD_TEXT_BYTES
                    })
                }) {
                    return ToolOutcome::failure_text(format!(
                        "op {}: edit_files content exceeds the text bound",
                        index + 1
                    ));
                }
            }
            let transaction = build_transaction(files, &mut guards)?;
            if let Err(error) = crate::mutation::preflight_result(&transaction) {
                return ToolOutcome::failure_text(error.message().as_str());
            }
            let value_inputs = invocation
                .want_value()
                .then(|| crate::patch::file_change_inputs(&transaction))
                .transpose()?;
            let result = context
                .environment()
                .mutate_workspace(transaction, operation_control(&context))
                .await;
            match result {
                Ok(result) => edit_outcome(&result, value_inputs),
                Err(error) => mutation_failure(error, &guards),
            }
        })
    }
}

fn build_transaction(
    files: Overlay,
    guards: &mut crate::mutation::WriteGuards,
) -> Result<WorkspaceMutationTransaction, ToolRuntimeError> {
    let mut normalized_guards = crate::mutation::WriteGuards::new();
    let changes = files
        .into_iter()
        .map(|(path, state)| {
            let guarded = guards.contains_key(&path);
            let mutation = WorkspaceMutation::new(
                path,
                state.original,
                state
                    .bytes
                    .map_or(WorkspaceReplacement::Missing, WorkspaceReplacement::content),
            )?;
            if guarded {
                normalized_guards.insert(mutation.path().to_path_buf(), mutation.expected());
            }
            Ok(mutation)
        })
        .collect::<Result<Vec<_>, plakous_tools::ExecutionBackendError>>()
        .map_err(backend_runtime_error)?;
    *guards = normalized_guards;
    WorkspaceMutationTransaction::new(changes).map_err(backend_runtime_error)
}

fn mutation_failure(
    error: plakous_tools::ExecutionBackendError,
    guards: &crate::mutation::WriteGuards,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if let Some(message) = crate::mutation::guarded_conflict(&error, guards) {
        return ToolOutcome::failure_text(message);
    }
    match error {
        error
            if matches!(
                error.kind(),
                ExecutionBackendErrorKind::RecoveryRequired
                    | ExecutionBackendErrorKind::Cancelled
                    | ExecutionBackendErrorKind::CleanupUnresolved
                    | ExecutionBackendErrorKind::TimedOut
            ) =>
        {
            Err(backend_runtime_error(error))
        }
        error => ToolOutcome::failure_text(error.message().as_str()),
    }
}

#[cfg(test)]
mod tests;

fn edit_outcome(
    result: &plakous_tools::WorkspaceMutationTransactionResult,
    inputs: Option<Vec<(String, Option<WorkspaceContentIdentity>)>>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    use plakous_tools::WorkspaceMutationDisposition as D;
    let outcome = ToolOutcome::success_text(crate::mutation::result_text(result))?;
    let Some(inputs) = inputs else {
        return Ok(outcome);
    };
    let changes = result
        .items()
        .iter()
        .zip(inputs)
        .filter_map(|(item, (path, revision))| {
            let action = match item.disposition() {
                D::Created => plakous_tools::FileChangeAction::Add,
                D::Updated => plakous_tools::FileChangeAction::Update,
                D::Deleted => plakous_tools::FileChangeAction::Delete,
                _ => return None,
            };
            Some(plakous_tools::FileChange {
                path,
                revision,
                action,
            })
        })
        .collect();
    outcome.with_value(plakous_tools::ExecutionValue::FileChanges(
        plakous_tools::FileChangesValue { changes },
    ))
}
