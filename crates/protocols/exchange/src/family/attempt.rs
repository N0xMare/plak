//! Single-admission, single-terminal attempt host for protocol families.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_channel::mpsc;
use futures_util::SinkExt;
use plakous_attempt::{
    AdmittedAwaitHooks, AdmittedAwaitPolicy, AdmittedCallDriver, AttemptBinding, ExchangeBreak,
    FamilyAttemptCore, FamilyCoreError, LedgerOnlyAdmission, PhysicalRequestBinding,
    PhysicalRequestOutcome, ProviderAcceptance, ProviderRefusal, ReplaySafety,
    ServiceAdmissionSeal, SinkDeliveryError, TerminalFacts, call_admitted_operation,
    race_sink_delivery,
};
use plakous_model::{
    AttemptId, BlockStart, CancellationToken, ContentBlock, Digest, ModelCallContext, ModelEvent,
    ModelHttpStatus, ModelProviderRequestId, ModelResponse, ModelRetryDelay, ToolExecutionOwner,
};
use tower_service::Service;

use super::FamilyWire;
use std::marker::PhantomData;

/// Maximum service polls permitted after admitted cancellation is observed.
use plakous_attempt::MAX_ADMITTED_CLEANUP_POLLS;

/// Exact immutable authority for one credential-free family operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FamilyOperationBinding {
    target: Digest,
    plan: Digest,
    operation: Digest,
    replay_safety: ReplaySafety,
}

impl FamilyOperationBinding {
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

/// Exact logical family-attempt identity.
pub type FamilyAttemptBinding = AttemptBinding<FamilyOperationBinding, AttemptId>;
/// Exact active physical-request identity.
pub type FamilyPhysicalRequestBinding = PhysicalRequestBinding<FamilyAttemptBinding>;
/// Bounded receiver for normalized observations.
pub type FamilyAttemptEventReceiver<W> =
    mpsc::Receiver<Result<ModelEvent, FamilyAttemptProtocolError<W>>>;

/// Redaction-safe family/protocol failure supplied by a dialect hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FamilyAttemptProtocolError<W: FamilyWire> {
    wire: PhantomData<W>,
    terminal_facts: Option<Box<TerminalFacts>>,
    message: String,
    detail: Option<plakous_model::SafeDiagnostic>,
    http_status: Option<ModelHttpStatus>,
    retry_after: Option<ModelRetryDelay>,
    refusal: Option<ProviderRefusal>,
    context_overflow: bool,
    usage_limit: bool,
    compatibility_drift: bool,
    exchange_break: ExchangeBreak,
}

impl<W: FamilyWire> FamilyAttemptProtocolError<W> {
    pub(super) fn with_terminal_facts(mut self, facts: TerminalFacts) -> Self {
        self.terminal_facts = Some(Box::new(facts));
        self
    }
    /// Conservative facts when this failure closed an admitted operation.
    #[must_use]
    pub fn terminal_facts(&self) -> Option<TerminalFacts> {
        self.terminal_facts.as_deref().copied()
    }
    /// Conservative acceptance when this failure closed an admitted operation.
    #[must_use]
    pub fn provider_acceptance(&self) -> Option<ProviderAcceptance> {
        self.terminal_facts()
            .map(TerminalFacts::provider_acceptance)
    }
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
            wire: PhantomData,
            terminal_facts: None,
            message: message.into(),
            detail: None,
            http_status: None,
            retry_after: None,
            refusal: None,
            context_overflow: false,
            usage_limit: false,
            compatibility_drift: false,
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
    pub const fn with_refusal(mut self, refusal: ProviderRefusal) -> Self {
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

    /// Marks a dialect-proven refusal of its compiled adapter contract.
    #[must_use]
    pub const fn with_compatibility_drift(mut self) -> Self {
        self.compatibility_drift = true;
        self.refusal = None;
        self
    }

    /// Whether the dialect proved a compiled-contract rejection.
    #[must_use]
    pub const fn compatibility_drift(&self) -> bool {
        self.compatibility_drift
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

    /// Returns bounded provider retry-delay advice, when present.
    #[must_use]
    pub const fn retry_after(&self) -> Option<ModelRetryDelay> {
        self.retry_after
    }
}

impl<W: FamilyWire> fmt::Display for FamilyAttemptProtocolError<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl<W: FamilyWire> std::error::Error for FamilyAttemptProtocolError<W> {}

/// Provider-neutral attempt failure with conservative terminal facts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FamilyAttemptError<W: FamilyWire> {
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
        source: FamilyAttemptProtocolError<W>,
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

impl<W: FamilyWire> FamilyAttemptError<W> {
    /// Creates a fact-bearing protocol failure.
    #[must_use]
    pub fn protocol(source: FamilyAttemptProtocolError<W>, facts: TerminalFacts) -> Self {
        Self::Protocol {
            source: source.with_terminal_facts(facts),
            facts,
        }
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

    /// Whether a dialect proved that its compiled adapter contract was refused.
    #[must_use]
    pub const fn compatibility_drift(&self) -> bool {
        match self {
            Self::Protocol { source, .. } => source.compatibility_drift(),
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

impl<W: FamilyWire> fmt::Display for FamilyAttemptError<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { .. } => write!(formatter, "{} attempt cancelled", W::NAME),
            Self::Delivery { .. } => write!(formatter, "{} observation delivery failed", W::NAME),
            Self::Protocol { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Observation { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Binding { .. } => write!(formatter, "{} response binding mismatch", W::NAME),
            Self::ServiceContract { .. } => {
                write!(formatter, "{} service contract violation", W::NAME)
            }
            Self::Unavailable => write!(formatter, "{} service unavailable", W::NAME),
        }
    }
}

impl<W: FamilyWire> std::error::Error for FamilyAttemptError<W> {}

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
pub struct FamilyAttemptEventSink<W: FamilyWire> {
    sender: mpsc::Sender<Result<ModelEvent, FamilyAttemptProtocolError<W>>>,
    cancellation: CancellationToken,
    terminal: Arc<AtomicBool>,
    core: Arc<FamilyAttemptCore>,
}

impl<W: FamilyWire> FamilyAttemptEventSink<W> {
    /// Emits one non-terminal observation under bounded backpressure.
    pub async fn send(&mut self, event: ModelEvent) -> Result<(), FamilyAttemptError<W>> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        if matches!(event, ModelEvent::ResponseCompleted(_)) {
            return Err(self.contract());
        }
        let visible = event_has_visible_output(&event);
        let hosted = event_has_hosted_effect(&event);
        if hosted && !W::HOSTED_EFFECTS {
            return Err(self.contract());
        }
        self.core
            .observe_received_event(hosted)
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
    ) -> Result<(), FamilyAttemptError<W>> {
        self.send_item(Ok(ModelEvent::Retry(retry)), false).await
    }

    /// Emits one terminal protocol error and closes the channel.
    pub async fn send_error(
        &mut self,
        error: FamilyAttemptProtocolError<W>,
    ) -> Result<(), FamilyAttemptError<W>> {
        self.send_item(Err(error), true).await
    }

    async fn send_item(
        &mut self,
        item: Result<ModelEvent, FamilyAttemptProtocolError<W>>,
        terminal: bool,
    ) -> Result<(), FamilyAttemptError<W>> {
        if self.cancellation.is_cancelled() {
            return Err(self.cancelled());
        }
        if terminal {
            if self
                .terminal
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(FamilyAttemptError::<W>::Delivery {
                    facts: self.core.terminal_facts(),
                });
            }
        } else if self.terminal.load(Ordering::Acquire) {
            return Err(FamilyAttemptError::<W>::Delivery {
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
                Err(FamilyAttemptError::<W>::Delivery {
                    facts: self.core.terminal_facts(),
                })
            }
        }
    }

    fn contract(&self) -> FamilyAttemptError<W> {
        FamilyAttemptError::ServiceContract {
            facts: self.core.terminal_facts(),
        }
    }
    fn cancelled(&self) -> FamilyAttemptError<W> {
        FamilyAttemptError::<W>::Cancelled {
            facts: self
                .core
                .observations_ref()
                .is_service_admitted()
                .then(|| self.core.terminal_facts()),
        }
    }
    fn observation(&self, source: FamilyCoreError) -> FamilyAttemptError<W> {
        FamilyAttemptError::<W>::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
}

impl<W: FamilyWire> fmt::Debug for FamilyAttemptEventSink<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FamilyAttemptEventSink<W>")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("terminal", &self.terminal.load(Ordering::Acquire))
            .field("facts", &self.core.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// One owned credential-free family logical operation.
pub struct FamilyAttempt<W: FamilyWire> {
    binding: FamilyAttemptBinding,
    context: ModelCallContext,
    events: FamilyAttemptEventSink<W>,
    core: Arc<FamilyAttemptCore>,
}

impl<W: FamilyWire> FamilyAttempt<W> {
    /// Creates an attempt and its bounded observation receiver.
    #[must_use]
    pub fn with_event_channel(
        operation: FamilyOperationBinding,
        context: ModelCallContext,
        capacity: NonZeroUsize,
    ) -> (Self, FamilyAttemptEventReceiver<W>) {
        let core = Arc::new(FamilyAttemptCore::new_with_policy(W::LEDGER_POLICY));
        let (sender, receiver) = mpsc::channel(capacity.get());
        let events = FamilyAttemptEventSink {
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
    pub const fn binding(&self) -> &FamilyAttemptBinding {
        &self.binding
    }
    /// Returns cancellation/deadline/attempt context.
    #[must_use]
    pub const fn context(&self) -> &ModelCallContext {
        &self.context
    }
    /// Returns mutable bounded observation delivery.
    #[must_use]
    pub const fn events_mut(&mut self) -> &mut FamilyAttemptEventSink<W> {
        &mut self.events
    }
    /// Publishes the checked terminal for this exact attempt at most once.
    pub async fn publish_terminal(
        &mut self,
        sealed: &FamilyAttemptResponse<W>,
    ) -> Result<(), FamilyAttemptError<W>> {
        if sealed.binding != self.binding || sealed.facts != self.terminal_facts() {
            return Err(FamilyAttemptError::Binding {
                facts: self.terminal_facts(),
            });
        }
        self.events
            .send_item(
                Ok(ModelEvent::ResponseCompleted(sealed.response.clone())),
                true,
            )
            .await
    }
    /// Records transient retry diagnostics alongside the physical-request ledger.
    pub(super) fn record_failure(
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
    /// Returns current conservative facts.
    #[must_use]
    pub fn terminal_facts(&self) -> TerminalFacts {
        self.core.terminal_facts()
    }
    /// Verifies private service admission.
    pub fn ensure_service_admitted(&self) -> Result<(), FamilyAttemptError<W>> {
        self.core
            .ensure_service_admitted()
            .map_err(|source| self.observation(source))
    }
    /// Starts one physical generation.
    pub fn observe_physical_request_started(
        &self,
    ) -> Result<FamilyPhysicalRequestBinding, FamilyAttemptError<W>> {
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
    ) -> Result<(), FamilyAttemptError<W>> {
        self.core
            .physical_request_finished(outcome)
            .map_err(|source| self.observation(source))
    }
    /// Records exact provider acceptance from a dialect hook.
    pub fn observe_provider_accepted(&self) -> Result<(), FamilyAttemptError<W>> {
        self.core
            .observe_provider_accepted()
            .map_err(|source| self.observation(source))
    }
    /// Reserves the one family authorization slot for the active generation.
    pub fn reserve_authorization(
        &self,
    ) -> Result<FamilyPhysicalRequestBinding, FamilyAttemptError<W>> {
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
        physical: &FamilyPhysicalRequestBinding,
    ) -> Result<(), FamilyAttemptError<W>> {
        if physical.attempt() != &self.binding {
            return Err(FamilyAttemptError::<W>::Binding {
                facts: self.terminal_facts(),
            });
        }
        self.core
            .validate_primary_auth(physical.generation())
            .map_err(|source| self.observation(source))
    }
    fn observation(&self, source: FamilyCoreError) -> FamilyAttemptError<W> {
        FamilyAttemptError::<W>::Observation {
            source,
            facts: self.core.terminal_facts(),
        }
    }
}

impl<W: FamilyWire> fmt::Debug for FamilyAttempt<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FamilyAttempt<W>")
            .field("binding", &self.binding)
            .field("context", &self.context)
            .field("events", &self.events)
            .field("facts", &self.terminal_facts())
            .finish_non_exhaustive()
    }
}

/// Checked successful terminal aggregate for one exact family attempt.
#[derive(Debug, PartialEq)]
pub struct FamilyAttemptResponse<W: FamilyWire> {
    binding: FamilyAttemptBinding,
    response: ModelResponse,
    request_id: ModelProviderRequestId,
    stop: W::Stop,
    facts: TerminalFacts,
}

impl<W: FamilyWire> FamilyAttemptResponse<W> {
    /// Seals one neutral projection as the sole successful terminal result.
    pub fn new(
        attempt: &FamilyAttempt<W>,
        projection: W::Projection,
        request_id: Option<ModelProviderRequestId>,
    ) -> Result<Self, FamilyAttemptError<W>> {
        attempt.ensure_service_admitted()?;
        let (response, stop) = W::into_parts(projection);
        let request_id = request_id
            .or_else(|| {
                response
                    .provider_request_id()
                    .and_then(|value| ModelProviderRequestId::new(value).ok())
            })
            .ok_or_else(|| FamilyAttemptError::Binding {
                facts: attempt.terminal_facts(),
            })?;
        if response.plan_digest() != attempt.binding.operation().plan()
            || response.provider_request_id() != Some(request_id.as_str())
        {
            return Err(FamilyAttemptError::<W>::Binding {
                facts: attempt.terminal_facts(),
            });
        }
        if (!W::HOSTED_EFFECTS && response_has_hosted_effect(&response))
            || response.validate_for_commit().is_err()
        {
            return Err(FamilyAttemptError::ServiceContract {
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
    pub const fn binding(&self) -> &FamilyAttemptBinding {
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
    /// Returns typed family stop facts.
    #[must_use]
    pub const fn stop(&self) -> &W::Stop {
        &self.stop
    }
    /// Returns conservative physical facts.
    #[must_use]
    pub const fn facts(&self) -> TerminalFacts {
        self.facts
    }
}

struct AwaitHooks<W: FamilyWire> {
    core: Arc<FamilyAttemptCore>,
    cancellation: CancellationToken,
    wire: PhantomData<W>,
}
impl<W: FamilyWire> AwaitHooks<W> {
    fn contract(&self) -> FamilyAttemptError<W> {
        FamilyAttemptError::<W>::ServiceContract {
            facts: self.core.terminal_facts(),
        }
    }
}
impl<W: FamilyWire> AdmittedAwaitHooks for AwaitHooks<W> {
    type Ok = FamilyAttemptResponse<W>;
    type Err = FamilyAttemptError<W>;
    fn on_cleanup_exhausted(&self) -> Self::Err {
        FamilyAttemptError::<W>::Cancelled {
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
            Err(FamilyAttemptError::<W>::Cancelled {
                facts: Some(self.core.terminal_facts()),
            })
        } else {
            result
        }
    }
}

struct CallContext {
    binding: FamilyAttemptBinding,
    core: Arc<FamilyAttemptCore>,
}
struct CallDriver<W: FamilyWire>(PhantomData<W>);
impl<W: FamilyWire> AdmittedCallDriver for CallDriver<W> {
    type Request = FamilyAttempt<W>;
    type Response = FamilyAttemptResponse<W>;
    type Error = FamilyAttemptError<W>;
    type Seal = LedgerOnlyAdmission;
    type Binding = CallContext;
    type Hooks = AwaitHooks<W>;
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
        FamilyAttemptError::<W>::Unavailable
    }
    fn on_cancel(request: &Self::Request) -> Self::Error {
        FamilyAttemptError::<W>::Cancelled {
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
            wire: PhantomData,
        };
        Ok((seal, hooks))
    }
    fn on_call_panic(seal: &mut Self::Seal, hooks: &Self::Hooks) -> Self::Error {
        ServiceAdmissionSeal::seal(seal);
        hooks.contract()
    }
    fn on_binding_mismatch(response: &Self::Response, expected: &Self::Binding) -> Self::Error {
        if response.binding == expected.binding {
            FamilyAttemptError::<W>::ServiceContract {
                facts: expected.core.terminal_facts(),
            }
        } else {
            FamilyAttemptError::<W>::Binding {
                facts: expected.core.terminal_facts(),
            }
        }
    }
    fn await_policy() -> AdmittedAwaitPolicy {
        AdmittedAwaitPolicy {
            max_cleanup_polls: MAX_ADMITTED_CLEANUP_POLLS.saturating_sub(1),
        }
    }
}

/// Privately admits and executes exactly one complete family operation.
pub async fn call_family_operation<W: FamilyWire, S>(
    service: &mut S,
    attempt: FamilyAttempt<W>,
) -> Result<FamilyAttemptResponse<W>, FamilyAttemptError<W>>
where
    S: Service<
            FamilyAttempt<W>,
            Response = FamilyAttemptResponse<W>,
            Error = FamilyAttemptError<W>,
        > + Send
        + 'static,
    S::Future:
        Future<Output = Result<FamilyAttemptResponse<W>, FamilyAttemptError<W>>> + Send + 'static,
{
    let cancelled = Box::pin(attempt.context.cancellation().clone().cancelled());
    call_admitted_operation(service, attempt, cancelled, CallDriver::<W>(PhantomData)).await
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
mod tests;
