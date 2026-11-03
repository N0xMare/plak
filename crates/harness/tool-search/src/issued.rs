//! The job-output search exception.
//!
//! Admission is [`plakous_tools::ExecutionEnvironment::read_job_output`], the
//! same check a read uses. A spelling that check does not recognise stays under
//! workspace containment. An admitted file is streamed in slices up to the
//! job-output read bound and searched as one text file; the jobs root is never
//! walked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use plakous_tools::{
    AdmittedTextQuery, AdmittedTextSearch, ExecutionBackendError, ExecutionBackendErrorKind,
    JobOutputSlice, MAX_STANDARD_TEXT_BYTES, RepositoryMatchCase, RepositoryQueryLimits,
    RepositoryQueryMatch, RepositoryQueryResult, ToolContext, ToolRuntimeError,
    ToolRuntimeErrorKind, backend_runtime_error, job_output_file_lost, job_output_runtime_error,
    operation_context, search_admitted_text,
};

/// Searches `path` when it is a job-output spelling.
///
/// `Ok(None)` means the path is not under the jobs root, so the caller keeps
/// ordinary containment. A refused spelling (another agent, an earlier process,
/// a lost or replaced file) is the same error a read of that path returns.
pub(crate) async fn search_issued(
    context: &ToolContext,
    path: &Path,
    pattern: Option<&str>,
    regex: bool,
    match_case: RepositoryMatchCase,
    glob: Option<&str>,
    limits: RepositoryQueryLimits,
) -> Result<Option<RepositoryQueryResult>, ToolRuntimeError> {
    let Some(issued) = path.to_str() else {
        return Ok(None);
    };
    let Some(first) = context
        .environment()
        .read_job_output(issued, 0, MAX_STANDARD_TEXT_BYTES)
    else {
        return Ok(None);
    };
    let mut slice = first.await.map_err(job_output_runtime_error)?;
    let control = operation_context(context);
    let query = AdmittedTextQuery {
        path,
        pattern,
        regex,
        match_case,
        glob,
        limits,
    };
    let mut scan = match search_admitted_text(&control, query).map_err(search_error)? {
        AdmittedTextSearch::Done(result) => return Ok(Some(result)),
        AdmittedTextSearch::Scan(scan) => scan,
    };
    let mut offset = 0_u64;
    loop {
        offset = offset.saturating_add(slice.bytes.len() as u64);
        let more = scan.push(&control, &slice.bytes).map_err(search_error)?;
        if !more || slice.complete || slice.bytes.is_empty() {
            break;
        }
        slice = next_slice(context, issued, offset).await?;
    }
    scan.finish(&control).map(Some).map_err(search_error)
}

/// The lines within `before` and `after` of each hit in an admitted file,
/// keyed by zero-based index and read again in slices. A line keeps no
/// terminator and at most a few bytes past the per-row bound, enough for the
/// row to show that it was cut; an index past the end is absent.
pub(crate) async fn lines_around_hits(
    context: &ToolContext,
    path: &Path,
    hits: &[RepositoryQueryMatch],
    before: usize,
    after: usize,
) -> Result<BTreeMap<usize, String>, ToolRuntimeError> {
    let wanted = hits
        .iter()
        .flat_map(|hit| {
            let index = usize::try_from(hit.line().saturating_sub(1)).unwrap_or(usize::MAX);
            index.saturating_sub(before)..=index.saturating_add(after)
        })
        .collect::<BTreeSet<_>>();
    let mut lines = BTreeMap::new();
    let (Some(issued), Some(&last)) = (path.to_str(), wanted.last()) else {
        return Ok(lines);
    };
    // A UTF-8 character cut at the cap ends past the bound the row keeps.
    let cap = crate::STANDARD_SEARCH_MATCH_BYTES + 4;
    let keep = |partial: &mut Vec<u8>, bytes: &[u8]| {
        let room = cap.saturating_sub(partial.len());
        partial.extend_from_slice(&bytes[..bytes.len().min(room)]);
    };
    let mut index = 0_usize;
    let mut partial = Vec::new();
    let mut offset = 0_u64;
    loop {
        let slice = next_slice(context, issued, offset).await?;
        offset = offset.saturating_add(slice.bytes.len() as u64);
        let mut rest = slice.bytes.as_slice();
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            if wanted.contains(&index) {
                keep(&mut partial, &rest[..end]);
                let line = if partial.len() < cap {
                    partial.strip_suffix(b"\r").unwrap_or(&partial)
                } else {
                    &partial
                };
                lines.insert(index, String::from_utf8_lossy(line).into_owned());
            }
            partial.clear();
            index += 1;
            if index > last {
                return Ok(lines);
            }
            rest = &rest[end + 1..];
        }
        if wanted.contains(&index) {
            keep(&mut partial, rest);
        }
        if slice.complete || slice.bytes.is_empty() {
            break;
        }
    }
    if !partial.is_empty() {
        lines.insert(index, String::from_utf8_lossy(&partial).into_owned());
    }
    Ok(lines)
}

async fn next_slice(
    context: &ToolContext,
    issued: &str,
    offset: u64,
) -> Result<JobOutputSlice, ToolRuntimeError> {
    context.check_cancelled()?;
    context
        .environment()
        .read_job_output(issued, offset, MAX_STANDARD_TEXT_BYTES)
        .ok_or_else(|| job_output_runtime_error(job_output_file_lost(issued)))?
        .await
        .map_err(job_output_runtime_error)
}

/// Maps a matcher failure. Invalid patterns stay argument errors; everything
/// else keeps the backend's kind.
fn search_error(error: ExecutionBackendError) -> ToolRuntimeError {
    if error.kind() == ExecutionBackendErrorKind::InvalidRequest {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            error.message().as_str(),
        )
    } else {
        backend_runtime_error(error)
    }
}
