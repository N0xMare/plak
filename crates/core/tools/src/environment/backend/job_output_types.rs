//! Job output file limits, issued-file values and their typed refusals.

use std::time::Duration;

use super::{ExecutionBackendError, ExecutionBackendErrorKind, backend_error};

/// Content bytes one job output file keeps before it stops spooling.
pub const MAX_JOB_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Line that ends a job output file which reached [`MAX_JOB_OUTPUT_FILE_BYTES`].
pub const JOB_OUTPUT_TRUNCATED_LINE: &str = "[output truncated at 64 MiB]";
/// Time a job output file is kept after its job's notice was consumed.
pub const JOB_OUTPUT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Age (since the last write or consumption) after which the first open of a
/// jobs root deletes any job output file, consumed or not.
pub const JOB_OUTPUT_STALE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Bytes all job output files under one root may occupy when a file is
/// created, counting the new file at its full bound.
pub const JOB_OUTPUT_ROOT_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Line reported wherever an issued file is named or read after spooling to
/// it failed; `written` is the content bytes the file holds before the failure.
#[must_use]
pub fn job_output_incomplete_line(written: u64) -> String {
    format!("[output file incomplete: write failed after {written} bytes]")
}

/// The typed refusal for an issued file that no longer exists.
#[must_use]
pub fn job_output_file_lost(path: &str) -> ExecutionBackendError {
    refusal(ExecutionBackendErrorKind::NotFound, path)
}

/// The issued output file of one promoted job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobOutputFile {
    /// The exact path the process tools print and the read exception admits.
    pub path: String,
    /// Content bytes the file holds when spooling to it failed.
    pub write_failed_after: Option<u64>,
}

/// Bytes read from one issued job output file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobOutputSlice {
    /// Bytes starting at the requested offset.
    pub bytes: Vec<u8>,
    /// Whether the read reached the current end of the file. A read never
    /// goes past the content bound plus its truncation line; a file that
    /// extends beyond that is never complete.
    pub complete: bool,
    /// Content bytes the file holds when spooling to it failed.
    pub write_failed_after: Option<u64>,
}

/// Typed, model-visible refusal for one issued path.
pub(super) fn refusal(kind: ExecutionBackendErrorKind, path: &str) -> ExecutionBackendError {
    let reason = match kind {
        ExecutionBackendErrorKind::NotFound => "no longer exists",
        ExecutionBackendErrorKind::SymlinkEscape => "crosses a symlink; refusing to read it",
        ExecutionBackendErrorKind::IdentityMismatch => {
            "was replaced, relinked or changed owner; refusing to read it"
        }
        ExecutionBackendErrorKind::Denied => {
            "is in a directory that is not private to this user; refusing to read it"
        }
        _ => "could not be read",
    };
    backend_error(kind, format!("job output file {path} {reason}"))
}
