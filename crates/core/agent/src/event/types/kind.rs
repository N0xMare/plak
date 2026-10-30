use super::{
    AgentErrorKind, Arc, BlockStart, BlockStarted, CompactionEventOutcome, CompactionEventReason,
    CompactionSummary, ContextInputParts, ConversationItemView, Deserialize, EVENT_ENVELOPE_BYTES,
    EffectDecisionPrompt, EffectDecisionRef, EffectDecisionResolution, EventToolProgress,
    EventVisibility, ModelEvent, NormalizedModelEventKind, Serialize, SessionWorkflowId, Timestamp,
    ToolAuditIdentity, ToolCallAttemptIdentity, ToolCallObservation, ToolCallerLineageView,
    ToolName, ToolProgressSummary, ToolResultObservation, ToolRuntimeErrorKind, TurnUsage,
    effect_decision_prompt_option_wire, fmt, serialized_size, tool_runtime_error_kind_wire,
};

/// Typed data carried by one causal event envelope.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub enum AgentEventKind {
    /// Hint that a process became session-owned; query `Agent::jobs` for state.
    JobPromoted {
        /// Vendor handle and process job number.
        number: u32,
    },
    /// Hint that a process finished; canonical job rows are delivery authority.
    JobFinished {
        /// Vendor handle and process job number.
        number: u32,
    },
    /// The owning agent event source reached its ordered terminal boundary.
    AgentClosed,
    /// A reopen applied a freshly derived output bound in place of the one the
    /// session was created under.
    ///
    /// A bound the product derives from a route's attestation moves when the
    /// provider's catalog moves. Reopen re-derives it rather than refusing the
    /// session, and says so here: the session's output contract changed, and a
    /// change to what the model may produce belongs in the transcript.
    OutputBoundRederived {
        /// The bound the session was created under.
        previous: Option<std::num::NonZeroU32>,
        /// The bound this reopen derived and will use.
        current: Option<std::num::NonZeroU32>,
    },
    /// The driver accepted a turn from its FIFO queue.
    TurnStarted,
    /// One canonical model operation began.
    ModelStarted,
    /// The admitted provider request is about to receive its first poll.
    /// Preparation and authority revalidation have completed.
    ModelDispatchStarted,
    /// A classified response reached its presentation boundary, before any
    /// tool rows. Content views receive this without the response aggregate.
    ResponseSettled {
        /// The response must not contribute saved presentation activity.
        discardable: bool,
    },
    /// Measured parts and configured tool names of the request just prepared.
    RequestPrepared {
        /// Measured byte partitions of the lowered request, when available.
        input_parts: Option<ContextInputParts>,
        /// Occupancy on the runtime context scale.
        occupied_tokens: Option<u64>,
        /// Complete request admission upper bound.
        prepared_input_upper_bound: Option<u64>,
        /// Exact configured names, removed from metadata-only projection.
        tool_names: Vec<ToolName>,
    },
    /// One normalized model event arrived.
    Model {
        /// Stable structural category retained under metadata projection.
        kind: NormalizedModelEventKind,
        /// Canonical content, absent from metadata-only projections.
        event: Option<Box<ModelEvent>>,
    },
    /// Policy paused one exact tool call for an operator decision.
    EffectDecisionRequired {
        /// Immutable identity of the validated invocation awaiting decision.
        identity: ToolAuditIdentity,
        /// Binding-digest-addressed decision reference.
        decision: EffectDecisionRef,
        /// Descriptive content, absent from metadata-only projections.
        #[serde(with = "effect_decision_prompt_option_wire")]
        prompt: Option<EffectDecisionPrompt>,
        /// Display timestamp corresponding to the monotonic expiry deadline.
        expires_at: Timestamp,
    },
    /// A pending decision reached a terminal resolution.
    EffectDecisionResolved {
        /// Exact identity captured by the corresponding required event.
        identity: ToolAuditIdentity,
        /// Exact reference originally published by the required event.
        decision: EffectDecisionRef,
        /// Terminal resolution.
        resolution: EffectDecisionResolution,
    },
    /// A complete local tool call passed the policy/decision boundary.
    ToolAuthorized {
        /// Immutable registered and provider-facing audit identity.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
    },
    /// An authorized local tool call crossed its exact dispatch boundary.
    ToolStarted {
        /// Exact identity captured at authorization.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Bounded redacted arguments, present only under content visibility.
        observation: Option<ToolCallObservation>,
    },
    /// A bounded local tool progress update.
    ToolProgress {
        /// Exact identity captured at authorization.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Content-free ordered progress counters retained at every visibility.
        summary: ToolProgressSummary,
        /// Progress content, absent from metadata-only projections.
        progress: Option<EventToolProgress>,
    },
    /// A model-visible tool rejection terminalized before a validated invocation existed.
    ToolPreparationRejected {
        /// Attempted model-facing identity without a canonical/executable identity claim.
        attempt: ToolCallAttemptIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,

        /// Stable redaction-safe preparation failure category.
        #[serde(with = "tool_runtime_error_kind_wire")]
        reason: ToolRuntimeErrorKind,
        /// The rejection's own descriptive message, bounded and redacted.
        ///
        /// The category alone does not diagnose a rejection: every schema
        /// refusal is `InvalidArguments`, while the message says which
        /// constraint the call missed. Present only under content visibility.
        detail: Option<Arc<str>>,
        /// Bounded redacted subject and arguments of the refused call.
        ///
        /// A rejected call never starts, so nothing else in the stream carries
        /// what it sent. Present only under content visibility.
        observation: Option<ToolCallObservation>,
    },
    /// A validated local tool call produced a canonical model-visible result or rejection.
    ToolCompleted {
        /// Exact validated identity retained throughout the call lifecycle.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Bounded redacted outcome, present only under content visibility.
        observation: Option<ToolResultObservation>,
    },
    /// Steering was admitted into the active turn's bounded queue.
    SteeringQueued {
        /// One-based position at admission time.
        position: usize,
    },
    /// Steering was committed at a safe boundary.
    SteeringApplied,
    /// A model request is waiting for a slot in a shared request budget.
    ///
    /// Published once per wait, before the request's operation deadline
    /// starts; the request proceeds with `ModelDispatchStarted` once a slot
    /// frees. `slots` is the host's label for the budget.
    ModelRequestQueued {
        /// The host-supplied label of the budget the request waits on.
        slots: String,
    },
    /// An undispatched request is waiting for the selected credential.
    ModelCredentialWait {
        /// True while parked; false when readiness, cancellation or shutdown ends it.
        waiting: bool,
    },
    /// The turn spent its model-operation budget and is wrapping up.
    ///
    /// Published when the runtime injects its budget note and before the one
    /// further round it runs with tool selection withdrawn. A surface that
    /// renders this tells the reader why the assistant stopped exploring; a
    /// turn that ends without it ended for some other reason.
    OperationBudgetExhausted,
    /// A response reached its output bound before its local tool call was
    /// complete, and the turn is retrying.
    ///
    /// Published when the runtime discards the truncated response, which ran
    /// nothing, and injects the note that tells the model to retry with less
    /// output per call. `bound` is the request's output bound, when it sent one.
    /// A surface that renders this explains why the streamed text it showed
    /// did not become part of the conversation.
    OutputLimitRetry {
        /// The per-response output bound the truncated request carried.
        bound: Option<std::num::NonZeroU32>,
    },
    /// Tool-result bodies were replaced so the turn could keep going.
    ///
    /// Published by the third compaction rung, which replaces the oldest
    /// completed tool-result bodies of the current turn with bounded
    /// placeholders when neither the provider-native facet nor the semantic
    /// fold can act. A surface that renders this tells the reader why earlier
    /// tool output is no longer quoted verbatim.
    ToolResultsReplaced {
        /// Tool results whose bodies were replaced.
        results: u32,
        /// Total bytes of the replaced bodies.
        bytes: u64,
        /// Unanchored whole-request estimate before the placeholder pass (the
        /// measured bound when the request has no parts).
        tokens_before: u64,
        /// Unanchored whole-request estimate after the placeholder pass, on
        /// the same basis as `tokens_before`.
        tokens_after: u64,
    },
    /// Observes the observation budget truncating one tool result.
    ///
    /// A clipped result changes what the model can answer from, and until this
    /// existed the only trace was the model's own prose: a surface saw a tool
    /// succeed and had no way to know its output had been cut. `bound` is the
    /// budget this result was measured against, which is what makes
    /// `bytes_dropped` mean anything to a reader.
    ObservationClipped {
        /// Tool whose observation was truncated.
        name: Arc<str>,
        /// Observation bytes that survived into canonical history.
        bytes_kept: u64,
        /// Observation bytes the budget removed.
        bytes_dropped: u64,
        /// Bytes still available to this result before it was clipped.
        bound: u64,
    },
    /// A committed next-round capacity fallback replaced one tool result body.
    ///
    /// These are bytes in the already bounded canonical result, so they must
    /// not be added to raw-result `ObservationClipped` byte counts.
    ObservationReplaced {
        /// Text bytes in the canonical result before replacement.
        bytes_before: u64,
        /// Text bytes in the committed replacement result.
        bytes_after: u64,
    },
    /// Conversation items became semantically committed.
    ConversationCommitted {
        /// Number of newly committed ordered items.
        item_count: usize,
        /// Canonical items, absent from metadata-only projections.
        items: Option<Arc<[ConversationItemView]>>,
    },
    /// Observes compaction starting on an active turn (tool-group boundary).
    CompactionStarted {
        /// Exact workflow identity shared with the change feed.
        workflow_id: SessionWorkflowId,
        /// Scheduled vs manual compact.
        reason: CompactionEventReason,
    },
    /// Observes compaction reaching a product-visible terminal on an active turn.
    CompactionFinished {
        /// Workflow identity from the matching started observation.
        workflow_id: SessionWorkflowId,
        /// Compact terminal classification.
        outcome: CompactionEventOutcome,
        /// Elapsed milliseconds from the matching started observation.
        elapsed_ms: u64,
        /// What a committed compaction did, when the successor records it.
        ///
        /// Always `None` for a non-committed terminal: there is nothing to
        /// report about a compaction that published no successor.
        summary: Option<CompactionSummary>,
        /// Redacted operator reason for a non-committed terminal.
        ///
        /// A `failed` or `unavailable` compaction that says only *that* it
        /// failed is undiagnosable from the log alone; the live growth runs of
        /// 2026-09-18 produced exactly that. `None` is for the outcomes that
        /// have nothing to explain.
        detail: Option<Arc<str>>,
    },
    /// Observes a completed turn-result boundary without replacing that result.
    TurnCompleted {
        /// Complete checked usage for the logical turn.
        usage: TurnUsage,
    },
    /// Observes a failed or cancelled turn-result boundary without terminal authority.
    TurnFailed {
        /// Redaction-safe category only; details arrive through the result.
        kind: AgentErrorKind,
        /// Complete checked usage observed before failure.
        usage: TurnUsage,
    },
}

impl AgentEventKind {
    /// Constructs a content-bearing normalized model event.
    #[must_use]
    pub fn model(event: ModelEvent) -> Self {
        Self::Model {
            kind: NormalizedModelEventKind::from(&event),
            event: Some(Box::new(event)),
        }
    }

    /// Returns a presentation-safe normalized model event when visible.
    #[must_use]
    pub fn model_event(&self) -> Option<&ModelEvent> {
        match self {
            Self::Model { event, .. } => event.as_deref(),
            _ => None,
        }
    }

    /// Returns immutable identity evidence for a validated tool lifecycle event.
    #[must_use]
    pub const fn tool_audit_identity(&self) -> Option<&ToolAuditIdentity> {
        match self {
            Self::EffectDecisionRequired { identity, .. }
            | Self::EffectDecisionResolved { identity, .. }
            | Self::ToolAuthorized { identity, .. }
            | Self::ToolStarted { identity, .. }
            | Self::ToolProgress { identity, .. }
            | Self::ToolCompleted { identity, .. } => Some(identity),
            _ => None,
        }
    }

    /// Returns the caller lineage retained on a tool lifecycle event.
    #[must_use]
    pub const fn tool_caller_lineage(&self) -> Option<&ToolCallerLineageView> {
        match self {
            Self::ToolAuthorized { lineage, .. }
            | Self::ToolStarted { lineage, .. }
            | Self::ToolProgress { lineage, .. }
            | Self::ToolPreparationRejected { lineage, .. }
            | Self::ToolCompleted { lineage, .. } => Some(lineage),
            _ => None,
        }
    }

    /// Returns attempted identity evidence for a pre-validation terminal rejection.
    #[must_use]
    pub const fn tool_call_attempt_identity(&self) -> Option<&ToolCallAttemptIdentity> {
        match self {
            Self::ToolPreparationRejected { attempt, .. } => Some(attempt),
            _ => None,
        }
    }

    pub(in crate::event) fn projected(&self, visibility: EventVisibility) -> Self {
        match self {
            Self::JobPromoted { number } => Self::JobPromoted { number: *number },
            Self::JobFinished { number } => Self::JobFinished { number: *number },
            // Two integers naming a budget: nothing to redact at any visibility.
            Self::OutputBoundRederived { previous, current } => Self::OutputBoundRederived {
                previous: *previous,
                current: *current,
            },
            Self::Model { kind, event } => Self::Model {
                kind: *kind,
                event: if visibility == EventVisibility::Content {
                    event
                        .as_deref()
                        .and_then(presentation_safe_model_event)
                        .map(Box::new)
                } else {
                    None
                },
            },
            Self::EffectDecisionRequired {
                identity,
                decision,
                prompt,
                expires_at,
            } => Self::EffectDecisionRequired {
                identity: identity.clone(),
                decision: decision.clone(),
                prompt: (visibility == EventVisibility::Content)
                    .then(|| prompt.clone())
                    .flatten(),
                expires_at: *expires_at,
            },
            Self::ConversationCommitted { item_count, items } => Self::ConversationCommitted {
                item_count: *item_count,
                items: (visibility == EventVisibility::Content)
                    .then(|| items.clone())
                    .flatten(),
            },
            Self::EffectDecisionResolved {
                identity,
                decision,
                resolution,
            } => Self::EffectDecisionResolved {
                identity: identity.clone(),
                decision: decision.clone(),
                resolution: *resolution,
            },
            Self::SteeringQueued { position } => Self::SteeringQueued {
                position: *position,
            },
            Self::ToolAuthorized { identity, lineage } => Self::ToolAuthorized {
                identity: identity.clone(),
                lineage: lineage.clone(),
            },
            Self::ToolStarted {
                identity,
                lineage,
                observation,
            } => Self::ToolStarted {
                identity: identity.clone(),
                lineage: lineage.clone(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::ToolProgress {
                identity,
                lineage,
                summary,
                progress,
            } => Self::ToolProgress {
                identity: identity.clone(),
                lineage: lineage.clone(),
                summary: *summary,
                progress: (visibility == EventVisibility::Content)
                    .then(|| progress.clone())
                    .flatten(),
            },
            Self::ToolPreparationRejected {
                attempt,
                lineage,
                reason,
                detail,
                observation,
            } => Self::ToolPreparationRejected {
                attempt: attempt.clone(),
                lineage: lineage.clone(),
                reason: *reason,
                detail: (visibility == EventVisibility::Content)
                    .then(|| detail.clone())
                    .flatten(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::ToolCompleted {
                identity,
                lineage,
                observation,
            } => Self::ToolCompleted {
                identity: identity.clone(),
                lineage: lineage.clone(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::TurnFailed { kind, usage } => Self::TurnFailed {
                kind: *kind,
                usage: *usage,
            },
            Self::AgentClosed => Self::AgentClosed,
            Self::TurnStarted => Self::TurnStarted,
            Self::ModelStarted => Self::ModelStarted,
            Self::ModelDispatchStarted => Self::ModelDispatchStarted,
            Self::ResponseSettled { discardable } => Self::ResponseSettled {
                discardable: *discardable,
            },
            Self::RequestPrepared {
                input_parts,
                occupied_tokens,
                prepared_input_upper_bound,
                tool_names,
            } => Self::RequestPrepared {
                input_parts: *input_parts,
                occupied_tokens: *occupied_tokens,
                prepared_input_upper_bound: *prepared_input_upper_bound,
                tool_names: if visibility == EventVisibility::Content {
                    tool_names.clone()
                } else {
                    Vec::new()
                },
            },
            Self::SteeringApplied => Self::SteeringApplied,
            Self::OperationBudgetExhausted => Self::OperationBudgetExhausted,
            Self::ModelRequestQueued { slots } => Self::ModelRequestQueued {
                slots: slots.clone(),
            },
            Self::ModelCredentialWait { waiting } => {
                Self::ModelCredentialWait { waiting: *waiting }
            }
            Self::OutputLimitRetry { bound } => Self::OutputLimitRetry { bound: *bound },
            Self::ToolResultsReplaced {
                results,
                bytes,
                tokens_before,
                tokens_after,
            } => Self::ToolResultsReplaced {
                results: *results,
                bytes: *bytes,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
            },
            Self::ObservationClipped {
                name,
                bytes_kept,
                bytes_dropped,
                bound,
            } => Self::ObservationClipped {
                name: Arc::clone(name),
                bytes_kept: *bytes_kept,
                bytes_dropped: *bytes_dropped,
                bound: *bound,
            },
            Self::ObservationReplaced {
                bytes_before,
                bytes_after,
            } => Self::ObservationReplaced {
                bytes_before: *bytes_before,
                bytes_after: *bytes_after,
            },
            Self::CompactionStarted {
                workflow_id,
                reason,
            } => Self::CompactionStarted {
                workflow_id: *workflow_id,
                reason: *reason,
            },
            Self::CompactionFinished {
                workflow_id,
                outcome,
                elapsed_ms,
                summary,
                detail,
            } => Self::CompactionFinished {
                workflow_id: *workflow_id,
                outcome: *outcome,
                elapsed_ms: *elapsed_ms,
                summary: *summary,
                detail: detail.clone(),
            },
            Self::TurnCompleted { usage } => Self::TurnCompleted { usage: *usage },
        }
    }

    pub(in crate::event) fn retained_bytes(&self) -> usize {
        let content = match self {
            Self::Model {
                event: Some(event), ..
            } => {
                // Retry logging facts are skipped on the wire but still own
                // strings while the event is retained in a live window.
                let logging_bytes = match event.as_ref() {
                    ModelEvent::Retry(retry) => retry
                        .request()
                        .map_or(0, |(host, route, _)| host.len().saturating_add(route.len())),
                    _ => 0,
                };
                serialized_size(event.as_ref()).saturating_add(logging_bytes)
            }
            Self::EffectDecisionRequired {
                identity,
                prompt: Some(prompt),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(prompt.description().as_str().len()),
            Self::ConversationCommitted {
                items: Some(items), ..
            } => items.iter().fold(0_usize, |total, item| {
                total.saturating_add(
                    128_usize.saturating_add(item.display_text().map_or(0, str::len)),
                )
            }),
            Self::ToolStarted {
                identity,
                observation: Some(observation),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(observation.retained_bytes()),
            Self::ToolCompleted {
                identity,
                observation: Some(observation),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(observation.retained_bytes()),
            Self::ToolProgress {
                identity,
                progress: Some(progress),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(progress.message().len()),
            Self::ToolAuthorized { identity, .. }
            | Self::EffectDecisionRequired {
                identity,
                prompt: None,
                ..
            }
            | Self::EffectDecisionResolved { identity, .. }
            | Self::ToolStarted {
                identity,
                observation: None,
                ..
            }
            | Self::ToolProgress {
                identity,
                progress: None,
                ..
            }
            | Self::ToolCompleted {
                identity,
                observation: None,
                ..
            } => identity.retained_bytes(),
            Self::ToolPreparationRejected {
                attempt,
                detail,
                observation,
                ..
            } => attempt
                .retained_bytes()
                .saturating_add(detail.as_ref().map_or(0, |detail| detail.len()))
                .saturating_add(
                    observation
                        .as_ref()
                        .map_or(0, ToolCallObservation::retained_bytes),
                ),
            Self::RequestPrepared { tool_names, .. } => {
                128 + tool_names
                    .iter()
                    .map(|name| name.as_str().len())
                    .sum::<usize>()
            }
            // Job notes carry two optional integers, retained inline.
            Self::AgentClosed
            | Self::JobPromoted { .. }
            | Self::JobFinished { .. }
            | Self::OutputBoundRederived { .. }
            | Self::TurnStarted
            | Self::ModelStarted
            | Self::ResponseSettled { .. }
            | Self::ModelDispatchStarted
            | Self::Model { event: None, .. }
            | Self::SteeringQueued { .. }
            | Self::SteeringApplied
            | Self::OperationBudgetExhausted
            | Self::ModelCredentialWait { .. }
            | Self::OutputLimitRetry { .. }
            | Self::ToolResultsReplaced { .. }
            | Self::ConversationCommitted { items: None, .. }
            | Self::ObservationClipped { .. }
            | Self::ObservationReplaced { .. }
            | Self::CompactionStarted { .. }
            | Self::TurnCompleted { .. }
            | Self::TurnFailed { .. } => 0,
            Self::ModelRequestQueued { slots } => slots.len(),
            Self::CompactionFinished { detail, .. } => {
                detail.as_ref().map_or(0, |detail| detail.len())
            }
        };
        let lineage = self
            .tool_caller_lineage()
            .map_or(0, ToolCallerLineageView::retained_bytes);
        EVENT_ENVELOPE_BYTES
            .saturating_add(content)
            .saturating_add(lineage)
    }
}

/// The presentation-safe projection of one model event, or `None` when a
/// Content view may see nothing of it.
///
/// Block boundaries are structural: a Content consumer needs `BlockStarted` to
/// attribute reasoning deltas to the visibility the provider declared for that
/// location and `BlockCompleted` to seal what it showed. They
/// pass with replay material stripped. A complete block, the completed
/// response, and replay bindings carry provider replay/opaque material and
/// never leave the runtime.
pub(super) fn presentation_safe_model_event(event: &ModelEvent) -> Option<ModelEvent> {
    match event {
        ModelEvent::Diagnostic(_)
        | ModelEvent::Retry(_)
        | ModelEvent::ResponseStarted(_)
        | ModelEvent::TextDelta(_)
        | ModelEvent::ReasoningDelta(_)
        | ModelEvent::ReasoningVisibilityBound(_)
        | ModelEvent::ToolInputDelta(_)
        | ModelEvent::CitationAdded(_)
        | ModelEvent::Usage(_)
        | ModelEvent::BlockCompleted(_) => Some(event.clone()),
        ModelEvent::BlockStarted(started) => {
            let block = match started.block() {
                BlockStart::Text { phase } => BlockStart::Text { phase: *phase },
                BlockStart::Reasoning { visibility, .. } => BlockStart::Reasoning {
                    visibility: *visibility,
                    replay: None,
                },
                BlockStart::ReasoningPending { .. } => {
                    BlockStart::ReasoningPending { replay: None }
                }
                BlockStart::ToolCall { .. } => started.block().clone(),
                BlockStart::Refusal => BlockStart::Refusal,
                // A complete block and any block kind this crate does not
                // know carry content it cannot vouch for.
                BlockStart::Complete(_) | _ => return None,
            };
            Some(ModelEvent::BlockStarted(BlockStarted::new(
                started.item_id(),
                started.location(),
                block,
            )))
        }
        _ => None,
    }
}

impl fmt::Debug for AgentEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut event = formatter.debug_struct("AgentEventKind");
        match self {
            Self::AgentClosed => event.field("kind", &"agent_closed"),
            Self::JobPromoted { number } => {
                event.field("kind", &"job_promoted").field("number", number)
            }
            Self::JobFinished { number } => {
                event.field("kind", &"job_finished").field("number", number)
            }
            Self::OutputBoundRederived { previous, current } => event
                .field("kind", &"output_bound_rederived")
                .field("previous", previous)
                .field("current", current),
            Self::TurnStarted => event.field("kind", &"turn_started"),
            Self::ModelStarted => event.field("kind", &"model_started"),
            Self::ModelDispatchStarted => event.field("kind", &"model_dispatch_started"),
            Self::ResponseSettled { discardable } => event
                .field("kind", &"response_settled")
                .field("discardable", discardable),
            Self::RequestPrepared { tool_names, .. } => event
                .field("kind", &"request_prepared")
                .field("tool_count", &tool_names.len()),
            Self::Model { kind, .. } => event
                .field("kind", &"model_event")
                .field("model_event_kind", kind),
            Self::EffectDecisionRequired {
                identity,
                decision,
                expires_at,
                ..
            } => event
                .field("kind", &"tool_decision_required")
                .field("identity", identity)
                .field("decision", decision)
                .field("expires_at", expires_at),
            Self::EffectDecisionResolved {
                identity,
                decision,
                resolution,
            } => event
                .field("kind", &"tool_decision_resolved")
                .field("identity", identity)
                .field("decision", decision)
                .field("resolution", resolution),
            Self::ToolAuthorized { identity, lineage } => event
                .field("kind", &"tool_authorized")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::ToolStarted {
                identity, lineage, ..
            } => event
                .field("kind", &"tool_started")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::ToolProgress {
                identity,
                lineage,
                progress,
                ..
            } => event
                .field("kind", &"tool_progress")
                .field("identity", identity)
                .field("lineage", lineage)
                .field("progress", progress),
            Self::ToolPreparationRejected {
                attempt,
                lineage,
                reason,
                ..
            } => event
                .field("kind", &"tool_preparation_rejected")
                .field("attempt", attempt)
                .field("lineage", lineage)
                .field("reason", reason)
                .field("detail", &"<redacted>")
                .field("observation", &"<redacted>"),
            Self::ToolCompleted {
                identity, lineage, ..
            } => event
                .field("kind", &"tool_completed")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::SteeringQueued { position } => event
                .field("kind", &"steering_queued")
                .field("position", position),
            Self::SteeringApplied => event.field("kind", &"steering_applied"),
            Self::OperationBudgetExhausted => event.field("kind", &"operation_budget_exhausted"),
            Self::ModelRequestQueued { slots } => event
                .field("kind", &"model_request_queued")
                .field("slots", slots),
            Self::ModelCredentialWait { waiting } => event
                .field("kind", &"model_credential_wait")
                .field("waiting", waiting),
            Self::OutputLimitRetry { bound } => event
                .field("kind", &"output_limit_retry")
                .field("bound", bound),
            Self::ToolResultsReplaced {
                results,
                bytes,
                tokens_before,
                tokens_after,
            } => event
                .field("kind", &"context_placeholders")
                .field("results", results)
                .field("bytes", bytes)
                .field("tokens_before", tokens_before)
                .field("tokens_after", tokens_after),
            Self::ObservationClipped {
                name,
                bytes_kept,
                bytes_dropped,
                bound,
            } => event
                .field("kind", &"observation_clipped")
                .field("name", name)
                .field("bytes_kept", bytes_kept)
                .field("bytes_dropped", bytes_dropped)
                .field("bound", bound),
            Self::ObservationReplaced {
                bytes_before,
                bytes_after,
            } => event
                .field("kind", &"observation_replaced")
                .field("bytes_before", bytes_before)
                .field("bytes_after", bytes_after),
            Self::ConversationCommitted { item_count, .. } => event
                .field("kind", &"conversation_committed")
                .field("item_count", item_count)
                .field("items", &"<redacted>"),
            Self::CompactionStarted {
                workflow_id,
                reason,
            } => event
                .field("kind", &"compaction_started")
                .field("workflow_id", workflow_id)
                .field("reason", reason),
            Self::CompactionFinished {
                workflow_id,
                outcome,
                elapsed_ms,
                summary,
                detail,
            } => event
                .field("kind", &"compaction_finished")
                .field("workflow_id", workflow_id)
                .field("outcome", outcome)
                .field("elapsed_ms", elapsed_ms)
                .field("summary", summary)
                .field("detail", &detail.is_some()),
            Self::TurnCompleted { usage } => {
                event.field("kind", &"turn_completed").field("usage", usage)
            }
            Self::TurnFailed { kind, usage } => event
                .field("kind", &"turn_failed")
                .field("error_kind", kind)
                .field("usage", usage),
        }
        .finish()
    }
}
