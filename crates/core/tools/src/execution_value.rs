//! Typed execution values: what one call produced, beside its presentation text.
//!
//! An executor may attach one [`ExecutionValue`] to its [`crate::ToolOutcome`].
//! The value exists only in transit to an in-process consumer such as a
//! local composition cell. It is never part of the model result, canonical
//! history, the ledger, checkpoints or exports, which keep the presentation
//! text alone.

use crate::WorkspaceContentIdentity;

/// One typed execution value.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionValue {
    /// One page of a text file or an issued job-output file.
    FileRead(FileReadValue),
    /// One image file.
    Image(ImageValue),
    /// Regex search matches.
    SearchHits(SearchHitsValue),
    /// One process session observation.
    Process(ProcessValue),
    /// The plan after a replacement.
    Plan(PlanValue),
}

/// One page of a file, as the read returned it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileReadValue {
    /// The path as the call named it.
    pub path: String,
    /// The page's text.
    pub text: String,
    /// The 1-based line the page starts at.
    pub first_line: u64,
    /// Lines in the page.
    pub lines: u64,
    /// Whether the page reaches the end of the file.
    pub eof: bool,
    /// Lines the call asked for that the page bound left out.
    pub lines_cut: u64,
    /// Lines in the page whose tail was clipped.
    pub clipped_lines: u64,
    /// Whether invalid UTF-8 was replaced.
    pub lossy: bool,
    /// Whole-file content identity of a workspace file; `None` for a job-output file.
    pub revision: Option<WorkspaceContentIdentity>,
}

/// One image file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageValue {
    /// The path as the call named it.
    pub path: String,
    /// The sniffed media type.
    pub media_type: String,
    /// The image bytes.
    pub bytes: Vec<u8>,
    /// Width and height, when the header states them.
    pub dimensions: Option<(u32, u32)>,
}

/// Regex search matches in walk order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHitsValue {
    /// The matches the result kept.
    pub hits: Vec<SearchHit>,
    /// Matches found beyond the ones kept.
    pub hits_cut: u64,
    /// Whether the walk stopped before searching everything.
    pub incomplete: bool,
}

/// One matching line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    /// Workspace-relative path.
    pub path: String,
    /// The 1-based line number.
    pub line: u64,
    /// The line's text.
    pub text: String,
}

/// One process session observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessValue {
    /// The canonical status token (`running`, `exited`, `timed_out`, ...).
    pub status: String,
    /// The exit code, when the process exited.
    pub exit_code: Option<i32>,
    /// The session id while the process runs.
    pub session_id: Option<u64>,
    /// Output since the previous observation, without the full-output line.
    pub output: String,
    /// Whether output was omitted from `output`.
    pub truncated: bool,
    /// The issued job-output file holding the full output, when there is one.
    pub full_output: Option<String>,
}

/// The plan after a replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanValue {
    /// The plan revision.
    pub revision: u64,
}
