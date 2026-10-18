//! Closed provider-neutral attempt host for complete Messages operations.

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
    PhysicalRequestOutcome, ProviderAcceptance, ReplaySafety, ServiceAdmissionSeal,
    SinkDeliveryError, TerminalFacts, call_admitted_operation, race_sink_delivery,
};
use plakous_model::{
    AttemptId, BlockStart, CancellationToken, ContentBlock, Digest, ModelCallContext, ModelEvent,
    ModelHttpStatus, ModelProviderRequestId, ModelResponse, ModelRetryDelay, ToolExecutionOwner,
};
use tower_service::Service;

use crate::{MessagesStopOutcome, MessagesTerminalProjection};

/// Maximum service polls permitted after admitted cancellation is observed.
pub use plakous_attempt::MAX_ADMITTED_CLEANUP_POLLS as MAX_MESSAGES_ADMITTED_CLEANUP_POLLS;

/// Exact immutable authority for one credential-free Messages operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesOperationBinding {
    target: Digest,
    plan: Digest,
    operation: Digest,
    replay_safety: ReplaySafety,
}

impl MessagesOperationBinding {
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

/// Exact logical Messages-attempt identity.
pub type MessagesAttemptBinding = AttemptBinding<MessagesOperationBinding, AttemptId>;
/// Exact active physical-request identity.
pub type MessagesPhysicalRequestBinding = PhysicalRequestBinding<MessagesAttemptBinding>;
/// Bounded receiver for normalized observations.
pub type MessagesAttemptEventReceiver =
    mpsc::Receiver<Result<ModelEvent, MessagesAttemptProtocolError>>;

/// Redaction-safe family/protocol failure supplied by a dialect hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesAttemptProtocolError {
    message: String,
    detail: Option<plakous_model::SafeDiagnostic>,
    http_status: Option<ModelHttpStatus>,
    retry_after: Option<ModelRetryDelay>,
    refusal: Option<MessagesProviderRefusal>,
    context_overflow: bool,
    usage_limit: bool,
    exchange_break: ExchangeBreak,
}

impl MessagesAttemptProtocolError {
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
            http_status: None,
            retry_after: None,
            refusal: None,
            context_overflow: false,
            usage_limit: false,
            exchange_break: ExchangeBreak::Protocol,
        }
    }
    /// Attaches a validated provider HTTP status.
    #[must_use]
    pub const fn with_http_status(mut self, status: ModelHttpStatus) -> Self {
        self.http_status = Some(status);
        self
    }
    /// Attaches bounded provider retry-delay advice.
    #[must_use]
    pub const fn with_retry_after(mut self, retry_after: ModelRetryDelay) -> Self {
        self.retry_after = Some(retry_after);
        self
    }
    /// Returns the redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Returns the typed HTTP status, when the failure came from a response.
    #[must_use]
    pub const fn http_status(&self) -> Option<ModelHttpStatus> {
        self.http_status
    }
    /// Names this failure as a refusal the provider itself identified.
    ///
    /// A dialect attaches this only when the provider said exactly why it
    /// refused. It does not by itself say the outcome is definite: the caller
    /// still weighs what the exchange produced.
    #[must_use]
    pub const fn with_refusal(mut self, refusal: MessagesProviderRefusal) -> Self {
        self.refusal = Some(refusal);
        self
    }

    /// Marks exhausted quota, billing, or entitlement: from the error body,
    /// or from a dialect that read the provider's attestation elsewhere. The
    /// marked error is permanent and its retry refusal is cleared.
    #[must_use]
    pub const fn with_usage_limit(mut self) -> Self {
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
    pub const fn refusal(&self) -> Option<MessagesProviderRefusal> {
        self.refusal
    }

    /// Returns bounded provider retry-delay advice, when present.
    #[must_use]
    pub const fn retry_after(&self) -> Option<ModelRetryDelay> {
        self.retry_after
    }
}

impl fmt::Display for MessagesAttemptProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MessagesAttemptProtocolError {}

/// Provider-neutral attempt failure with conservative terminal facts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MessagesAttemptError {
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
        source: MessagesAttemptProtocolError,
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

impl MessagesAttemptError {
    /// Creates a fact-bearing protocol failure.
    #[must_use]
    pub const fn protocol(source: MessagesAttemptProtocolError, facts: TerminalFacts) -> Self {
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

    /// Returns typed provider HTTP status from a protocol failure.
    #[must_use]
    pub const fn http_status(&self) -> Option<ModelHttpStatus> {
        match self {
            Self::Protocol { source, .. } => source.http_status(),
            _ => None,
        }
    }

    /// Returns bounded provider retry-delay advice from a protocol failure.
    #[must_use]
    pub const fn retry_after(&self) -> Option<ModelRetryDelay> {
        match self {
            Self::Protocol { source, .. } => source.retry_after(),
            _ => None,
        }
    }

    /// Returns the provider's own name for this refusal, when it gave one.
    #[must_use]
    pub const fn provider_refusal(&self) -> Option<MessagesProviderRefusal> {
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

/// The refusal vocabulary every wire shares.
///
/// This was a Messages-local enum. It moved to `plakous-attempt` when the
/// OpenRouter wires needed the same classification, because one vocabulary per
/// protocol is how the same provider condition ends up definite on one route
/// and unknown on another.
pub use plakous_attempt::ProviderRefusal as MessagesProviderRefusal;

impl fmt::Display for MessagesAttemptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { .. } => formatter.write_str("Messages attempt cancelled"),
            Self::Delivery { .. } => formatter.write_str("Messages observation delivery failed"),
            Self::Protocol { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Observation { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Binding { .. } => formatter.write_str("Messages response binding mismatch"),
            Self::ServiceContract { .. } => {
                formatter.write_str("Messages service contract violation")
            }
            Self::Unavailable => formatter.write_str("Messages service unavailable"),
        }
    }
}

impl std::error::Error for MessagesAttemptError {}

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
pub struct MessagesAttemptEventSink {
    sender: mpsc::Sender<Result<ModelEvent, MessagesAttemptProtocolError>>,
    cancellation: CancellationToken,
    terminal: Arc<AtomicBool>,
    core: Arc<FamilyAttemptCore>,
}

impl MessagesAttemptEventSink {
    /// Emits one non-terminal observation under bounded backpressure.
    pub async fn send(&mut self, event: ModelEvent) -> Result<(), MessagesAttemptError> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        let terminal = matches!(event, ModelEvent::ResponseCompleted(_));
        let visible = event_has_visible_output(&event);
        let hosted = event_has_hosted_effect(&event);
        self.core
            .observe_received_event(hosted)
            .map_err(|source| self.observation(source))?;
        self.send_item(Ok(event), terminal).await?;
        self.core.observe_after_emit(visible);
        Ok(())
    }

    /// Emits a typed retry notice without observing it as provider output:
    /// the attempt ledger is untouched.
    pub async fn send_retry(
        &mut self,
        retry: plakous_model::ModelRetry,
    ) -> Result<(), MessagesAttemptError> {
        self.send_item(Ok(ModelEvent::Retry(retry)), false).await
    }

    /// Emits one terminal protocol error and closes the channel.
    pub async fn send_error(
        &mut self,
        error: MessagesAttemptProtocolError,
    ) -> Result<(), MessagesAttemptError> {
        self.send_item(Err(error), true).await
    }

    async fn send_item(
        &mut self,
        item: Result<ModelEvent, MessagesAttemptProtocolError>,
        terminal: bool,
    ) -> Result<(), MessagesAttemptError> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        if terminal {
            if self
                .terminal
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(MessagesAttemptError::Delivery {
                    facts: self.core.terminal_facts(),
                });
            }
        } else if self.terminal.load(Ordering::Acquire) {
            return Err(MessagesAttemptError::Delivery {
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
                Err(MessagesAttemptError::Delivery {
                    facts: self.core.terminal_facts(),
                })
            }
        }
    }

    fn cancelled(&self) -> MessagesAttemptError {
        MessagesAttemptError::Cancelled {
            facts: self
                .core
                .observations_ref()
                .is_service_admitted()
                .then(|| self.core.terminal_facts()),
        }
    }
    fn observation(&self, source: FamilyCoreError) -> MessagesAttemptError {
        MessagesAttemptError::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
}

impl fmt::Debug for MessagesAttemptEventSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesAttemptEventSink")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("terminal", &self.terminal.load(Ordering::Acquire))
            .field("facts", &self.core.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// One owned credential-free Messages logical operation.
pub struct MessagesAttempt {
    binding: MessagesAttemptBinding,
    context: ModelCallContext,
    events: MessagesAttemptEventSink,
    core: Arc<FamilyAttemptCore>,
}

impl MessagesAttempt {
    /// Creates an attempt and its bounded observation receiver.
    #[must_use]
    pub fn with_event_channel(
        operation: MessagesOperationBinding,
        context: ModelCallContext,
        capacity: NonZeroUsize,
    ) -> (Self, MessagesAttemptEventReceiver) {
        let core = Arc::new(FamilyAttemptCore::new_with_policy(
            LedgerPolicy::MESSAGES_FAMILY,
        ));
        let (sender, receiver) = mpsc::channel(capacity.get());
        let events = MessagesAttemptEventSink {
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
    pub const fn binding(&self) -> &MessagesAttemptBinding {
        &self.binding
    }
    /// Returns cancellation/deadline/attempt context.
    #[must_use]
    pub const fn context(&self) -> &ModelCallContext {
        &self.context
    }
    /// Returns mutable bounded observation delivery.
    #[must_use]
    pub const fn events_mut(&mut self) -> &mut MessagesAttemptEventSink {
        &mut self.events
    }
    /// Records transient retry diagnostics alongside the physical-request ledger.
    pub(crate) fn record_failure(
        &self,
        point: plakous_attempt::FailurePoint,
        phase: Option<&'static str>,
        proven_unsent: bool,
        decline: Option<&'static str>,
    ) {
        self.core
            .observations_ref()
            .record_failure(point, phase, proven_unsent, decline);
    }
    /// Returns current conservative facts.
    #[must_use]
    pub fn terminal_facts(&self) -> TerminalFacts {
        self.core.terminal_facts()
    }
    /// Verifies private service admission.
    pub fn ensure_service_admitted(&self) -> Result<(), MessagesAttemptError> {
        self.core
            .ensure_service_admitted()
            .map_err(|source| self.observation(source))
    }
    /// Starts one physical generation.
    pub fn observe_physical_request_started(
        &self,
    ) -> Result<MessagesPhysicalRequestBinding, MessagesAttemptError> {
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
    ) -> Result<(), MessagesAttemptError> {
        self.core
            .physical_request_finished(outcome)
            .map_err(|source| self.observation(source))
    }
    /// Records exact provider acceptance from a dialect hook.
    pub fn observe_provider_accepted(&self) -> Result<(), MessagesAttemptError> {
        self.core
            .observe_provider_accepted()
            .map_err(|source| self.observation(source))
    }
    /// Reserves the one Messages authorization slot for the active generation.
    pub fn reserve_authorization(
        &self,
    ) -> Result<MessagesPhysicalRequestBinding, MessagesAttemptError> {
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
        physical: &MessagesPhysicalRequestBinding,
    ) -> Result<(), MessagesAttemptError> {
        if physical.attempt() != &self.binding {
            return Err(MessagesAttemptError::Binding {
                facts: self.terminal_facts(),
            });
        }
        self.core
            .validate_primary_auth(physical.generation())
            .map_err(|source| self.observation(source))
    }
    fn observation(&self, source: FamilyCoreError) -> MessagesAttemptError {
        MessagesAttemptError::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
}

impl fmt::Debug for MessagesAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesAttempt")
            .field("binding", &self.binding)
            .field("context", &self.context)
            .field("events", &self.events)
            .field("facts", &self.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// Checked successful terminal aggregate for one exact Messages attempt.
#[derive(Debug, PartialEq)]
pub struct MessagesAttemptResponse {
    binding: MessagesAttemptBinding,
    response: ModelResponse,
    request_id: ModelProviderRequestId,
    stop: MessagesStopOutcome,
    facts: TerminalFacts,
}

impl MessagesAttemptResponse {
    /// Seals one neutral projection as the sole successful terminal result.
    pub fn new(
        attempt: &MessagesAttempt,
        projection: MessagesTerminalProjection,
        request_id: ModelProviderRequestId,
    ) -> Result<Self, MessagesAttemptError> {
        attempt.ensure_service_admitted()?;
        let (response, _reported_model, stop) = projection.into_parts();
        if response.plan_digest() != attempt.binding.operation().plan()
            || response.provider_request_id() != Some(request_id.as_str())
        {
            return Err(MessagesAttemptError::Binding {
                facts: attempt.terminal_facts(),
            });
        }
        attempt
            .core
            .observe_terminal_response(
                !response.output().is_empty(),
                response_has_hosted_effect(&response),
            )
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
            stop,
            facts,
        })
    }
    /// Returns exact logical authority.
    #[must_use]
    pub const fn binding(&self) -> &MessagesAttemptBinding {
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
    /// Returns typed Messages stop facts.
    #[must_use]
    pub const fn stop(&self) -> &MessagesStopOutcome {
        &self.stop
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
    fn contract(&self) -> MessagesAttemptError {
        MessagesAttemptError::ServiceContract {
            facts: self.core.terminal_facts(),
        }
    }
}
impl AdmittedAwaitHooks for AwaitHooks {
    type Ok = MessagesAttemptResponse;
    type Err = MessagesAttemptError;
    fn on_cleanup_exhausted(&self) -> Self::Err {
        MessagesAttemptError::Cancelled {
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
            Err(MessagesAttemptError::Cancelled {
                facts: Some(self.core.terminal_facts()),
            })
        } else {
            result
        }
    }
}

struct CallContext {
    binding: MessagesAttemptBinding,
    core: Arc<FamilyAttemptCore>,
}
struct CallDriver;
impl AdmittedCallDriver for CallDriver {
    type Request = MessagesAttempt;
    type Response = MessagesAttemptResponse;
    type Error = MessagesAttemptError;
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
        MessagesAttemptError::Unavailable
    }
    fn on_cancel(request: &Self::Request) -> Self::Error {
        MessagesAttemptError::Cancelled {
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
            MessagesAttemptError::ServiceContract {
                facts: expected.core.terminal_facts(),
            }
        } else {
            MessagesAttemptError::Binding {
                facts: expected.core.terminal_facts(),
            }
        }
    }
    fn await_policy() -> AdmittedAwaitPolicy {
        AdmittedAwaitPolicy {
            max_cleanup_polls: MAX_MESSAGES_ADMITTED_CLEANUP_POLLS.saturating_sub(1),
        }
    }
}

/// Privately admits and executes exactly one complete Messages operation.
pub async fn call_messages_operation<S>(
    service: &mut S,
    attempt: MessagesAttempt,
) -> Result<MessagesAttemptResponse, MessagesAttemptError>
where
    S: Service<MessagesAttempt, Response = MessagesAttemptResponse, Error = MessagesAttemptError>
        + Send
        + 'static,
    S::Future:
        Future<Output = Result<MessagesAttemptResponse, MessagesAttemptError>> + Send + 'static,
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
#[allow(clippy::expect_used, reason = "tests assert typed failure paths")]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll};

    use futures::executor::block_on;
    use plakous_model::{DigestDomain, ResponseStarted};
    use tower_service::Service;
    use uuid::Uuid;

    use super::*;

    fn operation(label: &[u8]) -> Result<MessagesOperationBinding, Box<dyn std::error::Error>> {
        Ok(MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
            ReplaySafety::CostOnly,
        ))
    }

    fn hosted_operation(
        label: &[u8],
    ) -> Result<MessagesOperationBinding, Box<dyn std::error::Error>> {
        Ok(MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[label])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[label])?,
            ReplaySafety::ServerEffect,
        ))
    }

    fn context(
        id: u128,
        cancellation: CancellationToken,
    ) -> Result<ModelCallContext, Box<dyn std::error::Error>> {
        Ok(ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(id))?,
            cancellation,
            None,
        ))
    }

    fn started(plan: Digest) -> ModelEvent {
        ModelEvent::ResponseStarted(ResponseStarted::new(None, plan))
    }

    #[test]
    fn cancellation_wakes_backpressured_observation_delivery()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = CancellationToken::new();
        let binding = operation(b"backpressure")?;
        let plan = binding.plan();
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            binding,
            context(1, cancellation.clone())?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        let mut sink = attempt.events.clone();
        block_on(sink.send(started(plan)))?;
        let mut blocked = Box::pin(sink.send(started(plan)));
        let waker = futures::task::noop_waker();
        let mut task = Context::from_waker(&waker);
        assert!(blocked.as_mut().poll(&mut task).is_pending());
        cancellation.cancel();
        assert!(matches!(
            blocked.as_mut().poll(&mut task),
            Poll::Ready(Err(MessagesAttemptError::Cancelled { .. }))
        ));
        Ok(())
    }

    #[test]
    fn terminal_observation_is_emitted_at_most_once() -> Result<(), Box<dyn std::error::Error>> {
        let (attempt, mut receiver) = MessagesAttempt::with_event_channel(
            operation(b"terminal")?,
            context(2, CancellationToken::new())?,
            NonZeroUsize::new(2).ok_or("nonzero")?,
        );
        let mut first = attempt.events.clone();
        let mut retained = attempt.events.clone();
        let terminal = MessagesAttemptProtocolError::new("terminal fixture");
        block_on(first.send_error(terminal.clone()))?;
        assert_eq!(
            block_on(futures_util::StreamExt::next(&mut receiver)),
            Some(Err(terminal))
        );
        assert!(matches!(
            block_on(retained.send_error(MessagesAttemptProtocolError::new("second"))),
            Err(MessagesAttemptError::Delivery { .. })
        ));
        assert_eq!(block_on(futures_util::StreamExt::next(&mut receiver)), None);
        Ok(())
    }

    #[test]
    fn protocol_metadata_survives_attempt_error() -> Result<(), Box<dyn std::error::Error>> {
        let status = ModelHttpStatus::new(429)?;
        let retry_after = ModelRetryDelay::from_millis(2_000)?;
        let error = MessagesAttemptError::protocol(
            MessagesAttemptProtocolError::new("opaque rejection")
                .with_http_status(status)
                .with_retry_after(retry_after),
            TerminalFacts::preacceptance_test(),
        );
        assert_eq!(error.http_status(), Some(status));
        assert_eq!(error.retry_after(), Some(retry_after));
        Ok(())
    }

    struct AmbiguousService;
    impl Service<MessagesAttempt> for AmbiguousService {
        type Response = MessagesAttemptResponse;
        type Error = MessagesAttemptError;
        type Future = futures_util::future::Ready<Result<Self::Response, Self::Error>>;
        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, attempt: MessagesAttempt) -> Self::Future {
            let result = (|| {
                attempt.ensure_service_admitted()?;
                attempt.observe_physical_request_started()?;
                attempt
                    .observe_physical_request_finished(PhysicalRequestOutcome::AmbiguousPostSend)?;
                Err(MessagesAttemptError::protocol(
                    MessagesAttemptProtocolError::new("ambiguous fixture"),
                    attempt.terminal_facts(),
                ))
            })();
            futures_util::future::ready(result)
        }
    }

    #[test]
    fn admitted_ambiguous_send_never_claims_safe_rejection()
    -> Result<(), Box<dyn std::error::Error>> {
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            operation(b"ambiguous")?,
            context(3, CancellationToken::new())?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
            .expect_err("ambiguous attempt succeeded");
        let facts = error.facts().ok_or("admitted facts")?;
        assert_eq!(facts.physical_attempts(), 1);
        assert_eq!(
            facts.provider_acceptance(),
            ProviderAcceptance::PossiblyAccepted
        );
        assert_ne!(
            facts.retry_class(),
            plakous_attempt::RetryClass::BeforeAcceptance
        );
        Ok(())
    }

    #[test]
    fn hosted_capability_records_possible_effect_after_ambiguous_send()
    -> Result<(), Box<dyn std::error::Error>> {
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            hosted_operation(b"hosted-ambiguous")?,
            context(6, CancellationToken::new())?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
            .expect_err("ambiguous hosted attempt succeeded");
        let facts = error.facts().ok_or("admitted facts")?;
        assert_eq!(facts.replay_safety(), ReplaySafety::ServerEffect);
        assert!(facts.hosted_effects_possible());
        assert_eq!(facts.retry_class(), plakous_attempt::RetryClass::Never);
        Ok(())
    }

    struct CancelAfterAcceptedService;
    impl Service<MessagesAttempt> for CancelAfterAcceptedService {
        type Response = MessagesAttemptResponse;
        type Error = MessagesAttemptError;
        type Future = futures_util::future::Ready<Result<Self::Response, Self::Error>>;
        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, attempt: MessagesAttempt) -> Self::Future {
            let result = (|| {
                attempt.ensure_service_admitted()?;
                attempt.observe_physical_request_started()?;
                attempt.observe_provider_accepted()?;
                attempt.context().cancellation().cancel();
                Err(MessagesAttemptError::Cancelled {
                    facts: Some(attempt.terminal_facts()),
                })
            })();
            futures_util::future::ready(result)
        }
    }

    #[test]
    fn hosted_capability_survives_cancellation_after_acceptance()
    -> Result<(), Box<dyn std::error::Error>> {
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            hosted_operation(b"hosted-cancel")?,
            context(7, CancellationToken::new())?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        let error = block_on(call_messages_operation(
            &mut CancelAfterAcceptedService,
            attempt,
        ))
        .expect_err("cancelled hosted attempt succeeded");
        let facts = error.facts().ok_or("admitted facts")?;
        assert_eq!(facts.provider_acceptance(), ProviderAcceptance::Accepted);
        assert_eq!(facts.replay_safety(), ReplaySafety::ServerEffect);
        assert!(facts.hosted_effects_possible());
        assert_eq!(facts.retry_class(), plakous_attempt::RetryClass::Never);
        Ok(())
    }

    #[test]
    fn admitted_exact_acceptance_is_preserved_in_terminal_facts()
    -> Result<(), Box<dyn std::error::Error>> {
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            operation(b"accepted")?,
            context(5, CancellationToken::new())?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        attempt.core.mark_service_admitted()?;
        attempt.observe_physical_request_started()?;
        attempt.observe_provider_accepted()?;
        let facts = attempt.terminal_facts();
        assert_eq!(facts.physical_attempts(), 1);
        assert_eq!(facts.provider_acceptance(), ProviderAcceptance::Accepted);
        assert_ne!(
            facts.retry_class(),
            plakous_attempt::RetryClass::BeforeAcceptance
        );
        Ok(())
    }

    #[test]
    fn cancellation_before_admission_has_no_physical_facts()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (attempt, _receiver) = MessagesAttempt::with_event_channel(
            operation(b"cancel")?,
            context(4, cancellation)?,
            NonZeroUsize::new(1).ok_or("nonzero")?,
        );
        let error = block_on(call_messages_operation(&mut AmbiguousService, attempt))
            .expect_err("cancelled attempt succeeded");
        assert!(matches!(
            error,
            MessagesAttemptError::Cancelled { facts: None }
        ));
        Ok(())
    }
}
