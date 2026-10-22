//! Closed provider-neutral attempt host for complete Chat operations.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_channel::mpsc;
use futures_util::SinkExt;
use plakous_attempt::{
    AdmittedAwaitHooks, AdmittedAwaitPolicy, AdmittedCallDriver, AttemptBinding, ExchangeBreak,
    FamilyAttemptCore, FamilyCoreError, LedgerOnlyAdmission, LedgerPolicy, PhysicalRequestBinding,
    PhysicalRequestOutcome, ProviderAcceptance, ProviderRefusal, ReplaySafety,
    ServiceAdmissionSeal, SinkDeliveryError, TerminalFacts, call_admitted_operation,
    race_sink_delivery,
};
use plakous_model::{
    AttemptId, BlockStart, CancellationToken, ContentBlock, Digest, ModelCallContext, ModelEvent,
    ModelProviderRequestId, ModelResponse, ToolExecutionOwner,
};
use tower_service::Service;

use crate::{ChatFinishReason, ChatTerminalProjection};

/// Chat-family ledger policy: one service-gated physical request, one
/// authorization slot, and service closure at terminal success.
pub const CHAT_FAMILY_LEDGER_POLICY: LedgerPolicy = LedgerPolicy {
    require_service_admitted_for_dispatch: true,
    require_active_request_always: false,
    dual_auth_slots: false,
    clear_service_on_terminal_success: true,
};

/// Maximum service polls permitted after admitted cancellation is observed.
pub use plakous_attempt::MAX_ADMITTED_CLEANUP_POLLS as MAX_CHAT_ADMITTED_CLEANUP_POLLS;

/// Exact immutable authority for one credential-free Chat operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatOperationBinding {
    target: Digest,
    plan: Digest,
    operation: Digest,
    replay_safety: ReplaySafety,
}

impl ChatOperationBinding {
    /// Creates a binding from an exact target, validated plan, and lowered operation.
    #[must_use]
    pub const fn new(
        target: Digest,
        plan: Digest,
        operation: Digest,
        replay_safety: ReplaySafety,
    ) -> Self {
        Self {
            target,
            plan,
            operation,
            replay_safety,
        }
    }

    /// Returns the route/account/model/dialect target identity.
    #[must_use]
    pub const fn target(&self) -> Digest {
        self.target
    }
    /// Returns the validated canonical plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }
    /// Returns the exact credential-free operation identity.
    #[must_use]
    pub const fn operation(&self) -> Digest {
        self.operation
    }
    /// Returns what replaying the exact request body can cost.
    #[must_use]
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }
}

/// Exact logical Chat-attempt identity.
pub type ChatAttemptBinding = AttemptBinding<ChatOperationBinding, AttemptId>;
/// Exact active physical-request identity.
pub type ChatPhysicalRequestBinding = PhysicalRequestBinding<ChatAttemptBinding>;
/// Bounded receiver for normalized observations.
pub type ChatAttemptEventReceiver = mpsc::Receiver<Result<ModelEvent, ChatAttemptProtocolError>>;

/// Redaction-safe family/protocol failure supplied by a dialect hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatAttemptProtocolError {
    message: String,
    detail: Option<plakous_model::SafeDiagnostic>,
    terminal_facts: Option<TerminalFacts>,
    http_status: Option<u16>,
    refusal: Option<ProviderRefusal>,
    context_overflow: bool,
    usage_limit: bool,
    exchange_break: ExchangeBreak,
}

impl ChatAttemptProtocolError {
    /// Attaches the redacted physical transport tuple without changing human text.
    #[must_use]
    pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
        self.detail = Some(plakous_model::SafeDiagnostic::new(detail));
        self
    }

    /// Returns the optional redacted physical transport tuple.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail
            .as_ref()
            .map(plakous_model::SafeDiagnostic::as_str)
    }

    /// Creates a bounded, redaction-safe failure.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
            terminal_facts: None,
            http_status: None,
            refusal: None,
            context_overflow: false,
            usage_limit: false,
            exchange_break: ExchangeBreak::Protocol,
        }
    }

    /// Records the provider HTTP status this failure carried.
    #[must_use]
    pub const fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self
    }

    /// Names this failure as a refusal the provider itself identified.
    ///
    /// A dialect attaches this only when the provider said exactly why it
    /// refused. It does not by itself say the outcome is definite: the caller
    /// still weighs what the exchange produced.
    #[must_use]
    pub const fn with_refusal(mut self, refusal: ProviderRefusal) -> Self {
        self.refusal = Some(refusal);
        self
    }

    /// Returns the provider HTTP status, when the failure carried one.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Marks exhausted quota, billing, or entitlement from the error body.
    #[must_use]
    pub(crate) const fn with_usage_limit(mut self) -> Self {
        self.usage_limit = true;
        self.refusal = None;
        self
    }

    /// Whether the provider reported exhausted quota or entitlement.
    #[must_use]
    pub const fn usage_limit(&self) -> bool {
        self.usage_limit
    }

    /// Marks a rejection the provider gave because the request's input
    /// exceeded the model's context window. It is not a refusal to retry: the
    /// same request would be rejected again.
    #[must_use]
    pub const fn with_context_overflow(mut self) -> Self {
        self.context_overflow = true;
        self
    }

    /// Returns whether the provider rejected the input as over its window.
    #[must_use]
    pub const fn context_overflow(&self) -> bool {
        self.context_overflow
    }

    /// Names how the exchange broke; the kernel owning the exchange sets it.
    #[must_use]
    pub const fn with_exchange_break(mut self, exchange_break: ExchangeBreak) -> Self {
        self.exchange_break = exchange_break;
        self
    }

    /// Returns how the exchange broke, when no refusal was named.
    #[must_use]
    pub const fn exchange_break(&self) -> ExchangeBreak {
        self.exchange_break
    }

    /// Returns the provider's own name for this refusal, when it gave one.
    #[must_use]
    pub const fn refusal(&self) -> Option<ProviderRefusal> {
        self.refusal
    }

    pub(crate) const fn with_terminal_facts(mut self, facts: TerminalFacts) -> Self {
        self.terminal_facts = Some(facts);
        self
    }

    /// Returns the redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns conservative terminal facts when the failure closed an admitted attempt.
    #[must_use]
    pub const fn terminal_facts(&self) -> Option<TerminalFacts> {
        self.terminal_facts
    }

    /// Returns conservative provider acceptance when the failure closed an admitted attempt.
    #[must_use]
    pub const fn provider_acceptance(&self) -> Option<ProviderAcceptance> {
        match self.terminal_facts {
            Some(facts) => Some(facts.provider_acceptance()),
            None => None,
        }
    }
}

impl fmt::Display for ChatAttemptProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ChatAttemptProtocolError {}

/// Provider-neutral attempt failure with conservative terminal facts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChatAttemptError {
    /// Cooperative cancellation was acknowledged.
    Cancelled {
        /// Facts are present only after private service admission.
        facts: Option<TerminalFacts>,
    },
    /// Observation delivery failed or its one-terminal fence was violated.
    Delivery {
        /// Conservative facts at delivery failure.
        facts: TerminalFacts,
    },
    /// A dialect/transport/protocol hook returned a typed safe failure.
    Protocol {
        /// Redaction-safe dialect/transport failure.
        source: ChatAttemptProtocolError,
        /// Conservative facts at protocol failure.
        facts: TerminalFacts,
    },
    /// Admission, physical-generation, or terminal observation ordering failed.
    Observation {
        /// Shared lifecycle/observation failure.
        source: FamilyCoreError,
        /// Conservative facts at observation failure.
        facts: TerminalFacts,
    },
    /// A service returned authority for another logical operation.
    Binding {
        /// Conservative facts at binding rejection.
        facts: TerminalFacts,
    },
    /// A service violated admission or one-terminal result rules.
    ServiceContract {
        /// Conservative facts at service-contract failure.
        facts: TerminalFacts,
    },
    /// Readiness failed before private admission.
    Unavailable,
}

impl ChatAttemptError {
    /// Creates a fact-bearing protocol failure.
    #[must_use]
    pub const fn protocol(source: ChatAttemptProtocolError, facts: TerminalFacts) -> Self {
        Self::Protocol { source, facts }
    }

    /// Returns facts when private admission occurred.
    #[must_use]
    pub const fn facts(&self) -> Option<TerminalFacts> {
        match self {
            Self::Cancelled { facts } => *facts,
            Self::Delivery { facts }
            | Self::Protocol { facts, .. }
            | Self::Observation { facts, .. }
            | Self::Binding { facts }
            | Self::ServiceContract { facts } => Some(*facts),
            Self::Unavailable => None,
        }
    }

    /// Returns the conservative provider-acceptance fact, when admission
    /// reached a fact-bearing terminal path.
    #[must_use]
    pub const fn provider_acceptance(&self) -> Option<ProviderAcceptance> {
        match self.facts() {
            Some(facts) => Some(facts.provider_acceptance()),
            None => None,
        }
    }

    /// Returns the provider HTTP status from a protocol failure.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        match self {
            Self::Protocol { source, .. } => source.http_status(),
            _ => None,
        }
    }

    /// Returns the provider's own name for this refusal, when it gave one.
    #[must_use]
    pub const fn provider_refusal(&self) -> Option<ProviderRefusal> {
        match self {
            Self::Protocol { source, .. } => source.refusal(),
            _ => None,
        }
    }

    /// Whether a hook reported exhausted quota or entitlement.
    #[must_use]
    pub const fn usage_limit(&self) -> bool {
        match self {
            Self::Protocol { source, .. } => source.usage_limit(),
            _ => false,
        }
    }

    /// Returns whether the provider rejected the input as over its window.
    #[must_use]
    pub const fn context_overflow(&self) -> bool {
        match self {
            Self::Protocol { source, .. } => source.context_overflow(),
            _ => false,
        }
    }

    /// Returns how the exchange broke. Only a hook failure can be a
    /// transport break; every other variant is a local protocol failure.
    #[must_use]
    pub const fn exchange_break(&self) -> ExchangeBreak {
        match self {
            Self::Protocol { source, .. } => source.exchange_break(),
            _ => ExchangeBreak::Protocol,
        }
    }
}

impl fmt::Display for ChatAttemptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { .. } => formatter.write_str("Chat attempt cancelled"),
            Self::Delivery { .. } => formatter.write_str("Chat observation delivery failed"),
            Self::Protocol { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Observation { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Binding { .. } => formatter.write_str("Chat response binding mismatch"),
            Self::ServiceContract { .. } => formatter.write_str("Chat service contract violation"),
            Self::Unavailable => formatter.write_str("Chat service unavailable"),
        }
    }
}

impl std::error::Error for ChatAttemptError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryFailure {
    Closed,
    Cancelled,
    Panicked,
}

impl plakous_attempt::PanicAsError for DeliveryFailure {
    fn panicked() -> Self {
        Self::Panicked
    }
}
impl SinkDeliveryError for DeliveryFailure {
    fn cancelled() -> Self {
        Self::Cancelled
    }
}

/// Cloneable cancellation-aware bounded observation sink.
#[derive(Clone)]
pub struct ChatAttemptEventSink {
    sender: mpsc::Sender<Result<ModelEvent, ChatAttemptProtocolError>>,
    cancellation: CancellationToken,
    terminal: Arc<AtomicBool>,
    core: Arc<FamilyAttemptCore>,
}

impl ChatAttemptEventSink {
    /// Emits one non-terminal observation under bounded backpressure.
    pub async fn send(&mut self, event: ModelEvent) -> Result<(), ChatAttemptError> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        if matches!(event, ModelEvent::ResponseCompleted(_)) {
            return Err(self.contract());
        }
        let visible = event_has_visible_output(&event);
        let hosted = event_has_hosted_effect(&event);
        if hosted {
            return Err(self.contract());
        }
        self.core
            .observe_received_event(false)
            .map_err(|source| self.observation(source))?;
        self.send_item(Ok(event), false).await?;
        self.core.observe_after_emit(visible);
        Ok(())
    }

    /// Emits a typed retry notice without observing it as provider output:
    /// the attempt ledger is untouched.
    pub async fn send_retry(
        &mut self,
        retry: plakous_model::ModelRetry,
    ) -> Result<(), ChatAttemptError> {
        self.send_item(Ok(ModelEvent::Retry(retry)), false).await
    }

    /// Emits one terminal protocol error and closes the channel.
    pub async fn send_error(
        &mut self,
        error: ChatAttemptProtocolError,
    ) -> Result<(), ChatAttemptError> {
        self.send_item(Err(error), true).await
    }

    /// Delivers the already validated and sealed successful terminal.
    pub(crate) async fn send_terminal_response(
        &mut self,
        response: ModelResponse,
    ) -> Result<(), ChatAttemptError> {
        self.send_item(Ok(ModelEvent::ResponseCompleted(response)), true)
            .await
    }

    async fn send_item(
        &mut self,
        item: Result<ModelEvent, ChatAttemptProtocolError>,
        terminal: bool,
    ) -> Result<(), ChatAttemptError> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        if terminal {
            if self
                .terminal
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(ChatAttemptError::Delivery {
                    facts: self.core.terminal_facts(),
                });
            }
        } else if self.terminal.load(Ordering::Acquire) {
            return Err(ChatAttemptError::Delivery {
                facts: self.core.terminal_facts(),
            });
        }
        let cancellation = self.cancellation.clone();
        let send = self.sender.send(item);
        let result = race_sink_delivery(
            Box::pin(async move { send.await.map_err(|_| DeliveryFailure::Closed) }),
            Box::pin(cancellation.cancelled()),
        )
        .await;
        if terminal {
            self.sender.close_channel();
        }
        match result {
            Ok(()) => Ok(()),
            Err(DeliveryFailure::Cancelled) => Err(self.cancelled()),
            Err(DeliveryFailure::Closed | DeliveryFailure::Panicked) => {
                Err(ChatAttemptError::Delivery {
                    facts: self.core.terminal_facts(),
                })
            }
        }
    }

    fn cancelled(&self) -> ChatAttemptError {
        ChatAttemptError::Cancelled {
            facts: self
                .core
                .observations_ref()
                .is_service_admitted()
                .then(|| self.core.terminal_facts()),
        }
    }
    fn observation(&self, source: FamilyCoreError) -> ChatAttemptError {
        ChatAttemptError::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
    fn contract(&self) -> ChatAttemptError {
        ChatAttemptError::ServiceContract {
            facts: self.core.terminal_facts(),
        }
    }
}

impl fmt::Debug for ChatAttemptEventSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatAttemptEventSink")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("terminal", &self.terminal.load(Ordering::Acquire))
            .field("facts", &self.core.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// One owned credential-free Chat logical operation.
pub struct ChatAttempt {
    binding: ChatAttemptBinding,
    context: ModelCallContext,
    events: ChatAttemptEventSink,
    core: Arc<FamilyAttemptCore>,
}

impl ChatAttempt {
    /// Creates an attempt and its bounded observation receiver.
    #[must_use]
    pub fn with_event_channel(
        operation: ChatOperationBinding,
        context: ModelCallContext,
        capacity: NonZeroUsize,
    ) -> (Self, ChatAttemptEventReceiver) {
        let core = Arc::new(FamilyAttemptCore::new_with_policy(
            CHAT_FAMILY_LEDGER_POLICY,
        ));
        let (sender, receiver) = mpsc::channel(capacity.get());
        let events = ChatAttemptEventSink {
            sender,
            cancellation: context.cancellation().clone(),
            terminal: Arc::new(AtomicBool::new(false)),
            core: Arc::clone(&core),
        };
        let binding = AttemptBinding::new(operation, context.attempt_id());
        (
            Self {
                binding,
                context,
                events,
                core,
            },
            receiver,
        )
    }

    /// Returns exact target, plan, operation and attempt authority.
    #[must_use]
    pub const fn binding(&self) -> &ChatAttemptBinding {
        &self.binding
    }
    /// Records transient retry diagnostics alongside the physical-request ledger.
    pub(crate) fn record_failure(
        &self,
        point: plakous_attempt::FailurePoint,
        phase: Option<&'static str>,
        refusal: Option<ProviderRefusal>,
        proven_unsent: bool,
        decline: Option<&'static str>,
    ) {
        self.core
            .observations_ref()
            .record_failure(point, phase, refusal, proven_unsent, decline);
    }
    /// Returns cancellation/deadline/attempt context.
    #[must_use]
    pub const fn context(&self) -> &ModelCallContext {
        &self.context
    }
    /// Returns mutable bounded observation delivery.
    #[must_use]
    pub const fn events_mut(&mut self) -> &mut ChatAttemptEventSink {
        &mut self.events
    }
    /// Returns current conservative facts.
    #[must_use]
    pub fn terminal_facts(&self) -> TerminalFacts {
        self.core.terminal_facts()
    }
    /// Verifies private service admission.
    pub fn ensure_service_admitted(&self) -> Result<(), ChatAttemptError> {
        self.core
            .ensure_service_admitted()
            .map_err(|source| self.observation(source))
    }
    /// Starts one physical generation. The ledger admits another only after
    /// the previous one finished; the kernel's retry policy decides whether.
    pub fn observe_physical_request_started(
        &self,
    ) -> Result<ChatPhysicalRequestBinding, ChatAttemptError> {
        self.core
            .physical_request_started()
            .map_err(|source| self.observation(source))?;
        let generation = self.core.observation_snapshot().physical_attempts;
        Ok(PhysicalRequestBinding::new(
            self.binding.clone(),
            generation,
        ))
    }
    /// Finishes a physical generation without logical success.
    pub fn observe_physical_request_finished(
        &self,
        outcome: PhysicalRequestOutcome,
    ) -> Result<(), ChatAttemptError> {
        self.core
            .physical_request_finished(outcome)
            .map_err(|source| self.observation(source))
    }
    /// Records exact provider acceptance from a dialect hook.
    pub fn observe_provider_accepted(&self) -> Result<(), ChatAttemptError> {
        self.core
            .observe_provider_accepted()
            .map_err(|source| self.observation(source))
    }
    /// Reserves the one Chat authorization slot for the active generation.
    pub fn reserve_authorization(&self) -> Result<ChatPhysicalRequestBinding, ChatAttemptError> {
        let generation = self
            .core
            .reserve_primary_auth()
            .map_err(|source| self.observation(source))?;
        Ok(PhysicalRequestBinding::new(
            self.binding.clone(),
            generation,
        ))
    }
    /// Revalidates a non-cloneable authorization against the active generation.
    pub fn validate_authorization(
        &self,
        physical: &ChatPhysicalRequestBinding,
    ) -> Result<(), ChatAttemptError> {
        if physical.attempt() != &self.binding {
            return Err(ChatAttemptError::Binding {
                facts: self.terminal_facts(),
            });
        }
        self.core
            .validate_primary_auth(physical.generation())
            .map_err(|source| self.observation(source))
    }
    fn observation(&self, source: FamilyCoreError) -> ChatAttemptError {
        ChatAttemptError::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
}

impl fmt::Debug for ChatAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatAttempt")
            .field("binding", &self.binding)
            .field("context", &self.context)
            .field("events", &self.events)
            .field("facts", &self.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// Checked successful terminal aggregate for one exact Chat attempt.
#[derive(Debug, PartialEq)]
pub struct ChatAttemptResponse {
    binding: ChatAttemptBinding,
    response: ModelResponse,
    request_id: ModelProviderRequestId,
    finish: ChatFinishReason,
    facts: TerminalFacts,
}

impl ChatAttemptResponse {
    /// Seals one neutral projection as the sole successful terminal result.
    pub fn new(
        attempt: &ChatAttempt,
        projection: ChatTerminalProjection,
    ) -> Result<Self, ChatAttemptError> {
        attempt.ensure_service_admitted()?;
        let (response, finish) = projection.into_parts();
        let request_id = response
            .provider_request_id()
            .and_then(|value| ModelProviderRequestId::new(value).ok())
            .ok_or_else(|| ChatAttemptError::Binding {
                facts: attempt.terminal_facts(),
            })?;
        if response.plan_digest() != attempt.binding.operation().plan() {
            return Err(ChatAttemptError::Binding {
                facts: attempt.terminal_facts(),
            });
        }
        if response_has_hosted_effect(&response) {
            return Err(ChatAttemptError::ServiceContract {
                facts: attempt.terminal_facts(),
            });
        }
        response
            .validate_for_commit()
            .map_err(|_| ChatAttemptError::ServiceContract {
                facts: attempt.terminal_facts(),
            })?;
        attempt
            .core
            .observe_terminal_response(!response.output().is_empty(), false)
            .map_err(|source| attempt.observation(source))?;
        attempt
            .core
            .terminal_success()
            .map_err(|source| attempt.observation(source))?;
        let facts = attempt.terminal_facts();
        Ok(Self {
            binding: attempt.binding.clone(),
            response,
            request_id,
            finish,
            facts,
        })
    }
    /// Returns exact logical authority.
    #[must_use]
    pub const fn binding(&self) -> &ChatAttemptBinding {
        &self.binding
    }
    /// Returns the canonical response.
    #[must_use]
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }
    /// Returns provider request attribution.
    #[must_use]
    pub const fn request_id(&self) -> &ModelProviderRequestId {
        &self.request_id
    }
    /// Returns typed Chat stop facts.
    #[must_use]
    pub const fn finish(&self) -> &ChatFinishReason {
        &self.finish
    }
    /// Returns conservative physical facts.
    #[must_use]
    pub const fn facts(&self) -> TerminalFacts {
        self.facts
    }
}

struct AwaitHooks {
    core: Arc<FamilyAttemptCore>,
    cancellation: CancellationToken,
}
impl AwaitHooks {
    fn contract(&self) -> ChatAttemptError {
        ChatAttemptError::ServiceContract {
            facts: self.core.terminal_facts(),
        }
    }
}
impl AdmittedAwaitHooks for AwaitHooks {
    type Ok = ChatAttemptResponse;
    type Err = ChatAttemptError;
    fn on_cleanup_exhausted(&self) -> Self::Err {
        ChatAttemptError::Cancelled {
            facts: Some(self.core.terminal_facts()),
        }
    }
    fn on_poll_panic(&self) -> Self::Err {
        self.contract()
    }
    fn on_drop_future_panic(&self) -> Self::Err {
        self.contract()
    }
    fn finalize(
        &self,
        result: Result<Self::Ok, Self::Err>,
        saw_cancel: bool,
    ) -> Result<Self::Ok, Self::Err> {
        if (saw_cancel || self.cancellation.is_cancelled())
            && result
                .as_ref()
                .err()
                .is_some_and(|error| error.facts().is_none())
        {
            Err(ChatAttemptError::Cancelled {
                facts: Some(self.core.terminal_facts()),
            })
        } else {
            result
        }
    }
}

struct CallContext {
    binding: ChatAttemptBinding,
    core: Arc<FamilyAttemptCore>,
}
struct CallDriver;
impl AdmittedCallDriver for CallDriver {
    type Request = ChatAttempt;
    type Response = ChatAttemptResponse;
    type Error = ChatAttemptError;
    type Seal = LedgerOnlyAdmission;
    type Binding = CallContext;
    type Hooks = AwaitHooks;
    fn expected_context(request: &Self::Request) -> Self::Binding {
        CallContext {
            binding: request.binding.clone(),
            core: Arc::clone(&request.core),
        }
    }
    fn response_matches(response: &Self::Response, expected: &Self::Binding) -> bool {
        response.binding == expected.binding
            && response.facts == expected.core.terminal_facts()
            && response.facts.provider_acceptance() == ProviderAcceptance::Accepted
    }
    fn is_cancelled(request: &Self::Request) -> bool {
        request.context.cancellation().is_cancelled()
    }
    fn on_ready_panic(_request: &Self::Request) -> Self::Error {
        ChatAttemptError::Unavailable
    }
    fn on_cancel(request: &Self::Request) -> Self::Error {
        ChatAttemptError::Cancelled {
            facts: request
                .core
                .observations_ref()
                .is_service_admitted()
                .then(|| request.terminal_facts()),
        }
    }
    fn admit(request: &Self::Request) -> Result<(Self::Seal, Self::Hooks), Self::Error> {
        request
            .core
            .mark_service_admitted()
            .map_err(|source| request.observation(source))?;
        if request.binding.operation().replay_safety() == ReplaySafety::ServerEffect {
            request
                .core
                .declare_server_effect()
                .map_err(|source| request.observation(source))?;
        }
        let seal = LedgerOnlyAdmission::new(request.core.observations());
        let hooks = AwaitHooks {
            core: Arc::clone(&request.core),
            cancellation: request.context.cancellation().clone(),
        };
        Ok((seal, hooks))
    }
    fn on_call_panic(seal: &mut Self::Seal, hooks: &Self::Hooks) -> Self::Error {
        ServiceAdmissionSeal::seal(seal);
        hooks.contract()
    }
    fn on_binding_mismatch(response: &Self::Response, expected: &Self::Binding) -> Self::Error {
        if response.binding == expected.binding {
            ChatAttemptError::ServiceContract {
                facts: expected.core.terminal_facts(),
            }
        } else {
            ChatAttemptError::Binding {
                facts: expected.core.terminal_facts(),
            }
        }
    }
    fn await_policy() -> AdmittedAwaitPolicy {
        AdmittedAwaitPolicy {
            max_cleanup_polls: MAX_CHAT_ADMITTED_CLEANUP_POLLS.saturating_sub(1),
        }
    }
}

/// Privately admits and executes exactly one complete Chat operation.
pub async fn call_chat_operation<S>(
    service: &mut S,
    attempt: ChatAttempt,
) -> Result<ChatAttemptResponse, ChatAttemptError>
where
    S: Service<ChatAttempt, Response = ChatAttemptResponse, Error = ChatAttemptError>
        + Send
        + 'static,
    S::Future: Future<Output = Result<ChatAttemptResponse, ChatAttemptError>> + Send + 'static,
{
    let cancelled = Box::pin(attempt.context.cancellation().clone().cancelled());
    call_admitted_operation(service, attempt, cancelled, CallDriver).await
}

fn event_has_visible_output(event: &ModelEvent) -> bool {
    matches!(
        event,
        ModelEvent::BlockStarted(_)
            | ModelEvent::TextDelta(_)
            | ModelEvent::ReasoningDelta(_)
            | ModelEvent::ToolInputDelta(_)
            | ModelEvent::CitationAdded(_)
            | ModelEvent::BlockCompleted(_)
    ) || matches!(event, ModelEvent::ResponseCompleted(response) if !response.output().is_empty())
}
fn event_has_hosted_effect(event: &ModelEvent) -> bool {
    match event {
        ModelEvent::BlockStarted(started) => match started.block() {
            BlockStart::ToolCall { owner, .. } => {
                matches!(owner, ToolExecutionOwner::ProviderHosted { .. })
            }
            BlockStart::Complete(ContentBlock::ToolCall(call)) => {
                matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            }
            _ => false,
        },
        ModelEvent::ResponseCompleted(response) => response_has_hosted_effect(response),
        _ => false,
    }
}
fn response_has_hosted_effect(response: &ModelResponse) -> bool {
    response.output().iter().any(|item| item.blocks().iter().any(|block| matches!(block, ContentBlock::ToolCall(call) if matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. }))))
}

#[cfg(test)]
mod tests {
    use plakous_model::{DigestDomain, ModelCallContext};
    use uuid::Uuid;

    use super::*;

    fn attempt(label: &[u8]) -> Result<ChatAttempt, Box<dyn std::error::Error>> {
        let operation = ChatOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
            ReplaySafety::CostOnly,
        );
        let context = ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(1))?,
            CancellationToken::new(),
            None,
        );
        Ok(ChatAttempt::with_event_channel(operation, context, NonZeroUsize::MIN).0)
    }

    #[test]
    fn a_cost_only_operation_never_reports_a_hosted_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        let attempt = attempt(b"no-hosted-effects")?;
        assert_eq!(
            attempt.binding().operation().replay_safety(),
            ReplaySafety::CostOnly
        );
        attempt.core.mark_service_admitted()?;
        attempt.observe_physical_request_started()?;
        attempt.observe_provider_accepted()?;
        let facts = attempt.terminal_facts();
        assert_eq!(facts.replay_safety(), ReplaySafety::CostOnly);
        assert!(!facts.hosted_effects_possible());
        Ok(())
    }

    #[test]
    fn a_refused_generation_is_followed_by_a_fresh_one_never_an_overlapping_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let attempt = attempt(b"generations")?;
        attempt.core.mark_service_admitted()?;
        let first = attempt.observe_physical_request_started()?;
        assert!(attempt.observe_physical_request_started().is_err());
        attempt.observe_physical_request_finished(PhysicalRequestOutcome::DefiniteRejection)?;
        assert_eq!(
            attempt.terminal_facts().provider_acceptance(),
            ProviderAcceptance::NotAccepted
        );
        let second = attempt.observe_physical_request_started()?;
        assert_eq!((first.generation(), second.generation()), (1, 2));
        assert_eq!(attempt.reserve_authorization()?, second);
        assert_eq!(attempt.terminal_facts().physical_attempts(), 2);
        Ok(())
    }

    #[test]
    fn authorization_rejects_foreign_attempt_and_generation_bindings()
    -> Result<(), Box<dyn std::error::Error>> {
        let owner = attempt(b"binding-owner")?;
        owner.core.mark_service_admitted()?;
        let physical = owner.observe_physical_request_started()?;
        let reserved = owner.reserve_authorization()?;
        assert_eq!(physical, reserved);

        let wrong_generation = ChatPhysicalRequestBinding::new(owner.binding().clone(), 2);
        assert!(matches!(
            owner.validate_authorization(&wrong_generation),
            Err(ChatAttemptError::Observation { .. })
        ));

        let foreign = attempt(b"binding-foreign")?;
        let foreign_binding =
            ChatPhysicalRequestBinding::new(foreign.binding().clone(), physical.generation());
        assert!(matches!(
            owner.validate_authorization(&foreign_binding),
            Err(ChatAttemptError::Binding { .. })
        ));
        Ok(())
    }
}
