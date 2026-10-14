//! Typed session requests, outcomes and read-only reports.

use super::{
    Actor, AgentSessionsError, BranchId, CallId, CheckpointId, CompactionSummary, ContextStatus,
    Digest, DurableSessionAddress, EffectiveProviderConfigurationId, EntryId, FromStr,
    MAX_OPERATION_ID_BYTES, ModelAdapterProfileId, ModelFamilyId, ModelId, ModelRouteBinding,
    OpenedSession, ProtocolId, Provenance, RecoveredTurnHandle, RecoveryAction, RecoveryObligation,
    RouteId, RuntimeCheckpoint, RuntimeLifecycle, SelectedProviderConfigurationId, SessionId,
    SessionRef, SessionRuntimeBinding, Timestamp, ToolExecutorId, ToolName, ToolOutcome,
    WorkingRecoveryPhase, fmt,
};
use plakous_agent::internal::FeedSeq;

/// Caller-stable identity for one complete session workflow.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionOperationId(String);

impl SessionOperationId {
    /// Creates a bounded portable workflow identity.
    ///
    /// # Errors
    ///
    /// Returns invalid input unless the identity contains 1–128 portable ASCII
    /// identifier characters.
    pub fn new(value: impl Into<String>) -> Result<Self, AgentSessionsError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_OPERATION_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session operation identity is not a bounded portable identifier",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the stable portable value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionOperationId")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for SessionOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SessionOperationId {
    type Err = AgentSessionsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Exact current session, branch, and checkpoint address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[allow(
    clippy::struct_field_names,
    reason = "the suffix distinguishes three opaque identity domains in the wire tuple"
)]
pub struct SessionLocator {
    pub(super) session_id: SessionId,
    pub(super) branch_id: BranchId,
    pub(super) checkpoint_id: CheckpointId,
}

impl SessionLocator {
    /// Creates an exact semantic locator.
    #[must_use]
    pub const fn new(
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
    ) -> Self {
        Self {
            session_id,
            branch_id,
            checkpoint_id,
        }
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the independent branch identity.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }

    /// Returns the exact immutable checkpoint identity.
    #[must_use]
    pub const fn checkpoint_id(self) -> CheckpointId {
        self.checkpoint_id
    }

    pub(super) const fn session_ref(self) -> SessionRef {
        SessionRef::new(self.session_id, self.branch_id)
    }

    pub(super) const fn from_durable(address: DurableSessionAddress) -> Self {
        Self::new(
            address.session_id(),
            address.branch_id(),
            address.checkpoint_id(),
        )
    }

    pub(super) const fn durable(self) -> DurableSessionAddress {
        DurableSessionAddress::new(self.session_id, self.branch_id, self.checkpoint_id)
    }
}

impl fmt::Display for SessionLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}",
            self.session_id, self.branch_id, self.checkpoint_id
        )
    }
}

impl FromStr for SessionLocator {
    type Err = AgentSessionsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('/');
        let session_id = parse_locator_part(parts.next(), "session")?;
        let branch_id = parse_locator_part(parts.next(), "branch")?;
        let checkpoint_id = parse_locator_part(parts.next(), "checkpoint")?;
        if parts.next().is_some() {
            return Err(invalid_locator());
        }
        Ok(Self::new(session_id, branch_id, checkpoint_id))
    }
}

fn parse_locator_part<T>(part: Option<&str>, _field: &'static str) -> Result<T, AgentSessionsError>
where
    T: FromStr,
{
    part.ok_or_else(invalid_locator)?
        .parse()
        .map_err(|_| invalid_locator())
}

fn invalid_locator() -> AgentSessionsError {
    AgentSessionsError::new(
        AgentSessionsErrorKind::InvalidInput,
        "session locator is not three valid opaque identities",
    )
}

/// Request to atomically create and attach one durable session.
#[derive(Debug)]
pub struct CreateSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) runtime: SessionRuntimeBinding,
    pub(super) history: Option<super::PortableSessionHistory>,
}

impl CreateSessionRequest {
    /// Creates an exact caller-identified request.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, runtime: SessionRuntimeBinding) -> Self {
        Self {
            operation_id,
            runtime,
            history: None,
        }
    }

    /// Seeds the new session with a carried conversation instead of an empty
    /// one. Its provider replay material is kept only when `runtime` binds the
    /// account and model it was produced under; see
    /// [`super::PortableSessionHistory::retains_replay_for`].
    #[must_use]
    pub fn with_history(mut self, history: super::PortableSessionHistory) -> Self {
        self.history = Some(history);
        self
    }
}

/// Request to open one exact historical branch head.
#[derive(Debug)]
pub struct OpenSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) runtime: SessionRuntimeBinding,
}

impl OpenSessionRequest {
    /// Creates an exact caller-identified open request.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            runtime,
        }
    }
}

/// Request to reconcile only the obligations descended from one exact source.
///
/// `locator` is the original quiescent source bound into the operation
/// identity, not the later recovery-only Working head.
#[derive(Debug)]
pub struct RecoverSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) runtime: SessionRuntimeBinding,
}

impl RecoverSessionRequest {
    /// Creates an exact recovery request.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            runtime,
        }
    }
}

/// Explicit caller resolution for one exact ambiguous compaction operation.
///
/// This never authorizes provider lookup or redispatch. A later compaction is
/// a new operation and therefore requires a different [`SessionOperationId`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbandonCompactionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
}

/// Explicit no-replay abandonment of one exact recovery-only Code Cell root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbandonCodeCellRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
}

impl AbandonCodeCellRequest {
    /// Binds one retry-stable abandonment operation to the exact working root.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
        }
    }
}

impl AbandonCompactionRequest {
    /// Binds abandonment to the original compaction identity and source.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
        }
    }
}

/// Original claim/dispatch evidence for one Working effect recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionEffectEvidence {
    pub(super) claim_digest: Option<Digest>,
    pub(super) dispatched: bool,
}

impl SessionEffectEvidence {
    /// Prepared effect that has not been claimed or dispatched.
    #[must_use]
    pub const fn prepared() -> Self {
        Self {
            claim_digest: None,
            dispatched: false,
        }
    }

    /// Claimed mutating effect with the exact persisted claim-evidence digest.
    #[must_use]
    pub const fn claimed(claim_digest: Digest) -> Self {
        Self {
            claim_digest: Some(claim_digest),
            dispatched: false,
        }
    }

    /// Dispatched effect, optionally bound to the prior claim digest.
    #[must_use]
    pub const fn dispatched(claim_digest: Option<Digest>) -> Self {
        Self {
            claim_digest,
            dispatched: true,
        }
    }

    /// Returns the persisted claim-evidence digest, if any.
    #[must_use]
    pub const fn claim_digest(self) -> Option<Digest> {
        self.claim_digest
    }

    /// Reports whether physical dispatch was observed for this evidence.
    #[must_use]
    pub const fn was_dispatched(self) -> bool {
        self.dispatched
    }
}

/// Topology-blind request to finalize one Working effect from executor evidence.
pub struct RecoverEffectEvidenceRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) recovery_operation: u64,
    pub(super) evidence: SessionEffectEvidence,
    pub(super) executor: ToolExecutorId,
    pub(super) outcome: ToolOutcome,
}

impl RecoverEffectEvidenceRequest {
    /// Binds one retry-stable workflow to an exact Working effect obligation.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        recovery_operation: u64,
        evidence: SessionEffectEvidence,
        executor: ToolExecutorId,
        outcome: ToolOutcome,
    ) -> Self {
        Self {
            operation_id,
            locator,
            recovery_operation,
            evidence,
            executor,
            outcome,
        }
    }
}

impl fmt::Debug for RecoverEffectEvidenceRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoverEffectEvidenceRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("recovery_operation", &self.recovery_operation)
            .field("evidence", &self.evidence)
            .field("executor", &self.executor)
            .field("outcome", &self.outcome)
            .finish()
    }
}

/// Result of topology-blind effect-evidence recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecoverEffectEvidenceOutcome {
    /// The named source is already quiescent and required no effect finalization.
    NotRequired {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// The exact effect was finalized without re-execution, or an identical retry
    /// returned that first committed target.
    Finalized {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged recovery-only source.
        source: SessionLocator,
        /// Exact successor that holds the finalized effect.
        target: SessionLocator,
    },
    /// Evidence conflicted or the effect remains recovery-only; no new identity
    /// was minted and the caller turn stays `RecoveryRequired`.
    RecoveryRequired {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
    },
}

/// Visibility policy for one canonical-history page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryVisibility {
    /// Structural metadata only; display text is omitted.
    Metadata,
    /// Presentation-safe display text derived from canonical history.
    Content,
}

/// Current presentation revision for [`SessionHistoryPageRequest`].
///
/// Revision 2 projects a committed compaction segment as its own record
/// instead of an assistant item with no display text.
pub const SESSION_HISTORY_VISIBILITY_REVISION: u32 = 2;
pub(super) const MAX_SESSION_HISTORY_PAGE_ITEMS: u32 = 256;
pub(super) const MAX_SESSION_HISTORY_PAGE_BYTES: u32 = 65_536;

/// Pagination direction over committed canonical history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryDirection {
    /// Oldest remaining items first.
    Forward,
    /// Newest remaining items first.
    Backward,
}

/// Exact cursor bound to one checkpoint's canonical-history identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionHistoryCursor {
    pub(super) locator: SessionLocator,
    pub(super) history_identity: Digest,
    pub(super) next_index: u64,
}

impl SessionHistoryCursor {
    /// Returns the exact checkpoint this cursor was minted against.
    #[must_use]
    pub const fn locator(self) -> SessionLocator {
        self.locator
    }

    /// Returns the canonical-history identity bound into this cursor.
    #[must_use]
    pub const fn history_identity(self) -> Digest {
        self.history_identity
    }

    /// Returns the next item index in canonical order.
    #[must_use]
    pub const fn next_index(self) -> u64 {
        self.next_index
    }
}

/// Read-only canonical-history page. Opens no mutable driver or store authority.
pub struct SessionHistoryPageRequest {
    pub(super) locator: SessionLocator,
    pub(super) checkpoint_id: CheckpointId,
    pub(super) visibility: SessionHistoryVisibility,
    pub(super) visibility_revision: u32,
    pub(super) direction: SessionHistoryDirection,
    pub(super) max_items: u32,
    pub(super) max_bytes: u32,
    pub(super) cursor: Option<SessionHistoryCursor>,
}

impl SessionHistoryPageRequest {
    /// Binds one bounded history page to an exact checkpoint and visibility revision.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "history page identity is an exact eight-field request, not a decomposed builder"
    )]
    pub const fn new(
        locator: SessionLocator,
        checkpoint_id: CheckpointId,
        visibility: SessionHistoryVisibility,
        visibility_revision: u32,
        direction: SessionHistoryDirection,
        max_items: u32,
        max_bytes: u32,
        cursor: Option<SessionHistoryCursor>,
    ) -> Self {
        Self {
            locator,
            checkpoint_id,
            visibility,
            visibility_revision,
            direction,
            max_items,
            max_bytes,
            cursor,
        }
    }
}

impl fmt::Debug for SessionHistoryPageRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHistoryPageRequest")
            .field("locator", &self.locator)
            .field("checkpoint_id", &self.checkpoint_id)
            .field("visibility", &self.visibility)
            .field("visibility_revision", &self.visibility_revision)
            .field("direction", &self.direction)
            .field("max_items", &self.max_items)
            .field("max_bytes", &self.max_bytes)
            .field("cursor", &self.cursor)
            .finish()
    }
}

/// Why a history page cannot be served from the supplied cursor or revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryResnapshot {
    /// The caller visibility revision is not this release's page projection.
    VisibilityRevision,
    /// The cursor's canonical-history identity no longer matches the checkpoint.
    HistoryIdentity,
    /// The cursor was minted against a different checkpoint.
    Checkpoint,
}

/// One tool block on a history item, named without its payload.
///
/// A call carries the exact name the model called; a result carries only the call identity
/// it answers, because the canonical result block does not repeat the name.
/// Arguments and result content are never projected here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHistoryToolBlock {
    pub(super) call_id: CallId,
    pub(super) tool: Option<ToolName>,
}

impl SessionHistoryToolBlock {
    /// Returns the canonical tool-call identity this block belongs to.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the exact name the model called for a call block, absent for a
    /// result block.
    #[must_use]
    pub const fn tool(&self) -> Option<&ToolName> {
        self.tool.as_ref()
    }
}

/// Which committed compaction pass the checkpoint recorded for one segment.
///
/// These words are the projection's, not the manifest strategy identifiers.
/// A segment that the checkpoint's provenance does not name has no pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryCompactionPass {
    /// A semantic summary replaced a closed prefix.
    SemanticSummary,
    /// Tool-result bodies were replaced by placeholders.
    Placeholder,
    /// A provider-native compaction committed.
    Native,
    /// Unbound replay was omitted offline.
    ReplayOmission,
}

impl SessionHistoryCompactionPass {
    /// Stable parenthetical used in the compaction display phrase.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SemanticSummary => "semantic summary",
            Self::Placeholder => "placeholder pass",
            Self::Native => "native compaction",
            Self::ReplayOmission => "replay omission",
        }
    }
}

/// How one committed compaction segment is shown.
///
/// The summary ciphertext and any provider replay envelope stay in canonical
/// history and are not copied onto this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryCompaction {
    /// The segment is a compaction, and this checkpoint's provenance does not
    /// name it. The count and pass are omitted rather than invented.
    Unrecorded,
    /// Provenance names this segment.
    Recorded {
        /// Selected source segments the provenance says the pass covered.
        replaced_segments: u32,
        /// Which pass committed.
        pass: SessionHistoryCompactionPass,
    },
}

/// Presentation-safe canonical-history item. Display text is redacted in `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionHistoryItem {
    pub(super) entry_id: EntryId,
    pub(super) actor: Actor,
    pub(super) provenance: Provenance,
    pub(super) block_count: usize,
    pub(super) display_text: Option<String>,
    pub(super) tool_blocks: Vec<SessionHistoryToolBlock>,
    pub(super) tool_calls: usize,
    pub(super) tool_results: usize,
    pub(super) has_non_display_content: bool,
    /// Set only when this item's segment is a committed compaction.
    pub(super) compaction: Option<SessionHistoryCompaction>,
}

impl SessionHistoryItem {
    /// Returns the canonical conversation-entry identity.
    #[must_use]
    pub const fn entry_id(&self) -> EntryId {
        self.entry_id
    }

    /// Returns the responsible actor.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// Returns how this item entered the conversation.
    ///
    /// A steering segment must carry `Actor::User` to lower as a user-role
    /// message on every protocol, so the actor alone cannot distinguish a note
    /// the harness injected mid-turn from text the operator typed. Provenance
    /// can: the harness writes `Instruction`, typed input is `UserInput`.
    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// True when the harness authored this item rather than the operator.
    #[must_use]
    pub const fn is_harness_authored(&self) -> bool {
        matches!(self.actor, Actor::User) && matches!(self.provenance, Provenance::Instruction)
    }

    /// Returns the canonical block count.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    /// Returns displayable text when content visibility is granted.
    #[must_use]
    pub fn display_text(&self) -> Option<&str> {
        self.display_text.as_deref()
    }

    /// Returns the number of tool-call blocks without exposing arguments.
    #[must_use]
    pub const fn tool_calls(&self) -> usize {
        self.tool_calls
    }

    /// Returns this item's tool blocks in canonical order, named but payloadless.
    #[must_use]
    pub fn tool_blocks(&self) -> &[SessionHistoryToolBlock] {
        &self.tool_blocks
    }

    /// Returns the number of tool-result blocks without exposing result content.
    #[must_use]
    pub const fn tool_results(&self) -> usize {
        self.tool_results
    }

    /// Reports whether non-display content was intentionally omitted.
    #[must_use]
    pub const fn has_non_display_content(&self) -> bool {
        self.has_non_display_content
    }

    /// Returns the compaction projection when this item's segment is one.
    ///
    /// `None` for every other segment. The summary and provider replay are
    /// not recoverable from this value.
    #[must_use]
    pub const fn compaction(&self) -> Option<SessionHistoryCompaction> {
        self.compaction
    }

    /// The history-page kind: `compaction` for a committed compaction segment,
    /// otherwise the actor name.
    #[must_use]
    pub const fn projected_kind(&self) -> &'static str {
        if self.compaction.is_some() {
            "compaction"
        } else {
            self.actor.as_str()
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        128_usize
            .saturating_add(self.display_text.as_deref().map_or(0, str::len))
            .saturating_add(self.tool_blocks.iter().fold(0, |total, block| {
                total.saturating_add(
                    64_usize
                        .saturating_add(block.tool.as_ref().map_or(0, |tool| tool.as_str().len())),
                )
            }))
    }
}

impl fmt::Debug for SessionHistoryItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHistoryItem")
            .field("entry_id", &self.entry_id)
            .field("actor", &self.actor)
            .field("provenance", &self.provenance)
            .field("block_count", &self.block_count)
            .field(
                "display_text",
                &self.display_text.as_ref().map(|_| "<redacted>"),
            )
            .field("tool_calls", &self.tool_calls)
            .field("tool_blocks", &self.tool_blocks)
            .field("tool_results", &self.tool_results)
            .field("has_non_display_content", &self.has_non_display_content)
            .field("compaction", &self.compaction)
            .finish()
    }
}

/// Typed result of one read-only canonical-history page.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionHistoryPageOutcome {
    /// Bounded presentation items plus an optional next cursor.
    Page {
        /// Exact checkpoint that produced this page.
        locator: SessionLocator,
        /// Visibility used to project the items.
        visibility: SessionHistoryVisibility,
        /// Canonical-history identity bound into any next cursor.
        history_identity: Digest,
        /// Bounded presentation items in the requested direction.
        items: Vec<SessionHistoryItem>,
        /// Next page cursor, absent at the end of history.
        next: Option<SessionHistoryCursor>,
    },
    /// The caller must discard its cursor and reload from the current checkpoint.
    Resnapshot {
        /// Exact checkpoint currently observed.
        locator: SessionLocator,
        /// Why the supplied cursor or revision cannot be used.
        reason: SessionHistoryResnapshot,
    },
}

/// Read-only context query shape.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum ContextQuery {
    /// Measure the current exact model-input projection.
    Current,
    /// Measure the exact candidate projection with one uncommitted user input.
    WithPendingUserInput(String),
}

impl fmt::Debug for ContextQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current => formatter.write_str("Current"),
            Self::WithPendingUserInput(_) => formatter
                .debug_tuple("WithPendingUserInput")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

/// Caller-identified context-status request.
#[derive(Clone, Eq, PartialEq)]
pub struct ContextStatusRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) query: ContextQuery,
}

impl fmt::Debug for ContextStatusRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextStatusRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("query", &self.query)
            .finish()
    }
}

impl ContextStatusRequest {
    /// Creates a read-only exact-projection request.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        query: ContextQuery,
    ) -> Self {
        Self {
            operation_id,
            locator,
            query,
        }
    }
}

/// Product-owned reason for asking Plakous to consider compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompactReason {
    /// Explicit operator request.
    Manual,
    /// Product scheduling policy observed context pressure.
    Scheduled,
}

/// Safe-boundary compaction request. It intentionally carries no summary, cut,
/// token count, model override, or artifact policy.
#[derive(Debug)]
pub struct CompactSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) reason: CompactReason,
    pub(super) qualification: ContextQuery,
    pub(super) runtime: SessionRuntimeBinding,
}

impl CompactSessionRequest {
    /// Creates one exact compaction request.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        reason: CompactReason,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            reason,
            qualification: ContextQuery::Current,
            runtime,
        }
    }

    /// Qualifies scheduled compaction against one ephemeral pending input.
    ///
    /// The input is used only to decide whether the operation is needed. It is
    /// never added to the immutable source, compaction request, or recovery
    /// record.
    #[must_use]
    pub fn with_pending_user_input(mut self, input: String) -> Self {
        self.qualification = ContextQuery::WithPendingUserInput(input);
        self
    }
}

/// Quiescent copy-on-write branch request.
pub struct ForkSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) runtime: Option<SessionRuntimeBinding>,
}

impl ForkSessionRequest {
    /// Creates one exact source-preserving fork request.
    #[must_use]
    pub const fn new(operation_id: SessionOperationId, locator: SessionLocator) -> Self {
        Self {
            operation_id,
            locator,
            runtime: None,
        }
    }

    /// Creates a fork that keeps the source's history and binding and
    /// recompiles the rest from `runtime`, which must be freshly admitted for
    /// the source's own target: the target plan carries the caller's current
    /// instruction sources. An unchanged compilation publishes an exact
    /// ordinary fork; a changed one publishes a configuration transition onto
    /// the same target. Recover it with the same constructor. A runtime for
    /// another target is refused; that transition is a rebind.
    #[must_use]
    pub const fn recompiling(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            runtime: Some(runtime),
        }
    }
}

impl fmt::Debug for ForkSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForkSessionRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("recompiling", &self.runtime.is_some())
            .finish()
    }
}

/// Explicit non-secret target selectors for a future rebind.
///
/// This value is not revalidation authority. A rebind request must separately
/// own a [`SessionRuntimeBinding`] whose actual provider/model/account state is
/// freshly admitted and proven to match these selectors before publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTargetBinding {
    pub(super) protocol: ProtocolId,
    pub(super) route: RouteId,
    pub(super) model: ModelId,
    pub(super) selected_provider_configuration: SelectedProviderConfigurationId,
    pub(super) effective_provider_configuration: EffectiveProviderConfigurationId,
    pub(super) account_binding: Digest,
}

impl SessionTargetBinding {
    /// Creates one explicit target selection. No current default is consulted.
    #[must_use]
    pub const fn new(
        protocol: ProtocolId,
        route: RouteId,
        model: ModelId,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        account_binding: Digest,
    ) -> Self {
        Self {
            protocol,
            route,
            model,
            selected_provider_configuration,
            effective_provider_configuration,
            account_binding,
        }
    }

    /// Returns the target protocol/dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the target route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the target model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the exact caller-selected provider configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }

    /// Returns the exact compiled provider behavior identity.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }

    /// Returns the non-secret exact target account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }
}

/// Explicit source-preserving rebind request.
pub struct RebindSessionRequest {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) target: SessionTargetBinding,
    pub(super) runtime: SessionRuntimeBinding,
    pub(super) tool_transition: RebindToolTransition,
}

/// Tool-authority behavior for one explicit canonical rebind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebindToolTransition {
    /// Require the source and target to expose the exact same tool interface.
    PreserveExact,
    /// Adopt the exact qualified target request presentation while preserving
    /// canonical executors and nonincreasing workspace, effect, and policy authority.
    RecompileForTarget,
}

impl RebindSessionRequest {
    /// Creates one exact confirmed target transition with fresh sealed
    /// authority; it keeps the source's presentation and instruction sources.
    #[must_use]
    pub const fn new(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition: RebindToolTransition::PreserveExact,
        }
    }

    /// Creates a rebind that recompiles for its target: the qualified target
    /// presentation and the caller's current instruction sources.
    #[must_use]
    pub const fn recompiling_target(
        operation_id: SessionOperationId,
        locator: SessionLocator,
        target: SessionTargetBinding,
        runtime: SessionRuntimeBinding,
    ) -> Self {
        Self {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition: RebindToolTransition::RecompileForTarget,
        }
    }
}

impl fmt::Debug for RebindSessionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RebindSessionRequest")
            .field("operation_id", &self.operation_id)
            .field("locator", &self.locator)
            .field("target", &self.target)
            .field("runtime", &self.runtime)
            .field("tool_transition", &self.tool_transition)
            .finish()
    }
}

/// Redaction-safe exact historical model binding.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionBindingReport {
    pub(super) protocol: ProtocolId,
    pub(super) route: RouteId,
    pub(super) model: ModelId,
    pub(super) reported_model: Option<ModelId>,
    pub(super) model_family: Option<ModelFamilyId>,
    pub(super) adapter: ModelAdapterProfileId,
    pub(super) binding: Digest,
    pub(super) selected_provider_configuration: SelectedProviderConfigurationId,
    pub(super) effective_provider_configuration: EffectiveProviderConfigurationId,
    pub(super) descriptor_evidence: Digest,
    pub(super) account_authority: String,
    pub(super) account_reference: String,
    pub(super) account_revision: u64,
    pub(super) profile_authority: String,
    pub(super) profile_reference: String,
    pub(super) profile_revision: u64,
    pub(super) admission_observed_at: Timestamp,
    pub(super) admission_valid_until: Option<Timestamp>,
    pub(super) admission_evidence: Digest,
}

impl SessionBindingReport {
    pub(super) fn from_checkpoint(checkpoint: &RuntimeCheckpoint) -> Self {
        let manifest = checkpoint.manifest();
        Self::from_binding(
            manifest.binding(),
            manifest.provider_configuration().selected_identity(),
            manifest.provider_configuration().effective_identity(),
        )
    }

    pub(super) fn from_binding(
        binding: &ModelRouteBinding,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
    ) -> Self {
        Self {
            protocol: binding.protocol().clone(),
            route: binding.route().clone(),
            model: binding.requested_model().clone(),
            reported_model: binding.reported_model().cloned(),
            model_family: binding.model_family().cloned(),
            adapter: binding.adapter().clone(),
            binding: binding.model_binding(),
            selected_provider_configuration,
            effective_provider_configuration,
            descriptor_evidence: binding.descriptor().integrity(),
            account_authority: binding.account().authority().to_owned(),
            account_reference: binding.account().subject().to_owned(),
            account_revision: binding.account().revision(),
            profile_authority: binding.profile().authority().to_owned(),
            profile_reference: binding.profile().subject().to_owned(),
            profile_revision: binding.profile().revision(),
            admission_observed_at: binding.admission().observed_at(),
            admission_valid_until: binding.admission().valid_until(),
            admission_evidence: binding.admission().evidence().integrity(),
        }
    }

    /// Returns the exact historical protocol/dialect.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the exact historical route.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the exact historical requested model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the provider-reported model identity, when recorded.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }

    /// Returns the resolved model-family identity, when recorded.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the exact selected model-adapter profile.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }

    /// Returns the complete historical model-binding identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.binding
    }

    /// Returns the exact complete caller-selected provider configuration.
    #[must_use]
    pub const fn selected_provider_configuration(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration
    }

    /// Returns the exact compiled behavior-bearing provider configuration.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }

    /// Returns the exact persisted descriptor-evidence identity.
    #[must_use]
    pub const fn descriptor_evidence(&self) -> Digest {
        self.descriptor_evidence
    }

    /// Returns the authority namespace for the exact account reference.
    #[must_use]
    pub fn account_authority(&self) -> &str {
        &self.account_authority
    }

    /// Returns the non-secret exact account reference.
    #[must_use]
    pub fn account_reference(&self) -> &str {
        &self.account_reference
    }

    /// Returns the account-reference semantic revision.
    #[must_use]
    pub const fn account_revision(&self) -> u64 {
        self.account_revision
    }

    /// Returns the authority namespace for the exact adapter/profile reference.
    #[must_use]
    pub fn profile_authority(&self) -> &str {
        &self.profile_authority
    }

    /// Returns the non-secret exact adapter/profile reference.
    #[must_use]
    pub fn profile_reference(&self) -> &str {
        &self.profile_reference
    }

    /// Returns the adapter/profile reference semantic revision.
    #[must_use]
    pub const fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    /// Returns when the target admission was freshly observed.
    #[must_use]
    pub const fn admission_observed_at(&self) -> Timestamp {
        self.admission_observed_at
    }

    /// Returns the recorded target-admission expiry, when bounded.
    #[must_use]
    pub const fn admission_valid_until(&self) -> Option<Timestamp> {
        self.admission_valid_until
    }

    /// Returns the exact sealed admission-evidence identity.
    #[must_use]
    pub const fn admission_evidence(&self) -> Digest {
        self.admission_evidence
    }
}

impl fmt::Debug for SessionBindingReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionBindingReport")
            .field("protocol", &self.protocol)
            .field("route", &self.route)
            .field("model", &self.model)
            .field("reported_model", &self.reported_model)
            .field("model_family", &self.model_family)
            .field("adapter", &self.adapter)
            .field("binding", &self.binding)
            .field(
                "selected_provider_configuration",
                &self.selected_provider_configuration,
            )
            .field(
                "effective_provider_configuration",
                &self.effective_provider_configuration,
            )
            .field("descriptor_evidence", &self.descriptor_evidence)
            .field("account_authority", &self.account_authority)
            .field("account_reference", &"<redacted>")
            .field("account_revision", &self.account_revision)
            .field("profile_authority", &self.profile_authority)
            .field("profile_reference", &"<redacted>")
            .field("profile_revision", &self.profile_revision)
            .field("admission_observed_at", &self.admission_observed_at)
            .field("admission_valid_until", &self.admission_valid_until)
            .field("admission_evidence", &self.admission_evidence)
            .finish()
    }
}

/// Closed public recovery phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionRecoveryPhase {
    /// Provider/command dispatch evidence is unresolved.
    DispatchEvidence,
    /// Approval/tool effect evidence is unresolved.
    EffectReconciliation,
    /// Exact checkpoint commit acknowledgement is unresolved.
    CommitRecovery,
    /// Proven outcomes await terminal folding.
    Finalization,
    /// A proven pre-dispatch effect decision can be restored exactly.
    EffectDecision,
    /// A provider-hosted tool batch or server pause can be restored exactly.
    HostedContinuation,
}

/// Closed public recovery action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionRecoveryAction {
    /// Reconcile one exact commit operation.
    CommitRecovery,
    /// Obtain provider-owned evidence for the exact attempt.
    ProviderEvidenceRequired,
    /// Obtain executor-owned evidence for the exact effect.
    EffectEvidenceRequired,
    /// Abandon a possibly-started local composition without replaying its source or children.
    EvaluationAbandonmentRequired,
    /// Terminalize only with exact proof of non-dispatch.
    ProvenNotDispatched,
    /// Fold an already proven outcome.
    FinalizationRequired,
    /// Restore only the exact pre-dispatch control-plane decision capability.
    RestoreEffectDecision,
    /// Restore one exact provider-programmatic customer-tool batch.
    RestoreToolBatch,
    /// Restore one exact provider server pause before its next round.
    RestoreServerPause,
}

/// One exact unresolved operation report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryObligationReport {
    pub(super) operation_id: u64,
    pub(super) action: SessionRecoveryAction,
    pub(super) request_identity: Digest,
}

impl RecoveryObligationReport {
    /// Returns the persisted operation identity.
    #[must_use]
    pub const fn operation_id(self) -> u64 {
        self.operation_id
    }

    /// Returns the only recovery action authorized by the checkpoint.
    #[must_use]
    pub const fn action(self) -> SessionRecoveryAction {
        self.action
    }

    /// Returns the exact durable request identity to which evidence must bind.
    #[must_use]
    pub const fn request_identity(self) -> Digest {
        self.request_identity
    }
}

/// Exact recovery-only opening report. It never carries an ordinary `Agent`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRequiredReport {
    pub(super) locator: SessionLocator,
    pub(super) phase: SessionRecoveryPhase,
    pub(super) obligations: Vec<RecoveryObligationReport>,
}

impl RecoveryRequiredReport {
    pub(super) fn from_checkpoint(
        locator: SessionLocator,
        checkpoint: &RuntimeCheckpoint,
    ) -> Option<Self> {
        let lifecycle = checkpoint.manifest().lifecycle();
        let RuntimeLifecycle::Working {
            recovery_phase,
            obligations,
            ..
        } = lifecycle
        else {
            return None;
        };
        let obligations = obligations
            .iter()
            .copied()
            .map(|obligation| {
                checkpoint
                    .manifest()
                    .ledger()
                    .operations()
                    .into_iter()
                    .find(|record| record.operation_id() == obligation.operation_id())
                    .map(|record| obligation_report(obligation, record.request_identity()))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            locator,
            phase: recovery_phase_report(*recovery_phase),
            obligations,
        })
    }

    /// Returns the exact working checkpoint.
    #[must_use]
    pub const fn locator(&self) -> SessionLocator {
        self.locator
    }

    /// Returns the exact recorded recovery phase.
    #[must_use]
    pub const fn phase(&self) -> SessionRecoveryPhase {
        self.phase
    }

    /// Returns every exact unresolved operation.
    #[must_use]
    pub fn obligations(&self) -> &[RecoveryObligationReport] {
        &self.obligations
    }
}

fn recovery_phase_report(phase: WorkingRecoveryPhase) -> SessionRecoveryPhase {
    match phase {
        WorkingRecoveryPhase::DispatchEvidence => SessionRecoveryPhase::DispatchEvidence,
        WorkingRecoveryPhase::EffectReconciliation => SessionRecoveryPhase::EffectReconciliation,
        WorkingRecoveryPhase::CommitRecovery => SessionRecoveryPhase::CommitRecovery,
        WorkingRecoveryPhase::Finalization => SessionRecoveryPhase::Finalization,
        WorkingRecoveryPhase::EffectDecision => SessionRecoveryPhase::EffectDecision,
        WorkingRecoveryPhase::HostedContinuation => SessionRecoveryPhase::HostedContinuation,
    }
}

fn obligation_report(
    obligation: RecoveryObligation,
    request_identity: Digest,
) -> RecoveryObligationReport {
    let action = match obligation.action() {
        RecoveryAction::CommitRecovery => SessionRecoveryAction::CommitRecovery,
        RecoveryAction::ProviderEvidenceRequired => SessionRecoveryAction::ProviderEvidenceRequired,
        RecoveryAction::EffectEvidenceRequired => SessionRecoveryAction::EffectEvidenceRequired,
        RecoveryAction::EvaluationAbandonmentRequired => {
            SessionRecoveryAction::EvaluationAbandonmentRequired
        }
        RecoveryAction::ProvenNotDispatched => SessionRecoveryAction::ProvenNotDispatched,
        RecoveryAction::FinalizationRequired => SessionRecoveryAction::FinalizationRequired,
        RecoveryAction::RestoreEffectDecision => SessionRecoveryAction::RestoreEffectDecision,
        RecoveryAction::RestoreToolBatch => SessionRecoveryAction::RestoreToolBatch,
        RecoveryAction::RestoreServerPause => SessionRecoveryAction::RestoreServerPause,
    };
    RecoveryObligationReport {
        operation_id: obligation.operation_id(),
        action,
        request_identity,
    }
}

/// Exact missing authority which prevents a workflow from pretending success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionWorkflowUnavailable {
    /// No certified working recovery handler is available.
    WorkingRecovery,
    /// The driver cannot yet build and measure an exact candidate projection.
    ContextStatus,
    /// The private driver has no versioned no-tools compaction command.
    Compaction,
}

/// Typed result of durable session creation.
#[derive(Debug)]
#[non_exhaustive]
pub enum CreateSessionOutcome {
    /// A durable agent was created and attached.
    Created {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Created runtime.
        session: OpenedSession,
    },
}

/// Typed result of exact open.
#[derive(Debug)]
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "the success variant transfers the sole live driver capability without another allocation"
)]
pub enum OpenSessionOutcome {
    /// A certified quiescent runtime was attached.
    Opened {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Attached runtime.
        session: OpenedSession,
    },
    /// One original admitted turn was restored at its exact decision barrier.
    RestoredWorking {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Attached runtime; it admits no replacement for the active root.
        session: OpenedSession,
        /// Opaque control/result authority for the original turn.
        turn: RecoveredTurnHandle,
    },
    /// The selected checkpoint is recovery-only and carries no `Agent`.
    RecoveryRequired {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
        /// How long until this branch could be acquired, when the only thing
        /// standing in the way is another process's lease.
        ///
        /// A process that died mid-turn keeps its lease until it expires, so
        /// its session is recovery-only for that long and ordinarily openable
        /// afterwards. `None` when waiting will not help.
        retry_after_seconds: Option<u64>,
    },
}

/// Typed result of explicit recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecoverSessionOutcome {
    /// The source was already quiescent; no mutation occurred.
    NotRequired {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// The exact compaction successor was recovered or found finalized.
    Compacted {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Exact quiescent successor.
        target: SessionLocator,
    },
    /// The exact compaction operation was terminalized without a summary.
    Failed {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Quiescent failure descendant.
        target: SessionLocator,
    },
    /// The exact ambiguous operation was deliberately abandoned.
    Abandoned {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Quiescent abandonment descendant.
        target: SessionLocator,
    },
    /// Exact obligations remain, but no certified handler guessed or
    /// redispatched their outcomes.
    Unavailable {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
        /// Missing handler authority.
        reason: SessionWorkflowUnavailable,
    },
}

/// Result of deliberately terminalizing an exact ambiguous compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AbandonCompactionOutcome {
    /// The operation was not admitted and the source remains unchanged.
    NotRequired {
        /// Exact operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// Success had already atomically won before abandonment.
    Compacted {
        /// Exact operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
        /// Exact compacted successor.
        target: SessionLocator,
    },
    /// Failure had already atomically won before abandonment.
    Failed {
        /// Exact operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
        /// Exact quiescent failure descendant.
        target: SessionLocator,
    },
    /// Ambiguity was explicitly abandoned in a quiescent source-preserving descendant.
    Abandoned {
        /// Exact operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Exact quiescent abandonment descendant.
        target: SessionLocator,
    },
    /// The selected working state could not be safely abandoned.
    RecoveryRequired {
        /// Exact operation identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
    },
}

/// Result of explicitly abandoning one recovery-only local Code Cell.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AbandonCodeCellOutcome {
    /// The selected source is already quiescent and required no abandonment.
    NotRequired {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// The no-replay abandonment was finalized in one quiescent descendant.
    Finalized {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Exact unchanged recovery-only source.
        source: SessionLocator,
        /// Exact quiescent descendant.
        target: SessionLocator,
    },
    /// The exact working root still requires certified recovery.
    RecoveryRequired {
        /// Exact caller operation identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
    },
}

/// Checkpoint-bound context report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionContextStatus {
    pub(super) operation_id: SessionOperationId,
    pub(super) locator: SessionLocator,
    pub(super) kind: SessionContextStatusKind,
}

impl SessionContextStatus {
    /// Returns the exact caller workflow identity.
    #[must_use]
    pub const fn operation_id(&self) -> &SessionOperationId {
        &self.operation_id
    }

    /// Returns the exact checkpoint measured or refused.
    #[must_use]
    pub const fn locator(&self) -> SessionLocator {
        self.locator
    }

    /// Returns checked status without guessed units.
    #[must_use]
    pub const fn kind(&self) -> &SessionContextStatusKind {
        &self.kind
    }
}

/// Exact/bounded model context status or a typed refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionContextStatusKind {
    /// Descriptor-bound complete-projection status.
    Available(Box<ContextStatus>),
    /// Working state blocks an idle projection query.
    RecoveryRequired(RecoveryRequiredReport),
    /// No certified driver-owned measurement was available.
    Unavailable(SessionWorkflowUnavailable),
}

/// Compaction disposition. No variant accepts caller summary or cut authority.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompactSessionOutcome {
    /// One complete successor checkpoint was atomically published.
    Compacted {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Exact quiescent successor.
        target: SessionLocator,
        /// What the committed compaction did, when the successor records it.
        summary: Option<CompactionSummary>,
        /// Sequence of the feed record that published the successor; `None`
        /// for an outcome recovered from the workflow receipt.
        feed_seq: Option<FeedSeq>,
    },
    /// The exact projection already satisfies the target policy.
    NotNeeded {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// No complete eligible semantic range exists.
    NoEligibleRange {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
    },
    /// Paid work completed definitively without publishing a summary.
    Failed {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Quiescent failure descendant.
        target: SessionLocator,
        /// Redacted operator diagnostic when the live failure is still in hand.
        diagnostic: Option<String>,
    },
    /// The exact prior ambiguous operation was deliberately abandoned.
    Abandoned {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged semantic source.
        source: SessionLocator,
        /// Quiescent abandonment descendant.
        target: SessionLocator,
    },
    /// The checkpoint remains recovery-only.
    RecoveryRequired {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
        /// Redacted operator diagnostic when the live failure is still in hand.
        diagnostic: Option<String>,
    },
    /// This invocation was refused before admission: no model call or commit
    /// occurred. A lost reply instead returns
    /// [`AgentSessionsErrorKind::OutcomeUnknown`].
    Unavailable {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
        /// Missing authority.
        reason: SessionWorkflowUnavailable,
    },
}

/// Portability facts for a successful copy-on-write branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArtifactPortability {
    /// Native state was retained under an exact family proof.
    ProvenRetained,
    /// Unproved continuation/cache/family state was cleared.
    ClearedUnproved,
}

/// Exact fork result or typed refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ForkSessionOutcome {
    /// An independent target branch was atomically published.
    Forked {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
        /// New independent branch.
        target: SessionLocator,
        /// Family-artifact disposition.
        portability: ArtifactPortability,
        /// Sequence of the `Branched` record that published the target.
        feed_seq: Option<FeedSeq>,
    },
    /// Working source cannot fork.
    RecoveryRequired {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
    },
}

/// Explicit rebind result or typed refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RebindSessionOutcome {
    /// A new target-bound branch was atomically published.
    Rebound {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Exact unchanged source.
        source: SessionLocator,
        /// New target branch.
        target: SessionLocator,
        /// Cleared/retained native state proof.
        portability: ArtifactPortability,
        /// Complete freshly admitted target binding.
        binding: Box<SessionBindingReport>,
        /// Sequence of the `Branched` record that published the target.
        feed_seq: Option<FeedSeq>,
    },
    /// Working source cannot rebind.
    RecoveryRequired {
        /// Exact caller workflow identity.
        operation_id: SessionOperationId,
        /// Persisted recovery-only authority.
        recovery: RecoveryRequiredReport,
    },
}

/// Stable category for a high-level session workflow failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AgentSessionsErrorKind {
    /// Unconsumed process rows do not yet have durable terminal evidence.
    UnsettledProcesses,
    /// A public identity or locator was invalid.
    InvalidInput,
    /// The compaction endpoint refused admission because it is busy.
    Busy,
    /// The selected branch or checkpoint does not exist.
    NotFound,
    /// The selected checkpoint was superseded history that retention has
    /// reclaimed; the session continues from its current head.
    Pruned,
    /// The locator is no longer the branch head.
    StaleLocator,
    /// A stable operation identity was reused for different exact content.
    IdentityConflict,
    /// Session ownership or optimistic concurrency was lost.
    Ownership,
    /// A workflow reply was lost or its admitted outcome is unknown. A model
    /// call or commit may have occurred; the change feed remains authoritative.
    OutcomeUnknown,
    /// A retention bound of the store has no room for this write; deleting
    /// sessions frees it. [`AgentSessionsError::storage_pressure`] carries the
    /// bound, usage, threshold, limit and admission class.
    ///
    /// [`AgentSessionsError::storage_pressure`]: super::AgentSessionsError::storage_pressure
    StoragePressure,
    /// Physical storage is exhausted, independently of retention limits.
    StorageFull,
    /// The store is busy, unavailable or shutting down.
    StorageUnavailable,
    /// The store path itself is refused: a component of it is a symbolic link,
    /// or the database is not a private regular file. Retrying never helps; the
    /// message names the path and the remedy.
    UnsafeStorePath,
    /// Stored session state is corrupt or does not match the requested workflow.
    Integrity,
    /// The session or store was written by an incompatible build: it does not
    /// decode, or this build reconstructs different semantics from it. Start a
    /// new session.
    IncompatibleBuild,
    /// Current external authority did not revalidate.
    Revalidation,
    /// Internal release-owned workflow configuration is invalid.
    Internal,
}

/// Redaction-safe acknowledgement of one exact session archive transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveSessionReport {
    session_id: SessionId,
    previous_revision: u64,
    archived_revision: u64,
}

impl ArchiveSessionReport {
    pub(super) fn from_receipt(receipt: plakous_agent::internal::ArchiveReceipt) -> Self {
        Self {
            session_id: receipt.session().session_id(),
            previous_revision: receipt.previous().revision().get(),
            archived_revision: receipt.session().revision().get(),
        }
    }

    /// Returns the archived session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the exact pre-archive revision accepted by the store.
    #[must_use]
    pub const fn previous_revision(self) -> u64 {
        self.previous_revision
    }

    /// Returns the exact archived revision required by deletion.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
}

/// Outcome of reclaiming one session's superseded checkpoint history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PruneHistoryOutcome {
    /// The head was quiescent; the report says what was reclaimed, which is
    /// zero when nothing was superseded.
    Pruned(PruneHistoryReport),
    /// The head is an unfinished turn; recover it first, since recovery may
    /// still read older checkpoints.
    RecoveryRequired,
}

/// Redaction-safe account of one history prune.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PruneHistoryReport {
    locator: SessionLocator,
    pruned_checkpoints: u64,
}

impl PruneHistoryReport {
    pub(super) fn from_receipt(
        locator: SessionLocator,
        receipt: plakous_agent::internal::PruneReceipt,
    ) -> Self {
        Self {
            locator,
            pruned_checkpoints: receipt.pruned_checkpoints(),
        }
    }

    /// Returns the unchanged head the prune ran at.
    #[must_use]
    pub const fn locator(self) -> SessionLocator {
        self.locator
    }
    /// Returns removed superseded checkpoints.
    #[must_use]
    pub const fn pruned_checkpoints(self) -> u64 {
        self.pruned_checkpoints
    }
}

/// One archived session the store still holds: the state an interrupted
/// archive-then-delete leaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchivedSessionReport {
    session_id: SessionId,
    archived_revision: u64,
}

impl ArchivedSessionReport {
    pub(super) const fn new(session_id: SessionId, archived_revision: u64) -> Self {
        Self {
            session_id,
            archived_revision,
        }
    }

    /// Returns the archived session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the exact archived revision deletion requires.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
}

/// Redaction-safe acknowledgement of one exact semantic session deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeleteSessionReport {
    session_id: SessionId,
    archived_revision: u64,
    reclaimed_checkpoints: u64,
}

impl DeleteSessionReport {
    pub(super) fn from_receipt(receipt: plakous_agent::internal::DeleteReceipt) -> Self {
        Self {
            session_id: receipt.request().session_id(),
            archived_revision: receipt.request().expected_revision().get(),
            reclaimed_checkpoints: receipt.reclaimed_checkpoints(),
        }
    }

    /// Returns the deleted session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the exact archived revision that authorized deletion.
    #[must_use]
    pub const fn archived_revision(self) -> u64 {
        self.archived_revision
    }
    /// Returns removed checkpoint roots.
    #[must_use]
    pub const fn reclaimed_checkpoints(self) -> u64 {
        self.reclaimed_checkpoints
    }
}

impl CompactSessionOutcome {
    /// Returns the sequence of the feed record that made this outcome
    /// durable, so a live consumer knows how far to read before it reports
    /// it. `None` for an outcome that published nothing.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        match self {
            Self::Compacted { feed_seq, .. } => *feed_seq,
            _ => None,
        }
    }
}

impl ForkSessionOutcome {
    /// Returns the sequence of the feed record that made this outcome
    /// durable, so a live consumer knows how far to read before it reports
    /// it. `None` for an outcome that published nothing.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        match self {
            Self::Forked { feed_seq, .. } => *feed_seq,
            _ => None,
        }
    }
}

impl RebindSessionOutcome {
    /// Returns the sequence of the feed record that made this outcome
    /// durable, so a live consumer knows how far to read before it reports
    /// it. `None` for an outcome that published nothing.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        match self {
            Self::Rebound { feed_seq, .. } => *feed_seq,
            _ => None,
        }
    }
}

/// Objects physically reclaimed by one explicit deferred-work drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReclaimReport {
    objects: u64,
    object_bytes: u64,
}
impl ReclaimReport {
    pub(super) fn from_store(report: plakous_agent::internal::StoreReclaimReport) -> Self {
        Self {
            objects: report.objects(),
            object_bytes: report.object_bytes(),
        }
    }
    /// Returns deleted content-addressed objects.
    #[must_use]
    pub const fn objects(self) -> u64 {
        self.objects
    }
    /// Returns deleted payload and edge bytes.
    #[must_use]
    pub const fn object_bytes(self) -> u64 {
        self.object_bytes
    }
}
