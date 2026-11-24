use plakous_host_protocol::views::transcript::{RowKey, TranscriptScope};
use plakous_host_protocol::wire::{AttachmentId, FramedTranscriptRow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Opaque stable position before a whole turn, scoped to session and line
/// head. Appends and same-key re-exec do not invalidate it; prune does.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct HistoryCursor(pub String);

/// A retained viewport, never valid across host generations or nodes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetainedRevision {
    pub host_generation: u64,
    pub revision: u64,
    /// Host-allocated token for the exact scope, line head and viewport.
    pub token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachTranscript {
    /// Replace from a full newest page even if a retained revision was sent.
    FirstPage,
    /// Apply only when the client still holds this exact starting revision.
    Replay {
        from: RetainedRevision,
        to: RetainedRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub attachment: AttachmentId,
    pub scope: TranscriptScope,
    pub revision: RetainedRevision,
    pub part: u32,
    pub last: bool,
    /// Ascending transcript order within this logical page.
    pub items: Vec<PageItem>,
    pub has_older: bool,
    pub older_rows: u64,
    /// UTF-8 body bytes in all rows strictly before the logical page.
    pub older_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_cursor: Option<HistoryCursor>,
    /// Lengths of live rows on this part, in UTF-8 bytes. All parts together
    /// give the lengths of every live row at the baseline boundary.
    pub live_rows: Vec<LiveRowLength>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageItem {
    pub key: RowKey,
    pub row: FramedTranscriptRow,
    /// Omitted UTF-8 body bytes. Read the untruncated body by key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LiveRowLength {
    pub row: RowKey,
    pub len: u64,
}

/// Assistant or reasoning text at a stable row in this attachment's scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptStream {
    pub row: RowKey,
    /// Inclusive byte offset before this delta.
    pub offset: u64,
    /// Exclusive byte offset after this delta: offset + UTF-8 length of text.
    pub end: u64,
    pub text: String,
}
