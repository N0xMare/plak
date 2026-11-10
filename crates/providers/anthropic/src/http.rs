//! Anthropic HTTP dialect hooks for the neutral Messages exchange host.

#[cfg(feature = "native-auth")]
pub(crate) mod compatibility;
mod usage_limit;

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use plakous_exchange::network_failure;
use plakous_http::{
    Client, ErrorKind as HttpErrorKind, Failure as HttpFailure, HeaderMap, HeaderName, HeaderValue,
    Method, Origin, Phase as HttpPhase, Request as HttpRequest, SensitiveBytes, StatusCode,
    sensitive_header_value,
};
use plakous_messages::{
    MessagesAttempt, MessagesAttemptError, MessagesAttemptProtocolError, MessagesAttemptResponse,
    MessagesHttpAttemptService, MessagesHttpErrorHead, MessagesHttpHooks, MessagesHttpResponseHead,
    MessagesHttpSafety, MessagesOperationBinding, PhysicalRequestOutcome,
    PreparedMessagesHttpOperation, ProviderAcceptance, call_messages_operation,
};
use plakous_model::{
    Clock, IdGenerator, ModelHttpStatus, ModelProviderRequestId, ModelRetryDelay, SystemClock,
    SystemIdGenerator,
};
use tower_service::Service;

use crate::{
    ANTHROPIC_API_VERSION, ANTHROPIC_MESSAGES_PATH, AnthropicError, AnthropicErrorKind,
    AnthropicErrorPhase, AnthropicPlatformRoute, PreparedMessagesRequest, RetryAdvice, RetrySafety,
    classify_provider_error_with_safety, error::MAX_PROVIDER_ERROR_BODY_BYTES,
};

const CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const ACCEPT: HeaderName = HeaderName::from_static("accept");
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const USER_AGENT: HeaderName = HeaderName::from_static("user-agent");
const REQUEST_ID: HeaderName = HeaderName::from_static("request-id");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
const RATE_LIMIT_CLAIM: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-representative-claim");
const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Recommended nonzero family observation capacity.
pub(crate) const STANDARD_ANTHROPIC_EVENT_CAPACITY: NonZeroUsize = match NonZeroUsize::new(32) {
    Some(value) => value,
    None => unreachable!(),
};

/// Exact local exchange timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnthropicHttpDeadline(Duration);

impl AnthropicHttpDeadline {
    /// Creates a nonzero timeout.
    pub fn new(value: Duration) -> Result<Self, AnthropicError> {
        if value.is_zero() {
            return Err(invalid_configuration(
                "Anthropic HTTP exchange deadline must be nonzero",
            ));
        }
        Ok(Self(value))
    }
    /// Returns the exact duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }
}

/// Sealed Anthropic Messages HTTP factory.
#[derive(Clone)]
pub struct AnthropicLocalHttpServiceFactory {
    client: Client,
    dial_origin: Origin,
    route: AnthropicPlatformRoute,
    prepared: PreparedMessagesRequest,
    operation: PreparedMessagesHttpOperation,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    deadline: AnthropicHttpDeadline,
    renewal: Option<Arc<dyn ClaudeCredentialRenewal>>,
    redispatch_revalidation: Option<AnthropicRetryRevalidation>,
    dispatch_authorization: Option<AnthropicDispatchAuthorization>,
}

/// One physical dispatch's deferred subscription authorization.
#[derive(Clone)]
pub(crate) struct AnthropicDispatchAuthorization {
    pub(crate) authorize: Arc<dyn Fn() -> Result<(), AnthropicError> + Send + Sync>,
    pub(crate) transport_failed: Arc<dyn Fn(HttpFailure) + Send + Sync>,
}

/// A refreshed route and its optional one-use dispatch authorization.
pub struct AnthropicRetryGrant {
    pub(crate) route: AnthropicPlatformRoute,
    pub(crate) dispatch_authorization: Option<AnthropicDispatchAuthorization>,
}

impl AnthropicRetryGrant {
    /// Returns a refreshed route that requires no subscription permit.
    #[must_use]
    pub fn for_api_key(route: AnthropicPlatformRoute) -> Self {
        Self {
            route,
            dispatch_authorization: None,
        }
    }
}

/// Forced credential renewal after the provider answered a Messages dispatch
/// with HTTP 401. The renewed route keeps the prepared credential authority,
/// so the single re-dispatch is the same logical operation with a live grant.
pub trait ClaudeCredentialRenewal: Send + Sync + 'static {
    /// Forces one refresh and returns `route` carrying the renewed grant.
    fn renew_after_rejection<'a>(
        &'a self,
        route: &'a AnthropicPlatformRoute,
    ) -> Pin<Box<dyn Future<Output = Result<AnthropicRetryGrant, AnthropicError>> + Send + 'a>>;
}

/// Obtains a fresh dispatch grant for the same selected route before a safe retry.
pub type AnthropicRetryRevalidation = Arc<
    dyn Fn(
            AnthropicPlatformRoute,
        )
            -> Pin<Box<dyn Future<Output = Result<AnthropicRetryGrant, AnthropicError>> + Send>>
        + Send
        + Sync,
>;

impl AnthropicLocalHttpServiceFactory {
    /// Seals an exact HTTPS loopback fixture.
    #[doc(hidden)]
    pub fn new(
        client: Client,
        dial_origin: Origin,
        route: AnthropicPlatformRoute,
        prepared: PreparedMessagesRequest,
    ) -> Result<Self, AnthropicError> {
        Self::with_runtime(
            client,
            dial_origin,
            route,
            prepared,
            Arc::new(SystemClock),
            Arc::new(SystemIdGenerator),
            AnthropicHttpDeadline::new(DEFAULT_EXCHANGE_TIMEOUT)?,
        )
    }

    /// Seals a deterministic loopback fixture with injected identities.
    #[doc(hidden)]
    pub fn with_runtime(
        client: Client,
        dial_origin: Origin,
        route: AnthropicPlatformRoute,
        prepared: PreparedMessagesRequest,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        deadline: AnthropicHttpDeadline,
    ) -> Result<Self, AnthropicError> {
        require_loopback_origin(&dial_origin)?;
        Self::seal(client, dial_origin, route, prepared, clock, ids, deadline)
    }

    /// Seals one production Messages HTTP factory at the route's exact origin.
    pub fn for_route(
        client: Client,
        route: AnthropicPlatformRoute,
        prepared: PreparedMessagesRequest,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        deadline: AnthropicHttpDeadline,
    ) -> Result<Self, AnthropicError> {
        let dial_origin = Origin::parse(route.origin()).map_err(|error| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                error.to_string(),
            )
        })?;
        Self::seal(client, dial_origin, route, prepared, clock, ids, deadline)
    }

    fn seal(
        client: Client,
        dial_origin: Origin,
        route: AnthropicPlatformRoute,
        prepared: PreparedMessagesRequest,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        deadline: AnthropicHttpDeadline,
    ) -> Result<Self, AnthropicError> {
        prepared.validate_route(&route)?;
        let operation = PreparedMessagesHttpOperation::new(
            neutral_binding(&prepared),
            Arc::<[u8]>::from(prepared.body()),
            prepared.stream_binding()?,
        );
        Ok(Self {
            client,
            dial_origin,
            route,
            prepared,
            operation,
            clock,
            ids,
            deadline,
            renewal: None,
            redispatch_revalidation: None,
            dispatch_authorization: None,
        })
    }

    /// Attaches the forced-renewal seam that enables one refresh-and-retry
    /// after an HTTP 401. Routes without it keep the zero-retry exchange.
    #[must_use]
    pub fn with_credential_renewal(mut self, renewal: Arc<dyn ClaudeCredentialRenewal>) -> Self {
        self.renewal = Some(renewal);
        self
    }

    /// Enables revalidated retries for turns and unsent workflows; each
    /// re-sent generation first passes this revalidation.
    #[must_use]
    pub fn with_retry(mut self, revalidate: AnthropicRetryRevalidation) -> Self {
        self.redispatch_revalidation = Some(revalidate);
        self
    }

    /// Carries one subscription permit into the physical request builder.
    #[cfg(feature = "native-auth")]
    #[must_use]
    pub(crate) fn with_dispatch_authorization(
        mut self,
        authorization: AnthropicDispatchAuthorization,
    ) -> Self {
        self.dispatch_authorization = Some(authorization);
        self
    }

    /// Returns exact neutral operation authority.
    #[must_use]
    pub const fn operation_binding(&self) -> &MessagesOperationBinding {
        self.operation.binding()
    }

    /// Creates one fresh shared-family service.
    #[must_use]
    pub fn make_service(&self) -> AnthropicLocalHttpService {
        let hooks = AnthropicHttpHooks {
            route: self.route.clone(),
            prepared: self.prepared.clone(),
            renewal: self.renewal.clone(),
            redispatch_revalidation: self.redispatch_revalidation.clone(),
            dispatch_authorization: self.dispatch_authorization.clone(),
        };
        let inner = MessagesHttpAttemptService::new(
            self.client.clone(),
            self.dial_origin.clone(),
            self.operation.clone(),
            hooks,
            Arc::clone(&self.clock),
            Arc::clone(&self.ids),
            self.deadline.duration(),
            MAX_PROVIDER_ERROR_BODY_BYTES,
            self.prepared.turn_rooted(),
        );
        let inner = match self.renewal {
            Some(_) => inner.with_credential_rejected(anthropic_credential_rejected),
            None => inner,
        };
        AnthropicLocalHttpService {
            inner: if self.redispatch_revalidation.is_some() {
                inner.with_retry(anthropic_redispatch)
            } else {
                inner
            },
        }
    }
}

impl fmt::Debug for AnthropicLocalHttpServiceFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicLocalHttpServiceFactory")
            .field("dial_origin", &"<loopback>")
            .field("route", &self.route)
            .field("prepared", &self.prepared)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// Anthropic wrapper around the neutral Messages HTTP attempt service.
pub struct AnthropicLocalHttpService {
    inner: MessagesHttpAttemptService<AnthropicHttpHooks>,
}

impl Service<MessagesAttempt> for AnthropicLocalHttpService {
    type Response = MessagesAttemptResponse;
    type Error = MessagesAttemptError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, attempt: MessagesAttempt) -> Self::Future {
        self.inner.call(attempt)
    }
}

/// Starts one Anthropic Messages model stream from a sealed HTTP factory.
#[must_use]
pub fn start_anthropic_messages_model_call(
    factory: AnthropicLocalHttpServiceFactory,
    descriptor: plakous_model::ResolvedModelDescriptor,
    context: plakous_model::ModelCallContext,
) -> plakous_model::StartModelCall {
    Box::pin(async move {
        let (attempt, events) = MessagesAttempt::with_event_channel(
            factory.operation_binding().clone(),
            context,
            STANDARD_ANTHROPIC_EVENT_CAPACITY,
        );
        let pre_dispatch_diagnostics = factory
            .prepared
            .replay_diagnostics()
            .iter()
            .map(plakous_model::ModelDiagnostic::new)
            .chain(
                factory
                    .prepared
                    .historical_projection_diagnostics()
                    .iter()
                    .cloned(),
            )
            .collect();
        let mut service = factory.make_service();
        let operation =
            Box::pin(async move { call_local_anthropic_operation(&mut service, attempt).await });
        Ok(Box::pin(AnthropicMessagesModelStream {
            events,
            pre_dispatch_diagnostics,
            operation: Some(operation),
            descriptor,
            pending_protocol: None,
            pending_error: None,
            done: false,
        }) as plakous_model::ModelStream)
    })
}

type AnthropicMessagesOperation =
    Pin<Box<dyn Future<Output = Result<MessagesAttemptResponse, MessagesAttemptError>> + Send>>;

struct AnthropicMessagesModelStream {
    pre_dispatch_diagnostics: VecDeque<plakous_model::ModelDiagnostic>,
    events: plakous_messages::MessagesAttemptEventReceiver,
    operation: Option<AnthropicMessagesOperation>,
    descriptor: plakous_model::ResolvedModelDescriptor,
    pending_protocol: Option<String>,
    pending_error: Option<plakous_model::ModelError>,
    done: bool,
}

impl Stream for AnthropicMessagesModelStream {
    type Item = Result<plakous_model::ModelEvent, plakous_model::ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if let Some(diagnostic) = self.pre_dispatch_diagnostics.pop_front() {
            return Poll::Ready(Some(Ok(plakous_model::ModelEvent::Diagnostic(diagnostic))));
        }
        if let Some(mut operation) = self.operation.take() {
            if self.pending_protocol.is_none() {
                match Pin::new(&mut self.events).poll_next(cx) {
                    Poll::Ready(Some(Ok(event))) => {
                        self.operation = Some(operation);
                        return Poll::Ready(Some(Ok(event)));
                    }
                    Poll::Ready(Some(Err(error))) => {
                        self.pending_protocol = Some(error.message().to_owned());
                    }
                    Poll::Ready(None) | Poll::Pending => {}
                }
            }
            match operation.as_mut().poll(cx) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(error)) => {
                    self.pending_error = Some(anthropic_model_error(
                        &self.descriptor,
                        Some(&error),
                        &error.to_string(),
                    ));
                }
                Poll::Pending => {
                    self.operation = Some(operation);
                    return Poll::Pending;
                }
            }
        }
        // Polling the operation can enqueue observations and finish in the
        // same poll. Drain them in order before the one terminal item.
        if self.pending_protocol.is_none() {
            match Pin::new(&mut self.events).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
                Poll::Ready(Some(Err(error))) => {
                    self.pending_protocol = Some(error.message().to_owned());
                }
                Poll::Ready(None) => {}
                Poll::Pending if self.pending_error.is_none() => return Poll::Pending,
                Poll::Pending => {}
            }
        }
        self.done = true;
        // The operation error carries terminal facts, so it outranks a
        // protocol error the observation channel reported.
        let error = self.pending_error.take().or_else(|| {
            self.pending_protocol
                .take()
                .map(|message| anthropic_model_error(&self.descriptor, None, &message))
        });
        Poll::Ready(error.map(Err))
    }
}

#[allow(
    clippy::fn_params_excessive_bools,
    reason = "each fact is an independent terminal observation, not a mode flag"
)]
fn classify_messages_attempt_failure(
    acceptance: Option<ProviderAcceptance>,
    hosted_effects_possible: bool,
    refusal: Option<plakous_messages::MessagesProviderRefusal>,
    http_status: Option<ModelHttpStatus>,
    retry_after: Option<ModelRetryDelay>,
    exchange_break: plakous_messages::ExchangeBreak,
    message: &str,
) -> (
    plakous_model::ModelErrorKind,
    plakous_model::ModelRetryability,
) {
    use plakous_messages::MessagesProviderRefusal;
    use plakous_model::{ModelErrorKind, ModelRetryRisk, ModelRetryability};
    // A refusal the provider named exactly, on an exchange that started no
    // hosted effect, is a definite outcome. The provider said what it did
    // instead of the work, and the work left nothing behind: no response was
    // committed, the canonical history is untouched, and no tool ran. There is
    // nothing to reconcile and nothing to recover.
    //
    // Acceptance does not change that, and neither does visible output. Text
    // that streamed and was then abandoned is discarded with the turn; it
    // creates no state anything could fail to reconcile. Only a hosted effect
    // does, because that is the one thing that may have happened outside what
    // this client can see.
    if let Some(refusal) = refusal
        && !hosted_effects_possible
    {
        let retryability = match acceptance {
            Some(ProviderAcceptance::NotAccepted) => ModelRetryability::Eligible { retry_after },
            _ => ModelRetryability::Unsafe {
                risk: ModelRetryRisk::DuplicateInferenceOrBilling,
            },
        };
        return match refusal {
            MessagesProviderRefusal::RateLimit => (ModelErrorKind::RateLimited, retryability),
            MessagesProviderRefusal::Overloaded => (ModelErrorKind::Unavailable, retryability),
            MessagesProviderRefusal::Deadline => (ModelErrorKind::Timeout, retryability),
        };
    }
    if hosted_effects_possible {
        return (
            ModelErrorKind::OutcomeUnknown,
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect,
            },
        );
    }
    match acceptance {
        Some(ProviderAcceptance::NotAccepted)
            if http_status.is_some_and(|status| status.get() == 401) =>
        {
            (ModelErrorKind::Authentication, ModelRetryability::Permanent)
        }
        Some(ProviderAcceptance::NotAccepted)
            if http_status.is_some_and(|status| status.get() == 403) =>
        {
            (ModelErrorKind::Authorization, ModelRetryability::Permanent)
        }
        Some(ProviderAcceptance::NotAccepted)
            if http_status.is_some_and(|status| status.get() == 429) =>
        {
            (
                ModelErrorKind::RateLimited,
                ModelRetryability::Eligible { retry_after },
            )
        }
        _ if message.contains("Cancelled") || message.contains("cancelled") => {
            (ModelErrorKind::Cancelled, ModelRetryability::Permanent)
        }
        // A break the provider did not name, with no hosted effect possible,
        // is a definite failure: the partial response is discarded with the
        // turn and nothing replays it, because the inference may have run.
        Some(ProviderAcceptance::Accepted | ProviderAcceptance::PossiblyAccepted) => (
            break_kind(exchange_break),
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::DuplicateInferenceOrBilling,
            },
        ),
        _ => (break_kind(exchange_break), ModelRetryability::Permanent),
    }
}

const fn break_kind(
    exchange_break: plakous_messages::ExchangeBreak,
) -> plakous_model::ModelErrorKind {
    match exchange_break {
        plakous_messages::ExchangeBreak::Transport => plakous_model::ModelErrorKind::Transport,
        plakous_messages::ExchangeBreak::Protocol => plakous_model::ModelErrorKind::Protocol,
    }
}

fn anthropic_model_error(
    descriptor: &plakous_model::ResolvedModelDescriptor,
    error: Option<&MessagesAttemptError>,
    message: &str,
) -> plakous_model::ModelError {
    use plakous_model::{ModelError, ModelErrorContext, ModelErrorPhase};
    let acceptance = error.and_then(MessagesAttemptError::provider_acceptance);
    let hosted_effects_possible = error
        .and_then(MessagesAttemptError::facts)
        .is_some_and(|facts| facts.hosted_effects_possible());
    let refusal = error.and_then(MessagesAttemptError::provider_refusal);
    let http_status = error.and_then(MessagesAttemptError::http_status);
    let retry_after = error.and_then(MessagesAttemptError::retry_after);
    let exchange_break = error.map_or(
        plakous_messages::ExchangeBreak::Protocol,
        MessagesAttemptError::exchange_break,
    );
    // An input over the window is refused before any inference runs, so it is
    // definite; the runtime compacts and sends a new, smaller request.
    let (kind, retryability) = if error.is_some_and(MessagesAttemptError::compatibility_drift)
        && !hosted_effects_possible
    {
        (
            plakous_model::ModelErrorKind::InvalidConfiguration,
            plakous_model::ModelRetryability::Permanent,
        )
    } else if error.is_some_and(MessagesAttemptError::usage_limit) && !hosted_effects_possible {
        (
            plakous_model::ModelErrorKind::QuotaExceeded,
            plakous_model::ModelRetryability::Permanent,
        )
    } else if error.is_some_and(MessagesAttemptError::context_overflow) && !hosted_effects_possible
    {
        (
            plakous_model::ModelErrorKind::ContextLimit,
            plakous_model::ModelRetryability::Permanent,
        )
    } else {
        classify_messages_attempt_failure(
            acceptance,
            hosted_effects_possible,
            refusal,
            http_status,
            retry_after,
            exchange_break,
            message,
        )
    };
    let model_error = ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::Terminal,
        retryability,
    )
    .map(|context| match http_status {
        Some(status) => context.with_http_status(status),
        None => context,
    })
    .and_then(|context| ModelError::contextual(kind, message, context))
    .unwrap_or_else(|_| ModelError::new(kind, message));
    let model_error = model_error.with_attempt_summary(
        error
            .and_then(MessagesAttemptError::facts)
            .map_or_else(String::new, |facts| facts.summary().to_string()),
    );
    match error {
        Some(MessagesAttemptError::Protocol { source, .. }) => source.detail().map_or_else(
            || model_error.clone(),
            |detail| model_error.clone().with_source(detail),
        ),
        _ => model_error,
    }
}

/// Executes one exact local operation through private Messages admission.
pub async fn call_local_anthropic_operation(
    service: &mut AnthropicLocalHttpService,
    attempt: MessagesAttempt,
) -> Result<MessagesAttemptResponse, MessagesAttemptError> {
    call_messages_operation(service, attempt).await
}

#[derive(Clone)]
struct AnthropicHttpHooks {
    route: AnthropicPlatformRoute,
    prepared: PreparedMessagesRequest,
    renewal: Option<Arc<dyn ClaudeCredentialRenewal>>,
    redispatch_revalidation: Option<AnthropicRetryRevalidation>,
    dispatch_authorization: Option<AnthropicDispatchAuthorization>,
}

fn anthropic_redispatch(
    hooks: &AnthropicHttpHooks,
) -> Pin<
    Box<dyn Future<Output = Result<AnthropicHttpHooks, MessagesAttemptProtocolError>> + Send + '_>,
> {
    Box::pin(async move {
        let revalidate = hooks.redispatch_revalidation.as_ref().ok_or_else(|| {
            MessagesAttemptProtocolError::new("Anthropic transient retry authority is unavailable")
        })?;
        let grant = revalidate(hooks.route.clone())
            .await
            .map_err(|error| protocol_source(&error))?;
        let mut refreshed = hooks.clone();
        refreshed.route = grant.route;
        refreshed.dispatch_authorization = grant.dispatch_authorization;
        Ok(refreshed)
    })
}

/// Credential-rejection hook for the shared Messages attempt host: after an
/// HTTP 401 head, force one refresh so the single re-dispatch carries the
/// renewed grant. A rejected grant fails closed here and the provider's own
/// 401 is returned unchanged.
fn anthropic_credential_rejected(
    hooks: &AnthropicHttpHooks,
) -> Pin<
    Box<dyn Future<Output = Result<AnthropicHttpHooks, MessagesAttemptProtocolError>> + Send + '_>,
> {
    Box::pin(async move {
        let renewal = hooks.renewal.as_ref().ok_or_else(|| {
            protocol_source(&invalid_configuration(
                "Claude credential renewal was not installed",
            ))
        })?;
        let grant = renewal
            .renew_after_rejection(&hooks.route)
            .await
            .map_err(|error| protocol_source(&error))?;
        Ok(AnthropicHttpHooks {
            route: grant.route,
            prepared: hooks.prepared.clone(),
            renewal: hooks.renewal.clone(),
            redispatch_revalidation: hooks.redispatch_revalidation.clone(),
            dispatch_authorization: grant.dispatch_authorization,
        })
    })
}

struct AnthropicErrorMetadata {
    status: u16,
    request_id: Option<String>,
    retry_after_millis: Option<u64>,
    fable_included_limit: bool,
    subscription_limit: Option<usage_limit::SubscriptionLimit>,
}

impl MessagesHttpHooks<plakous_messages::MessagesWire> for AnthropicHttpHooks {
    type ErrorMetadata = AnthropicErrorMetadata;

    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        attempt: &MessagesAttempt,
        operation: &PreparedMessagesHttpOperation,
    ) -> Result<HttpRequest, MessagesAttemptProtocolError> {
        if operation.binding() != &neutral_binding(&self.prepared)
            || operation.body() != self.prepared.body()
        {
            return Err(protocol_source(&binding_error()));
        }
        let authorization = self
            .route
            .authorization(attempt, self.prepared.target_binding())
            .map_err(|error| protocol_source(&error))?;
        let oauth = self.route.is_claude_oauth();
        let credential = authorization
            .expose_for(
                attempt,
                self.prepared.target_binding(),
                &self.route,
                |secret| {
                    if oauth {
                        sensitive_header_value(format!("Bearer {secret}").into_bytes()).map_err(
                            |_| {
                                invalid_configuration(
                                    "Claude OAuth bearer token could not be represented as an HTTP header",
                                )
                            },
                        )
                    } else {
                        anthropic_api_key_header(secret)
                    }
                },
            )
            .map_err(|error| protocol_source(&error))?
            .map_err(|error| protocol_source(&error))?;
        let beta = if oauth {
            Some(self.prepared.dialect().beta_header().ok_or_else(|| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth beta header was missing",
                ))
            })?)
        } else {
            None
        };
        let oauth_identity = if oauth {
            let session_id = self.prepared.oauth_session_id().ok_or_else(|| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth request omitted its logical session identity",
                ))
            })?;
            Some((attempt.binding().attempt_id().to_string(), session_id))
        } else {
            None
        };
        let request = messages_request(
            client,
            dial_origin,
            credential,
            beta.as_deref(),
            oauth_identity
                .as_ref()
                .map(|(attempt_id, session_id)| (attempt_id.as_str(), *session_id)),
            operation.body(),
        )
        .map_err(|error| protocol_source(&error))?;
        if let Some(authorize) = &self.dispatch_authorization {
            if attempt.context().cancellation().is_cancelled() {
                return Err(self.cancellation_error());
            }
            if attempt
                .context()
                .deadline()
                .is_some_and(|deadline| SystemClock.now() >= deadline)
            {
                return Err(MessagesAttemptProtocolError::new(
                    "Anthropic dispatch deadline elapsed",
                )
                .with_refusal(plakous_messages::MessagesProviderRefusal::Deadline));
            }
            (authorize.authorize)().map_err(|error| protocol_source(&error))?;
        }
        Ok(request)
    }

    fn classify_response_head(
        &self,
        response: &plakous_http::Response,
    ) -> Result<MessagesHttpResponseHead<Self::ErrorMetadata>, MessagesAttemptProtocolError> {
        let status = response.status();
        let request_id = optional_ascii_header(response.headers(), &REQUEST_ID);
        if status != StatusCode::OK {
            // A 4xx other than 408 refuses the request; so do 503 and 529,
            // which say the service did not take it. Other 5xx may follow
            // work the provider started.
            let outcome = if (status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT)
                || matches!(status.as_u16(), 503 | 529)
            {
                PhysicalRequestOutcome::DefiniteRejection
            } else {
                PhysicalRequestOutcome::AmbiguousPostSend
            };
            return Ok(MessagesHttpResponseHead::Error(MessagesHttpErrorHead {
                metadata: AnthropicErrorMetadata {
                    status: status.as_u16(),
                    request_id,
                    retry_after_millis: retry_after_millis(response.headers()),
                    fable_included_limit: self.route.is_claude_oauth()
                        && self.prepared.model().as_str() == "claude-fable-5-1"
                        && status == StatusCode::TOO_MANY_REQUESTS
                        && is_fable_included_limit(response.headers()),
                    subscription_limit: (self.route.is_claude_oauth()
                        && status == StatusCode::TOO_MANY_REQUESTS)
                        .then(|| {
                            usage_limit::SubscriptionLimit::from_headers(
                                response.headers(),
                                std::time::SystemTime::now(),
                            )
                        })
                        .flatten(),
                },
                outcome,
                provider_accepted: false,
            }));
        }
        let request_id = response_header(response.headers(), &REQUEST_ID)
            .map_err(|error| protocol_source(&error))?
            .ok_or_else(|| {
                protocol_source(&invalid_response(
                    "Anthropic success response omitted request-id",
                ))
            })?;
        if !is_event_stream(response.headers()) {
            return Err(protocol_source(&invalid_response(
                "Anthropic success response was not text/event-stream",
            )));
        }
        let request_id = ModelProviderRequestId::new(request_id)
            .map_err(|_| protocol_source(&invalid_response("Anthropic request-id was unsafe")))?;
        Ok(MessagesHttpResponseHead::Stream(request_id))
    }

    fn classify_error_response(
        &self,
        metadata: Self::ErrorMetadata,
        body: &[u8],
        safety: MessagesHttpSafety,
    ) -> MessagesAttemptProtocolError {
        #[cfg(feature = "native-auth")]
        if self.route.is_claude_oauth()
            && let Some(sent) = self.prepared.dialect().beta_header()
            && let Some(reason) = compatibility::unknown_beta(metadata.status, body, &sent)
        {
            let error = MessagesAttemptProtocolError::new(reason.message("Claude"))
                .with_compatibility_drift();
            return ModelHttpStatus::new(metadata.status)
                .map_or(error.clone(), |status| error.with_http_status(status));
        }
        let error = classify_provider_error_with_safety(
            Some(metadata.status),
            body,
            metadata.request_id.as_deref(),
            AnthropicErrorPhase::Connect,
            RetrySafety::new(
                safety.provider_accepted,
                safety.visible_output,
                safety.hosted_effects_possible,
            ),
        )
        .with_retry_after_millis(metadata.retry_after_millis);
        // The included-limit claim attests exhausted quota, not a transient
        // rate limit: the typed kind carries it, never the notice text.
        if metadata.fable_included_limit && error.kind() == AnthropicErrorKind::RateLimited {
            protocol_source(&error.with_notice("You've reached your Fable limit."))
                .with_usage_limit()
        } else if let Some(limit) = metadata.subscription_limit
            && error.kind() == AnthropicErrorKind::RateLimited
        {
            protocol_source(&error.with_notice(limit.notice())).with_usage_limit()
        } else {
            protocol_source(&error)
        }
    }

    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        request_id: Option<&str>,
    ) -> MessagesAttemptProtocolError {
        protocol_source(&transport_error(failure, request_id)).with_detail(failure.to_string())
    }

    fn transport_failed(&self, failure: HttpFailure) {
        if let Some(authorization) = &self.dispatch_authorization {
            (authorization.transport_failed)(failure);
        }
    }

    fn cancellation_error(&self) -> MessagesAttemptProtocolError {
        protocol_source(&AnthropicError::new(
            AnthropicErrorKind::Cancelled,
            AnthropicErrorPhase::Connect,
            "Anthropic operation cancelled",
        ))
    }

    fn classify_stream_error(
        &self,
        error: plakous_messages::MessagesStreamError,
    ) -> MessagesAttemptProtocolError {
        annotate_messages_stream_terminal(
            protocol_source(&crate::stream::map_stream_error(error.clone())),
            &error,
        )
    }
}

fn annotate_messages_stream_terminal(
    error: MessagesAttemptProtocolError,
    stream: &plakous_messages::MessagesStreamError,
) -> MessagesAttemptProtocolError {
    if stream.context_overflow() {
        error.with_context_overflow()
    } else {
        error
    }
}

fn is_fable_included_limit(headers: &plakous_http::Headers) -> bool {
    let mut claims = headers
        .iter()
        .filter_map(|(name, value)| (name == RATE_LIMIT_CLAIM).then_some(value));
    claims
        .next()
        .is_some_and(|claim| claim.as_bytes() == b"seven_day_overage_included")
        && claims.next().is_none()
}

fn neutral_binding(prepared: &PreparedMessagesRequest) -> MessagesOperationBinding {
    MessagesOperationBinding::new(
        prepared.binding_digest(),
        prepared.plan_digest(),
        prepared.operation_digest(),
        prepared.replay_safety(),
    )
}

fn require_loopback_origin(origin: &Origin) -> Result<(), AnthropicError> {
    let authority = origin.binding_authority();
    let address = authority.parse::<std::net::SocketAddr>().map_err(|_| {
        invalid_configuration("Anthropic local transport requires an explicit loopback IP and port")
    })?;
    if !address.ip().is_loopback() {
        return Err(invalid_configuration(
            "Anthropic local transport requires an explicit loopback IP and port",
        ));
    }
    Ok(())
}

pub(crate) fn anthropic_api_key_header(secret: &str) -> Result<HeaderValue, AnthropicError> {
    sensitive_header_value(secret.as_bytes().to_vec()).map_err(|_| {
        invalid_configuration("Anthropic API key could not be represented as an HTTP header")
    })
}

fn response_header(
    headers: &plakous_http::Headers,
    name: &HeaderName,
) -> Result<Option<String>, AnthropicError> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| invalid_response("Anthropic response metadata was not ASCII"))?;
    if !crate::error::valid_provider_metadata(value, crate::MAX_PROVIDER_REQUEST_ID_BYTES) {
        return Err(invalid_response(
            "Anthropic provider request ID was invalid",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn optional_ascii_header(headers: &plakous_http::Headers, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Parses `Retry-After` with the shared kernel parser (whole seconds or
/// HTTP-date). Advice beyond the representable 24 h saturates there, so the
/// retry kernel's 60 s clamp still applies instead of dropping the advice.
fn retry_after_millis(headers: &plakous_http::Headers) -> Option<u64> {
    retry_after_advice_millis(headers.get(&RETRY_AFTER)?.to_str().ok()?)
}

fn retry_after_advice_millis(value: &str) -> Option<u64> {
    let advice = plakous_attempt::retry_after_seconds(value)?;
    Some(
        u64::try_from(advice.as_millis()).map_or(ModelRetryDelay::MAX_MILLIS, |millis| {
            millis.min(ModelRetryDelay::MAX_MILLIS)
        }),
    )
}

fn is_event_stream(headers: &plakous_http::Headers) -> bool {
    headers
        .get(&CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn http_phase_name(phase: HttpPhase) -> &'static str {
    match phase {
        HttpPhase::Resolution => "resolution",
        HttpPhase::Connect => "connect",
        HttpPhase::Tls => "tls",
        HttpPhase::RequestWrite => "request_write",
        HttpPhase::RequestWriteOrResponseHead => "request_write_or_response_head",
        HttpPhase::ResponseHead => "response_head",
        HttpPhase::ResponseBody => "response_body",
        HttpPhase::Cleanup => "cleanup",
        _ => "unknown",
    }
}

/// Binds a credential-free probe to the executor's exact origin and route.
#[must_use]
pub(crate) fn bind_reachability_probe(
    client: Client,
    origin: Origin,
    route: plakous_model::RouteId,
) -> plakous_model::ReachabilityProbe {
    plakous_model::ReachabilityProbe::new(route, move |cancellation, deadline| {
        let client = client.clone();
        let origin = origin.clone();
        Box::pin(async move {
            let http_cancellation = plakous_http::CancellationToken::new();
            let probe = client.probe(
                &origin,
                plakous_http::ExchangeControl::with_cancellation(
                    deadline,
                    http_cancellation.clone(),
                ),
            );
            tokio::pin!(probe);
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => { http_cancellation.cancel(); probe.await }
                result = &mut probe => result,
            };
            result.map_err(|failure| {
                network_failure(failure).unwrap_or(plakous_model::ModelNetworkFailure {
                    phase: plakous_model::ModelRetryPhase::Cleanup,
                    sent: plakous_model::ModelNetworkTransmission::NotSent,
                    timed_out: false,
                })
            })
        })
    })
}

fn transport_failure_message(kind: HttpErrorKind, phase: HttpPhase) -> String {
    let sentence = match kind {
        HttpErrorKind::Cancelled => "Anthropic HTTP exchange was cancelled",
        HttpErrorKind::DeadlineExceeded => "Anthropic HTTP exchange exceeded its deadline",
        HttpErrorKind::LimitExceeded(_) => "Anthropic HTTP exchange exceeded a resource bound",
        HttpErrorKind::InvalidResponse => "Anthropic HTTP response was invalid",
        _ => "Anthropic HTTP transport failed",
    };
    format!("{sentence} during {}", http_phase_name(phase))
}

fn transport_error(failure: HttpFailure, request_id: Option<&str>) -> AnthropicError {
    let message = match failure.kind() {
        HttpErrorKind::Cancelled
        | HttpErrorKind::LimitExceeded(_)
        | HttpErrorKind::InvalidResponse => {
            transport_failure_message(failure.kind(), failure.phase())
        }
        _ => network_failure(failure).map_or_else(
            || transport_failure_message(failure.kind(), failure.phase()),
            |cause| cause.to_string(),
        ),
    };
    let kind = match failure.kind() {
        HttpErrorKind::Cancelled => AnthropicErrorKind::Cancelled,
        HttpErrorKind::DeadlineExceeded => AnthropicErrorKind::Timeout,
        HttpErrorKind::LimitExceeded(_) => AnthropicErrorKind::ResourceLimit,
        HttpErrorKind::InvalidResponse => AnthropicErrorKind::InvalidResponse,
        _ => AnthropicErrorKind::Transport,
    };
    let phase = match failure.phase() {
        HttpPhase::ResponseBody | HttpPhase::Cleanup => AnthropicErrorPhase::Stream,
        _ => AnthropicErrorPhase::Connect,
    };
    let retry = if failure.transmission().peer_cannot_have_acted() {
        RetryAdvice::BeforeAcceptance
    } else {
        RetryAdvice::Never
    };
    AnthropicError::new(kind, phase, message).with_provider_metadata(
        None,
        None,
        request_id.map(str::to_owned),
        retry,
    )
}

fn insert_oauth_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), AnthropicError> {
    let header_value = HeaderValue::from_str(value)
        .map_err(|_| invalid_configuration("Claude OAuth attribution header was invalid"))?;
    headers.insert(HeaderName::from_static(name), header_value);
    Ok(())
}

/// The exact Messages request of one physical exchange, turn or side
/// request: `beta` and `oauth` (request id, logical session) are present
/// exactly on the Claude OAuth route, which wears the reference client's
/// headers; the API-key route sends `x-api-key`.
pub(crate) fn messages_request(
    client: &Client,
    dial_origin: Origin,
    credential: HeaderValue,
    beta: Option<&str>,
    oauth: Option<(&str, plakous_model::SessionId)>,
    body: &[u8],
) -> Result<HttpRequest, AnthropicError> {
    let mut headers = HeaderMap::new();
    let path = if let (Some(beta), Some((request_id, session_id))) = (beta, oauth) {
        headers.insert(AUTHORIZATION, credential);
        headers.insert(
            ANTHROPIC_BETA,
            HeaderValue::from_str(beta)
                .map_err(|_| invalid_configuration("Claude OAuth beta header was invalid"))?,
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(crate::CLAUDE_OAUTH_MESSAGES_USER_AGENT),
        );
        headers.insert(
            HeaderName::from_static("anthropic-dangerous-direct-browser-access"),
            HeaderValue::from_static("true"),
        );
        headers.insert(
            HeaderName::from_static("x-app"),
            HeaderValue::from_static("cli"),
        );
        insert_oauth_header(
            &mut headers,
            "x-stainless-arch",
            crate::claude_oauth_stainless_arch(),
        )?;
        headers.insert(
            HeaderName::from_static("x-stainless-lang"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_LANG),
        );
        insert_oauth_header(
            &mut headers,
            "x-stainless-os",
            crate::claude_oauth_stainless_os(),
        )?;
        headers.insert(
            HeaderName::from_static("x-stainless-package-version"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_PACKAGE_VERSION),
        );
        headers.insert(
            HeaderName::from_static("x-stainless-retry-count"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_RETRY_COUNT),
        );
        headers.insert(
            HeaderName::from_static("x-stainless-runtime"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_RUNTIME),
        );
        headers.insert(
            HeaderName::from_static("x-stainless-runtime-version"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_RUNTIME_VERSION),
        );
        headers.insert(
            HeaderName::from_static("x-stainless-timeout"),
            HeaderValue::from_static(crate::CLAUDE_OAUTH_STAINLESS_TIMEOUT),
        );
        insert_oauth_header(&mut headers, "x-client-request-id", request_id)?;
        insert_oauth_header(
            &mut headers,
            "x-claude-code-session-id",
            &session_id.to_string(),
        )?;
        crate::oauth::CLAUDE_OAUTH_MESSAGES_PATH
    } else {
        headers.insert(X_API_KEY, credential);
        ANTHROPIC_MESSAGES_PATH
    };
    let oauth = beta.is_some();
    headers.insert(
        ANTHROPIC_VERSION,
        HeaderValue::from_static(ANTHROPIC_API_VERSION),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(if oauth {
            "application/json"
        } else {
            "text/event-stream"
        }),
    );
    HttpRequest::new(
        dial_origin,
        Method::POST,
        path,
        headers,
        SensitiveBytes::new(body.to_vec()).into_bytes(),
        client.limits(),
    )
    .map_err(|error| invalid_configuration(format!("Anthropic HTTP request was invalid: {error}")))
}

fn protocol_source(error: &AnthropicError) -> MessagesAttemptProtocolError {
    let source = MessagesAttemptProtocolError::new(format!(
        "anthropic/{:?} status={:?} request_id={:?}: {}",
        error.kind(),
        error.status(),
        error.request_id(),
        error.message()
    ));
    // Carry the provider's own name for the refusal forward. The formatted
    // message above is diagnostic text no classifier should have to parse.
    let source = match error.kind() {
        AnthropicErrorKind::RateLimited => {
            source.with_refusal(plakous_messages::MessagesProviderRefusal::RateLimit)
        }
        AnthropicErrorKind::Overloaded => {
            source.with_refusal(plakous_messages::MessagesProviderRefusal::Overloaded)
        }
        AnthropicErrorKind::Timeout => {
            source.with_refusal(plakous_messages::MessagesProviderRefusal::Deadline)
        }
        AnthropicErrorKind::Limit => source.with_context_overflow(),
        _ => source,
    };
    let source = match error
        .status()
        .and_then(|status| ModelHttpStatus::new(status).ok())
    {
        Some(status) => source.with_http_status(status),
        None => source,
    };
    match error
        .retry_after_millis()
        .and_then(|millis| ModelRetryDelay::from_millis(millis).ok())
    {
        Some(retry_after) => source.with_retry_after(retry_after),
        None => source,
    }
}

fn invalid_configuration(message: impl Into<String>) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        message,
    )
}

fn invalid_response(message: impl Into<String>) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidResponse,
        AnthropicErrorPhase::Connect,
        message,
    )
}

fn binding_error() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::BindingMismatch,
        AnthropicErrorPhase::Prepare,
        "Anthropic HTTP authority did not match the neutral Messages operation",
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests fail immediately on invalid typed fixtures"
)]
mod tests;
