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
    /// Content search matches.
    SearchHits(SearchHitsValue),
    /// Directory entries.
    Listing(ListingValue),
    /// The files one edit transaction changed.
    FileChanges(FileChangesValue),
    /// One process session observation.
    Process(ProcessValue),
    /// The plan revision after a replacement.
    Plan(PlanValue),
}

impl ExecutionValue {
    /// Retained in-memory bytes, including collection capacity and owned strings.
    /// This producer bound is separate from canonical result-part encoding.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> usize {
        let heap = match self {
            Self::FileRead(value) => value.path.capacity().saturating_add(value.text.capacity()),
            Self::SearchHits(value) => value.hits.iter().fold(
                value
                    .hits
                    .capacity()
                    .saturating_mul(size_of::<SearchHit>())
                    .saturating_add(value.query.capacity()),
                |bytes, hit| {
                    bytes
                        .saturating_add(hit.path.capacity())
                        .saturating_add(hit.text.capacity())
                },
            ),
            Self::Listing(value) => value.entries.iter().fold(
                value
                    .entries
                    .capacity()
                    .saturating_mul(size_of::<ListedEntry>())
                    .saturating_add(value.path.capacity()),
                |bytes, entry| bytes.saturating_add(entry.path.capacity()),
            ),
            Self::FileChanges(value) => value.changes.iter().fold(
                value
                    .changes
                    .capacity()
                    .saturating_mul(size_of::<FileChange>()),
                |bytes, change| bytes.saturating_add(change.path.capacity()),
            ),
            Self::Process(value) => value
                .status
                .capacity()
                .saturating_add(value.output.capacity())
                .saturating_add(value.full_output.as_ref().map_or(0, String::capacity))
                .saturating_add(value.program.as_ref().map_or(0, String::capacity)),
            Self::Plan(_) => 0,
        };
        size_of::<Self>().saturating_add(heap)
    }
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
    /// Requested lines known to be omitted by the page bound, not an exact
    /// remaining-file count. Use `eof`, `first_line`, and `lines` to resume.
    pub lines_cut: u64,
    /// Lines in the page whose tail was clipped.
    pub clipped_lines: u64,
    /// Whether invalid UTF-8 was replaced.
    pub lossy: bool,
    /// Whole-file content identity of a workspace file; `None` for a job-output file.
    pub revision: Option<WorkspaceContentIdentity>,
}

/// Content search matches in presented order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHitsValue {
    /// The query or pattern as the call named it, or for a filename-glob
    /// listing the searched path and the glob joined by `/`: its first 4096
    /// bytes, cut on a character boundary.
    pub query: String,
    /// The matches the result kept.
    pub hits: Vec<SearchHit>,
    /// Known matching lines not retained, including those outside the
    /// presented window. This counts matches, not displayed rows or files.
    pub hits_cut: u64,
    /// Whether the walk stopped before searching everything.
    pub incomplete: bool,
}

/// One matching line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    /// Path relative to the execution environment's working directory.
    pub path: String,
    /// The 1-based line number.
    pub line: u64,
    /// The line's text.
    pub text: String,
    /// Whether the repository query clipped this matching line.
    pub truncated: bool,
}

/// Directory entries in walk order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListingValue {
    /// The listed directory as the call named it.
    pub path: String,
    /// The entries the result kept.
    pub entries: Vec<ListedEntry>,
    /// Entries found beyond the ones kept.
    pub entries_cut: u64,
    /// Whether the walk stopped before visiting everything.
    pub incomplete: bool,
}

/// One directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedEntry {
    /// Path relative to the listed directory.
    pub path: String,
    /// What the entry is, without following a symbolic link.
    pub kind: ListedEntryKind,
    /// Size in bytes of a regular file; zero otherwise.
    pub size: u64,
}

/// What a directory entry is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListedEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link, not followed.
    Link,
    /// Anything else.
    Other,
}

/// The files one edit transaction changed, in canonical path order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangesValue {
    /// One row per changed path.
    pub changes: Vec<FileChange>,
}

/// One changed path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    /// Path relative to the execution environment's working directory after the change.
    pub path: String,
    /// What happened to it.
    pub action: FileChangeAction,
    /// The new whole-file content identity; `None` after a delete.
    pub revision: Option<WorkspaceContentIdentity>,
}

/// What an edit did to one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChangeAction {
    /// Created.
    Add,
    /// Changed in place.
    Update,
    /// Removed.
    Delete,
}

/// One process session observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessValue {
    /// Issued job number, including after a terminal observation.
    pub job_id: u64,
    /// Known output bytes skipped by the canonical observation window.
    /// Additional tail truncation may have no known byte count.
    pub omitted_bytes: u64,
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
    /// For a start, the program its command runs: a basename of at most 32
    /// bytes in `[A-Za-z0-9._+-]`, or `other`. Never an argument, path,
    /// assignment or credential-shaped word. `None` for any later observation.
    pub program: Option<String>,
}

/// The plan revision after a replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanValue {
    /// The plan revision.
    pub revision: u64,
}
