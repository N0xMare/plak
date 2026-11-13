//! Independent job-only cross-build carrier. These encodings are frozen at
//! version one; native frames and opaque process payloads are not carrier types.

use serde::{Deserialize, Serialize};

/// Current endpoint version; the initial version has no version-zero speaker.
pub const VERSION: u32 = 1;
/// Body bound, checked before allocating a received frame.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum bytes returned by a read or submitted by a write.
pub const MAX_BYTES: usize = 64 * 1024;
/// Inventory page size, not a drainer-count limit.
pub const PAGE_SIZE: usize = 64;

/// Whether this build speaks the endpoint version.
#[must_use]
pub const fn admits(version: u32) -> bool {
    version == VERSION || (VERSION > 1 && version == VERSION - 1)
}

/// Bounded, nonempty identity with no controls or filesystem interpretation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl TryFrom<String> for Id {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err("invalid retention identity");
        }
        Ok(Self(value))
    }
}

impl From<Id> for String {
    fn from(value: Id) -> Self {
        value.0
    }
}

impl Id {
    /// Opaque identity text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Frozen owner-CAS vocabulary, independent of native `LineOwner`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    /// Stable agent line.
    pub line: Id,
    /// Head generation.
    pub generation: u64,
    /// Head branch.
    pub branch: Id,
    /// Head lease fence.
    pub fence: u64,
}

/// Immutable connection scope, supplied by the daemon from its native peer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    /// Endpoint version, not native protocol version.
    pub version: u32,
    /// Paired host identity.
    pub host_id: Id,
    /// Instance of that host holding its native connection.
    pub host_instance: Id,
}

/// Original process worker; never an intermediate relay or arbitrary path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// Private build-directory key.
    pub build: Id,
    /// Endpoint codec version.
    pub version: u32,
    /// Exact original daemon instance, never a PID.
    pub instance: Id,
}

/// Greeting before any scope or job data is accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// Direct worker identity.
    pub origin: Origin,
}

/// Frozen stop attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model request.
    Model,
    /// User request.
    User,
    /// Session release.
    SessionExit,
    /// Owning attempt ended.
    AttemptEnded,
    /// Original construction-bound cap expired.
    HardCap,
}

/// Original-worker outcome; unknown never authorizes a replacement start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    /// Still supervised.
    Running {},
    /// Natural terminal outcome.
    Exited {
        /// Exit code if available.
        code: Option<i32>,
    },
    /// Explicit stop.
    Stopped {
        /// Attribution.
        reason: StopReason,
    },
    /// Ownership relinquished, not proof of death.
    Released {},
    /// No exact outcome.
    OutcomeUnknown {},
}

/// Frozen process-poll outcome, distinct from summarized job status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessStatus {
    /// Still running.
    Running {},
    /// Natural exit.
    Exited {
        /// Exit code if known.
        code: Option<i32>,
    },
    /// Original worker lost.
    OutcomeUnknown {},
    /// Ownership relinquished.
    Released {},
    /// Explicit termination.
    Terminated {},
    /// Cancellation.
    Cancelled {},
    /// Original hard cap.
    TimedOut {},
    /// Original output cap.
    OutputLimit {},
    /// Supervision failure.
    Failed {},
    /// Cleanup remains uncertain.
    CleanupUnresolved {},
}

/// Frozen job snapshot; session identity remains original-worker-qualified.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Hex-encoded allocation, never a PID.
    pub session: Id,
    /// Per-line monotonic number.
    pub number: u32,
    /// Bounded presentation, never executable input.
    pub command: String,
    /// Original UTC start, milliseconds.
    pub started_at_ms: u64,
    /// Display attribution, never authority.
    pub attribution: String,
    /// Original elapsed duration; relays do not restart caps.
    pub elapsed_ms: u64,
    /// Current outcome.
    pub status: Status,
    /// Original-worker terminal/disown proof, not a claim that an unknown child died.
    pub terminal: bool,
    /// Bounded retained output tail.
    pub output: String,
    /// Original allocation generation.
    pub owner_generation: u64,
}

/// One line and direct-original provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    /// Current owner floor.
    pub owner: Owner,
    /// Original worker, never an intermediate relay.
    pub origin: Origin,
    /// Jobs in this page.
    pub jobs: Vec<Job>,
    /// Lowest unused job number, including consumed allocations.
    pub next_number: u32,
    /// Exact original worker was lost; cached facts never authorize PID adoption.
    pub lost: bool,
    /// Its original native line is permanently disabled, even when no jobs remain.
    pub native_disabled: bool,
}

/// Fixed verbs. No arbitrary verb, start, workspace effect or effect lookup.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Scope inventory without adoption.
    Lines {
        /// Position in sorted inventory.
        after: usize,
    },
    /// CAS the owner and irreversibly disable native control.
    Transfer {
        /// Observed owner.
        from: Owner,
        /// Intended successor or identical accepted retry tuple.
        to: Owner,
    },
    /// Current job page.
    JobsSnapshot {
        /// Exact accepted owner.
        owner: Owner,
        /// Page position.
        after: usize,
    },
    /// Bounded long-poll of job snapshots, not a carried service stream.
    JobsWatch {
        /// Exact accepted owner.
        owner: Owner,
        /// Last observed sequence.
        sequence: u64,
        /// At most one second.
        wait_ms: u64,
    },
    /// Raw output at an original-worker cursor.
    Read {
        /// Exact owner.
        owner: Owner,
        /// Original allocation.
        session: Id,
        /// Raw cursor.
        cursor: u64,
        /// At most `MAX_BYTES`.
        max_bytes: usize,
    },
    /// Read waiting up to one second for bytes or terminal state.
    Poll {
        /// Exact owner.
        owner: Owner,
        /// Original allocation.
        session: Id,
        /// Raw cursor.
        cursor: u64,
        /// At most `MAX_BYTES`.
        max_bytes: usize,
        /// At most one second.
        wait_ms: u64,
    },
    /// At-most-once input to an existing promoted job.
    Write {
        /// Exact owner.
        owner: Owner,
        /// Original allocation.
        session: Id,
        /// Hex bytes, bounded before decoding.
        bytes: String,
        /// Close input afterwards.
        close: bool,
    },
    /// Stop a promoted job, never an arbitrary PID.
    Stop {
        /// Exact owner.
        owner: Owner,
        /// Original allocation.
        session: Option<Id>,
        /// Attribution.
        reason: StopReason,
        /// At most five seconds.
        grace_ms: u64,
    },
    /// Relinquish a promoted job without killing it.
    Disown {
        /// Exact owner.
        owner: Owner,
    },
    /// Consume committed terminal receipts, never running jobs.
    Ack {
        /// Exact owner.
        owner: Owner,
        /// Original allocations.
        sessions: Vec<Id>,
        /// Delivered job-control reply identities to mark spent.
        controls: Vec<Id>,
    },
}

/// Existing job-control key, not a grant or an arbitrary effect identifier.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum Key {
    /// A line-owner control.
    Control {
        /// Existing control identity.
        control_id: Id,
    },
    /// A job control within a model tool call; never a start or workspace effect.
    Effect {
        /// Existing call identity.
        call_id: Id,
        /// Existing attempt identity.
        attempt_id: Id,
    },
}

/// Existing at-most-once job-control journal metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    /// Stable key from the native peer, not a fresh relay key.
    pub key: Key,
    /// Exact native operation digest.
    pub digest: Id,
    /// A recovery request can return a receipt but never dispatch an unseen key.
    pub replay: bool,
    /// Age retained by the original caller, for the existing expiry fence.
    pub age_ms: Option<u64>,
}

/// Correlation identity, not an authority grant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// Correlates one answer.
    pub id: Id,
    /// Exact original instance expected by caller.
    pub instance: Id,
    /// Required for write/stop/disown; forbidden on observations and transfer.
    pub mutation: Option<Mutation>,
    /// Closed verb set.
    pub request: Request,
}

/// Raw record without UTF-8 interpretation.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Original sequence.
    pub sequence: u64,
    /// Original byte cursor at the start of the record.
    pub cursor: u64,
    /// stdout, stderr or terminal.
    pub stream: String,
    /// Hex bytes.
    pub bytes: String,
}

/// Closed refusals; no fallback start or automatic effect replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Finish/consume/disown through a retained compatible build.
    IncompatibleVersion,
    /// Exact original worker died; never adopt remembered PIDs.
    DrainerLost,
    /// Owner or holder CAS failed.
    StaleOwner,
    /// Not a promoted job of this host's line.
    UnknownJob,
    /// Invalid bounds or encoding.
    Malformed,
    /// Same key with different operation bytes.
    ConflictingControl,
    /// The original worker cannot prove an unseen recovery key ran.
    NotSeen,
    /// Its exact answer was already acknowledged.
    Spent,
    /// Durable recording failed before dispatch.
    Persistence,
    /// Dispatched control has no exact outcome.
    OutcomeUnknown,
    /// Receipt capacity exhausted before dispatch.
    Backpressure,
    /// Foreground work or its terminal report has not been settled by the host.
    UnsettledForeground,
}

/// Frozen answers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case", deny_unknown_fields)]
pub enum Answer {
    /// Completed.
    Ok {},
    /// Accepted owner/ban, with atomically observed original-worker obligations.
    Transferred {
        /// Counter floor observed under the transfer gate.
        next_number: u32,
        /// Unconsumed original-worker jobs remain; delegated jobs are separate origins.
        has_jobs: bool,
    },
    /// Exact process-stop result.
    Stopped {
        /// Original-worker outcome.
        status: ProcessStatus,
    },
    /// Sorted inventory page.
    Lines {
        /// Line facts.
        lines: Vec<Line>,
        /// Next position if more remains.
        next: Option<usize>,
    },
    /// Job page or bounded watch result.
    Jobs {
        /// Job facts.
        jobs: Vec<Job>,
        /// Next position.
        next: Option<usize>,
        /// Snapshot sequence.
        sequence: u64,
        /// Lowest unused number.
        next_number: u32,
    },
    /// Raw output and a separate terminal outcome.
    Output {
        /// Raw records.
        records: Vec<Record>,
        /// Next raw cursor.
        next_cursor: u64,
        /// More bytes remain.
        more_output: bool,
        /// Bytes no longer retained before this cursor.
        skipped_bytes: u64,
        /// Current outcome.
        status: ProcessStatus,
    },
    /// Explicit refusal.
    Refused {
        /// Why it did not complete.
        reason: Refusal,
    },
}

/// Answer addressed to a call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    /// Original correlation/control identity.
    pub id: Id,
    /// Exact answer.
    pub answer: Answer,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("number", &self.number)
            .field("session", &self.session)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("sequence", &self.sequence)
            .field("cursor", &self.cursor)
            .field("stream", &self.stream)
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verb = match self {
            Self::Lines { .. } => "lines",
            Self::Transfer { .. } => "transfer",
            Self::JobsSnapshot { .. } => "jobs_snapshot",
            Self::JobsWatch { .. } => "jobs_watch",
            Self::Read { .. } => "read",
            Self::Poll { .. } => "poll",
            Self::Write { .. } => "write",
            Self::Stop { .. } => "stop",
            Self::Disown { .. } => "disown",
            Self::Ack { .. } => "ack",
        };
        f.debug_struct("RetentionRequest")
            .field("verb", &verb)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "frozen carrier fixtures")]
mod tests;
