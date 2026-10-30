use super::{
    CompactReason, CompactSessionOutcome, CompactSessionRequest, ContextQuery,
    ContextStatusRequest, FeedSeq, SessionContextStatus, SessionContextStatusKind, SessionLocator,
    SessionOperationId, SessionRuntimeBinding, UserInput, fmt,
};

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
    pub fn with_pending_user_input(mut self, input: UserInput) -> Self {
        self.qualification = ContextQuery::WithPendingUserInput(input);
        self
    }
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
