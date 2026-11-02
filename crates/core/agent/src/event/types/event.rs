use super::{
    AgentEvent, AgentEventKind, Arc, AttemptId, BranchId, CallId, DataClassification,
    DriverInstanceId, EventCursor, EventDeliveryClass, EventId, EventOrigin, EventScope,
    EventSequence, EventVisibility, NormalizedModelEventKind, RunId, SessionId, Timestamp, TurnId,
    fmt,
};

impl AgentEvent {
    /// Returns the branch sequence.
    #[must_use]
    pub const fn branch_sequence(&self) -> EventSequence {
        self.branch_sequence
    }

    /// Returns the unique event identity.
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns the branch identity.
    #[must_use]
    pub const fn branch_id(&self) -> BranchId {
        self.branch_id
    }

    /// Returns the agent identity.
    #[must_use]
    pub const fn agent_id(&self) -> DriverInstanceId {
        self.agent_id
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the logical turn identity when applicable.
    #[must_use]
    pub const fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    /// Returns the model attempt identity when applicable.
    #[must_use]
    pub const fn attempt_id(&self) -> Option<AttemptId> {
        self.attempt_id
    }

    /// Returns the tool-call identity when applicable.
    #[must_use]
    pub const fn call_id(&self) -> Option<CallId> {
        self.call_id
    }

    /// Returns the sequence of the change-feed record of the commit this
    /// event reports (a turn terminal, a committed compaction), or the
    /// committed prefix at `ModelStarted`, when using a durable store.
    /// Observation only: the feed is authority.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<crate::feed::FeedSeq> {
        self.feed_seq
    }

    /// Returns the observation timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns the closest causal predecessor when one exists.
    #[must_use]
    pub const fn causal_parent(&self) -> Option<EventId> {
        self.causal_parent
    }

    /// Returns the originating component.
    #[must_use]
    pub const fn origin(&self) -> EventOrigin {
        self.origin
    }

    /// Returns the data-sensitivity classification.
    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    /// Returns the envelope schema version.
    #[must_use]
    pub const fn event_schema(&self) -> u32 {
        self.event_schema
    }

    /// Returns the projection policy revision.
    #[must_use]
    pub const fn projection_revision(&self) -> u32 {
        self.projection_revision
    }

    /// Returns the immutable visibility scope of this delivered projection.
    #[must_use]
    pub const fn visibility(&self) -> EventVisibility {
        self.visibility
    }

    /// Returns the immutable selection scope of this delivered projection.
    #[must_use]
    pub const fn scope(&self) -> EventScope {
        self.scope
    }

    /// Returns the event-kind schema version.
    #[must_use]
    pub const fn event_kind_version(&self) -> u32 {
        self.event_kind_version
    }

    /// Returns the typed payload.
    #[must_use]
    pub const fn kind(&self) -> &AgentEventKind {
        &self.kind
    }

    /// Returns the explicit backpressure class for this event.
    #[must_use]
    pub const fn delivery_class(&self) -> EventDeliveryClass {
        match &self.kind {
            AgentEventKind::Model {
                kind:
                    NormalizedModelEventKind::TextDelta
                    | NormalizedModelEventKind::ReasoningDelta
                    | NormalizedModelEventKind::ToolInputDelta,
                ..
            } => EventDeliveryClass::ModelDelta,
            AgentEventKind::ToolProgress { .. } => EventDeliveryClass::ToolProgress,
            AgentEventKind::AgentClosed
            | AgentEventKind::JobPromoted { .. }
            | AgentEventKind::JobFinished { .. }
            | AgentEventKind::OutputBoundRederived { .. }
            | AgentEventKind::TurnStarted
            | AgentEventKind::ModelStarted
            | AgentEventKind::ResponseSettled { .. }
            | AgentEventKind::ModelDispatchStarted
            | AgentEventKind::RequestPrepared { .. }
            | AgentEventKind::Model { .. }
            | AgentEventKind::EffectDecisionRequired { .. }
            | AgentEventKind::EffectDecisionResolved { .. }
            | AgentEventKind::ToolAuthorized { .. }
            | AgentEventKind::ToolStarted { .. }
            | AgentEventKind::ToolPreparationRejected { .. }
            | AgentEventKind::ToolCompleted { .. }
            | AgentEventKind::SteeringQueued { .. }
            | AgentEventKind::SteeringApplied
            | AgentEventKind::OperationBudgetExhausted
            | AgentEventKind::ModelRequestQueued { .. }
            | AgentEventKind::ModelCredentialWait { .. }
            | AgentEventKind::OutputLimitRetry { .. }
            | AgentEventKind::ToolResultsReplaced { .. }
            | AgentEventKind::ObservationClipped { .. }
            | AgentEventKind::ObservationReplaced { .. }
            | AgentEventKind::ConversationCommitted { .. }
            | AgentEventKind::CompactionStarted { .. }
            | AgentEventKind::CompactionFinished { .. }
            | AgentEventKind::TurnCompleted { .. }
            | AgentEventKind::TurnFailed { .. } => EventDeliveryClass::Structural,
        }
    }

    /// Returns a cursor through this event in its immutable projection scope.
    #[must_use]
    pub const fn cursor(&self) -> EventCursor {
        EventCursor {
            session_id: self.session_id,
            branch_id: self.branch_id,
            run_id: self.run_id,
            sequence: self.branch_sequence,
            event_schema: self.event_schema,
            projection_revision: self.projection_revision,
            visibility: self.visibility,
            scope: self.scope,
        }
    }

    pub(in crate::event) fn projected(
        &self,
        visibility: EventVisibility,
        scope: EventScope,
    ) -> Arc<Self> {
        let kind = self.kind.projected(visibility);
        let retained_bytes = kind.retained_bytes();
        // Spelled out so the source kind is not cloned only to be replaced.
        Arc::new(Self {
            branch_sequence: self.branch_sequence,
            event_id: self.event_id,
            session_id: self.session_id,
            branch_id: self.branch_id,
            agent_id: self.agent_id,
            run_id: self.run_id,
            turn_id: self.turn_id,
            attempt_id: self.attempt_id,
            call_id: self.call_id,
            feed_seq: self.feed_seq,
            timestamp: self.timestamp,
            causal_parent: self.causal_parent,
            origin: self.origin,
            classification: self.classification,
            event_schema: self.event_schema,
            projection_revision: self.projection_revision,
            visibility,
            scope,
            event_kind_version: self.event_kind_version,
            kind,
            retained_bytes,
            metadata_retained_bytes: self.metadata_retained_bytes,
            content_retained_bytes: self.content_retained_bytes,
        })
    }

    pub(in crate::event) const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub(in crate::event) const fn projected_retained_bytes(
        &self,
        visibility: EventVisibility,
    ) -> usize {
        match visibility {
            EventVisibility::Metadata => self.metadata_retained_bytes,
            EventVisibility::Content => self.content_retained_bytes,
        }
    }
}

impl fmt::Debug for AgentEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEvent")
            .field("branch_sequence", &self.branch_sequence)
            .field("event_id", &self.event_id)
            .field("session_id", &self.session_id)
            .field("branch_id", &self.branch_id)
            .field("agent_id", &self.agent_id)
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("timestamp", &self.timestamp)
            .field("causal_parent", &self.causal_parent)
            .field("origin", &self.origin)
            .field("classification", &self.classification)
            .field("event_schema", &self.event_schema)
            .field("projection_revision", &self.projection_revision)
            .field("visibility", &self.visibility)
            .field("scope", &self.scope)
            .field("event_kind_version", &self.event_kind_version)
            .field("data", &self.kind)
            .finish_non_exhaustive()
    }
}
