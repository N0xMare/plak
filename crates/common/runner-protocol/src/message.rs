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

/// Diagnostic report of a foreground process stopped when authority ended.
/// This is not a job terminal and authorizes no process operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedForeground {
    /// Tool call and attempt key, or `unkeyed` for a host probe.
    pub call_key: String,
    /// Former leader PID, diagnostic only.
    pub pid: u32,
    /// How the terminate path joined it.
    pub ended: String,
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
    /// An inspecting connection reads the runner's line table and pings,
    /// and nothing else: it owns no line, so it needs no pairing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inspect: bool,
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
    /// One step of a tree operation (a mint, seal, adopt, unmint, refresh
    /// or pin): keyed by the tree and the operation id the host recorded
    /// for it, run at most once per key, and fenced by the tree's custody on
    /// the host rather than by a line owner.
    TreeOperation {
        /// The tree the operation belongs to.
        tree_id: String,
        /// The operation's id, unique per tree.
        operation_id: String,
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
    /// On a replay, how long ago the host first claimed the request. A
    /// runner keeps a key's answer for 24 hours only, so a replay at least
    /// that old is answered [`RunnerFrame::Expired`]: its key may have run
    /// and been forgotten, and an unseen key proves nothing any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
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
    /// The owner the host knows the runner accepts for the line, or none
    /// when it knows of none. A transfer to a newer owner is accepted only
    /// from the accepted owner, a compare-and-swap; a stale `from` is
    /// answered [`RunnerFrame::StaleOwner`] naming the accepted owner, from
    /// which a host holding the newer lease may transfer again.
    pub from: Option<LineOwner>,
    /// The new owner: it must be the line's accepted owner or a newer one.
    /// Sending the accepted owner again is accepted whatever `from` says, so
    /// a lost acknowledgement can be retried.
    pub to: LineOwner,
}

/// One line a runner holds processes for, as [`RunnerFrame::Lines`] lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineStatus {
    /// The host the line belongs to.
    pub host_id: String,
    /// The line.
    pub line: String,
    /// The owner the runner accepts for the line, if any transferred.
    pub owner: Option<LineOwner>,
    /// Whether the line's owner holds it now: false once its host's owner
    /// lease expired with no connection, until a newer owner of the same
    /// host takes it over. Such a line's processes are orphans.
    pub adopted: bool,
    /// The line's jobs, as `jobs.snapshot` answers them, in the execution
    /// environment's schema.
    pub jobs: Value,
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
    /// never transferred, or is the accepted owner of a line orphaned since:
    /// transfer it first.
    OwnerNotTransferred,
    /// The host's identity is not on the runner's allowlist: pair it on
    /// the runner first.
    NotPaired {
        /// The host's identity.
        host_id: String,
    },
    /// An inspecting connection sent a frame other than a line-table
    /// question or a ping.
    InspectOnly,
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
    /// Asks for the runner's line table: this host's lines, or every
    /// host's on an inspecting connection.
    Lines {
        /// Names the answer.
        request_id: String,
    },
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
        /// Foreground processes stopped after their owner lost authority,
        /// diagnostic only: the next full open recovers their tool calls.
        stopped: Vec<StoppedForeground>,
    },
    /// The owner change could not be recorded; the line's owner is
    /// unchanged.
    TransferRefused {
        /// The transfer.
        control_id: String,
        /// Why.
        detail: String,
    },
    /// A replay at least as old as the runner's 24-hour retention: the
    /// runner may have run its key and forgotten it, so its outcome is
    /// unknown. Nothing ran now.
    Expired {
        /// The request.
        request_id: String,
    },
    /// The answer to [`HostFrame::Lines`].
    Lines {
        /// The question.
        request_id: String,
        /// The lines, in line order.
        lines: Vec<LineStatus>,
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
