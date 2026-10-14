#![allow(
    missing_docs,
    reason = "unpublished versioned process-test protocol has self-describing field names"
)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the parent/child JSON-lines protocol and durable evidence rows.
pub const PROTOCOL_VERSION: u16 = 6;

/// A public or honestly split crash-boundary vocabulary recognized by the harness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashBoundary {
    /// Run to normal completion without pausing.
    None,
    /// Exact old quiescent checkpoint after facade release.
    GenesisReleased,
    /// First loopback provider dispatch admission.
    ProviderDispatchOne,
    /// First loopback provider terminal evidence.
    ProviderOutcomeOne,
    /// Mutating loopback effect dispatch.
    EffectDispatch,
    /// Mutating loopback effect outcome.
    EffectOutcome,
    /// Second loopback provider dispatch admission.
    ProviderDispatchTwo,
    /// Provider success before the private checkpoint publication interval.
    ProviderOutcomeTwo,
    /// Exact new quiescent checkpoint after facade release.
    CheckpointReleased,
    /// Local-composition evaluation was durably admitted for dispatch.
    CompositionDispatch,
    /// The external composition host reported that the cell started.
    CompositionCellStarted,
    /// One nested child crossed its external dispatch boundary.
    CompositionChildDispatch,
    /// One nested child produced physical outcome evidence.
    CompositionChildOutcome,
    /// The host yielded a live cell back to the driver.
    CompositionYielded,
    /// A wait for the yielded cell crossed its external dispatch boundary.
    CompositionWaitDispatch,
    /// The host produced the cell's terminal response.
    CompositionCellTerminal,
    /// Termination and host-session shutdown completed their joined cleanup.
    CompositionJoined,
}

impl CrashBoundary {
    /// Returns the evidence strength available without exposing runtime internals.
    #[must_use]
    pub const fn evidence_kind(self) -> BoundaryEvidenceKind {
        match self {
            Self::ProviderOutcomeTwo
            | Self::CompositionDispatch
            | Self::CompositionCellStarted
            | Self::CompositionChildDispatch
            | Self::CompositionChildOutcome
            | Self::CompositionYielded
            | Self::CompositionWaitDispatch
            | Self::CompositionCellTerminal
            | Self::CompositionJoined => BoundaryEvidenceKind::HonestSplit,
            Self::None
            | Self::GenesisReleased
            | Self::ProviderDispatchOne
            | Self::ProviderOutcomeOne
            | Self::EffectDispatch
            | Self::EffectOutcome
            | Self::ProviderDispatchTwo
            | Self::CheckpointReleased => BoundaryEvidenceKind::ExactPublic,
        }
    }
}

/// Whether a cut is one exact public seam or brackets an inaccessible private fence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryEvidenceKind {
    /// The cut follows an exact facade/model/tool boundary.
    ExactPublic,
    /// The cut is the observable provider side of a private publication interval.
    HonestSplit,
}

/// One command sent to a `--child` process as a JSON line.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Create/reopen a facade session and execute the loopback turn.
    Run {
        /// Protocol schema version.
        version: u16,
        /// `SQLite` database path.
        database: PathBuf,
        /// Stable workspace identity used by exact runtime revalidation.
        workspace: PathBuf,
        /// Fsynced physical provider/effect evidence path.
        evidence: PathBuf,
        /// Fsynced last-known public session locator path.
        state: PathBuf,
        /// Boundary at which the child remains pending for `SIGKILL`.
        cut: CrashBoundary,
    },
    /// Reopen and classify the exact branch without dispatching new work.
    Probe {
        /// Protocol schema version.
        version: u16,
        /// `SQLite` database path.
        database: PathBuf,
        /// Stable workspace identity used by exact runtime revalidation.
        workspace: PathBuf,
        /// Existing physical evidence path, used only by inert test doubles.
        evidence: PathBuf,
        /// Fsynced last-known public session locator path.
        state: PathBuf,
    },
}

impl ControlCommand {
    /// Validates the exact protocol revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the command names an unsupported protocol schema.
    pub fn validate_version(&self) -> Result<(), &'static str> {
        let version = match self {
            Self::Run { version, .. } | Self::Probe { version, .. } => *version,
        };
        (version == PROTOCOL_VERSION)
            .then_some(())
            .ok_or("unsupported control protocol version")
    }
}

/// One versioned JSON-line reply from a child process.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChildReply {
    /// The command was decoded and admitted.
    Started {
        /// Protocol schema version.
        version: u16,
    },
    /// Fsynced evidence reached a named crash boundary.
    Boundary {
        /// Protocol schema version.
        version: u16,
        /// Reached boundary.
        boundary: CrashBoundary,
        /// Exact or split classification.
        evidence_kind: BoundaryEvidenceKind,
        /// Monotonic durable evidence sequence.
        evidence_sequence: u64,
    },
    /// A no-cut run completed and released its exact checkpoint.
    Finished {
        /// Protocol schema version.
        version: u16,
        /// Final public locator.
        locator: String,
    },
    /// A read-only probe classified the persisted branch.
    Probed {
        /// Protocol schema version.
        version: u16,
        /// Closed facade-visible state class.
        report: ProbeReport,
    },
    /// The child failed with a bounded, redaction-safe diagnostic.
    Failed {
        /// Protocol schema version.
        version: u16,
        /// Failure text.
        message: String,
    },
}

/// Read-only exact persisted-state classification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProbeReport {
    /// Exact quiescent state before the root user turn.
    OldExact {
        /// Immutable source checkpoint recorded before the root turn.
        source: String,
        /// Exact public locator.
        locator: String,
        /// Canonical item count, necessarily zero for this fixture.
        canonical_items: usize,
    },
    /// Exact quiescent state after the complete provider/tool/provider turn.
    NewExact {
        /// Immutable source checkpoint recorded before the root turn.
        source: String,
        /// Exact public locator.
        locator: String,
        /// Canonical item count, necessarily four for this fixture.
        canonical_items: usize,
        /// Final canonical assistant text.
        final_text: String,
    },
    /// A turn a lost process left, closed by the reopen that found it.
    ///
    /// The session is ordinary again, and the history says what was given up:
    /// an unfinished model request whose outcome nobody observed. Distinct from
    /// `OldExact` because something did happen, and from `NewExact` because the
    /// turn never produced an answer.
    RecoveredByAbandonment {
        /// Immutable source checkpoint recorded before the root turn.
        source: String,
        /// Exact public locator after the abandonment was committed.
        locator: String,
        /// Canonical item count: the prompt, plus the one note.
        canonical_items: usize,
        /// The provider attempts whose outcome was never observed.
        abandoned_operations: Vec<u64>,
        /// The canonical note the abandonment left in the history.
        note: String,
    },
    /// Persisted working authority that forbids ordinary resume/redispatch.
    RecoveryRequired {
        /// Immutable quiescent source checkpoint.
        source: String,
        /// Exact recovery-only locator.
        locator: String,
        /// Closed public recovery phase.
        phase: RecoveryPhase,
        /// Exact unresolved operation/action reports.
        obligations: Vec<RecoveryFact>,
        /// Public workflows proven unable to bypass this Working checkpoint.
        blocked: Vec<BlockedWorkingWorkflow>,
    },
}

/// Closed typed projection of the public Plakous recovery phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    /// Provider/command dispatch evidence is unresolved.
    DispatchEvidence,
    /// Approval/tool-effect evidence is unresolved.
    EffectReconciliation,
    /// Exact checkpoint commit acknowledgement is unresolved.
    CommitRecovery,
    /// Proven outcomes await terminal folding.
    Finalization,
}

/// Closed public workflows that must not escape a Working checkpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedWorkingWorkflow {
    /// Explicit semantic recovery remained recovery-only.
    Recover,
    /// Context status returned the same recovery report.
    ContextStatus,
    /// Compaction returned the same recovery report.
    Compact,
    /// Fork returned the same recovery report.
    Fork,
    /// Rebind returned the same recovery report.
    Rebind,
}

/// Redaction-safe projection of one exact recovery obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryFact {
    /// Persisted operation identity.
    pub operation_id: u64,
    /// Closed public recovery action.
    pub action: RecoveryAction,
}

/// Closed typed projection of the public Plakous recovery action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// Reconcile one exact commit operation.
    CommitRecovery,
    /// Obtain provider-owned evidence for the exact attempt.
    ProviderEvidenceRequired,
    /// Obtain executor-owned evidence for the exact effect.
    EffectEvidenceRequired,
    /// Abandon one possibly-started local composition without replay.
    EvaluationAbandonmentRequired,
    /// Terminalize only with exact proof of non-dispatch.
    ProvenNotDispatched,
    /// Fold an already proven outcome.
    FinalizationRequired,
}
