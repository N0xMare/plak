//! Typed execution values: what one call produced, beside its presentation text.
//!
//! An executor may attach one [`ExecutionValue`] to its [`crate::ToolOutcome`].
//! The value exists only in transit to an in-process consumer such as a
//! local composition cell. It is never part of the model result, canonical
//! history, the ledger, checkpoints or exports, which keep the presentation
//! text alone.

use crate::{RetrievalSource, WorkspaceContentIdentity};

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
    /// Directory entries.
    Listing(ListingValue),
    /// The files one edit transaction changed.
    FileChanges(FileChangesValue),
    /// One process session observation.
    Process(ProcessValue),
    /// The plan after a replacement.
    Plan(PlanValue),
    /// A hosted retrieval's answer.
    Answer(AnswerValue),
    /// One fetched page.
    Page(PageValue),
    /// One MCP tool result.
    Mcp(McpValue),
}

/// A hosted retrieval's answer with the sources the provider reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnswerValue {
    /// The side model's answer.
    pub text: String,
    /// Sources in first-reported order, without duplicate URLs.
    pub sources: Vec<RetrievalSource>,
}

/// One fetched page, as `fetch_url` returned it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageValue {
    /// The URL requested.
    pub url: String,
    /// The URL after redirects.
    pub final_url: String,
    /// The HTTP status.
    pub status: u16,
    /// The declared content type.
    pub content_type: Option<String>,
    /// The page as text (HTML converted to Markdown); empty for an image.
    pub text: String,
    /// The image, when the page is one.
    pub image: Option<ImageValue>,
    /// Whether the body was cut at the byte cap before conversion.
    pub truncated: bool,
    /// Text bytes left out of `text` past the inline bound.
    pub omitted_bytes: u64,
    /// The issued job-output file holding the whole text, when it was kept.
    pub full_output: Option<String>,
}

/// One MCP tool result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpValue {
    /// The structured content when the server sent it, otherwise the text.
    pub content: serde_json::Value,
    /// Inline images in the result, in order.
    pub images: Vec<McpImageValue>,
}

/// One inline MCP image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpImageValue {
    /// The declared media type.
    pub media_type: String,
    /// The base64 image data as the server sent it.
    pub data: String,
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

/// Directory entries in walk order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListingValue {
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

/// The files one edit transaction changed, in op order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangesValue {
    /// One row per changed path.
    pub changes: Vec<FileChange>,
}

/// One changed path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    /// Workspace-relative path after the change.
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
    /// Moved to `path`.
    Move,
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
