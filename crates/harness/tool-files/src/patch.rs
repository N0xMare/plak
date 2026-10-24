use crate::codex_patch::{
    CodexPatchOp, apply_codex_update_chunks, looks_like_codex_patch, parse_codex_patch,
};
use crate::text::{
    PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION, PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT,
    PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY, PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
    PLAKOUS_TOOL_EDIT_AMBIGUOUS, PLAKOUS_TOOL_EDIT_EMPTY, PLAKOUS_TOOL_EDIT_NOT_FOUND,
    PLAKOUS_TOOL_EDITS_FORM, PLAKOUS_TOOL_EDITS_OVERLAP, PLAKOUS_TOOL_PATCH_HUNK_CONFLICT,
    PLAKOUS_TOOL_PATCH_KIND_CHANGED, PLAKOUS_TOOL_PATCH_MALFORMED,
    PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8, PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
    PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT, PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS,
    PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND, PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
};
use diffy::patch_set::{FileOperation, ParseOptions, PatchKind, PatchSet};
use plakous_model::{ToolDefinition, ToolEffect};
use plakous_tools::object_schema;
use plakous_tools::{
    APPLY_PATCH_CAPABILITY_NAME, MAX_STANDARD_PATCH_CHANGES, MAX_STANDARD_TEXT_BYTES,
};
use plakous_tools::{
    ExecutionBackendErrorKind, ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome,
    ToolRuntimeError, ToolRuntimeErrorKind, WorkspaceExpectedFileState, WorkspaceMutation,
    WorkspaceMutationDisposition, WorkspaceMutationTransaction, WorkspaceReadSliceRequest,
    WorkspaceReplacement,
};
use plakous_tools::{backend_runtime_error, decode_arguments, operation_control};
use plakous_tools::{standard_definition, workspace_concurrency};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::relative_workspace_path;

#[derive(Clone)]
pub(super) struct ApplyPatchTool;

impl ApplyPatchTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                APPLY_PATCH_CAPABILITY_NAME,
                PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
                object_schema(
                    json!({
                        "patch": {"type": "string"},
                        "file_path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean"},
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_string": {"type": "string"},
                                    "new_string": {"type": "string"}
                                },
                                "required": ["old_string", "new_string"],
                                "additionalProperties": false
                            }
                        }
                    }),
                    &[],
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
struct PatchArgs {
    patch: Option<String>,
    file_path: Option<PathBuf>,
    old_string: Option<String>,
    new_string: Option<String>,
    replace_all: Option<bool>,
    edits: Option<Vec<ExactEdit>>,
}

/// One exact replacement of a multi-edit call, matched against the original
/// file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactEdit {
    old_string: String,
    new_string: String,
}

enum ParsedPatchKind {
    Create,
    Modify,
    Delete,
}

struct ParsedPatch {
    path: PathBuf,
    kind: ParsedPatchKind,
    patch: String,
}

impl ToolExecutor for ApplyPatchTool {
    #[allow(
        clippy::too_many_lines,
        reason = "the patch preflight keeps parsing, exact reads, transformation, and the sole mutation boundary visibly ordered"
    )]
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let patch_from_utf8 = match invocation.arguments().value().as_ref() {
                Value::String(text) => Some(text.as_str()),
                _ => None,
            };
            let args: PatchArgs = if patch_from_utf8.is_some() {
                PatchArgs {
                    patch: patch_from_utf8.map(str::to_owned),
                    file_path: None,
                    old_string: None,
                    new_string: None,
                    replace_all: None,
                    edits: None,
                }
            } else {
                decode_arguments(&invocation)?
            };
            if args.edits.is_some()
                && (args.file_path.is_none()
                    || args.patch.is_some()
                    || args.old_string.is_some()
                    || args.new_string.is_some()
                    || args.replace_all.is_some())
            {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_EDITS_FORM);
            }
            let parsed = if args.file_path.is_some() {
                Vec::new()
            } else {
                let Some(patch) = args.patch.as_deref() else {
                    return ToolOutcome::failure_text(PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT);
                };
                if looks_like_codex_patch(patch) {
                    Vec::new()
                } else {
                    match parse_unified_patch(patch) {
                        Ok(parsed) => parsed,
                        Err(error) => return ToolOutcome::failure_text(error.message().as_str()),
                    }
                }
            };
            let mut changes = Vec::new();
            if let Some(path) = args.file_path.clone() {
                let mutation = match args.edits.as_deref() {
                    Some(edits) => multi_edit_mutation(&context, path, edits).await,
                    None => {
                        search_replace_mutation(
                            &context,
                            path,
                            args.old_string.as_deref().unwrap_or(""),
                            args.new_string.as_deref().unwrap_or(""),
                            args.replace_all.unwrap_or(false),
                        )
                        .await
                    }
                };
                match mutation {
                    Ok(change) => changes.push(change),
                    Err(error)
                        if matches!(
                            error.kind(),
                            ToolRuntimeErrorKind::InvalidArguments
                                | ToolRuntimeErrorKind::WorkspacePolicy
                        ) =>
                    {
                        return ToolOutcome::failure_text(error.message().as_str());
                    }
                    Err(error) => return Err(error),
                }
            } else if args.patch.as_deref().is_some_and(looks_like_codex_patch) {
                match codex_patch_mutations(&context, args.patch.as_deref().unwrap_or("")).await {
                    Ok(codex_changes) => changes = codex_changes,
                    Err(error)
                        if matches!(
                            error.kind(),
                            ToolRuntimeErrorKind::InvalidArguments
                                | ToolRuntimeErrorKind::WorkspacePolicy
                        ) =>
                    {
                        return ToolOutcome::failure_text(error.message().as_str());
                    }
                    Err(error) => return Err(error),
                }
            } else {
                for parsed in parsed {
                    context.check_cancelled()?;
                    let relative = match relative_workspace_path(&context, parsed.path) {
                        Ok(path) => path,
                        Err(error) => {
                            return ToolOutcome::failure_text(error.message().as_str());
                        }
                    };
                    let Ok(patch) = diffy::Patch::from_str(&parsed.patch) else {
                        return ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_MALFORMED);
                    };
                    match parsed.kind {
                        ParsedPatchKind::Create => {
                            let Ok(replacement) = diffy::apply("", &patch) else {
                                return ToolOutcome::failure_text(
                                    PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY,
                                );
                            };
                            let change = match WorkspaceMutation::new(
                                relative,
                                WorkspaceExpectedFileState::Missing,
                                WorkspaceReplacement::content(replacement.into_bytes()),
                            ) {
                                Ok(change) => change,
                                Err(error) => {
                                    return ToolOutcome::failure_text(error.message().as_str());
                                }
                            };
                            changes.push(change);
                        }
                        ParsedPatchKind::Modify | ParsedPatchKind::Delete => {
                            let read = context
                                .environment()
                                .read_workspace(
                                    match WorkspaceReadSliceRequest::start(
                                        &relative,
                                        MAX_STANDARD_TEXT_BYTES,
                                    ) {
                                        Ok(request) => request,
                                        Err(error) => {
                                            return ToolOutcome::failure_text(
                                                error.message().as_str(),
                                            );
                                        }
                                    },
                                    operation_control(&context),
                                )
                                .await;
                            let read = match read {
                                Ok(read) => read,
                                Err(error)
                                    if error.kind() != ExecutionBackendErrorKind::Cancelled =>
                                {
                                    return ToolOutcome::failure_text(error.message().as_str());
                                }
                                Err(error) => return Err(backend_runtime_error(error)),
                            };
                            if read.next().is_some() {
                                return ToolOutcome::failure_text(
                                    PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
                                );
                            }
                            let Ok(original) = std::str::from_utf8(read.bytes()) else {
                                return ToolOutcome::failure_text(
                                    PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
                                );
                            };
                            let Ok(replacement) = diffy::apply(original, &patch) else {
                                return ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_HUNK_CONFLICT);
                            };
                            let replacement = match parsed.kind {
                                ParsedPatchKind::Modify => {
                                    WorkspaceReplacement::content(replacement.into_bytes())
                                }
                                ParsedPatchKind::Delete if replacement.is_empty() => {
                                    WorkspaceReplacement::Missing
                                }
                                ParsedPatchKind::Delete => {
                                    return ToolOutcome::failure_text(
                                        PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
                                    );
                                }
                                ParsedPatchKind::Create => {
                                    return ToolOutcome::failure_text(
                                        PLAKOUS_TOOL_PATCH_KIND_CHANGED,
                                    );
                                }
                            };
                            let change = match WorkspaceMutation::new(
                                relative,
                                WorkspaceExpectedFileState::Content(read.content()),
                                replacement,
                            ) {
                                Ok(change) => change,
                                Err(error) => {
                                    return ToolOutcome::failure_text(error.message().as_str());
                                }
                            };
                            changes.push(change);
                        }
                    }
                }
            }
            context
                .progress()
                .report("preflighting workspace patch", None, None)?;
            let transaction = match WorkspaceMutationTransaction::new(changes) {
                Ok(transaction) => transaction,
                Err(error) => {
                    return ToolOutcome::failure_text(error.message().as_str());
                }
            };
            let result = context
                .environment()
                .mutate_workspace(transaction, operation_control(&context))
                .await;
            match result {
                Ok(result) => {
                    let changed_files = result
                        .items()
                        .iter()
                        .filter(|item| {
                            item.disposition() != WorkspaceMutationDisposition::Unchanged
                        })
                        .count();
                    ToolOutcome::success_text(format!("changed_files={changed_files}"))
                }
                Err(error) if error.kind() == ExecutionBackendErrorKind::Conflict => {
                    ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT)
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
                    Err(backend_runtime_error(error))
                }
                Err(error) => ToolOutcome::failure_text(error.message().as_str()),
            }
        })
    }
}

async fn search_replace_mutation(
    context: &ToolContext,
    path: PathBuf,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<WorkspaceMutation, ToolRuntimeError> {
    let path = relative_workspace_path(context, path)?;
    if old_string.is_empty() {
        let expected = if replace_all {
            WorkspaceExpectedFileState::Any
        } else {
            WorkspaceExpectedFileState::Missing
        };
        return WorkspaceMutation::new(
            path,
            expected,
            WorkspaceReplacement::content(new_string.as_bytes().to_vec()),
        )
        .map_err(|error| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                error.message().as_str(),
            )
        });
    }
    let read = read_patch_target(context, &path).await?;
    let original = std::str::from_utf8(read.bytes()).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
        )
    })?;
    let matches = original.matches(old_string).count();
    if matches == 0 {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND,
        ));
    }
    if !replace_all && matches > 1 {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS,
        ));
    }
    let next = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };
    WorkspaceMutation::new(
        path,
        WorkspaceExpectedFileState::Content(read.content()),
        WorkspaceReplacement::content(next.into_bytes()),
    )
    .map_err(|error| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            error.message().as_str(),
        )
    })
}

/// Every edit matched exactly once against the original file and applied in
/// one replacement, so a later edit never sees an earlier one's text.
async fn multi_edit_mutation(
    context: &ToolContext,
    path: PathBuf,
    edits: &[ExactEdit],
) -> Result<WorkspaceMutation, ToolRuntimeError> {
    let invalid =
        |message: &str| ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message);
    let at = |template: &str, index: usize| template.replace("{index}", &index.to_string());
    if let Some(index) = edits.iter().position(|edit| edit.old_string.is_empty()) {
        return Err(invalid(&at(PLAKOUS_TOOL_EDIT_EMPTY, index)));
    }
    let path = relative_workspace_path(context, path)?;
    let read = read_patch_target(context, &path).await?;
    let original = std::str::from_utf8(read.bytes())
        .map_err(|_| invalid(PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8))?;
    let mut spans = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let mut found = original.match_indices(edit.old_string.as_str());
        let Some((start, _)) = found.next() else {
            return Err(invalid(&at(PLAKOUS_TOOL_EDIT_NOT_FOUND, index)));
        };
        if found.next().is_some() {
            return Err(invalid(&at(PLAKOUS_TOOL_EDIT_AMBIGUOUS, index)));
        }
        spans.push((
            start,
            start + edit.old_string.len(),
            edit.new_string.as_str(),
            index,
        ));
    }
    spans.sort_by_key(|(start, _, _, _)| *start);
    if let Some(pair) = spans.windows(2).find(|pair| pair[0].1 > pair[1].0) {
        let (first, second) = (pair[0].3.min(pair[1].3), pair[0].3.max(pair[1].3));
        return Err(invalid(
            &PLAKOUS_TOOL_EDITS_OVERLAP
                .replace("{first}", &first.to_string())
                .replace("{second}", &second.to_string()),
        ));
    }
    let mut next = String::with_capacity(original.len());
    let mut cursor = 0;
    for (start, end, replacement, _) in spans {
        next.push_str(&original[cursor..start]);
        next.push_str(replacement);
        cursor = end;
    }
    next.push_str(&original[cursor..]);
    WorkspaceMutation::new(
        path,
        WorkspaceExpectedFileState::Content(read.content()),
        WorkspaceReplacement::content(next.into_bytes()),
    )
    .map_err(|error| invalid(error.message().as_str()))
}

async fn codex_patch_mutations(
    context: &ToolContext,
    patch: &str,
) -> Result<Vec<WorkspaceMutation>, ToolRuntimeError> {
    let ops = parse_codex_patch(patch)?;
    let mut changes = Vec::with_capacity(ops.len());
    for op in ops {
        context.check_cancelled()?;
        match op {
            CodexPatchOp::Add { path, contents } => {
                let target = relative_workspace_path(context, path)?;
                changes.push(
                    WorkspaceMutation::new(
                        target,
                        WorkspaceExpectedFileState::Missing,
                        WorkspaceReplacement::content(contents.into_bytes()),
                    )
                    .map_err(|error| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidArguments,
                            error.message().as_str(),
                        )
                    })?,
                );
            }
            CodexPatchOp::Delete { path } => {
                let target = relative_workspace_path(context, path)?;
                let read = read_patch_target(context, &target).await?;
                changes.push(
                    WorkspaceMutation::new(
                        target,
                        WorkspaceExpectedFileState::Content(read.content()),
                        WorkspaceReplacement::Missing,
                    )
                    .map_err(|error| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidArguments,
                            error.message().as_str(),
                        )
                    })?,
                );
            }
            CodexPatchOp::Update {
                path,
                move_to,
                chunks,
            } => {
                let target = relative_workspace_path(context, path)?;
                let read = read_patch_target(context, &target).await?;
                let original = std::str::from_utf8(read.bytes()).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
                    )
                })?;
                let next = apply_codex_update_chunks(&target.to_string_lossy(), original, &chunks)?;
                let replacement = if let Some(destination) = move_to {
                    let destination = relative_workspace_path(context, destination)?;
                    changes.push(
                        WorkspaceMutation::new(
                            destination,
                            WorkspaceExpectedFileState::Missing,
                            WorkspaceReplacement::content(next.into_bytes()),
                        )
                        .map_err(backend_runtime_error)?,
                    );
                    WorkspaceReplacement::Missing
                } else {
                    WorkspaceReplacement::content(next.into_bytes())
                };
                changes.push(
                    WorkspaceMutation::new(
                        target,
                        WorkspaceExpectedFileState::Content(read.content()),
                        replacement,
                    )
                    .map_err(|error| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidArguments,
                            error.message().as_str(),
                        )
                    })?,
                );
            }
        }
    }
    Ok(changes)
}

async fn read_patch_target(
    context: &ToolContext,
    path: &Path,
) -> Result<plakous_tools::WorkspaceReadSlice, ToolRuntimeError> {
    let read = context
        .environment()
        .read_workspace(
            WorkspaceReadSliceRequest::start(path, MAX_STANDARD_TEXT_BYTES)
                .map_err(backend_runtime_error)?,
            operation_control(context),
        )
        .await;
    match read {
        Ok(read) if read.next().is_none() => Ok(read),
        Ok(_) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
        )),
        Err(error) if error.kind() != ExecutionBackendErrorKind::Cancelled => {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                error.message().as_str(),
            ))
        }
        Err(error) => Err(backend_runtime_error(error)),
    }
}

fn parse_unified_patch(patch_text: &str) -> Result<Vec<ParsedPatch>, ToolRuntimeError> {
    let mut parsed = Vec::new();
    for file in PatchSet::parse(patch_text, ParseOptions::gitdiff()) {
        let file = file.map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidArguments,
                "patch text is not a valid git-style unified diff",
                &error,
            )
        })?;
        if parsed.len() == MAX_STANDARD_PATCH_CHANGES {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "patch contains too many files",
            ));
        }
        if file
            .old_mode()
            .zip(file.new_mode())
            .is_some_and(|(old, new)| old != new)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "patch mode changes are unsupported",
            ));
        }
        let PatchKind::Text(text_patch) = file.patch() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "binary patches are unsupported",
            ));
        };
        // diffy borrows hunk bodies from the input. A body reaching physical EOF
        // without LF was not followed by Git's explicit no-newline marker.
        if text_patch
            .hunks()
            .iter()
            .flat_map(diffy::Hunk::lines)
            .any(|line| {
                let (diffy::Line::Context(body)
                | diffy::Line::Delete(body)
                | diffy::Line::Insert(body)) = line;
                !body.ends_with('\n')
                    && body.as_bytes().as_ptr_range().end
                        == patch_text.as_bytes().as_ptr_range().end
            })
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "patch hunk lines must end with LF; use an explicit \\ No newline at end of file marker for content without a final newline",
            ));
        }
        let operation = file.operation().strip_prefix(1);
        let (kind, target_path) = match operation {
            FileOperation::Create(target_path) => (ParsedPatchKind::Create, target_path),
            FileOperation::Delete(target_path) => (ParsedPatchKind::Delete, target_path),
            FileOperation::Modify { original, modified } if original == modified => {
                (ParsedPatchKind::Modify, original)
            }
            FileOperation::Modify { .. }
            | FileOperation::Rename { .. }
            | FileOperation::Copy { .. } => {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "patch rename and copy operations are unsupported",
                ));
            }
        };
        parsed.push(ParsedPatch {
            path: PathBuf::from(target_path.into_owned()),
            kind,
            patch: text_patch.to_string(),
        });
    }
    if parsed.is_empty() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "patch contains no file changes",
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::parse_unified_patch;

    #[test]
    fn unified_patch_rejects_unterminated_physical_hunk_bodies() {
        for (range, body) in [
            ("-0,0 +1", "+token"),
            ("-1 +0,0", "-token"),
            ("-1 +1", " token"),
        ] {
            let patch =
                format!("diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ {range} @@\n{body}");
            assert!(parse_unified_patch(&patch).is_err(), "{body}");
            assert!(parse_unified_patch(&format!("{patch}\n")).is_ok(), "{body}");
            for marker_end in ["", "\n"] {
                let marked = format!("{patch}\n\\ No newline at end of file{marker_end}");
                let parsed = parse_unified_patch(&marked).expect("explicit marker is valid");
                let patch = diffy::Patch::from_str(&parsed[0].patch).unwrap();
                let (before, after) = if body.starts_with('+') {
                    ("", "token")
                } else if body.starts_with('-') {
                    ("token", "")
                } else {
                    ("token", "token")
                };
                assert_eq!(diffy::apply(before, &patch).unwrap(), after);
            }
        }
    }
}
