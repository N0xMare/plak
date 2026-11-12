use super::{nullable, required};
use serde::{Deserialize, Serialize};

/// A saved workspace, never a caller-selected runner or absolute pathname.
#[derive(
    Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTarget {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Exact bytes. Rendering and incremental UTF-8 decoding belong to the client.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileContent {
    pub revision: String,
    pub size: u64,
    pub offset: u64,
    pub bytes_b64: String,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<String>")]
    pub next: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileMatch {
    pub path: String,
    pub line: u64,
    pub column: u64,
    pub bytes_b64: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStopReason {
    TreeEntries,
    Files,
    Matches,
    ScannedBytes,
    ResultBytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileQuerySummary {
    pub skipped_entries: u64,
    pub skipped_binary: u64,
    pub skipped_oversize: u64,
    pub skipped_changed: u64,
    pub truncated: bool,
    #[serde(deserialize_with = "required")]
    #[schemars(required, schema_with = "nullable::<FileStopReason>")]
    pub stop_reason: Option<FileStopReason>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeEntry {
    pub entry: String,
    pub path: String,
    pub index: String,
    pub worktree: String,
    pub untracked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangesStatus {
    pub revision: String,
    pub base: String,
    pub git_view: bool,
    pub entries: Vec<ChangeEntry>,
    pub truncated: bool,
}

/// File and VCS failures remain product responses, not generic transport refusals.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceReadFailure {
    InvalidPath,
    ProtectedPath,
    NotFound,
    NotRegularFile,
    PermissionDenied,
    FileTooLarge,
    ContentChanged,
    InvalidCursor,
    Deadline,
    Io,
    NotARepository,
    VcsUnavailable,
    VcsUnsupported,
    TooLarge,
    ChangesConflict,
}
