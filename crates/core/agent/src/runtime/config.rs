//! Provider-neutral runtime policies, budgets and implementation identity.

use crate::{AgentError, AgentErrorKind, SubscriptionLimits};
use plakous_model::{ContextAdmissionRequirement, GenerationLimits, ReasoningPolicy, Timestamp};
use std::time::Duration;
impl AgentRequestPolicy {
    /// Creates an exact portable generation and reasoning policy.
    #[must_use]
    pub const fn new(generation_limits: GenerationLimits, reasoning: ReasoningPolicy) -> Self {
        Self {
            generation_limits,
            reasoning,
            context_admission_requirement: ContextAdmissionRequirement::AnyBounded,
        }
    }

    /// Replaces portable generation limits.
    #[must_use]
    pub fn with_generation_limits(mut self, generation_limits: GenerationLimits) -> Self {
        self.generation_limits = generation_limits;
        self
    }

    /// Replaces portable reasoning intent.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: ReasoningPolicy) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Requires an exact context-evidence class before provider-visible work.
    #[must_use]
    pub const fn with_context_admission_requirement(
        mut self,
        requirement: ContextAdmissionRequirement,
    ) -> Self {
        self.context_admission_requirement = requirement;
        self
    }

    /// Returns portable generation limits.
    #[must_use]
    pub const fn generation_limits(&self) -> &GenerationLimits {
        &self.generation_limits
    }

    /// Returns portable reasoning intent.
    #[must_use]
    pub const fn reasoning(&self) -> ReasoningPolicy {
        self.reasoning
    }

    /// Returns the minimum context evidence class required for every model operation.
    #[must_use]
    pub const fn context_admission_requirement(&self) -> ContextAdmissionRequirement {
        self.context_admission_requirement
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            prompt_queue_capacity: 16,
            control_queue_capacity: 64,
            steering_queue_capacity: 16,
            follow_up_queue_capacity: 16,
            event_capacity: 256,
            event_byte_capacity: crate::event::MAX_EVENT_DEBT_BYTES,
            max_event_subscribers: 128,
            max_model_rounds: 32,
            tool_progress_capacity: 64,
            observation_budget_cap_bytes: None,
            effect_decision_timeout: Duration::from_secs(300),
            model_cleanup_timeout: Duration::from_secs(30),
            model_operation_timeout: Duration::from_secs(300),
            scheduled_compaction_enabled: false,
        }
    }
}

impl AgentConfig {
    /// Replaces the prompt queue item bound.
    #[must_use]
    pub const fn with_prompt_queue_capacity(mut self, capacity: usize) -> Self {
        self.prompt_queue_capacity = capacity;
        self
    }

    /// Replaces the control queue item bound.
    #[must_use]
    pub const fn with_control_queue_capacity(mut self, capacity: usize) -> Self {
        self.control_queue_capacity = capacity;
        self
    }

    /// Replaces the steering queue item bound.
    #[must_use]
    pub const fn with_steering_queue_capacity(mut self, capacity: usize) -> Self {
        self.steering_queue_capacity = capacity;
        self
    }

    /// Replaces the follow-up queue item bound.
    #[must_use]
    pub const fn with_follow_up_queue_capacity(mut self, capacity: usize) -> Self {
        self.follow_up_queue_capacity = capacity;
        self
    }

    /// Replaces the shared retained-event item bound and default turn-feed debt bound.
    #[must_use]
    pub const fn with_event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity;
        self
    }

    /// Replaces the shared retained-event byte bound and default turn-feed debt bound.
    #[must_use]
    pub const fn with_event_byte_capacity(mut self, capacity: usize) -> Self {
        self.event_byte_capacity = capacity;
        self
    }

    /// Replaces the user-created presentation subscriber bound.
    #[must_use]
    pub const fn with_max_event_subscribers(mut self, maximum: usize) -> Self {
        self.max_event_subscribers = maximum;
        self
    }

    /// Replaces the model-operation limit per logical turn.
    #[must_use]
    pub const fn with_max_model_rounds(mut self, maximum: usize) -> Self {
        self.max_model_rounds = maximum;
        self
    }

    /// Caps the aggregate per-model-round observation budget, never raising it.
    ///
    /// A cap can only shrink the budget the policy already derives, so setting
    /// one can never let a round observe more than it otherwise would.
    #[must_use]
    pub const fn with_observation_budget_cap_bytes(mut self, cap: Option<usize>) -> Self {
        self.observation_budget_cap_bytes = cap;
        self
    }

    /// Replaces the per-tool progress queue item bound.
    #[must_use]
    pub const fn with_tool_progress_capacity(mut self, capacity: usize) -> Self {
        self.tool_progress_capacity = capacity;
        self
    }

    /// Replaces the operator effect-decision deadline.
    #[must_use]
    pub const fn with_effect_decision_timeout(mut self, timeout: Duration) -> Self {
        self.effect_decision_timeout = timeout;
        self
    }

    /// Replaces the joined model cleanup deadline.
    #[must_use]
    pub const fn with_model_cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.model_cleanup_timeout = timeout;
        self
    }

    /// Replaces the model-operation start bound. It bounds a model call until
    /// its stream starts and the non-streaming model operations (recovery
    /// lookups, authority revalidation, local-composition waits); a started
    /// stream is bounded by its provider transport idle bound instead.
    #[must_use]
    pub const fn with_model_operation_timeout(mut self, timeout: Duration) -> Self {
        self.model_operation_timeout = timeout;
        self
    }

    /// Enables or disables scheduled compact at the tool-group fence.
    #[must_use]
    pub const fn with_scheduled_compaction_enabled(mut self, enabled: bool) -> Self {
        self.scheduled_compaction_enabled = enabled;
        self
    }

    /// Reports whether the tool-group fence may start Scheduled compact.
    #[must_use]
    pub const fn scheduled_compaction_enabled(self) -> bool {
        self.scheduled_compaction_enabled
    }

    pub(super) fn validate(self) -> Result<Self, AgentError> {
        if self.prompt_queue_capacity == 0
            || self.control_queue_capacity == 0
            || self.steering_queue_capacity == 0
            || self.follow_up_queue_capacity == 0
            || self.event_capacity == 0
            || self.event_byte_capacity == 0
            || self.max_event_subscribers == 0
            || self.max_model_rounds == 0
            || self.tool_progress_capacity == 0
            || self.effect_decision_timeout.is_zero()
            || self.model_cleanup_timeout.is_zero()
            || self.model_operation_timeout.is_zero()
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "agent capacities and round limits must be nonzero",
            ));
        }
        if let Err(error) = SubscriptionLimits::new(self.event_capacity, self.event_byte_capacity) {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                error.to_string(),
            ));
        }
        if self.prompt_queue_capacity > 4_096
            || self.control_queue_capacity > 4_096
            || self.steering_queue_capacity > 4_096
            || self.follow_up_queue_capacity > 4_096
            || self.event_capacity > 65_536
            || self.event_byte_capacity > 256 * 1024 * 1024
            || self.max_event_subscribers > 4_096
            || self.max_model_rounds > 1_024
            || self.tool_progress_capacity > 4_096
            || self.effect_decision_timeout > Duration::from_secs(86_400)
            || self.model_cleanup_timeout > Duration::from_secs(3_600)
            || self.model_operation_timeout > Duration::from_secs(3_600)
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "agent capacities or round limits exceed the K0a safety ceiling",
            ));
        }
        Ok(self)
    }
}

pub(super) fn timestamp_after(timestamp: Timestamp, duration: Duration) -> Timestamp {
    let nanoseconds = timestamp
        .nanoseconds()
        .saturating_add(duration.subsec_nanos());
    let carry = u64::from(nanoseconds >= 1_000_000_000);
    let nanoseconds = nanoseconds % 1_000_000_000;
    let seconds = timestamp
        .seconds()
        .saturating_add(duration.as_secs())
        .saturating_add(carry);
    Timestamp::new(seconds, nanoseconds).unwrap_or(timestamp)
}

pub(super) fn timestamp_remaining(now: Timestamp, deadline: Timestamp) -> Duration {
    if deadline <= now {
        return Duration::ZERO;
    }
    let mut seconds = deadline.seconds() - now.seconds();
    let nanoseconds = if deadline.nanoseconds() >= now.nanoseconds() {
        deadline.nanoseconds() - now.nanoseconds()
    } else {
        seconds = seconds.saturating_sub(1);
        1_000_000_000 + deadline.nanoseconds() - now.nanoseconds()
    };
    Duration::new(seconds, nanoseconds)
}

/// Immutable provider-neutral policy applied to every model request round.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct AgentRequestPolicy {
    generation_limits: GenerationLimits,
    reasoning: ReasoningPolicy,
    context_admission_requirement: ContextAdmissionRequirement,
}

/// Stack every executor thread that polls this runtime's actor needs.
///
/// The turn loop is one chain of `async fn` polls: `run` → `run_loop` →
/// `run_admitted_turn` → `run_turn` → `run_turn_from_state` →
/// `execute_tool_batch` → `execute_local_composition_outer` → cell driver →
/// serial nested dispatch → `execute_restored_admitted` →
/// `execute_mutating_effect` → `apply_process_session_update` →
/// `prepare_operation` → `publish_checkpoint` → bundle codec. Unoptimized, a
/// debug build keeps one by-value temporary per awaited future in each poll
/// frame, so the frames of that chain measure 2.09 MiB at the deepest known
/// path — a nested `exec_command` that outlives its yield window and is
/// recorded as a live process session (the product host
/// aborted with `tokio-rt-worker has overflowed its stack` on the 2 MiB
/// `std::thread` default while a child ran `sleep 40`). Boxing the futures
/// does not shrink those frames; only the executor's thread stack does. Hosts
/// build their executor with `thread_stack_size(RUNTIME_EXECUTOR_STACK_BYTES)`
/// and the runtime's own tests drive that chain on exactly this much stack.
pub const RUNTIME_EXECUTOR_STACK_BYTES: usize = 8 << 20;

pub(super) const RUNTIME_COMPONENT_CODEC_REVISION: u32 = 1;

/// Hard limits for the initial in-memory agent path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentConfig {
    /// Maximum admitted prompts waiting behind the active turn.
    pub(in crate::runtime) prompt_queue_capacity: usize,
    /// Maximum control requests awaiting actor acknowledgement.
    pub(in crate::runtime) control_queue_capacity: usize,
    /// Maximum steering inputs retained for the active exact turn.
    pub(in crate::runtime) steering_queue_capacity: usize,
    /// Maximum follow-up turns retained behind the active exact turn.
    pub(in crate::runtime) follow_up_queue_capacity: usize,
    /// Maximum retained live events per turn before a subscriber lags.
    pub(in crate::runtime) event_capacity: usize,
    /// Maximum retained live-event bytes and default subscriber byte debt.
    pub(in crate::runtime) event_byte_capacity: usize,
    /// Maximum simultaneous event subscriptions for one agent.
    pub(in crate::runtime) max_event_subscribers: usize,
    /// Maximum model operations within one logical turn.
    pub(in crate::runtime) max_model_rounds: usize,
    /// Maximum queued progress messages for one running tool.
    pub(in crate::runtime) tool_progress_capacity: usize,
    /// A cap on the per-turn observation budget, never a raise.
    ///
    /// The budget is derived from the prepared request's own headroom, which
    /// on a real route is hundreds of kilobytes, so nothing a test can
    /// plausibly return from a tool reaches it. An end-to-end case that needs
    /// to watch a real process clip a real tool result therefore has to lower
    /// the bound from outside. The runtime does not read the environment; the
    /// composition root does, and passes the figure here.
    pub(in crate::runtime) observation_budget_cap_bytes: Option<usize>,
    /// Maximum time an operator may leave a tool decision pending.
    pub(in crate::runtime) effect_decision_timeout: Duration,
    /// Maximum joined cleanup time after cancelling one model operation.
    pub(in crate::runtime) model_cleanup_timeout: Duration,
    /// Start bound for one model call and bound for non-streaming model operations.
    pub(in crate::runtime) model_operation_timeout: Duration,
    /// When true, the driver may start Scheduled compact after a committed tool group.
    pub(in crate::runtime) scheduled_compaction_enabled: bool,
}
