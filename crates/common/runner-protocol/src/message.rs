//! The frames each end sends.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{LineOwner, VersionTriple};

/// Operation verbs. Payload schemas belong to the execution environment.
pub mod verbs {
    /// Read one bounded slice of a workspace file.
    pub const READ_WORKSPACE: &str = "workspace.read";
    /// Run one bounded repository query.
    pub const QUERY_REPOSITORY: &str = "repository.query";
    /// Preflight and publish one workspace mutation transaction.
    pub const MUTATE_WORKSPACE: &str = "workspace.mutate";
    /// Run one literal process to completion.
    pub const EXECUTE_LITERAL_PROCESS: &str = "process.execute";
    /// Start one process session on the owner's line.
    pub const PROCESS_START: &str = "process.start";
    /// Write to a session's stdin.
    pub const PROCESS_WRITE: &str = "process.write";
    /// Read a session's output from a cursor, waiting for some.
    pub const PROCESS_POLL: &str = "process.poll";
    /// Stop one session.
    pub const PROCESS_STOP: &str = "process.stop";
    /// Promote a foreground session to a job of the line.
    pub const PROCESS_PROMOTE: &str = "process.promote";
    /// The line's jobs.
    pub const JOBS_SNAPSHOT: &str = "jobs.snapshot";
    /// Stop every job of the line.
    pub const JOBS_STOP: &str = "jobs.stop";
    /// Disown the line's leftover processes.
    pub const JOBS_DISOWN: &str = "jobs.disown";
    /// Follow the line's job events; the result names the stream that
    /// carries them, one JSON line per change.
    pub const JOBS_WATCH: &str = "jobs.watch";
    /// Spawn a service whose stdio is a stream; the result names the stream
    /// in its `stream` field.
    pub const SERVICE_SPAWN: &str = "service.spawn";
    /// Terminate a spawned service.
    pub const SERVICE_TERMINATE: &str = "service.terminate";
}

/// Capabilities a [`Hello`] may advertise. A host sends a verb or a frame
/// family only when the runner advertised it; anything else is answered
/// [`Refusal::Unsupported`].
pub mod capabilities {
    /// The four request/response workspace and process verbs.
    pub const WORKSPACE_VERBS: &str = "workspace-verbs";
    /// Process sessions and job control on the owner's line, and services
    /// whose stdio is a credit-paced stream.
    pub const STREAMS: &str = "streams";
}

/// The runner's first frame on a connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// The runner's version triple; the host binds only an exact match.
    pub version: VersionTriple,
    /// The runner build's binary id: evidence, never compared for binding.
    pub binary_id: Option<String>,
    /// The runner's stable identity, as its home records it.
    pub runner_id: String,
    /// This runner process: a new instance is a restart, and the host treats
    /// every request it had pending on the old one as of unknown outcome.
    pub instance: String,
    /// The machine boot the runner process belongs to.
    pub boot_id: String,
    /// The execution backend descriptor the runner's operations present.
    pub descriptor: Value,
    /// What the runner implements (see [`capabilities`]).
    pub capabilities: Vec<String>,
    /// The runner's `PATH`, fresh authority for its processes.
    pub path: Option<String>,
    /// The runner's `HOME`, fresh authority for its processes.
    pub home: Option<String>,
    /// How many jobs the runner keeps running at once across hosts.
    pub job_bound: u32,
}

/// The host's answer to [`Hello`]: who it is, and the version it speaks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    /// The host's stable identity.
    pub host_id: String,
    /// This host process.
    pub host_instance: String,
    /// The host's version triple; it must equal the runner's.
    pub version: VersionTriple,
}

/// How a request is keyed (spec 01 §8.4).
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestClass {
    /// One tool call's effect: keyed by the owner's line, the call and its
    /// attempt, and run at most once per key.
    EffectCall {
        /// The tool call.
        call_id: String,
        /// The call's attempt.
        attempt_id: String,
    },
    /// A read the host makes for itself (a probe, a build read): keyed by its
    /// request id alone, and never recorded.
    BuildRead,
    /// An observation of one job of the owner's line from a cursor:
    /// idempotent, so never recorded.
    ProcessObservation {
        /// The job's number within the line.
        job: u32,
        /// The output position to observe from.
        cursor: u64,
    },
    /// A stop, disown or other control of the owner's line's processes: keyed
    /// by the line and the control id, under the line's owner.
    OwnerControl {
        /// Unique per line.
        control_id: String,
    },
}

/// One operation: a verb and its payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// See [`verbs`].
    pub verb: String,
    /// The verb's request, in the execution environment's schema.
    pub payload: Value,
}

/// One request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Unique per host connection lifetime; answers name it.
    pub request_id: String,
    /// How the request is keyed.
    pub class: RequestClass,
    /// The line head the request runs for. An effect call names one.
    pub owner: Option<LineOwner>,
    /// What remains of the caller's deadline when sent, in milliseconds.
    /// Durations only: the two ends share no clock.
    pub deadline_ms: Option<u64>,
    /// Lowercase hex SHA-256 of the canonical JSON of `operation`: the same
    /// key with another digest is a different request.
    pub digest: String,
    /// The host resends a request it may already have sent: the runner then
    /// answers a key it never saw with [`RunnerFrame::NotSeen`] and runs
    /// nothing.
    pub replay: bool,
    /// What to do.
    pub operation: Operation,
}

/// An owner change of one line, sent after the new owner's lease is granted
/// and before its first request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferOwner {
    /// Unique per host connection lifetime; the answer names it.
    pub control_id: String,
    /// The owner the host last transferred to, if any.
    pub from: Option<LineOwner>,
    /// The new owner: it must be the line's accepted owner or a newer one.
    /// Sending the accepted owner again is accepted, so a lost
    /// acknowledgement can be retried.
    pub to: LineOwner,
}

/// One frame of a multiplexed stream: a long-lived process's output, its
/// input, or the credit that paces either direction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamFrame {
    /// The stream, as the request that opened it was answered.
    pub stream: u64,
    /// Per-stream sequence number, from zero, gap-free.
    pub seq: u64,
    /// What happened on the stream.
    pub event: StreamEvent,
}

/// One stream event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamEvent {
    /// A chunk of data in the stream's own schema.
    Data {
        /// The chunk.
        payload: Value,
    },
    /// The receiver may accept this many more bytes.
    Credit {
        /// The granted bytes.
        bytes: u64,
    },
    /// The stream ended.
    Close {
        /// Why, when it did not end normally.
        reason: Option<String>,
    },
}

/// A backend error from an operation that ran, or was refused by the backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    /// The execution environment's error, in its own schema.
    pub error: Value,
    /// Whether the runner proved the request had no effect.
    pub proven_no_effect: bool,
}

/// Why the runner refused a frame without running anything.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum Refusal {
    /// A request arrived before [`Bind`].
    Unbound,
    /// The host's [`Bind`] named another version triple.
    VersionMismatch {
        /// The runner's own triple.
        runner: VersionTriple,
    },
    /// The runner does not implement this verb or frame.
    Unsupported {
        /// The verb or frame kind.
        what: String,
    },
    /// The request's owner is newer than the line's accepted owner and was
    /// never transferred.
    OwnerNotTransferred,
    /// An effect call named no owner, or a frame was otherwise unusable.
    Malformed {
        /// What was wrong.
        detail: String,
    },
}

/// Frames the host sends.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "frames are encoded at once, never stored"
)]
pub enum HostFrame {
    /// The answer to [`Hello`].
    Bind(Bind),
    /// One request.
    Request(Request),
    /// Cancels a running request; its answer still comes.
    Cancel {
        /// The request.
        request_id: String,
    },
    /// Asks a running request to yield; its answer still comes.
    Preempt {
        /// The request.
        request_id: String,
        /// The yield, in the execution environment's schema.
        value: Value,
    },
    /// The host recorded the request's answer; the runner may forget it.
    Ack {
        /// The request.
        request_id: String,
    },
    /// An owner change.
    TransferOwner(TransferOwner),
    /// Liveness.
    Ping {
        /// Echoed in [`RunnerFrame::Pong`].
        nonce: u64,
    },
    /// A stream frame.
    Stream(StreamFrame),
}

/// Frames the runner sends.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "frames are encoded at once, never stored"
)]
pub enum RunnerFrame {
    /// The runner's first frame.
    Hello(Hello),
    /// The request ran to a result.
    Result {
        /// The request.
        request_id: String,
        /// The result, in the execution environment's schema.
        payload: Value,
    },
    /// The request ran, or was refused by the backend, with this error.
    Error {
        /// The request.
        request_id: String,
        /// The error.
        error: WireError,
    },
    /// The same key and digest is still running; its answer comes once.
    InProgress {
        /// The request.
        request_id: String,
    },
    /// A replayed key the runner never saw: nothing ran, and nothing under
    /// that key will.
    NotSeen {
        /// The request.
        request_id: String,
    },
    /// The same key arrived with another digest; nothing ran.
    StaleRequest {
        /// The request.
        request_id: String,
    },
    /// The key ran and its answer was acknowledged under another request:
    /// this request's outcome is unknown to the runner now.
    Spent {
        /// The request.
        request_id: String,
    },
    /// The request's or transfer's owner is older than the line's accepted
    /// owner; nothing ran.
    StaleOwner {
        /// The refused request, if a request.
        request_id: Option<String>,
        /// The refused transfer, if a transfer.
        control_id: Option<String>,
        /// The owner the runner accepts for the line.
        current: LineOwner,
    },
    /// The owner change is recorded.
    TransferAccepted {
        /// The transfer.
        control_id: String,
    },
    /// The frame was refused without running anything.
    Refused {
        /// The refused request, if a request.
        request_id: Option<String>,
        /// Why.
        refusal: Refusal,
    },
    /// The answer to [`HostFrame::Ping`].
    Pong {
        /// The ping's nonce.
        nonce: u64,
    },
    /// A stream frame.
    Stream(StreamFrame),
}
