//! One workspace-path rule and one patch preflight.
//!
//! Bounded reads, exact-content patching and image inspection, plus every
//! provider lowering of them: `Write`, `write` and `search_replace` are
//! lowerings of `apply_patch`, not separate capabilities (spec 02 §6.3), so
//! they live here with the preflight that makes them exact, and the Codex
//! freeform patch envelope with the executor it shares.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod codex_patch;
mod edit_files;
mod text;

pub use edit_files::{EDIT_FILES_NAME, EDIT_FILES_REGISTERED_TOOL_ID, edit_files_registration};
use text::{
    PLAKOUS_TOOL_EDIT_AMBIGUOUS, PLAKOUS_TOOL_EDIT_EMPTY, PLAKOUS_TOOL_EDIT_NOT_FOUND,
    PLAKOUS_TOOL_EDITS_FORM, PLAKOUS_TOOL_EDITS_OVERLAP, PLAKOUS_TOOL_READ_BINARY,
};

pub use text::{
    PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION, PLAKOUS_GROK_READ_DESCRIPTION,
    PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION, PLAKOUS_GROK_WRITE_DESCRIPTION,
    PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION, PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT,
    PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY, PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
    PLAKOUS_TOOL_IMAGE_TOO_LARGE, PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
    PLAKOUS_TOOL_PATCH_HUNK_CONFLICT, PLAKOUS_TOOL_PATCH_KIND_CHANGED,
    PLAKOUS_TOOL_PATCH_MALFORMED, PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
    PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE, PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT,
    PLAKOUS_TOOL_READ_FILE_DESCRIPTION, PLAKOUS_TOOL_READ_OFFSET_PAST_EOF,
    PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS, PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND,
    PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diffy::patch_set::{FileOperation, ParseOptions, PatchKind, PatchSet};
use plakous_model::{
    ImageBlock, MediaSource, ProjectedCustomToolFormatV2, TextBlock, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolResultPart, Utf8TextContractV1,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::codex_patch::{
    CODEX_APPLY_PATCH_GRAMMAR_V1, CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
    CODEX_PATCH_ENVELOPE_MAX_BYTES, CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID, CodexPatchOp,
    apply_codex_update_chunks, codex_patch_envelope_definition, looks_like_codex_patch,
    parse_codex_patch,
};
use plakous_tools::CodingPresentation;
use plakous_tools::PromptPin;
use plakous_tools::object_schema;
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, RegisteredToolId, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolProjectionLoweringV2, ToolRegistration,
};
use plakous_tools::{
    ExecutionBackendErrorKind, ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome,
    ToolRuntimeError, ToolRuntimeErrorKind, WorkspaceExpectedFileState, WorkspaceMutation,
    WorkspaceMutationDisposition, WorkspaceMutationTransaction, WorkspaceReadSliceRequest,
    WorkspaceReplacement,
};
use plakous_tools::{MenuBinding, MenuEntry, MenuShape, ToolPack, tool_name};

use plakous_tools::{
    APPLY_PATCH_CAPABILITY_NAME, INSPECT_IMAGE_CAPABILITY_NAME, MAX_STANDARD_IMAGE_BYTES,
    MAX_STANDARD_PATCH_CHANGES, MAX_STANDARD_TEXT_BYTES, READ_FILE_CAPABILITY_NAME,
};
use plakous_tools::{
    backend_runtime_error, content_runtime_error, decode_arguments, operation_control,
};
use plakous_tools::{
    standard_definition, standard_executor_id, standard_registration,
    standard_registration_with_executor, workspace_concurrency,
};

fn relative_workspace_path(
    context: &ToolContext,
    path: impl AsRef<Path>,
) -> Result<PathBuf, ToolRuntimeError> {
    context.workspace().relative_input(path)
}

#[derive(Clone)]
pub(crate) struct ReadFileTool;

impl ReadFileTool {
    pub(crate) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                READ_FILE_CAPABILITY_NAME,
                PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
                object_schema(
                    json!({
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 1},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                    }),
                    &["path"],
                ),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: PathBuf,
    offset: Option<f64>,
    limit: Option<f64>,
}

impl ToolExecutor for ReadFileTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: ReadFileArgs = decode_arguments(&invocation)?;
            let path_display = args.path.display().to_string();
            let subject: String = path_display.chars().take(100).collect();
            context.progress().report(&subject, None, None)?;
            let start_line = args
                .offset
                .map_or(Ok(1), |value| exact_u64(value, "read offset is invalid"))?;
            if start_line == 0 {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "read offset must be a 1-indexed line",
                ));
            }
            let limit = args
                .limit
                .map_or(Ok(200), |value| exact_u64(value, "read limit is invalid"))?
                .clamp(1, 2_000);
            let leading = read_leading_lines(&context, &args, start_line, limit).await?;
            let revision = leading.revision;
            let (mut bytes, complete) = (leading.bytes, leading.complete);
            if bytes.iter().take(8_192).any(|byte| *byte == 0) {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_READ_BINARY);
            }
            if !complete {
                drop_cut_utf8_tail(&mut bytes);
            }
            let (text, lossy) = match String::from_utf8(bytes) {
                Ok(text) => (text, false),
                Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
            };
            let lines = text.split_inclusive('\n').collect::<Vec<_>>();
            let start_index = usize::try_from(
                start_line
                    .saturating_sub(1)
                    .saturating_sub(leading.skipped_lines),
            )
            .unwrap_or(usize::MAX);
            let available = lines.len() + usize::from(leading.dropped_partial);
            if start_index > available || (start_index == available && start_line != 1) {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_READ_OFFSET_PAST_EOF);
            }
            let take = usize::try_from(limit).unwrap_or(lines.len());
            let mut page = String::new();
            let mut presented = 0_usize;
            let mut selected = 0_usize;
            let mut clipped_lines = 0_u64;
            for line in lines.iter().skip(start_index).take(take) {
                let (line, clipped) = clip_long_line(line);
                let line_number =
                    start_line.saturating_add(u64::try_from(selected).unwrap_or(u64::MAX));
                presented = presented.saturating_add(presented_line_bytes(&line, line_number));
                if selected != 0 && presented > READ_PAGE_PRESENTED_BYTES {
                    break;
                }
                page.push_str(&line);
                selected += 1;
                clipped_lines += u64::from(clipped);
            }
            let next_line = start_line.saturating_add(u64::try_from(selected).unwrap_or(0));
            let eof = complete && start_index.saturating_add(selected) >= lines.len();
            if eof && let Some(written) = leading.write_failed_after {
                push_incomplete_line(&mut page, written);
            }
            let wanted = lines.len().saturating_sub(start_index).min(take);
            let value = plakous_tools::FileReadValue {
                path: path_display,
                text: page.clone(),
                first_line: start_line,
                lines: selected as u64,
                eof,
                lines_cut: wanted.saturating_sub(selected) as u64,
                clipped_lines,
                lossy,
                revision,
            };
            let result = plakous_tools::WorkspaceFileReadResult {
                content: page,
                offset: start_line,
                lines: selected as u64,
                eof,
                next_offset: if eof { None } else { Some(next_line) },
                lossy,
                clipped_lines,
                workspace_generation: context.workspace().generation_stamp(),
            };
            let encoded = serde_json::to_string(&result).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "workspace read result could not be encoded",
                    &error,
                )
            })?;
            Ok(ToolOutcome::success_text(encoded)?
                .with_execution_value(plakous_tools::ExecutionValue::FileRead(value)))
        })
    }
}

/// A read gathers at most this many bytes of content for one page.
const READ_RESPONSE_BYTES: usize = 262_144;
/// One page's content, measured as it is presented ([`presented_line_bytes`]),
/// fits this bound. It leaves 4,096 bytes for page metadata, the presentation
/// wrapper and notes under the runtime's per-result model-visible cut (the
/// semantic-JSON maximum string size less its 4,096-byte envelope reserve), so
/// no presentation of a whole page is ever cut there.
const READ_PAGE_PRESENTED_BYTES: usize =
    plakous_model::SemanticJsonV1::maximum_string_bytes() - 2 * 4_096;
/// A line longer than this many characters is clipped with a marker.
const READ_LINE_CHARS: usize = 2_000;

/// The bytes one read page is cut from.
struct LeadingBytes {
    /// Bytes from the start of line `1 + skipped_lines`.
    bytes: Vec<u8>,
    /// Whether `bytes` reach the end of the file.
    complete: bool,
    /// Complete lines read and dropped before `bytes`.
    skipped_lines: u64,
    /// The file ended inside a dropped line before the requested one.
    dropped_partial: bool,
    /// Content bytes a job output file holds when spooling to it failed.
    write_failed_after: Option<u64>,
    /// Whole-file content identity of a workspace file.
    revision: Option<plakous_tools::WorkspaceContentIdentity>,
}

/// Reads from the start of the file until the `limit` lines from
/// `start_line` on are in hand or the file ends, resuming exact slices of the
/// same content up to the workspace file bound.
async fn read_leading_lines(
    context: &ToolContext,
    args: &ReadFileArgs,
    start_line: u64,
    limit: u64,
) -> Result<LeadingBytes, ToolRuntimeError> {
    if let Some(issued) = args.path.to_str()
        && let Some(first) =
            context
                .environment()
                .read_job_output(issued, 0, MAX_STANDARD_TEXT_BYTES)
    {
        return read_job_output_page(context, issued, first, start_line, limit).await;
    }
    let wanted_lines = start_line.saturating_sub(1).saturating_add(limit);
    let path = relative_workspace_path(context, &args.path)?;
    let mut request = WorkspaceReadSliceRequest::start(path, MAX_STANDARD_TEXT_BYTES)
        .map_err(backend_runtime_error)?;
    let mut bytes = Vec::new();
    let mut newlines = 0_u64;
    let mut revision = None;
    let leading = |bytes, complete, revision| LeadingBytes {
        bytes,
        complete,
        skipped_lines: 0,
        dropped_partial: false,
        write_failed_after: None,
        revision,
    };
    loop {
        context.check_cancelled()?;
        let slice = context
            .environment()
            .read_workspace(request, operation_control(context))
            .await
            .map_err(backend_runtime_error)?;
        newlines += slice
            .bytes()
            .iter()
            .fold(0_u64, |count, byte| count + u64::from(*byte == b'\n'));
        bytes.extend_from_slice(slice.bytes());
        revision.get_or_insert(slice.content());
        let Some(next) = slice.next().cloned() else {
            return Ok(leading(bytes, true, revision));
        };
        if newlines >= wanted_lines {
            return Ok(leading(bytes, false, revision));
        }
        if bytes.len() as u64 >= plakous_tools::MAX_WORKSPACE_FILE_BYTES {
            return Ok(leading(bytes, false, revision));
        }
        request = WorkspaceReadSliceRequest::resume(next, MAX_STANDARD_TEXT_BYTES)
            .map_err(backend_runtime_error)?;
    }
}

/// The read exception: a path that byte-equals a job output file this agent's
/// process service issued is read from that file, re-verified on every slice.
/// Lines before `start_line` are counted while streaming and dropped, so a read
/// holds at most one slice plus one page ([`READ_RESPONSE_BYTES`]) of content.
async fn read_job_output_page(
    context: &ToolContext,
    path: &str,
    first: plakous_tools::ExecutionBackendFuture<plakous_tools::JobOutputSlice>,
    start_line: u64,
    limit: u64,
) -> Result<LeadingBytes, ToolRuntimeError> {
    let skip = start_line.saturating_sub(1);
    let mut slice = first;
    let mut offset = 0_u64;
    let mut page = LeadingBytes {
        bytes: Vec::new(),
        complete: false,
        skipped_lines: 0,
        dropped_partial: false,
        write_failed_after: None,
        revision: None,
    };
    let mut kept_lines = 0_u64;
    loop {
        context.check_cancelled()?;
        let read = slice.await.map_err(job_output_runtime_error)?;
        offset += read.bytes.len() as u64;
        page.write_failed_after = read.write_failed_after;
        let mut rest = read.bytes.as_slice();
        while page.skipped_lines < skip && !rest.is_empty() {
            if let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
                page.skipped_lines += 1;
                page.dropped_partial = false;
                rest = &rest[end + 1..];
            } else {
                page.dropped_partial = true;
                rest = &[];
            }
        }
        let room = (READ_RESPONSE_BYTES + 1).saturating_sub(page.bytes.len());
        let kept = &rest[..rest.len().min(room)];
        kept_lines += kept
            .iter()
            .fold(0_u64, |count, byte| count + u64::from(*byte == b'\n'));
        page.bytes.extend_from_slice(kept);
        if read.complete && kept.len() == rest.len() {
            page.complete = true;
            return Ok(page);
        }
        if read.bytes.is_empty() {
            // The read bound: the file holds more than spooling ever writes.
            return Ok(page);
        }
        if page.bytes.len() > READ_RESPONSE_BYTES {
            // Keep whole lines, so the next offset resumes after the last one
            // shown; a single overlong line is clipped by the page instead.
            if let Some(end) = page.bytes.iter().rposition(|byte| *byte == b'\n') {
                page.bytes.truncate(end + 1);
            }
            return Ok(page);
        }
        if kept_lines >= limit {
            return Ok(page);
        }
        slice = context
            .environment()
            .read_job_output(path, offset, MAX_STANDARD_TEXT_BYTES)
            .ok_or_else(|| job_output_runtime_error(plakous_tools::job_output_file_lost(path)))?;
    }
}

/// Drops the incomplete UTF-8 sequence (at most three bytes) a read bound cut
/// at the end of `bytes`, so `lossy` reports only invalid file bytes.
fn drop_cut_utf8_tail(bytes: &mut Vec<u8>) {
    let tail = bytes.len().saturating_sub(3);
    if let Some(lead) = (tail..bytes.len())
        .rev()
        .find(|index| bytes[*index] & 0xc0 != 0x80)
        && std::str::from_utf8(&bytes[lead..]).is_err_and(|error| error.error_len().is_none())
    {
        bytes.truncate(lead);
    }
}

/// Job output refusals are policy refusals, including a replaced file and a
/// non-private directory.
fn job_output_runtime_error(error: plakous_tools::ExecutionBackendError) -> ToolRuntimeError {
    match error.kind() {
        ExecutionBackendErrorKind::IdentityMismatch | ExecutionBackendErrorKind::Denied => {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::WorkspacePolicy,
                error.message().as_str(),
            )
            .with_proven_no_effect(true)
        }
        _ => backend_runtime_error(error),
    }
}

/// Ends the last page of a job output file whose spool failed with the
/// incomplete-file line, unless the file already ends with it.
fn push_incomplete_line(page: &mut String, written: u64) {
    let line = plakous_tools::job_output_incomplete_line(written);
    if page.trim_end_matches('\n').ends_with(&line) {
        return;
    }
    if !page.is_empty() && !page.ends_with('\n') {
        page.push('\n');
    }
    page.push_str(&line);
    page.push('\n');
}

/// Bytes one page line costs in the largest presentation of a read: its text
/// JSON-escaped twice (the neutral result's `content` string inside a
/// `{"result": string}` wrapper) plus a cat-n line-number prefix (at least six
/// digits and an escaped tab).
fn presented_line_bytes(line: &str, line_number: u64) -> usize {
    let escaped_twice = line
        .chars()
        .map(|character| match character {
            '"' | '\\' => 4,
            '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 3,
            '\0'..='\u{1f}' => 7,
            other => other.len_utf8(),
        })
        .sum::<usize>();
    let digits = line_number.checked_ilog10().map_or(1, |log| log + 1).max(6);
    escaped_twice + usize::try_from(digits).unwrap_or(20) + 2
}

/// Clips one line to [`READ_LINE_CHARS`] characters, keeping its terminator.
fn clip_long_line(line: &str) -> (std::borrow::Cow<'_, str>, bool) {
    let body = line.trim_end_matches(['\n', '\r']);
    let terminator = &line[body.len()..];
    let Some((cut, _)) = body.char_indices().nth(READ_LINE_CHARS) else {
        return (std::borrow::Cow::Borrowed(line), false);
    };
    let omitted = body[cut..].chars().count();
    (
        std::borrow::Cow::Owned(format!(
            "{} [line clipped: {omitted} more characters]{terminator}",
            &body[..cut]
        )),
        true,
    )
}

fn exact_u64(value: f64, message: &'static str) -> Result<u64, ToolRuntimeError> {
    value
        .to_string()
        .parse::<u64>()
        .map_err(|_| ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message))
}

#[derive(Clone)]
pub(crate) struct ApplyPatchTool;

impl ApplyPatchTool {
    pub(crate) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
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
            CodexPatchOp::Update { path, chunks, .. } => {
                let target = relative_workspace_path(context, path)?;
                let read = read_patch_target(context, &target).await?;
                let original = std::str::from_utf8(read.bytes()).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8,
                    )
                })?;
                let next = apply_codex_update_chunks(&target.to_string_lossy(), original, &chunks)?;
                changes.push(
                    WorkspaceMutation::new(
                        target,
                        WorkspaceExpectedFileState::Content(read.content()),
                        WorkspaceReplacement::content(next.into_bytes()),
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

#[derive(Clone)]
pub(crate) struct InspectImageTool;

impl InspectImageTool {
    pub(crate) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                INSPECT_IMAGE_CAPABILITY_NAME,
                PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
                object_schema(
                    json!({
                        "path": {"type": "string"}
                    }),
                    &["path"],
                ),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectImageArgs {
    path: PathBuf,
}

impl ToolExecutor for InspectImageTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: InspectImageArgs = decode_arguments(&invocation)?;
            context
                .progress()
                .report("reading workspace image", None, None)?;
            let path_display = args.path.display().to_string();
            let path = relative_workspace_path(&context, args.path)?;
            let result = context
                .environment()
                .read_workspace(
                    WorkspaceReadSliceRequest::start(path, MAX_STANDARD_IMAGE_BYTES)
                        .map_err(backend_runtime_error)?,
                    operation_control(&context),
                )
                .await
                .map_err(backend_runtime_error)?;
            if result.next().is_some() {
                return ToolOutcome::failure_text(PLAKOUS_TOOL_IMAGE_TOO_LARGE);
            }
            let info = inspect_image_header(result.bytes())?;
            let dimensions = info.dimensions().map_or_else(
                || String::from("unknown"),
                |(width, height)| format!("{width}x{height}"),
            );
            let metadata = TextBlock::new(
                format!(
                    "media_type={} bytes={} dimensions={dimensions}",
                    info.media_type(),
                    result.bytes().len()
                ),
                None,
            )
            .map_err(content_runtime_error)?;
            let image = ImageBlock::new(
                info.media_type(),
                MediaSource::bytes(result.bytes().as_ref().to_vec())
                    .map_err(content_runtime_error)?,
            )
            .map_err(content_runtime_error)?;
            let value = plakous_tools::ImageValue {
                path: path_display,
                media_type: info.media_type().to_owned(),
                bytes: result.bytes().as_ref().to_vec(),
                dimensions: info.dimensions(),
            };
            Ok(ToolOutcome::success(vec![
                ToolResultPart::Text(metadata),
                ToolResultPart::Image(image),
            ])?
            .with_execution_value(plakous_tools::ExecutionValue::Image(value)))
        })
    }
}

fn inspect_image_header(bytes: &[u8]) -> Result<plakous_tools::ImageHeader, ToolRuntimeError> {
    plakous_tools::sniff_image(bytes).map_err(|error| match error {
        plakous_tools::ImageSniffError::Dimensions => ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "image dimensions exceed the configured limit",
        ),
        _ => ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "workspace file is not a supported image",
        ),
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {

    use super::*;

    #[test]
    fn a_presented_line_costs_its_twice_escaped_json_and_a_cat_n_prefix() {
        let every_ascii = (0_u8..=127).map(char::from).collect::<String>();
        for line in [every_ascii.as_str(), "λ \"q\" \\ tab\t\r\n", "", "plain\n"] {
            let twice = serde_json::to_string(&serde_json::to_string(line).unwrap()).unwrap();
            // Two pairs of enclosing quotes, the outer pair escaping the inner.
            let escaped_twice = twice.len() - 6;
            assert_eq!(presented_line_bytes(line, 1), escaped_twice + 8, "{line:?}");
            assert_eq!(presented_line_bytes(line, 12_345_678), escaped_twice + 10);
        }
    }

    #[test]
    fn a_failed_job_output_spool_ends_the_last_page_once() {
        let line = "[output file incomplete: write failed after 7 bytes]";
        let mut page = String::from("partial");
        push_incomplete_line(&mut page, 7);
        assert_eq!(page, format!("partial\n{line}\n"));
        // The writer's best-effort marker already ends the file.
        push_incomplete_line(&mut page, 7);
        assert_eq!(page, format!("partial\n{line}\n"));
        let mut page = String::new();
        push_incomplete_line(&mut page, 7);
        assert_eq!(page, format!("{line}\n"));
    }

    #[test]
    fn line_clipping_counts_content_characters_not_lf_or_crlf() {
        for terminator in ["\n", "\r\n"] {
            for content_chars in [1_999, 2_000] {
                let line = format!("{}{terminator}", "é".repeat(content_chars));
                let (clipped, was_clipped) = clip_long_line(&line);
                assert!(
                    !was_clipped,
                    "{content_chars} characters before {terminator:?}"
                );
                assert_eq!(clipped, line);
            }

            let line = format!("{}{terminator}", "é".repeat(2_001));
            let (clipped, was_clipped) = clip_long_line(&line);
            assert!(was_clipped, "2001 characters before {terminator:?}");
            assert_eq!(
                clipped,
                format!(
                    "{} [line clipped: 1 more characters]{terminator}",
                    "é".repeat(2_000)
                )
            );
        }
    }

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

    #[test]
    fn image_headers_are_signature_bound_and_dimension_limited() -> Result<(), ToolRuntimeError> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&3_u32.to_be_bytes());
        let info = inspect_image_header(&png)?;
        assert_eq!(info.media_type(), "image/png");
        assert_eq!(info.dimensions(), Some((2, 3)));
        assert!(inspect_image_header(b"not-an-image").is_err());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The files pack.
// ---------------------------------------------------------------------------

// Adapted from pi 1.1.0's file tool descriptions (MIT); see the README License section.
const PI_READ_DESCRIPTION: &str = "Read the contents of a text file; images and binary files are refused. Output is truncated to 2000 lines or 256 KiB (whichever is hit first) and lines longer than 2000 characters are clipped. Use offset/limit for large files. When you need the full file, continue with offset until complete. path is relative to the workspace or absolute under it, or exactly a path a bash result or completion notice named on its `full output:` line.";
const PI_EDIT_DESCRIPTION: &str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. The edits apply together or not at all.";
const PI_WRITE_DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";
const CLAUDE_READ_DESCRIPTION: &str = "Reads a file from the local filesystem.\n\n- Reads up to 2000 lines by default.\n- If you already know which part of the file to read, only read that part. This is important for larger files/context efficiency.\n- `offset` is the 1-indexed first line (omit to start at line 1). `limit` is how many lines to return (default 2000, max 2000).\n- The result is cat -n text: a 6-character file line number, a tab, then the line. Numbers are the file's real line numbers (offset 10 starts at line 10).\n- If truncated, a last line `... more lines; next_offset=N` means call again with that offset.\n- Lines longer than 2000 characters are clipped.\n- Text files only: binary files, images and PDFs are refused (there is no `pages` parameter).\n- `file_path` is an absolute path under the workspace root, a workspace-relative path without `..`, or exactly a path a Bash result or completion notice named on its `full output:` line; other paths are refused, as is a `full output:` path issued by an earlier process (before a restart or reload) or another agent.\n- A read of an output file whose writing failed ends with `[output file incomplete: write failed after N bytes]`; the file stops there.\n- An empty file succeeds with an empty body, missing file/directory returns an error.";
const CLAUDE_EDIT_DESCRIPTION: &str = "Performs exact string replacement in a file.\n\n- `old_string` must be non-empty and match the file exactly, including indentation, and be unique — the edit fails otherwise.\n- `replace_all: true` replaces every occurrence instead.\n- Use Write to create a new file.";
const CLAUDE_WRITE_DESCRIPTION: &str = "Writes a file to the local filesystem.\n\n- Creates the file, or overwrites it when it exists. Missing parent directories are created.\n- Prefer Edit for partial changes; read an existing file before overwriting it.";

/// The Codex freeform patch envelope: the same apply-patch executor under the
/// registration a Codex costume expects. It is the files pack's registration
/// because it is the files pack's executor.
fn codex_patch_envelope_registration(
    apply_executor_id: &ToolExecutorId,
    apply_executor: Arc<dyn ToolExecutor>,
) -> Result<ToolRegistration, ToolRuntimeError> {
    Ok(ToolRegistration::new(
        RegisteredToolId::new(CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID)?,
        apply_executor_id.clone(),
        ToolExecutorBindingIdentity::new(
            4,
            &[
                b"standard-coding-tool",
                apply_executor_id.as_str().as_bytes(),
            ],
        )?,
        codex_patch_envelope_definition()?,
        apply_executor,
    )
    .nested_eligible())
}

/// One workspace-path rule and one patch preflight: bounded reads, exact-content
/// patching, and image inspection, plus every provider lowering of them.
///
/// `Write`, `write`, and `search_replace` are lowerings of `apply_patch`, not
/// separate capabilities (spec 02 §6.3), so they live here with the preflight
/// that makes them exact.
#[derive(Clone, Copy, Debug, Default)]
pub struct FilesToolPack;

impl ToolPack for FilesToolPack {
    fn id(&self) -> &'static str {
        "plakous.files.v3"
    }

    /// Three canonical capabilities and the Codex freeform patch envelope,
    /// which is the same executor under a provider-familiar registration.
    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let (apply_definition, apply_tool) = ApplyPatchTool::new()?;
        let apply_executor: Arc<dyn ToolExecutor> = Arc::new(apply_tool);
        let apply_executor_id = standard_executor_id("apply-patch")?;
        Ok(vec![
            standard_registration("read-file", ReadFileTool::new())?,
            standard_registration_with_executor(
                "apply-patch",
                apply_definition,
                Arc::clone(&apply_executor),
                &apply_executor_id,
            )?,
            standard_registration("inspect-image", InspectImageTool::new())?,
            codex_patch_envelope_registration(&apply_executor_id, apply_executor)?,
        ])
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the four vocabularies of one pack stay in one menu"
    )]
    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let read_file = tool_name(
            READ_FILE_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        let apply_patch = tool_name(
            APPLY_PATCH_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        Ok(match presentation {
            CodingPresentation::Pi => pi_menu(&read_file, &apply_patch),
            CodingPresentation::Codex => vec![codex_patch_envelope_entry()?],
            CodingPresentation::Grok => vec![
                MenuEntry::new(
                    "read_file",
                    vec![MenuBinding::by_effect(read_file)],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_READ_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "target_file": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 1},
                                "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                            }),
                            &["target_file"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokReadFile,
                ),
                MenuEntry::new(
                    "search_replace",
                    vec![MenuBinding::by_effect(apply_patch.clone())],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            }),
                            &["file_path", "old_string", "new_string"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokSearchReplace,
                ),
                MenuEntry::new(
                    "write",
                    vec![MenuBinding::by_effect(apply_patch)],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_WRITE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "content": {"type": "string"}
                            }),
                            &["file_path", "content"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokWrite,
                ),
            ],
            CodingPresentation::Claude => vec![
                MenuEntry::new(
                    "Read",
                    vec![MenuBinding::by_effect(read_file)],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_READ_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 1},
                                "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                            }),
                            &["file_path"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeReadFile,
                ),
                MenuEntry::new(
                    "Edit",
                    vec![MenuBinding::by_effect(apply_patch.clone())],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_EDIT_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            }),
                            &["file_path", "old_string", "new_string"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeEdit,
                ),
                MenuEntry::new(
                    "Write",
                    vec![MenuBinding::by_effect(apply_patch)],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_WRITE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "content": {"type": "string"}
                            }),
                            &["file_path", "content"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeWrite,
                ),
            ],
        })
    }

    fn verify(
        &self,
        presentation: CodingPresentation,
        registry: &FrozenToolRegistry,
    ) -> Result<(), ToolRuntimeError> {
        if presentation != CodingPresentation::Codex {
            return Ok(());
        }
        let name = tool_name(
            CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
            "Codex patch envelope name is invalid",
        )?;
        let identity = registry.identity(&name).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Codex patch envelope capability is not registered",
            )
        })?;
        if identity.tool_id().as_str() != CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Codex patch envelope identity is not enrolled",
            ));
        }
        Ok(())
    }

    fn pins(&self) -> &'static [PromptPin] {
        FILES_PROMPT_PINS
    }
}

/// Pi 1.1.0's `read`, `edit` and `write`, with pi's schemas and property
/// descriptions; `read` is text only, as pi 1.1.0's is.
fn pi_menu(
    read_file: &plakous_model::ToolName,
    apply_patch: &plakous_model::ToolName,
) -> Vec<MenuEntry> {
    let path = |verb: &str| {
        json!({
            "type": "string",
            "description": format!("Path to the file to {verb} (relative or absolute)")
        })
    };
    vec![
        MenuEntry::new(
            "read",
            vec![MenuBinding::by_effect(read_file.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_READ_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("read"),
                        "offset": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Line number to start reading from (1-indexed)"
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Maximum number of lines to read"
                        }
                    }),
                    &["path"],
                ),
            },
            ToolProjectionLoweringV2::PiRead,
        ),
        MenuEntry::new(
            "edit",
            vec![MenuBinding::by_effect(apply_patch.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_EDIT_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("edit"),
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {
                                        "type": "string",
                                        "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                                    },
                                    "newText": {
                                        "type": "string",
                                        "description": "Replacement text for this targeted edit."
                                    }
                                },
                                "required": ["oldText", "newText"],
                                "additionalProperties": false
                            }
                        }
                    }),
                    &["path", "edits"],
                ),
            },
            ToolProjectionLoweringV2::PiEdit,
        ),
        MenuEntry::new(
            "write",
            vec![MenuBinding::by_effect(apply_patch.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_WRITE_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("write"),
                        "content": {"type": "string", "description": "Content to write to the file"}
                    }),
                    &["path", "content"],
                ),
            },
            ToolProjectionLoweringV2::PiWrite,
        ),
    ]
}

/// The Codex freeform `apply_patch` envelope, shared by the Direct menu and the
/// Codex Code Mode outer surface.
/// The Codex freeform `apply_patch` menu entry, for the compositions that
/// re-raise it inside a cell.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in bound cannot be frozen.
pub fn codex_patch_envelope_entry() -> Result<MenuEntry, ToolRuntimeError> {
    let envelope = tool_name(
        CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
        "Codex patch envelope name is invalid",
    )?;
    Ok(MenuEntry::new(
        "apply_patch",
        vec![MenuBinding::exact(
            CanonicalProcessOperationV2::Start,
            envelope,
        )],
        MenuShape::CustomUtf8 {
            description: Arc::from(PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION),
            input: Utf8TextContractV1::new(CODEX_PATCH_ENVELOPE_MAX_BYTES).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "Codex patch envelope UTF-8 contract is invalid",
                    &error,
                )
            })?,
            format: ProjectedCustomToolFormatV2::lark(CODEX_APPLY_PATCH_GRAMMAR_V1).map_err(
                |error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "Codex patch envelope grammar is invalid",
                        &error,
                    )
                },
            )?,
        },
        ToolProjectionLoweringV2::CodexApplyPatch,
    ))
}

/// Every model-visible string the files pack owns.
static FILES_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "files.read_binary",
        PLAKOUS_TOOL_READ_BINARY,
        "622c5d46b0083489e29bb3dce008f3c89d1604a5be8398713dff8f2d047c11d3",
    ),
    PromptPin::new(
        "files.read_file",
        PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
        "f95b2f3663ceed6a005e0b3d37899fff9c0d18d2b300b1d121677c879d27bdd9",
    ),
    PromptPin::new(
        "files.apply_patch",
        PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
        "cb25774962dfede33bb342df9f1ad13ba32a06bd975b6d6610eccc021e6c62d9",
    ),
    PromptPin::new(
        "files.inspect_image",
        PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
        "ae28b85d0568f2003cce69f9f1599fc5845a58f5c825138495002d5f783ae7f2",
    ),
    PromptPin::new(
        "files.codex.apply_patch",
        PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION,
        "eb737a50bb07bf1278f2c4f89f28e45afadac8ce1a0b25629a5c48de4e895096",
    ),
    PromptPin::new(
        "files.grok.read_file",
        PLAKOUS_GROK_READ_DESCRIPTION,
        "78bd813467dc5832c582fd2e249ff0ea15457b6541b82dbe8b5397ff221ae181",
    ),
    PromptPin::new(
        "files.grok.search_replace",
        PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION,
        "eb2fc3d2bb93a3fb2022c78bb646b7f7c9a4f44455abbece931ed2c7041e76b9",
    ),
    PromptPin::new(
        "files.grok.write",
        PLAKOUS_GROK_WRITE_DESCRIPTION,
        "702c05152d0d708e25c97b1f89528dfa5afa20b58882e6a4d77ee4fc7550b34b",
    ),
    PromptPin::new(
        "files.pi.read",
        PI_READ_DESCRIPTION,
        "c460b305783eb51b6fbcdb96cab67b76d45d1974f416917bceb8c18c76bce810",
    ),
    PromptPin::new(
        "files.pi.edit",
        PI_EDIT_DESCRIPTION,
        "409f9e32af7d38aac3a09b4f4303953cf9821bdb02ab81c2eca43c68ef4e536d",
    ),
    PromptPin::new(
        "files.pi.write",
        PI_WRITE_DESCRIPTION,
        "c7d18538f7549a0cea0b1f47ae7309066be2a56c8217530eba35efdb45e94971",
    ),
    PromptPin::new(
        "files.claude.Read",
        CLAUDE_READ_DESCRIPTION,
        "d406130f15c7bbd6908b7e74d4e564b063babbce65e4aa6c35602f186e377b13",
    ),
    PromptPin::new(
        "files.claude.Edit",
        CLAUDE_EDIT_DESCRIPTION,
        "9ec37949f13550d1f740faca439dea0d378bad2d61eb64ce6d5ec64c9e14960e",
    ),
    PromptPin::new(
        "files.claude.Write",
        CLAUDE_WRITE_DESCRIPTION,
        "8af8c3181d6045cc674cdbb772ff58478b7119c6eba56023746cf068927a54ea",
    ),
];
