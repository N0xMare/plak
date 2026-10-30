//! The target skeleton the OpenRouter Chat and Messages wires share: the
//! prepared projection, the joined attempt stream, and the gateway error
//! classification. Each wire supplies its labels, revisions and error
//! mapping as a small [`WireProjection`] and [`AttemptWire`].

use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use plakous_attempt::{ExchangeBreak, FailureRisk, PhysicalRequestOutcome, TerminalFacts};
use plakous_http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use plakous_model::{
    CompiledProviderConfiguration, CountObservation, Digest, DigestDomain, ModelDiagnostic,
    ModelError, ModelErrorContext, ModelErrorKind, ModelErrorPhase, ModelEvent, ModelHttpStatus,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelRetryRisk,
    ModelRetryability, PreparedCacheUse, PreparedCallRecord, PreparedModelProjection,
    PreparedOutputLimit, PreparedTokenProjection, ProtocolId, ResolvedModelDescriptor,
    RuntimeModelAuthorityError, ValidatedRequestPlan,
};

use crate::{OpenRouterChatRouteIdentity, OpenRouterKeyGeneration};

pub(crate) struct GatewayExecution {
    pub(crate) client: plakous_http::Client,
    pub(crate) origin: plakous_http::Origin,
    pub(crate) clock: std::sync::Arc<dyn plakous_model::Clock>,
    pub(crate) timeout: std::time::Duration,
    pub(crate) error_bytes: usize,
    pub(crate) event_capacity: usize,
    pub(crate) turn_rooted: bool,
}

pub(crate) fn prepare_model_call<A, W, H>(
    record: PreparedCallRecord,
    descriptor: ResolvedModelDescriptor,
    execution: GatewayExecution,
    operation: plakous_exchange::family::PreparedFamilyHttpOperation<W>,
    hooks: H,
    redispatch: Option<plakous_exchange::family::FamilyRedispatchFn<W, H>>,
    diagnostics: VecDeque<ModelDiagnostic>,
) -> plakous_model::PreparedModelCall
where
    W: plakous_exchange::family::FamilyWire + Unpin,
    H: plakous_exchange::family::FamilyHttpHooks<W>,
    A: AttemptWire<
            Response = plakous_exchange::family::FamilyAttemptResponse<W>,
            AttemptError = plakous_exchange::family::FamilyAttemptError<W>,
            ProtocolError = plakous_exchange::family::FamilyAttemptProtocolError<W>,
        > + 'static,
{
    use plakous_exchange::family::{
        FamilyAttempt, FamilyHttpAttemptService, call_family_operation,
    };
    plakous_model::PreparedModelCall::new(record, move |call_context| {
        Box::pin(async move {
            let capacity = std::num::NonZeroUsize::new(execution.event_capacity)
                .unwrap_or(std::num::NonZeroUsize::MIN);
            let (attempt, receiver) = FamilyAttempt::<W>::with_event_channel(
                operation.binding().clone(),
                call_context,
                capacity,
            );
            let mut service = FamilyHttpAttemptService::new(
                execution.client,
                execution.origin,
                operation,
                hooks,
                execution.clock,
                std::sync::Arc::new(plakous_model::SystemIdGenerator),
                execution.timeout,
                execution.error_bytes,
                execution.turn_rooted,
            );
            if let Some(redispatch) = redispatch {
                service = service.with_retry(redispatch);
            }
            let operation =
                Box::pin(async move { call_family_operation(&mut service, attempt).await });
            Ok(Box::pin(JoinedStream::<A, _>::new(
                descriptor,
                diagnostics,
                operation,
                receiver,
            )) as plakous_model::ModelStream)
        })
    })
}

pub(crate) fn valid_transport_bounds(
    timeout: std::time::Duration,
    capacity: usize,
    error_bytes: usize,
) -> bool {
    !timeout.is_zero() && capacity != 0 && error_bytes != 0
}

pub(crate) fn runtime_authority(
    descriptor: &ResolvedModelDescriptor,
    identity: &OpenRouterChatRouteIdentity,
    grant: &OpenRouterKeyGeneration,
    checked_at: plakous_model::Timestamp,
    expires_at: plakous_model::Timestamp,
) -> Result<plakous_model::RuntimeModelAuthority, RuntimeModelAuthorityError> {
    plakous_model::RuntimeModelAuthority::new(
        descriptor,
        identity.account_binding(),
        identity.credential_authority_binding(),
        generation_number(grant),
        checked_at,
        Some(expires_at),
    )
}

pub(crate) fn gateway_capabilities(
    evidence: &crate::OpenRouterChatModelEvidence,
    provenance: plakous_model::FactProvenance,
    constraint: &str,
) -> plakous_model::CapabilitySet {
    use plakous_model::{
        Capability, CapabilityConstraints, CapabilitySet, FeatureSupport, Support,
    };
    let constraints = CapabilityConstraints::new(None, Some(constraint.to_owned()));
    let mut features = [
        Capability::Streaming,
        Capability::Tools,
        Capability::Reasoning,
    ]
    .into_iter()
    .map(|capability| {
        (
            capability,
            FeatureSupport::new(Support::Native, constraints.clone(), provenance),
        )
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(image) = crate::chat::image_input_fact(evidence) {
        features.insert(Capability::ImageInput, image);
    }
    CapabilitySet::new(features, std::collections::BTreeMap::new())
}

pub(crate) fn compile_configuration(
    selected: plakous_model::SelectedProviderConfiguration,
    descriptor: &ResolvedModelDescriptor,
    implementation: plakous_model::ModelImplementationIdentity,
    authority: plakous_model::ResolvedProviderAuthorityBinding,
    policy: plakous_model::FamilyPolicyEnvelope,
) -> Result<CompiledProviderConfiguration, plakous_model::ProviderConfigurationError> {
    let effective = plakous_model::EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation,
        authority,
        selected.behavior().clone(),
        policy,
    )?;
    CompiledProviderConfiguration::new(selected, effective)
}

#[derive(Clone)]
pub(crate) struct GatewayErrorMetadata {
    status: u16,
    retry_after: Option<std::time::Duration>,
}
impl GatewayErrorMetadata {
    pub(crate) const fn status(&self) -> u16 {
        self.status
    }
    pub(crate) const fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }
}

/// Saturate provider advice at the representable 24 h, before retry's 60 s clamp.
pub(crate) fn retry_delay(advice: std::time::Duration) -> Option<plakous_model::ModelRetryDelay> {
    let millis = u64::try_from(advice.as_millis())
        .unwrap_or(u64::MAX)
        .min(plakous_model::ModelRetryDelay::MAX_MILLIS);
    plakous_model::ModelRetryDelay::from_millis(millis).ok()
}

pub(crate) fn classify_response_head<W: plakous_exchange::family::FamilyWire>(
    response: &plakous_http::Response,
    request_id: impl FnOnce() -> Result<
        Option<plakous_model::ModelProviderRequestId>,
        plakous_exchange::family::FamilyAttemptProtocolError<W>,
    >,
) -> Result<
    plakous_exchange::family::FamilyHttpResponseHead<GatewayErrorMetadata>,
    plakous_exchange::family::FamilyAttemptProtocolError<W>,
> {
    use plakous_exchange::family::{
        FamilyAttemptProtocolError, FamilyHttpErrorHead, FamilyHttpResponseHead,
    };
    let status = response.status();
    if status != StatusCode::OK {
        return Ok(FamilyHttpResponseHead::Error(FamilyHttpErrorHead {
            metadata: GatewayErrorMetadata {
                status: status.as_u16(),
                retry_after: response
                    .headers()
                    .get(&HeaderName::from_static("retry-after"))
                    .and_then(|value| value.to_str().ok())
                    .and_then(plakous_attempt::retry_after_seconds),
            },
            outcome: post_send_status_outcome(status),
            provider_accepted: false,
        }));
    }
    let content_type = response
        .headers()
        .get(&HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !is_sse_content_type(content_type) {
        return Err(FamilyAttemptProtocolError::new(format!(
            "OpenRouter {} success was not an SSE response",
            W::NAME
        )));
    }
    Ok(request_id()?.map_or(
        FamilyHttpResponseHead::StreamWithoutRequestId,
        FamilyHttpResponseHead::Stream,
    ))
}

/// The labels and revisions one wire's prepared projection carries.
pub(crate) struct WireProjection {
    /// The wire name in rejection messages: `Chat` or `Messages`.
    pub(crate) label: &'static str,
    /// The domain prefix of the wire digest.
    pub(crate) wire_domain: &'static [u8],
    pub(crate) family_id: &'static str,
    pub(crate) lowering_revision: &'static str,
    pub(crate) projection_revision: &'static str,
    pub(crate) accounting_revision: &'static str,
}

impl WireProjection {
    pub(crate) fn prepare_http<W: plakous_exchange::family::FamilyWire>(
        target: Digest,
        plan: &ValidatedRequestPlan,
        operation: Digest,
        replay: plakous_attempt::ReplaySafety,
        body: &[u8],
        stream: W::StreamBinding,
    ) -> plakous_exchange::family::PreparedFamilyHttpOperation<W> {
        use plakous_exchange::family::{FamilyOperationBinding, PreparedFamilyHttpOperation};
        PreparedFamilyHttpOperation::new(
            FamilyOperationBinding::new(target, plan.digest(), operation, replay),
            std::sync::Arc::<[u8]>::from(body),
            stream,
        )
    }
    /// Admits a plan to this wire's compiled configuration: its descriptor
    /// binding, its selected and effective identities, its behavior, its plan
    /// admission and the preparation authority.
    pub(crate) fn admit_plan(
        &self,
        descriptor: &ResolvedModelDescriptor,
        configuration: &CompiledProviderConfiguration,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
    ) -> Result<(), ModelPreparationError> {
        descriptor.validate_plan_binding(plan)?;
        if plan.request().selected_provider_configuration_id() != configuration.selected_identity()
            || plan.request().effective_provider_configuration_id()
                != configuration.effective_identity()
            || !crate::execution::request_matches_configuration(
                plan.request(),
                configuration.selected().behavior(),
            )
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                format!("OpenRouter {} request configuration is foreign", self.label),
            ));
        }
        descriptor.validate_plan_admission(plan.request(), plan)?;
        context
            .authority()
            .validate(descriptor, context.prepared_at())?;
        Ok(())
    }

    /// Admits the route the preparation authority names and returns the
    /// configuration's enforced output bound.
    pub(crate) fn admit_route(
        &self,
        identity: &OpenRouterChatRouteIdentity,
        configuration: &CompiledProviderConfiguration,
        context: &ModelPreparationContext,
    ) -> Result<NonZeroU32, ModelPreparationError> {
        if context.authority().account_binding() != identity.account_binding()
            || context.authority().credential_authority_binding()
                != identity.credential_authority_binding()
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        configuration
            .selected()
            .behavior()
            .output_bound
            .bound()
            .ok_or_else(|| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    format!(
                        "OpenRouter {} requires an enforced output bound",
                        self.label
                    ),
                )
            })
    }

    /// Measures one lowered body and seals its prepared-call record.
    pub(crate) fn prepare_record(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        body: &[u8],
        output_bound: NonZeroU32,
    ) -> Result<PreparedCallRecord, ModelPreparationError> {
        let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[self.wire_domain, body])
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    format!("OpenRouter {} wire identity failed", self.label),
                )
            })?;
        let wire_size = u64::try_from(body.len()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                format!("OpenRouter {} request is too large", self.label),
            )
        })?;
        let observation = descriptor
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .map(|certificate| {
                CountObservation::new(
                    certificate.certificate_id(),
                    certificate.revision(),
                    wire_size,
                )
            })
            .transpose()
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    format!("OpenRouter {} context observation is invalid", self.label),
                )
            })?;
        let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, observation)?
            .with_input_parts(plakous_model::ContextInputParts::measure_wire(body))
            .with_wire_input(
                body,
                Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                context,
            )?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            output_bound,
            "max_tokens",
            wire_digest,
            self.lowering_revision,
        )?;
        let projection = PreparedModelProjection::new(
            descriptor,
            plan,
            context,
            ProtocolId::new(self.family_id).map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    format!("{} family identity is invalid", self.label),
                )
            })?,
            self.lowering_revision,
            self.projection_revision,
            self.accounting_revision,
            wire_digest,
            wire_size,
            token_projection,
            context
                .cache_affinity()
                .map_or(PreparedCacheUse::None, |affinity| {
                    PreparedCacheUse::RuntimeAffinity {
                        affinity: affinity.identity(),
                    }
                }),
            Some(output_limit),
        )?;
        PreparedCallRecord::for_prepared(descriptor, plan, projection, context)
    }
}

pub(crate) fn rejection(
    kind: ModelPreparationErrorKind,
    message: impl AsRef<str>,
) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

/// One wire's attempt: its response and errors, and how they become model
/// errors.
pub(crate) trait AttemptWire {
    type Response;
    type AttemptError;
    type ProtocolError;

    fn model_error(descriptor: &ResolvedModelDescriptor, error: &Self::AttemptError) -> ModelError;

    fn observation_error(
        descriptor: &ResolvedModelDescriptor,
        error: &Self::ProtocolError,
    ) -> ModelError;
}

pub(crate) type WireOperation<W> = Pin<
    Box<
        dyn Future<Output = Result<<W as AttemptWire>::Response, <W as AttemptWire>::AttemptError>>
            + Send,
    >,
>;

/// The model stream of one attempt: diagnostics known before dispatch, then
/// the attempt's events, then its joined terminal.
pub(crate) struct JoinedStream<W: AttemptWire, R> {
    descriptor: ResolvedModelDescriptor,
    pre_dispatch_diagnostics: VecDeque<ModelDiagnostic>,
    operation: WireOperation<W>,
    receiver: R,
    operation_done: bool,
    finished: bool,
    terminal_error: Option<ModelError>,
    observation_error: Option<W::ProtocolError>,
    wire: PhantomData<fn() -> W>,
}

impl<W: AttemptWire, R> JoinedStream<W, R> {
    pub(crate) fn new(
        descriptor: ResolvedModelDescriptor,
        pre_dispatch_diagnostics: VecDeque<ModelDiagnostic>,
        operation: WireOperation<W>,
        receiver: R,
    ) -> Self {
        Self {
            descriptor,
            pre_dispatch_diagnostics,
            operation,
            receiver,
            operation_done: false,
            finished: false,
            terminal_error: None,
            observation_error: None,
            wire: PhantomData,
        }
    }
}

impl<W, R> Stream for JoinedStream<W, R>
where
    W: AttemptWire,
    W::ProtocolError: Unpin,
    R: Stream<Item = Result<ModelEvent, W::ProtocolError>> + Unpin,
{
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Some(diagnostic) = self.pre_dispatch_diagnostics.pop_front() {
            return Poll::Ready(Some(Ok(ModelEvent::Diagnostic(diagnostic))));
        }
        match Pin::new(&mut self.receiver).poll_next(context) {
            Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(Err(error))) => self.observation_error = Some(error),
            Poll::Ready(None) | Poll::Pending => {}
        }
        if !self.operation_done {
            match self.operation.as_mut().poll(context) {
                Poll::Pending => {}
                Poll::Ready(result) => {
                    self.operation_done = true;
                    self.terminal_error = result
                        .err()
                        .map(|error| W::model_error(&self.descriptor, &error));
                }
            }
        }
        // Polling the operation can enqueue a successful prefix and its error
        // together. Drain that prefix before publishing the joined terminal.
        loop {
            match Pin::new(&mut self.receiver).poll_next(context) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
                Poll::Ready(Some(Err(error))) => self.observation_error = Some(error),
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        if !self.operation_done {
            return Poll::Pending;
        }
        self.finished = true;
        let error = self.terminal_error.take().or_else(|| {
            self.observation_error
                .take()
                .map(|error| W::observation_error(&self.descriptor, &error))
        });
        Poll::Ready(error.map(Err))
    }
}

/// Classifies a failure the provider did not name.
///
/// Only a possible hosted effect leaves the outcome unknown. A break after
/// acceptance without one is a definite failure of its own kind: the partial
/// output is discarded with the turn and nothing replays it.
pub(crate) fn inferred_failure_kind(
    facts: Option<TerminalFacts>,
    exchange_break: ExchangeBreak,
) -> ModelErrorKind {
    if facts.map(TerminalFacts::failure_risk) == Some(FailureRisk::HostedEffect) {
        return ModelErrorKind::OutcomeUnknown;
    }
    match exchange_break {
        ExchangeBreak::Transport => ModelErrorKind::Transport,
        ExchangeBreak::Protocol => ModelErrorKind::Protocol,
    }
}

/// Builds a gateway operation error whose retryability states the risk its
/// facts leave behind, so no caller mistakes a post-acceptance failure for a
/// replayable one. The HTTP status the provider named, before the stream or
/// inside it, rides on the typed context.
pub(crate) fn operation_error(
    descriptor: &ResolvedModelDescriptor,
    kind: ModelErrorKind,
    facts: Option<TerminalFacts>,
    http_status: Option<u16>,
    message: String,
) -> ModelError {
    let retryability = match (kind, facts.map(TerminalFacts::failure_risk)) {
        (ModelErrorKind::Cancelled, _) | (_, None | Some(FailureRisk::None)) => {
            ModelRetryability::Permanent
        }
        (_, Some(FailureRisk::DuplicateInferenceOrBilling)) => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
        (_, Some(FailureRisk::HostedEffect)) => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
    };
    ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::Terminal,
        retryability,
    )
    .map(
        |context| match http_status.and_then(|status| ModelHttpStatus::new(status).ok()) {
            Some(status) => context.with_http_status(status),
            None => context,
        },
    )
    .and_then(|context| ModelError::contextual(kind, message.clone(), context))
    .unwrap_or_else(|_| ModelError::new(kind, message))
}

/// Reads a refusal the provider named as the definite outcome it is.
///
/// No hosted effect ran and the provider said why it did no work, so there is
/// nothing to reconcile and nothing to recover: no response was committed and
/// the canonical history is untouched. Acceptance does not change that, and on
/// this route it especially does not — `OpenRouter` commits HTTP 200 at
/// acceptance and then reports upstream failures inside the stream, so every
/// in-band refusal is post-acceptance by construction.
///
/// Visible output does not change it either. The live shape had a chunk stream
/// before the `520` arrived; that text is discarded with the turn and creates
/// no state anything could fail to reconcile. Only a hosted effect does.
///
/// `None` means this failure names no refusal, and the caller's own reading
/// stands.
pub(crate) fn named_refusal_kind(
    refusal: Option<plakous_attempt::ProviderRefusal>,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    if facts.is_some_and(TerminalFacts::hosted_effects_possible) {
        return None;
    }
    match refusal? {
        plakous_attempt::ProviderRefusal::RateLimit => Some(ModelErrorKind::RateLimited),
        plakous_attempt::ProviderRefusal::Overloaded => Some(ModelErrorKind::Unavailable),
        plakous_attempt::ProviderRefusal::Deadline => Some(ModelErrorKind::Timeout),
    }
}

/// Exhausted entitlement is terminal unless hosted effects make the outcome unknown.
pub(crate) fn usage_limit_kind(
    usage_limit: bool,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    (usage_limit && !facts.is_some_and(TerminalFacts::hosted_effects_possible))
        .then_some(ModelErrorKind::QuotaExceeded)
}

/// A rejection of the input as over the model's window is its own kind: no
/// inference ran, and the runtime answers it by compacting and sending a new,
/// smaller request rather than by retrying this one.
pub(crate) fn context_overflow_kind(
    context_overflow: bool,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    (context_overflow && !facts.is_some_and(TerminalFacts::hosted_effects_possible))
        .then_some(ModelErrorKind::ContextLimit)
}

/// Whether an OpenRouter error body names a context-window overflow, in its
/// own message, the upstream's raw message, or its code.
pub(crate) fn body_reports_context_overflow(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let code = value
        .pointer("/error/code")
        .and_then(serde_json::Value::as_str);
    ["/error/message", "/error/metadata/raw", "/message"]
        .into_iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
        .chain(std::iter::once(""))
        .any(|message| plakous_model::provider_rejection_is_context_overflow(code, message))
}

pub(crate) fn is_sse_content_type(value: &str) -> bool {
    value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim()
        .eq_ignore_ascii_case("text/event-stream")
}

/// Classifies a non-200 head on every OpenRouter wire.
pub(crate) fn post_send_status_outcome(status: StatusCode) -> PhysicalRequestOutcome {
    // OpenRouter commits HTTP 200 at provider acceptance. Later policy or
    // rate-limit errors are carried inside that stream, not initial 403/429,
    // and an initial 401 is the gateway refusing the credential itself before
    // acceptance. A 503 (no provider available) or 529 is a refusal before
    // any upstream took the request.
    if matches!(status.as_u16(), 401 | 403 | 429 | 503 | 529) {
        PhysicalRequestOutcome::DefiniteRejection
    } else {
        PhysicalRequestOutcome::AmbiguousPostSend
    }
}

pub(crate) fn generation_number(grant: &OpenRouterKeyGeneration) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

/// The route's truthful attribution headers, when it has attribution.
pub(crate) fn insert_attribution(identity: &OpenRouterChatRouteIdentity, headers: &mut HeaderMap) {
    if let Some(attribution) = identity.attribution() {
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static(attribution.referer()),
        );
        headers.insert(
            HeaderName::from_static("x-openrouter-title"),
            HeaderValue::from_static(attribution.title()),
        );
        headers.insert(
            HeaderName::from_static("x-openrouter-categories"),
            HeaderValue::from_static(crate::OpenRouterAttribution::CATEGORY),
        );
    }
}

#[cfg(test)]
mod tests;
