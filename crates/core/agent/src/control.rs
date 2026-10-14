//! Closed caller command vocabulary and exact-turn control capabilities.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use plakous_model::{
    AgentId, AttemptId, BranchId, CallId, CheckpointId, ContextStatus, Digest, EventId,
    SafeDiagnostic, SessionId, TurnId,
};
use plakous_tools::{EffectDecisionAuditId, ToolPolicyDenial};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

const SESSION_ENDPOINT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

use crate::feed::FeedSeq;
use crate::runtime::{AgentBuilder, Turn, TurnResultHandle, TurnShared};

/// Caller-supplied idempotency identity for one state-changing agent command.
#[derive(Clone, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CommandId(String);

impl CommandId {
    /// Creates a bounded redaction-safe command identity.
    ///
    /// # Errors
    ///
    /// Returns invalid input unless the identity is 1–128 portable ASCII
    /// identifier characters.
    pub fn new(value: impl Into<String>) -> Result<Self, ControlError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(ControlError::new(
                ControlErrorKind::InvalidInput,
                "command identity is not a bounded portable identifier",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the portable identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn transient(
        agent_id: AgentId,
        nonce: &AtomicU64,
        operation: &str,
    ) -> Result<Self, ControlError> {
        let sequence = nonce.fetch_add(1, Ordering::Relaxed);
        if sequence == u64::MAX {
            return Err(ControlError::new(
                ControlErrorKind::Backpressure,
                "transient command identity space is exhausted",
            ));
        }
        Self::new(format!("transient:{agent_id}:{operation}:{sequence}"))
    }
}

impl fmt::Debug for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("CommandId").field(&self.0).finish()
    }
}

/// Operator decision carried by an exact decision-resolution command.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EffectDecisionChoice {
    /// Allow one exact binding under an application audit identity.
    Allow(EffectDecisionAuditId),
    /// Deny one exact binding without tool dispatch.
    Deny(ToolPolicyDenial),
}

/// Closed public command vocabulary accepted by the private runtime owner.
#[derive(Clone)]
pub enum TurnCommand {
    /// Admit a host-authored corrective turn, not operator input.
    StartInstruction {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Canonical instruction text.
        text: String,
    },
    /// Admit a new root turn.
    Start {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Canonical user text.
        text: String,
    },
    /// Admit a later root turn behind the named active turn.
    FollowUp {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Exact active root targeted by the handoff.
        target_turn_id: TurnId,
        /// Canonical user text.
        text: String,
    },
    /// Queue steering for an exact active turn's next safe boundary.
    Steer {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Exact active root.
        turn_id: TurnId,
        /// Canonical steering text.
        text: String,
    },
    /// Resolve one exact pending tool decision.
    ResolveEffectDecision {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Exact pending decision address.
        reference: EffectDecisionRef,
        /// Operator decision.
        decision: EffectDecisionChoice,
    },
    /// Request joined cancellation of an exact admitted turn.
    Cancel {
        /// Stable caller idempotency identity.
        command_id: CommandId,
        /// Exact queued or active root.
        turn_id: TurnId,
    },
    /// Close admission and join all runtime-owned work.
    Shutdown {
        /// Stable caller idempotency identity.
        command_id: CommandId,
    },
}

impl TurnCommand {
    /// Returns the caller idempotency identity.
    #[must_use]
    pub const fn command_id(&self) -> &CommandId {
        match self {
            Self::Start { command_id, .. }
            | Self::StartInstruction { command_id, .. }
            | Self::FollowUp { command_id, .. }
            | Self::Steer { command_id, .. }
            | Self::ResolveEffectDecision { command_id, .. }
            | Self::Cancel { command_id, .. }
            | Self::Shutdown { command_id } => command_id,
        }
    }

    /// Returns the exact root identity addressed by this command, when any.
    #[must_use]
    pub const fn target_turn_id(&self) -> Option<TurnId> {
        match self {
            Self::FollowUp { target_turn_id, .. } => Some(*target_turn_id),
            Self::Steer { turn_id, .. } | Self::Cancel { turn_id, .. } => Some(*turn_id),
            Self::ResolveEffectDecision { reference, .. } => Some(reference.turn_id()),
            Self::Start { .. } | Self::StartInstruction { .. } | Self::Shutdown { .. } => None,
        }
    }
}

impl fmt::Debug for TurnCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct(match self {
            Self::Start { .. } => "Start",
            Self::StartInstruction { .. } => "StartInstruction",
            Self::FollowUp { .. } => "FollowUp",
            Self::Steer { .. } => "Steer",
            Self::ResolveEffectDecision { .. } => "ResolveEffectDecision",
            Self::Cancel { .. } => "Cancel",
            Self::Shutdown { .. } => "Shutdown",
        });
        debug.field("command_id", self.command_id());
        if let Some(turn_id) = self.target_turn_id() {
            debug.field("turn_id", &turn_id);
        }
        debug.finish_non_exhaustive()
    }
}

/// Stable acknowledgement disposition for a caller-identified command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandAckDisposition {
    /// This submission was classified and admitted.
    Accepted,
    /// Exact canonical content under this identity was already classified.
    Duplicate,
    /// The command was classified without changing runtime semantic state.
    Rejected,
}

/// One stable acknowledgement of command classification, distinct from result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandAck {
    command_id: CommandId,
    disposition: CommandAckDisposition,
    sequence: u64,
    turn_id: Option<TurnId>,
    rejection: Option<ControlError>,
    feed_seq: Option<FeedSeq>,
}

impl CommandAck {
    pub(crate) const fn accepted(
        command_id: CommandId,
        sequence: u64,
        turn_id: Option<TurnId>,
    ) -> Self {
        Self {
            command_id,
            disposition: CommandAckDisposition::Accepted,
            sequence,
            turn_id,
            rejection: None,
            feed_seq: None,
        }
    }

    pub(crate) const fn rejected(
        command_id: CommandId,
        sequence: u64,
        turn_id: Option<TurnId>,
        rejection: ControlError,
    ) -> Self {
        Self {
            command_id,
            disposition: CommandAckDisposition::Rejected,
            sequence,
            turn_id,
            rejection: Some(rejection),
            feed_seq: None,
        }
    }

    pub(crate) fn duplicate(command_id: CommandId, original: &Self) -> Self {
        Self {
            command_id,
            disposition: CommandAckDisposition::Duplicate,
            sequence: original.sequence,
            turn_id: original.turn_id,
            rejection: original.rejection.clone(),
            feed_seq: original.feed_seq,
        }
    }

    /// Names the feed record that made this classification durable.
    pub(crate) const fn with_feed_seq(mut self, feed_seq: Option<FeedSeq>) -> Self {
        self.feed_seq = feed_seq;
        self
    }

    /// Returns the exact caller identity.
    #[must_use]
    pub const fn command_id(&self) -> &CommandId {
        &self.command_id
    }

    /// Returns accepted, duplicate, or rejected disposition.
    #[must_use]
    pub const fn disposition(&self) -> CommandAckDisposition {
        self.disposition
    }

    /// Returns the stable actor classification sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the admitted or targeted root identity, when applicable.
    #[must_use]
    pub const fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    /// Returns the stable redaction-safe rejection, including for a duplicate
    /// of an originally rejected command.
    #[must_use]
    pub const fn rejection(&self) -> Option<&ControlError> {
        self.rejection.as_ref()
    }

    /// Returns the sequence of the change-feed record that made this
    /// classification durable; a duplicate names the original's record.
    /// `None` for a classification nothing committed (an unrecorded refusal,
    /// a runtime without a durable store) and for a duplicate answered from
    /// a classification rebuilt at open, whose record this owner never saw.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        self.feed_seq
    }

    /// Reports whether the original command classification was accepted.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        self.rejection.is_none()
            && matches!(
                self.disposition,
                CommandAckDisposition::Accepted | CommandAckDisposition::Duplicate
            )
    }
}

/// Exact public address of one pending tool decision.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct EffectDecisionRef {
    event_id: EventId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    binding_digest: Digest,
}

impl EffectDecisionRef {
    pub(crate) const fn new(
        event_id: EventId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        call_id: CallId,
        binding_digest: Digest,
    ) -> Self {
        Self {
            event_id,
            turn_id,
            attempt_id,
            call_id,
            binding_digest,
        }
    }

    /// Returns the event which first described this pending decision.
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    /// Returns the exact logical turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the model attempt which requested the tool.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the canonical tool-call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the authoritative binding digest displayed with this decision.
    #[must_use]
    pub const fn binding_digest(&self) -> Digest {
        self.binding_digest
    }
}

/// Stable category for a command rejection or ingress failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ControlErrorKind {
    /// The bounded command ingress is currently full.
    Backpressure,
    /// The actor command ledger cannot admit more work without recovery.
    LedgerFull,
    /// The actor has stopped accepting commands.
    Shutdown,
    /// The command addresses a turn owned by another capability.
    ForeignTurn,
    /// The named turn or decision has left the applicable phase.
    Stale,
    /// The exact authorization binding does not match the pending request.
    WrongEffectDecision,
    /// The exact pending decision expired before this decision was applied.
    Expired,
    /// The exact pending decision was removed by cancellation.
    Cancelled,
    /// Input is valid only for another runtime phase.
    WrongPhase,
    /// A bounded actor-owned queue has no remaining capacity.
    QueueFull,
    /// Command input failed canonical validation.
    InvalidInput,
    /// A command identity was reused with different canonical content.
    IdentityConflict,
    /// The private actor rejected an inconsistent transition.
    Internal,
    /// The session store has no ordinary headroom left for new work; the
    /// remaining reserve is kept for settlement and recovery.
    StoragePressure,
    /// Durable admission was definitely rejected without recording the prompt.
    AdmissionRejected,
    /// Durable admission may have committed and requires exact reconciliation.
    OutcomeUnknown,
    /// Retention reclaimed the command's recorded result or all evidence of
    /// its original admission sequence. Its exact binding is still checked.
    Reclaimed,
}

/// Redaction-safe command-plane error.
#[derive(Clone, Eq, PartialEq)]
pub struct ControlError {
    kind: ControlErrorKind,
    message: SafeDiagnostic,
    /// The refusal behind a `StoragePressure` rejection; never persisted,
    /// because that rejection is never recorded.
    capacity: Option<crate::session::SessionStoreCapacity>,
    source: Option<Box<crate::AgentError>>,
}

impl ControlError {
    pub(crate) fn new(kind: ControlErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message),
            capacity: None,
            source: None,
        }
    }

    pub(crate) fn storage_pressure(capacity: crate::session::SessionStoreCapacity) -> Self {
        Self {
            capacity: Some(capacity),
            source: Some(Box::new(crate::AgentError::from_store(
                crate::session::SessionStoreError::Capacity(capacity),
            ))),
            ..Self::new(
                ControlErrorKind::StoragePressure,
                format!(
                    "new work was refused under storage pressure: {}",
                    crate::session::SessionStoreError::Capacity(capacity)
                ),
            )
        }
    }

    pub(crate) const fn capacity(&self) -> Option<crate::session::SessionStoreCapacity> {
        self.capacity
    }

    pub(crate) fn admission_rejected(error: crate::AgentError) -> Self {
        let mut rejection = Self::new(
            ControlErrorKind::AdmissionRejected,
            format!("prompt was not admitted: {}", error.message()),
        );
        rejection.source = Some(Box::new(error));
        rejection
    }

    pub(crate) fn outcome_unknown(error: crate::AgentError) -> Self {
        let mut rejection = Self::new(ControlErrorKind::OutcomeUnknown, error.message());
        rejection.source = Some(Box::new(error));
        rejection
    }

    pub(crate) fn agent_source(&self) -> Option<&crate::AgentError> {
        self.source.as_deref()
    }

    /// Returns the stable rejection category.
    #[must_use]
    pub const fn kind(&self) -> ControlErrorKind {
        self.kind
    }

    /// Returns a bounded redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        self.message.as_str()
    }
}

impl fmt::Debug for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("capacity", &self.capacity)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl Error for ControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|error| error as &dyn Error)
    }
}

/// Actor acknowledgement for exact-turn cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CancellationStatus {
    /// The request transitioned queued or active work toward cancellation.
    Requested,
    /// Cancellation had already been requested for this exact live turn.
    AlreadyRequested,
    /// The exact turn was already terminal when classified.
    AlreadyTerminal,
}

/// Actor acknowledgement for a binding-addressed decision decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EffectDecisionStatus {
    /// The actor minted a one-shot dispatch grant for the exact pending request.
    Allowed,
    /// The actor denied the exact pending request without dispatch.
    Denied,
}

/// Actor acknowledgement for bounded steering admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SteeringStatus {
    /// The steering input is queued for the next safe model boundary.
    Queued {
        /// One-based position in the active turn's steering queue.
        position: usize,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum ActorCommandEffect {
    Root,
    Cancel(CancellationStatus),
    EffectDecision(EffectDecisionStatus),
    Steer(SteeringStatus),
    Shutdown,
    Rejected,
}

pub(crate) struct ActorCommandReply {
    pub(crate) acknowledgement: CommandAck,
    pub(crate) turn: Option<Turn>,
    pub(crate) result: Option<TurnResultHandle>,
    pub(crate) effect: ActorCommandEffect,
}

#[derive(Clone)]
pub(crate) struct AgentContextStatusRequest {
    pub(crate) session_id: SessionId,
    pub(crate) branch_id: BranchId,
    pub(crate) checkpoint_id: CheckpointId,
    pub(crate) pending_user_input: Option<String>,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentCompactionReason {
    Manual,
    Scheduled,
}

#[doc(hidden)]
pub struct AgentCompactSessionRequest {
    pub(crate) operation_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) branch_id: BranchId,
    pub(crate) checkpoint_id: CheckpointId,
    pub(crate) reason: AgentCompactionReason,
    pub(crate) pending_user_input: Option<String>,
    pub(crate) runtime: Option<AgentBuilder>,
}

impl AgentCompactSessionRequest {
    /// Constructs one exact driver-owned workflow request.
    #[doc(hidden)]
    #[must_use]
    pub fn new(
        operation_id: String,
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
        reason: AgentCompactionReason,
        runtime: AgentBuilder,
    ) -> Self {
        Self::with_pending_user_input(
            operation_id,
            session_id,
            branch_id,
            checkpoint_id,
            reason,
            None,
            runtime,
        )
    }

    /// Constructs one workflow with ephemeral scheduled-pressure qualification.
    #[doc(hidden)]
    #[must_use]
    pub fn with_pending_user_input(
        operation_id: String,
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
        reason: AgentCompactionReason,
        pending_user_input: Option<String>,
        runtime: AgentBuilder,
    ) -> Self {
        Self {
            operation_id,
            session_id,
            branch_id,
            checkpoint_id,
            reason,
            pending_user_input,
            runtime: Some(runtime),
        }
    }
}

/// Narrow component-internal result of one driver-owned compaction workflow.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentCompactSessionOutcome {
    /// One complete successor checkpoint was atomically published.
    Compacted {
        /// Workflow whose successor was published (including an offline fallback).
        workflow_id: crate::SessionWorkflowId,
        /// Exact unchanged semantic source.
        source: CheckpointId,
        /// Exact published successor (Quiescent idle, Working at the tool-group fence).
        target: CheckpointId,
        /// What the committed compaction did, when the successor records it.
        ///
        /// `None` only when the published successor carries no compaction
        /// provenance to read, which a committed compaction should not produce;
        /// a caller treats it as "committed, figures unknown" rather than
        /// refusing.
        summary: Option<CompactionSummary>,
        /// Sequence of the feed record that published the successor, when a
        /// durable store committed it.
        feed_seq: Option<FeedSeq>,
    },
    /// The scheduled source already fit its admitted capacity.
    NotNeeded,
    /// The closed protection policy selected no replaceable prefix.
    NoEligibleRange,
    /// Paid work completed definitively but no semantic replacement was published.
    Failed {
        /// Quiescent failure descendant over the unchanged semantic source.
        checkpoint: CheckpointId,
        /// Redacted operator diagnostic from the exact terminal failure.
        diagnostic: Option<String>,
        /// Usage from a completed, durably recorded provider response, if reported.
        usage: Option<plakous_model::Usage>,
    },
    /// Exact provider or publication evidence remains unresolved.
    RecoveryRequired {
        /// Latest recovery-only checkpoint.
        checkpoint: CheckpointId,
        /// Redacted operator diagnostic when the live failure is still in hand.
        diagnostic: Option<String>,
    },
    /// Exact preparation/admission was unavailable before dispatch.
    Unavailable,
}

/// Which rung of the compaction ladder committed.
///
/// A product surface says this in the operator's vocabulary; the durable
/// strategy identifiers stay in the manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum CompactionRung {
    /// The provider-native facet compacted remotely.
    NativeFacet,
    /// The driver replaced a closed prefix with a semantic summary.
    SemanticFold,
    /// Oldest-first replacement of tool-result bodies with placeholders.
    ToolResultPlaceholders,
    /// Unbound opaque and reasoning replay omitted offline; tool results kept.
    ReplayOmission,
}

impl CompactionRung {
    /// The operator-facing verb for this rung.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::NativeFacet => "compacted natively",
            Self::SemanticFold => "folded",
            Self::ToolResultPlaceholders => "replaced",
            Self::ReplayOmission => "omitted replay",
        }
    }
}

/// What a committed compaction did, as the successor checkpoint records it.
///
/// Figures rather than a bare "committed": the operator is being told that
/// history they can no longer see verbatim has been replaced, and without the
/// before and after they cannot tell whether it bought anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CompactionSummary {
    /// Which rung committed.
    pub rung: CompactionRung,
    /// History segments the compaction replaced.
    pub segments: u32,
    /// Independently measured prepared input before the compaction.
    pub input_tokens_before: u64,
    /// Independently measured prepared input after it.
    pub input_tokens_after: u64,
    /// Unanchored occupancy of the source, when measured: the `bytes÷4`
    /// estimate of the whole request, on the same basis as the after figure.
    pub occupancy_tokens_before: Option<u64>,
    /// Unanchored occupancy of the successor (`bytes÷4` of the whole
    /// request), when measured.
    pub occupancy_tokens_after: Option<u64>,
}

impl CompactionSummary {
    /// Overlays chrome occupancy measured from the same prepares as the audits.
    #[must_use]
    pub const fn with_occupancy(mut self, before: Option<u64>, after: Option<u64>) -> Self {
        self.occupancy_tokens_before = before;
        self.occupancy_tokens_after = after;
        self
    }
}

pub(crate) enum ActorControl {
    Background {
        turn_id: TurnId,
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    RecordJobTerminals {
        reply: oneshot::Sender<Result<bool, crate::AgentError>>,
    },
    Command {
        command: TurnCommand,
        reply: oneshot::Sender<ActorCommandReply>,
    },
    ContextStatus {
        request: AgentContextStatusRequest,
        reply: oneshot::Sender<Result<ContextStatus, AgentSessionEndpointError>>,
    },
    CompactSession {
        request: Box<AgentCompactSessionRequest>,
        reply: oneshot::Sender<Result<AgentCompactSessionOutcome, AgentSessionEndpointError>>,
    },
}

/// Private-facade result when a session workflow cannot inspect one driver.
///
/// This component-internal vocabulary is deliberately not re-exported by the
/// `plakous` facade. It carries no store or terminal-result authority.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentSessionEndpointError {
    /// The bounded actor ingress is currently full.
    Backpressure,
    /// The branch has admitted work and is not quiescent.
    Busy,
    /// The expected exact locator no longer names the driver's checkpoint.
    Stale {
        /// Current session identity.
        session_id: SessionId,
        /// Current branch identity.
        branch_id: BranchId,
        /// Current immutable checkpoint identity.
        checkpoint_id: CheckpointId,
    },
    /// The endpoint is closed or exact preparation is unavailable. For
    /// compaction, this invocation has not been admitted.
    Unavailable,
    /// The reply was lost or the workflow outcome cannot be proved.
    OutcomeUnknown,
    /// Candidate user input failed canonical validation.
    InvalidInput,
    /// Exact compaction preparation or admission rejected this source.
    Rejected {
        /// Redacted operator-facing reason.
        reason: String,
    },
}

/// Narrow component-internal endpoint for serialized session workflows.
///
/// Context inspection remains bounded and read-only. Compaction is a joined
/// paid workflow owned entirely by the existing driver. Neither path can
/// manufacture a [`crate::TurnResult`].
#[doc(hidden)]
#[derive(Clone)]
pub struct AgentSessionEndpoint {
    controls: mpsc::Sender<ActorControl>,
}

impl AgentSessionEndpoint {
    pub(crate) const fn new(controls: mpsc::Sender<ActorControl>) -> Self {
        Self { controls }
    }

    /// Creates a non-owning registry entry.
    #[must_use]
    pub fn downgrade(&self) -> WeakAgentSessionEndpoint {
        WeakAgentSessionEndpoint {
            controls: self.controls.downgrade(),
        }
    }

    /// Inspects one exact checkpoint through its existing private driver.
    ///
    /// # Errors
    ///
    /// Returns promptly on bounded ingress pressure, active work, a stale
    /// locator, actor shutdown, invalid input, or unavailable exact proof.
    pub async fn context_status(
        &self,
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
        pending_user_input: Option<String>,
    ) -> Result<ContextStatus, AgentSessionEndpointError> {
        let (reply, receiver) = oneshot::channel();
        self.controls
            .try_send(ActorControl::ContextStatus {
                request: AgentContextStatusRequest {
                    session_id,
                    branch_id,
                    checkpoint_id,
                    pending_user_input,
                },
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AgentSessionEndpointError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => AgentSessionEndpointError::Unavailable,
            })?;
        tokio::time::timeout(SESSION_ENDPOINT_REPLY_TIMEOUT, receiver)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(Err(AgentSessionEndpointError::Unavailable))
    }

    /// Runs one exact no-tools compaction through the existing private driver.
    #[doc(hidden)]
    pub async fn compact_session(
        &self,
        request: AgentCompactSessionRequest,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let (reply, receiver) = oneshot::channel();
        self.controls
            .try_send(ActorControl::CompactSession {
                request: Box::new(request),
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AgentSessionEndpointError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => AgentSessionEndpointError::Unavailable,
            })?;
        receiver
            .await
            .unwrap_or(Err(AgentSessionEndpointError::OutcomeUnknown))
    }
}

impl fmt::Debug for AgentSessionEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSessionEndpoint")
            .finish_non_exhaustive()
    }
}

/// Non-owning private-facade locator for one session endpoint.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct WeakAgentSessionEndpoint {
    controls: mpsc::WeakSender<ActorControl>,
}

impl WeakAgentSessionEndpoint {
    /// Upgrades while the private driver still owns its bounded ingress.
    #[must_use]
    pub fn upgrade(&self) -> Option<AgentSessionEndpoint> {
        self.controls.upgrade().map(AgentSessionEndpoint::new)
    }
}

/// Exact-turn convenience capability routed through the closed command path.
#[derive(Clone)]
pub struct TurnControl {
    pub(crate) agent_id: AgentId,
    pub(crate) turn_id: TurnId,
    pub(crate) shared: Arc<TurnShared>,
    pub(crate) controls: mpsc::Sender<ActorControl>,
    pub(crate) command_nonce: Arc<AtomicU64>,
}

impl fmt::Debug for TurnControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnControl")
            .field("turn_id", &self.turn_id)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl TurnControl {
    /// Returns the targeted turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Requests cancellation without waiting for the turn to settle.
    ///
    /// The actor flips the cancellation token before it persists the command
    /// (Option A). Settlement remains observable through [`Self::wait_until_settled`]
    /// or the terminal [`TurnResult`](crate::TurnResult).
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn request_cancel(&self) -> Result<CancellationStatus, ControlError> {
        let command_id = self.transient_id("cancel")?;
        let reply = self
            .submit(TurnCommand::Cancel {
                command_id,
                turn_id: self.turn_id,
            })
            .await?;
        let ActorCommandEffect::Cancel(status) = reply.effect else {
            return Err(command_reply_error(&reply));
        };
        Ok(status)
    }

    /// Requests joined cancellation with a runtime-generated transient identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn cancel(&self) -> Result<CancellationStatus, ControlError> {
        let status = self.request_cancel().await?;
        if !matches!(status, CancellationStatus::AlreadyTerminal) {
            self.wait_until_settled().await;
        }
        Ok(status)
    }

    /// Waits until the admitted turn leaves its pending settlement phase.
    pub async fn wait_until_settled(&self) {
        self.shared.wait_until_settled().await;
    }

    /// Reports whether the admitted turn has left its pending settlement phase.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.shared.is_settled()
    }

    /// Requests cancellation under an exact caller identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn cancel_with_id(&self, command_id: CommandId) -> Result<CommandAck, ControlError> {
        self.submit_exact(TurnCommand::Cancel {
            command_id,
            turn_id: self.turn_id,
        })
        .await
    }

    /// Allows one exact pending binding with a transient command identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress, binding, or actor classification failure.
    pub async fn allow_effect(
        &self,
        reference: EffectDecisionRef,
        decision_audit_id: EffectDecisionAuditId,
    ) -> Result<EffectDecisionStatus, ControlError> {
        self.resolve_effect_decision(reference, EffectDecisionChoice::Allow(decision_audit_id))
            .await
    }

    /// Denies one exact pending binding with a transient command identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress, binding, or actor classification failure.
    pub async fn deny_tool(
        &self,
        reference: EffectDecisionRef,
        denial: ToolPolicyDenial,
    ) -> Result<EffectDecisionStatus, ControlError> {
        self.resolve_effect_decision(reference, EffectDecisionChoice::Deny(denial))
            .await
    }

    /// Resolves one exact decision under a caller command identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress, binding, or actor classification failure.
    pub async fn resolve_effect_decision_with_id(
        &self,
        command_id: CommandId,
        reference: EffectDecisionRef,
        decision: EffectDecisionChoice,
    ) -> Result<CommandAck, ControlError> {
        self.submit_exact(TurnCommand::ResolveEffectDecision {
            command_id,
            reference,
            decision,
        })
        .await
    }

    /// Queues steering with a runtime-generated transient identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn steer(&self, text: impl Into<String>) -> Result<SteeringStatus, ControlError> {
        let reply = self
            .submit(TurnCommand::Steer {
                command_id: self.transient_id("steer")?,
                turn_id: self.turn_id,
                text: text.into(),
            })
            .await?;
        match reply.effect {
            ActorCommandEffect::Steer(status) => Ok(status),
            _ => Err(command_reply_error(&reply)),
        }
    }

    /// Backgrounds top-level process calls already in flight in this turn.
    ///
    /// This edge does not affect later dispatches or nested calls.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress failure or an error if this turn is no longer active.
    pub async fn background(&self) -> Result<(), ControlError> {
        let (reply, receiver) = oneshot::channel();
        self.controls
            .try_send(ActorControl::Background {
                turn_id: self.turn_id,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControlError::new(
                    ControlErrorKind::Backpressure,
                    "agent command ingress is full",
                ),
                mpsc::error::TrySendError::Closed(_) => control_shutdown(),
            })?;
        receiver.await.map_err(|_| {
            ControlError::new(
                ControlErrorKind::OutcomeUnknown,
                "background acknowledgement was lost",
            )
        })?
    }

    /// Queues steering under an exact caller command identity.
    ///
    /// # Errors
    ///
    /// Returns a typed ingress or actor classification failure.
    pub async fn steer_with_id(
        &self,
        command_id: CommandId,
        text: impl Into<String>,
    ) -> Result<CommandAck, ControlError> {
        self.submit_exact(TurnCommand::Steer {
            command_id,
            turn_id: self.turn_id,
            text: text.into(),
        })
        .await
    }

    /// Reports whether actor-owned cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancellation().is_cancelled()
    }

    async fn resolve_effect_decision(
        &self,
        reference: EffectDecisionRef,
        decision: EffectDecisionChoice,
    ) -> Result<EffectDecisionStatus, ControlError> {
        let reply = self
            .submit(TurnCommand::ResolveEffectDecision {
                command_id: self.transient_id("decision")?,
                reference,
                decision,
            })
            .await?;
        match reply.effect {
            ActorCommandEffect::EffectDecision(status) => Ok(status),
            _ => Err(command_reply_error(&reply)),
        }
    }

    async fn submit_exact(&self, command: TurnCommand) -> Result<CommandAck, ControlError> {
        let reply = self.submit(command).await?;
        Ok(reply.acknowledgement)
    }

    async fn submit(&self, command: TurnCommand) -> Result<ActorCommandReply, ControlError> {
        submit_actor_command(&self.controls, command).await
    }

    fn transient_id(&self, operation: &str) -> Result<CommandId, ControlError> {
        CommandId::transient(self.agent_id, &self.command_nonce, operation)
    }
}

fn command_reply_error(reply: &ActorCommandReply) -> ControlError {
    reply
        .acknowledgement
        .rejection()
        .cloned()
        .unwrap_or_else(|| {
            ControlError::new(
                ControlErrorKind::Internal,
                "actor command acknowledgement did not match its typed effect",
            )
        })
}

pub(crate) async fn submit_actor_command(
    controls: &mpsc::Sender<ActorControl>,
    command: TurnCommand,
) -> Result<ActorCommandReply, ControlError> {
    let (reply, receiver) = oneshot::channel();
    controls
        .try_send(ActorControl::Command { command, reply })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ControlError::new(
                ControlErrorKind::Backpressure,
                "agent command ingress is full",
            ),
            mpsc::error::TrySendError::Closed(_) => control_shutdown(),
        })?;
    receiver.await.map_err(|_| {
        ControlError::new(
            ControlErrorKind::OutcomeUnknown,
            "command acknowledgement was lost",
        )
    })
}

pub(crate) fn control_shutdown() -> ControlError {
    ControlError::new(
        ControlErrorKind::Shutdown,
        "agent actor is not accepting commands",
    )
}

#[cfg(test)]
mod endpoint_tests {
    use std::error::Error;

    use super::*;
    use uuid::Uuid;

    #[test]
    fn storage_pressure_control_error_preserves_the_typed_agent_source()
    -> Result<(), Box<dyn Error>> {
        let store_error = crate::session::SessionStoreError::admission_capacity(
            crate::session::StoreCapacityBound::Checkpoints,
            57,
            64,
            crate::session::StoreAdmission::Ordinary,
        );
        let crate::session::SessionStoreError::Capacity(capacity) = store_error else {
            return Err("capacity fact".into());
        };
        let error = ControlError::storage_pressure(capacity);
        let source = error
            .source()
            .and_then(|source| source.downcast_ref::<crate::AgentError>())
            .ok_or("typed source")?;
        assert_eq!(source.store_error(), Some(store_error));
        Ok(())
    }

    #[tokio::test]
    async fn lost_command_reply_is_unknown_not_a_definite_refusal() -> Result<(), Box<dyn Error>> {
        let (controls, mut receiver) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            drop(receiver.recv().await);
        });
        let error = submit_actor_command(
            &controls,
            TurnCommand::Start {
                command_id: CommandId::new("lost-reply")?,
                text: "must reconcile".into(),
            },
        )
        .await
        .err()
        .ok_or("lost reply must not be accepted")?;
        assert_eq!(error.kind(), ControlErrorKind::OutcomeUnknown);
        task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn compact_endpoint_lost_reply_is_not_unavailable() -> Result<(), Box<dyn Error>> {
        let (controls, mut receiver) = mpsc::channel(1);
        let endpoint = AgentSessionEndpoint::new(controls);
        let task = tokio::spawn(async move {
            drop(receiver.recv().await);
        });
        let error = endpoint
            .compact_session(compact_endpoint_request()?)
            .await
            .err()
            .ok_or("lost reply must fail")?;
        assert_eq!(error, AgentSessionEndpointError::OutcomeUnknown);
        task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn compact_endpoint_full_and_closed_never_ingress() -> Result<(), Box<dyn Error>> {
        let (controls, receiver) = mpsc::channel(1);
        let (reply, _reply_receiver) = oneshot::channel();
        assert!(
            controls
                .try_send(ActorControl::CompactSession {
                    request: Box::new(compact_endpoint_request()?),
                    reply,
                })
                .is_ok()
        );
        let endpoint = AgentSessionEndpoint::new(controls);
        assert_eq!(
            endpoint.compact_session(compact_endpoint_request()?).await,
            Err(AgentSessionEndpointError::Backpressure)
        );
        drop(receiver);
        assert_eq!(
            endpoint.compact_session(compact_endpoint_request()?).await,
            Err(AgentSessionEndpointError::Unavailable)
        );
        Ok(())
    }

    fn compact_endpoint_request() -> Result<AgentCompactSessionRequest, Box<dyn Error>> {
        Ok(AgentCompactSessionRequest {
            operation_id: "compact:endpoint".into(),
            session_id: SessionId::try_from(Uuid::from_u128(1))?,
            branch_id: BranchId::try_from(Uuid::from_u128(2))?,
            checkpoint_id: CheckpointId::try_from(Uuid::from_u128(3))?,
            reason: AgentCompactionReason::Manual,
            pending_user_input: None,
            runtime: None,
        })
    }

    #[tokio::test]
    async fn session_endpoint_reports_bounded_ingress_backpressure() -> Result<(), Box<dyn Error>> {
        let session_id = SessionId::try_from(Uuid::from_u128(1))?;
        let branch_id = BranchId::try_from(Uuid::from_u128(2))?;
        let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(3))?;
        let (controls, _receiver) = mpsc::channel(1);
        let (reply, _reply_receiver) = oneshot::channel();
        assert!(
            controls
                .try_send(ActorControl::ContextStatus {
                    request: AgentContextStatusRequest {
                        session_id,
                        branch_id,
                        checkpoint_id,
                        pending_user_input: None,
                    },
                    reply,
                })
                .is_ok()
        );
        let endpoint = AgentSessionEndpoint::new(controls);
        assert_eq!(
            endpoint
                .context_status(session_id, branch_id, checkpoint_id, None)
                .await,
            Err(AgentSessionEndpointError::Backpressure)
        );
        Ok(())
    }
}
