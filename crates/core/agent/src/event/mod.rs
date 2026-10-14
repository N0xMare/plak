//! Versioned causal observations with one atomic hub and one shared projection policy.
//! Publication serializes preparation through notification; subscriptions remain bounded cursors.

mod hub;
mod projection;
mod types;

pub(crate) use hub::EventPipeline;
pub use hub::subscription::{
    EventSubscription, ResumeOutcome, SubscriptionEnd, TurnEventError, TurnEvents, ViewAttachment,
    ViewError,
};
pub(crate) use hub::{EventContext, EventHub, EventHubLimits};
pub use projection::{
    AgentViewReducer, AgentViewSnapshot, AgentViewState, ConversationItemView,
    PendingEffectDecisionView, ReducerError, ToolActivityView, TurnView, TurnViewState,
};
pub(crate) use types::MAX_EVENT_DEBT_BYTES;
pub use types::{
    AgentEvent, AgentEventKind, CompactionEventOutcome, CompactionEventReason,
    EffectDecisionResolution, EventCursor, EventDelivery, EventDeliveryClass, EventLossKind,
    EventLostRange, EventOrigin, EventScope, EventSequence, EventToolProgress, EventVisibility,
    MIN_EVENT_DEBT_BYTES, NormalizedModelEventKind, SubscriptionLimitError, SubscriptionLimits,
    ToolAuditIdentity, ToolCallAttemptIdentity, ToolCallObservation, ToolCallerClass,
    ToolCallerLineageView, ToolProgressSummary, ToolResultObservation, ViewRequest,
};

#[cfg(test)]
mod test_support;
