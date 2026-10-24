use crate::text::{
    PLAKOUS_TOOL_READ_BINARY, PLAKOUS_TOOL_READ_FILE_DESCRIPTION, PLAKOUS_TOOL_READ_OFFSET_PAST_EOF,
};
use plakous_model::{ToolConcurrency, ToolDefinition, ToolEffect};
use plakous_tools::object_schema;
use plakous_tools::standard_definition;
use plakous_tools::{
    ExecutionBackendErrorKind, ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome,
    ToolRuntimeError, ToolRuntimeErrorKind, WorkspaceReadSliceRequest,
};
use plakous_tools::{MAX_STANDARD_TEXT_BYTES, READ_FILE_CAPABILITY_NAME};
use plakous_tools::{backend_runtime_error, decode_arguments, operation_control};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

use crate::relative_workspace_path;

#[derive(Clone)]
pub(super) struct ReadFileTool;

impl ReadFileTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
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
            let page_bytes = if invocation
                .exposure()
                .is_some_and(|binding| binding.exposed_name().as_str() == "read")
            {
                50 * 1024
            } else {
                READ_RESPONSE_BYTES
            };
            let leading = read_leading_lines(&context, &args, start_line, limit).await?;
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
                if selected != 0
                    && (presented > READ_PAGE_PRESENTED_BYTES
                        || page.len() + line.len() > page_bytes)
                {
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
            ToolOutcome::success_text(encoded)
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
    let leading = |bytes, complete| LeadingBytes {
        bytes,
        complete,
        skipped_lines: 0,
        dropped_partial: false,
        write_failed_after: None,
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
        let Some(next) = slice.next().cloned() else {
            return Ok(leading(bytes, true));
        };
        if newlines >= wanted_lines {
            return Ok(leading(bytes, false));
        }
        if bytes.len() as u64 >= plakous_tools::MAX_WORKSPACE_FILE_BYTES {
            return Ok(leading(bytes, false));
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{clip_long_line, presented_line_bytes, push_incomplete_line};

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
}
