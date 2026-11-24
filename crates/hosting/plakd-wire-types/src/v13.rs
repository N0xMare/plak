//! Protocol 13 client definitions. No serving path uses these types yet.

use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::views::transcript::{RowKey, TranscriptScope};
use plakous_host_protocol::wire::{
    AttachmentId, ClientDraftId, NodeAddress, RequestId, RunnerLabel,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::lifecycle::HostLifecycleTarget;

mod directory;
pub use directory::*;
mod service_tier;
pub use service_tier::*;
mod shared;
use shared::{
    SharedAnswer, SharedClientFrame, SharedHostFrame, SharedObservation, SharedOp, SharedRefusal,
};
mod transcript;
pub use transcript::*;
mod usage;
pub use usage::*;

/// Draft client version; the served wire version is still protocol 12.
pub const WIRE_VERSION: u32 = 13;
pub const PAGE_TARGET_BYTES: usize = 256 * 1024;
pub const SESSION_PREVIEW_BYTES: usize = 160;
pub const ATTACHMENT_STATUS_INTERVAL_SECONDS: u64 = 10;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ClientFrame {
    Draft(ClientFrameV13),
    Shared(Box<SharedClientFrame>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "frame", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientFrameV13 {
    Request { id: RequestId, op: Box<Op> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Op {
    Draft(OpV13),
    Shared(Box<SharedOp>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpV13 {
    Host {
        request: Box<HostRequest>,
    },
    Attach {
        node: NodeAddress,
        /// Resume only an exact retained viewport for this node in this host generation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained: Option<RetainedRevision>,
    },
    OlderHistory {
        attachment: AttachmentId,
        cursor: HistoryCursor,
    },
    ReadTranscriptRow {
        attachment: AttachmentId,
        row: RowKey,
        /// Start of the requested body slice, at a UTF-8 boundary.
        offset: u64,
    },
    AttachmentStatus {
        attachment: AttachmentId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HostFrame {
    Draft(Box<HostFrameV13>),
    Shared(Box<SharedHostFrame>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "frame", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostFrameV13 {
    Response {
        id: RequestId,
        outcome: Outcome,
    },
    Event {
        attachment: AttachmentId,
        seq: u64,
        class: ObservationClass,
        observation: Box<Observation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<ClientDraftId>,
        blobs: Vec<String>,
    },
    /// Attach first page or replacement baseline. Parts are ordered from zero.
    Snapshot(Baseline),
    /// A logical older page; parts do not advance the history cursor.
    HistoryPage {
        attachment: AttachmentId,
        id: RequestId,
        part: u32,
        last: bool,
        items: Vec<PageItem>,
        has_older: bool,
        older_rows: u64,
        older_bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        older_cursor: Option<HistoryCursor>,
    },
    AttachmentStatus {
        attachment: AttachmentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<RequestId>,
        deltas_evicted: u64,
        patches_skipped: u64,
        baselines_sent: u64,
        seq_gaps: u64,
        bytes_queued: u64,
        queue_len: u64,
    },
    /// The blob was definitely not admitted. Silence is not this outcome.
    BlobRefused {
        id: String,
        reason: BlobRefusal,
    },
    Directory {
        attachment: AttachmentId,
        seq: u64,
        snapshot: bool,
        rows: Vec<SessionRow>,
    },
    OptionsProgress {
        request_id: RequestId,
        runner: RunnerLabel,
        phase: Option<RunnerPhase>,
        row: Option<Box<RunnerOptions>>,
    },
    /// Advisory after Welcome; not a handshake result or admission verdict.
    HostLifecycle {
        host_generation: u64,
        phase: HostLifecyclePhase,
        reason: HostLifecycleReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<HostLifecycleTarget>,
        /// At send time, from the host's monotonic deadline, not wall time.
        /// Zero means elapsed or no active countdown.
        seconds_remaining: u64,
        /// Optional wall-clock deadline (RFC 3339 UTC); for display only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(extend("format" = "date-time"))]
        deadline_at: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "retry_hint"
        )]
        #[schemars(range(max = 30000))]
        retry_after_ms: Option<u32>,
    },
}

fn retry_hint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let value = Option::<u32>::deserialize(d)?;
    if value.is_some_and(|value| value > 30_000) {
        return Err(serde::de::Error::custom("retry_after_ms exceeds 30000"));
    }
    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostLifecyclePhase {
    Starting,
    Draining,
    Restarting,
    Stopping,
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostLifecycleReason {
    Startup,
    PairedUpgrade,
    CrossKeyUpgrade,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlobRefusal {
    Malformed,
    TooLarge,
    PendingLimit,
    Discarded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Box<Answer>),
    Refused(Box<RequestRefusal>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Answer {
    Draft(AnswerV13),
    Shared(Box<SharedAnswer>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "answer", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerV13 {
    Attached {
        attachment: AttachmentId,
        transcript: AttachTranscript,
    },
    HistoryStarted,
    TranscriptRow {
        row: RowKey,
        offset: u64,
        end: u64,
        text: String,
        last: bool,
    },
    AttachmentStatusRequested,
    Sessions {
        rows: Vec<SessionRow>,
    },
    Options {
        options: Box<SessionOptions>,
    },
    Host {
        response: Box<HostResponse>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestRefusal {
    Draft(RequestRefusalV13),
    Shared(SharedRefusal),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestRefusalV13 {
    /// Prune, line-head rebind, or a cursor outside the attachment's scope.
    CursorExpired,
    PlacementRefused {
        runner: RunnerLabel,
        reason: Box<PlacementReason>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Observation {
    Draft(ObservationV13),
    Shared(Box<SharedObservation>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationV13 {
    Extension(SurfaceObservation),
    TemplateReadiness(Vec<TemplateRow>),
    TranscriptStream(TranscriptStream),
    TranscriptToolStream {
        scope: TranscriptScope,
        tool: Box<plakous_host_protocol::wire::WireToolItem>,
    },
    UsageLimits(Box<UsageLimitsView>),
}
