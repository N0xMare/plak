//! The per-session change feed (spec 01 §5.11).
//!
//! Every mutation a `SessionStore` accepts for a runtime session appends one
//! [`FeedRecord`] in the same atomic publication. The store assigns the
//! sequence and stamps `committed_at_ms`; the runtime supplies the facts, which
//! one pure function derives from the parent and successor checkpoints
//! ([`facts`]). The store never interprets them.

pub(crate) mod facts;
mod wire;

use plakous_model::{
    AttemptId, BranchId, CallId, CheckpointId, EntryId, FinishReason, HistorySegmentKind,
    ModelErrorKind, NamespaceId, SessionId, ToolName, TurnId, TurnOrigin,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session::{BranchHead, BranchRevision, SessionRevision, SessionWorkflowId};
use crate::{
    AbandonedTurnClaim, AbandonmentCause, AgentErrorKind, CommandId, ContextOverflowRemedy,
    ControlErrorKind, TurnUsage,
};

/// Largest canonical encoding of one record, the page bound. A mutation whose
/// record would exceed it is refused before commit.
pub const MAX_FEED_RECORD_BYTES: usize = 1024 * 1024;
/// Retained records per session above which the store trims the oldest.
pub const MAX_FEED_RECORDS_PER_SESSION: u64 = 262_144;
/// Retained encoded bytes per session above which the store trims the oldest.
pub const MAX_FEED_BYTES_PER_SESSION: u64 = 64 * 1024 * 1024;
/// Most records one trim transaction deletes.
pub const MAX_FEED_TRIM_BATCH: usize = 4_096;
/// Most encoded bytes one feed read returns; one record always fits.
pub const MAX_FEED_PAGE_BYTES: usize = MAX_FEED_RECORD_BYTES;

const FEED_RECORD_CODEC_VERSION: u32 = 1;
/// Longest session operation identity a fact names.
const MAX_FEED_OPERATION_ID_BYTES: usize = 128;
/// Longest presentation subject a fact carries.
const MAX_FEED_SUBJECT_BYTES: usize = 240;

/// The position of one record in its session's feed.
///
/// Sequences start at 1 and are gap-free per runtime session across every
/// branch. [`FeedSeq::ZERO`] is the cursor before the first record and the
/// tail of a feed that has none.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct FeedSeq(u64);

impl FeedSeq {
    /// The cursor before the first record.
    pub const ZERO: Self = Self(0);

    /// Reconstructs a sequence or cursor.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric sequence.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next sequence, or `None` on exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// One exact checkpoint address inside a runtime session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FeedLocator {
    /// Runtime session.
    pub session_id: SessionId,
    /// Branch.
    pub branch_id: BranchId,
    /// Checkpoint on that branch.
    pub checkpoint_id: CheckpointId,
}

/// Whether an advanced head may resume ordinarily or is recovery-only.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedHeadKind {
    /// The head is quiescent.
    Quiescent,
    /// The head carries unresolved work.
    Working,
}

/// How a new branch was published.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedBranchTransition {
    /// An ordinary or recompiling fork.
    Fork,
    /// A rebind onto a new binding.
    Rebind,
}

/// The authority change one record's mutation made.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedChange {
    /// A create or seeded create published the genesis checkpoint.
    Created {
        /// Genesis checkpoint.
        genesis: CheckpointId,
        /// Its branch revision.
        #[serde(with = "branch_revision")]
        revision: BranchRevision,
    },
    /// A commit advanced the branch head.
    Advanced {
        /// The head the commit replaced.
        parent: CheckpointId,
        /// The new head.
        checkpoint: CheckpointId,
        /// The new branch revision.
        #[serde(with = "branch_revision")]
        revision: BranchRevision,
        /// The new head's kind.
        head: FeedHeadKind,
    },
    /// A fork, recompiling fork or rebind published this record's branch.
    Branched {
        /// The source head, in the same session.
        source: FeedLocator,
        /// The target branch's genesis checkpoint.
        target: CheckpointId,
        /// The session topology revision after publication.
        #[serde(with = "session_revision")]
        revision: SessionRevision,
        /// Fork or rebind.
        transition: FeedBranchTransition,
    },
    /// A lease release or abandonment released this branch.
    LeaseReleased {
        /// The branch head at release.
        head: CheckpointId,
        /// The released fence.
        fence: u64,
    },
    /// History pruning reclaimed checkpoints of this branch.
    Pruned {
        /// The branch head the prune ran at.
        head: CheckpointId,
        /// The new pruning horizon.
        #[serde(with = "branch_revision")]
        horizon: BranchRevision,
    },
    /// The session was archived. The record names the session's initial
    /// branch and applies to every branch.
    Archived {
        /// The archived session revision.
        #[serde(with = "session_revision")]
        revision: SessionRevision,
    },
}

/// Header of one record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeedHeader {
    /// Runtime session.
    pub session_id: SessionId,
    /// Sequence in the session's feed.
    pub seq: FeedSeq,
    /// Branch the change applies to.
    pub branch_id: BranchId,
    /// The store's wall clock at commit, Unix milliseconds. Observational
    /// only: never ordering or authority.
    pub committed_at_ms: u64,
    /// The change the mutation made.
    pub change: FeedChange,
}

/// The command classes a fact names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedCommandKind {
    /// A root turn admission.
    Root {
        /// Who initiated the turn.
        #[serde(with = "wire::turn_origin")]
        origin: TurnOrigin,
    },
    /// Steering for an active turn.
    Steer,
    /// A cancellation request.
    Cancel,
    /// An effect-decision resolution.
    EffectDecision,
    /// A shutdown request.
    Shutdown,
}

/// Who issued an effect, as the committed call block and the ledger say.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedCallerLineage {
    /// The model called the tool directly.
    Direct,
    /// A provider-hosted program called the tool.
    ProviderProgrammatic {
        /// The provider-neutral hosted-program kind.
        kind: NamespaceId,
        /// The provider execution that emitted the call.
        execution_id: String,
    },
    /// A local Code Cell called the tool.
    LocalProgrammatic {
        /// The owning Code Cell operation.
        evaluation_operation_id: u64,
        /// The Code Cell's outer marker call, while the ledger retains its
        /// evaluation record.
        parent_call_id: Option<CallId>,
        /// The evaluator cell that emitted the call.
        cell_id: String,
        /// The cell's acceptance sequence for this call.
        request_sequence: u32,
        /// The local-composition recursion depth.
        depth: u16,
    },
}

/// The operation classes a fact names.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedOperationKind {
    /// One physical provider attempt.
    ProviderAttempt {
        /// Attempt identity.
        attempt_id: AttemptId,
    },
    /// One tool effect.
    Effect {
        /// Tool-call identity.
        call_id: CallId,
        /// The tool name the model saw.
        exposed_name: ToolName,
        /// Who issued the call.
        lineage: FeedCallerLineage,
        /// The bounded, redacted presentation subject a live `ToolStarted`
        /// carries, when one exists.
        subject: Option<String>,
    },
    /// One process session.
    ProcessSession {
        /// Its job number, once it was promoted to a job.
        job_number: Option<u32>,
    },
    /// One Code Cell evaluation.
    CodeCell {
        /// The outer call identity.
        call_id: CallId,
        /// The tool name the model saw for the outer call.
        exposed_name: ToolName,
        /// The outer call's presentation subject, derived as an effect's.
        subject: Option<String>,
    },
    /// One compaction workflow.
    Compaction {
        /// Derived workflow identity.
        #[serde(with = "wire::workflow_id")]
        workflow_id: SessionWorkflowId,
    },
}

/// The operation states a fact reports.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedOperationState {
    /// The dispatch fence was crossed.
    Dispatched,
    /// A typed outcome is proven.
    Completed,
    /// The physical outcome is unknown.
    OutcomeUnknown,
    /// Evidence proves no dispatch.
    ProvenNotDispatched,
    /// Proven evidence was folded into checkpoint authority.
    Finalized,
}

/// A `ContextLimit` failure's shape.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct FeedContextLimit {
    /// The turn's prompt was withheld from history.
    pub withheld: bool,
    /// What a host can offer.
    #[serde(with = "wire::overflow_remedy")]
    pub remedy: ContextOverflowRemedy,
}

/// Why a turn was cancelled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedCancelCause {
    /// A caller cancelled the live turn.
    Requested,
    /// A newly opened owner abandoned the unfinished turn.
    Abandoned {
        /// The strongest claim the closure could honestly make.
        #[serde(with = "wire::abandoned_claim")]
        claim: AbandonedTurnClaim,
        /// How the turn came to be unfinished.
        #[serde(with = "wire::abandonment_cause")]
        cause: AbandonmentCause,
        /// A model round of the turn completed before it was lost.
        model_round_completed: bool,
        /// A tool round of the turn completed before it was lost.
        tool_round_completed: bool,
    },
}

/// A root turn's terminal outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedTurnOutcome {
    /// The turn completed.
    Completed {
        /// Its finish reason (never `Refusal`).
        #[serde(with = "wire::finish_reason")]
        finish: FinishReason,
    },
    /// The model refused the request.
    Refused,
    /// The turn failed.
    Failed {
        /// Agent error class.
        #[serde(with = "wire::agent_error_kind")]
        kind: AgentErrorKind,
        /// Model error class, when the failure has one.
        #[serde(with = "wire::model_error_kind::option")]
        model_kind: Option<ModelErrorKind>,
        /// Shape of a `ContextLimit` (or protected-history) failure.
        context: Option<FeedContextLimit>,
    },
    /// The turn was cancelled.
    Cancelled {
        /// Why.
        cause: FeedCancelCause,
    },
    /// The turn ended recovery-required.
    RecoveryRequired,
}

/// Which compaction pass produced a semantic successor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedCompactPass {
    /// The provider's native compaction.
    Native,
    /// A paid harness summary.
    Summary,
    /// Offline tool-result placeholders.
    Placeholders,
    /// Offline replay omission.
    ReplayOmission,
}

/// The settled outcome of one workflow.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedWorkflowOutcome {
    /// A compaction published its semantic successor.
    Compacted {
        /// The compacted source.
        source: FeedLocator,
        /// The successor.
        target: FeedLocator,
        /// The pass that produced it.
        pass: FeedCompactPass,
        /// Source segments the successor replaced or rewrote.
        selected_segments: u64,
        /// The independently measured input tokens before the pass.
        tokens_before: u64,
        /// The independently measured input tokens of the successor.
        tokens_after: u64,
    },
    /// A compaction found nothing it could replace.
    NoEligibleRange,
    /// A live compaction finished without a semantic replacement.
    Failed {
        /// Its summary request may have been sent: false only when
        /// not-dispatched evidence proves it never was.
        summary_sent: bool,
        /// The recorded failure's agent error class.
        #[serde(with = "wire::agent_error_kind")]
        kind: AgentErrorKind,
        /// Its model error class, when it has one.
        #[serde(with = "wire::model_error_kind::option")]
        model_kind: Option<ModelErrorKind>,
    },
    /// A closure commit settled an admitted workflow whose owner was lost.
    Abandoned {
        /// Exact evidence proves its summary request was never sent.
        summary_unsent: bool,
    },
    /// A fork or rebind published its target.
    Published {
        /// The published target.
        target: FeedLocator,
    },
}

/// Why a compaction workflow was admitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedCompactReason {
    /// A host asked the session compaction endpoint.
    Endpoint,
    /// A turn's own fence folded its history.
    TurnFence,
}

/// The workflow classes a fact names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedWorkflowKind {
    /// A compaction.
    Compact {
        /// Why it was admitted.
        reason: FeedCompactReason,
    },
    /// A fork.
    Fork,
    /// A rebind.
    Rebind,
}

/// One neutral statement about what a mutation made durable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedFact {
    /// The session was created.
    SessionCreated {
        /// The create operation identity.
        operation_id: String,
        /// The genesis was seeded from portable history.
        seeded: bool,
    },
    /// A command was recorded as accepted.
    CommandAccepted {
        /// Caller command identity.
        command_id: CommandId,
        /// Command class.
        kind: FeedCommandKind,
        /// Bound root turn.
        turn_id: Option<TurnId>,
        /// Driver command sequence.
        sequence: u64,
        /// The command operation the same commit recorded for it (a steer's
        /// text address for `feed_call`), when there is one.
        operation_id: Option<u64>,
    },
    /// A command was recorded as rejected.
    CommandRejected {
        /// Caller command identity.
        command_id: CommandId,
        /// Command class.
        kind: FeedCommandKind,
        /// The recorded typed refusal.
        #[serde(with = "wire::control_error_kind")]
        error: ControlErrorKind,
    },
    /// One history segment holding new entries was appended.
    HistoryAppended {
        /// Owning turn, when turn-owned.
        turn_id: Option<TurnId>,
        /// Segment kind.
        #[serde(with = "wire::segment_kind")]
        segment: HistorySegmentKind,
        /// Ordinal of its first item in the checkpoint's canonical history.
        first_ordinal: u64,
        /// Its item count.
        count: u64,
        /// Entry identity of its first item.
        first_entry: EntryId,
        /// Entry identity of its last item.
        last_entry: EntryId,
        /// Call ids of the tool results it commits, in order.
        tool_results: Vec<CallId>,
    },
    /// Canonical history rewrote entries it already held in place (a
    /// placeholder or replay-omission pass). One fact per commit.
    HistoryRewritten {
        /// Owning turn, when turn-owned.
        turn_id: Option<TurnId>,
        /// Ordinal of the first rewritten item in the checkpoint's history.
        first_ordinal: u64,
        /// Items from `first_ordinal` through the last rewritten item.
        count: u64,
    },
    /// An operation reached a reported state.
    OperationAdvanced {
        /// Operation identity.
        operation_id: u64,
        /// Owning turn, when turn-owned.
        turn_id: Option<TurnId>,
        /// Operation class.
        kind: FeedOperationKind,
        /// The state it reached.
        state: FeedOperationState,
    },
    /// A root turn's terminal was committed.
    TurnTerminal {
        /// Root turn.
        turn_id: TurnId,
        /// Root command identity.
        command_id: CommandId,
        /// Outcome summary.
        outcome: FeedTurnOutcome,
        /// The terminal committed a closed tool exchange.
        closed_exchange: bool,
        /// Queued steering was settled unapplied.
        steering_settled_unapplied: bool,
        /// The turn's exact usage.
        usage: TurnUsage,
    },
    /// A workflow was admitted.
    WorkflowAdmitted {
        /// Derived workflow identity.
        #[serde(with = "wire::workflow_id")]
        workflow_id: SessionWorkflowId,
        /// The request's operation identity; `None` only for a turn's own
        /// fold.
        operation_id: Option<String>,
        /// Workflow class.
        kind: FeedWorkflowKind,
    },
    /// A workflow was settled.
    WorkflowSettled {
        /// Derived workflow identity.
        #[serde(with = "wire::workflow_id")]
        workflow_id: SessionWorkflowId,
        /// The request's operation identity; `None` only for a turn's own
        /// fold.
        operation_id: Option<String>,
        /// Workflow class.
        kind: FeedWorkflowKind,
        /// How it settled.
        outcome: FeedWorkflowOutcome,
    },
}

/// One record: a header and the ordered facts of its mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeedRecord {
    header: FeedHeader,
    facts: Vec<FeedFact>,
}

impl FeedRecord {
    /// Creates a backend-assembled record.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(header: FeedHeader, facts: Vec<FeedFact>) -> Self {
        Self { header, facts }
    }

    /// Returns the header.
    #[must_use]
    pub const fn header(&self) -> &FeedHeader {
        &self.header
    }

    /// Returns the ordered facts.
    #[must_use]
    pub fn facts(&self) -> &[FeedFact] {
        &self.facts
    }

    /// Returns the canonical encoding, refusing one above
    /// [`MAX_FEED_RECORD_BYTES`].
    ///
    /// # Errors
    ///
    /// Returns [`FeedCodecError::RecordTooLarge`] above the bound.
    pub fn encode(&self) -> Result<Vec<u8>, FeedCodecError> {
        let bytes = serde_json::to_vec(&FeedRecordEnvelope {
            version: FEED_RECORD_CODEC_VERSION,
            record: self,
        })
        .map_err(|_| FeedCodecError::Malformed)?;
        if bytes.len() > MAX_FEED_RECORD_BYTES {
            return Err(FeedCodecError::RecordTooLarge { bytes: bytes.len() });
        }
        Ok(bytes)
    }

    /// Decodes one canonical encoding.
    ///
    /// # Errors
    ///
    /// Refuses another codec version, malformed or non-canonical bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FeedCodecError> {
        if bytes.len() > MAX_FEED_RECORD_BYTES {
            return Err(FeedCodecError::RecordTooLarge { bytes: bytes.len() });
        }
        let envelope: FeedRecordEnvelope<Self> =
            serde_json::from_slice(bytes).map_err(|_| FeedCodecError::Malformed)?;
        if envelope.version != FEED_RECORD_CODEC_VERSION {
            return Err(FeedCodecError::UnsupportedVersion(envelope.version));
        }
        if envelope.record.encode()? != bytes {
            return Err(FeedCodecError::Noncanonical);
        }
        if !envelope.record.within_writer_bounds() {
            return Err(FeedCodecError::Malformed);
        }
        Ok(envelope.record)
    }

    /// Whether every field is one this build's writer could have produced.
    fn within_writer_bounds(&self) -> bool {
        self.header.seq.get() >= 1
            && self.facts.iter().all(|fact| match fact {
                FeedFact::SessionCreated { operation_id, .. } => valid_operation_id(operation_id),
                FeedFact::WorkflowAdmitted { operation_id, .. }
                | FeedFact::WorkflowSettled { operation_id, .. } => {
                    operation_id.as_deref().is_none_or(valid_operation_id)
                }
                FeedFact::OperationAdvanced {
                    kind:
                        FeedOperationKind::Effect { subject, .. }
                        | FeedOperationKind::CodeCell { subject, .. },
                    ..
                } => subject.as_deref().is_none_or(valid_subject),
                _ => true,
            })
    }
}

/// A session operation identity as the runtime admits one.
fn valid_operation_id(operation_id: &str) -> bool {
    !operation_id.is_empty()
        && operation_id.len() <= MAX_FEED_OPERATION_ID_BYTES
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// A presentation subject as the runtime bounds one: one line of at most
/// [`MAX_FEED_SUBJECT_BYTES`].
fn valid_subject(subject: &str) -> bool {
    subject.len() <= MAX_FEED_SUBJECT_BYTES && !subject.contains(['\n', '\r'])
}

#[derive(Deserialize, Serialize)]
struct FeedRecordEnvelope<R> {
    version: u32,
    record: R,
}

/// A feed record encoding failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FeedCodecError {
    /// The encoding exceeds [`MAX_FEED_RECORD_BYTES`].
    #[error("feed record of {bytes} bytes exceeds its bound")]
    RecordTooLarge {
        /// Encoded size.
        bytes: usize,
    },
    /// The bytes do not decode.
    #[error("feed record encoding is malformed")]
    Malformed,
    /// The bytes decode but are not the canonical encoding.
    #[error("feed record encoding is not canonical")]
    Noncanonical,
    /// Another codec version.
    #[error("feed record codec version {0} is unsupported")]
    UnsupportedVersion(u32),
}

/// The retained window of one session's feed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedTail {
    retained_from: FeedSeq,
    tail: FeedSeq,
}

impl FeedTail {
    /// Creates a backend-observed window.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(retained_from: FeedSeq, tail: FeedSeq) -> Self {
        Self {
            retained_from,
            tail,
        }
    }

    /// The oldest retained sequence (`tail + 1` when nothing is retained).
    #[must_use]
    pub const fn retained_from(self) -> FeedSeq {
        self.retained_from
    }

    /// The newest assigned sequence.
    #[must_use]
    pub const fn tail(self) -> FeedSeq {
        self.tail
    }
}

/// One bounded read of a session's feed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedPage {
    /// Records after the cursor, oldest first, with the window observed in the
    /// same read. Empty at the tail.
    Records {
        /// The records.
        records: Vec<FeedRecord>,
        /// The window when the page was read.
        window: FeedTail,
    },
    /// The cursor is below the retained window; nothing older is recoverable.
    BehindRetention {
        /// The window when the page was read.
        window: FeedTail,
        /// The session's current branch heads.
        heads: Vec<BranchHead>,
    },
}

mod branch_revision {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::session::BranchRevision;

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` contract passes the field by reference"
    )]
    pub(super) fn serialize<S: Serializer>(
        revision: &BranchRevision,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(revision.get())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BranchRevision, D::Error> {
        u64::deserialize(deserializer).map(BranchRevision::from_store)
    }
}

mod session_revision {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::session::SessionRevision;

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` contract passes the field by reference"
    )]
    pub(super) fn serialize<S: Serializer>(
        revision: &SessionRevision,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(revision.get())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SessionRevision, D::Error> {
        u64::deserialize(deserializer).map(SessionRevision::from_store)
    }
}

#[cfg(test)]
mod tests;
