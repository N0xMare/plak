//! Anthropic HTTP dialect hooks for the neutral Messages exchange host.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
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
    AnthropicErrorPhase, AnthropicPlatformRoute, MAX_PROVIDER_ERROR_BODY_BYTES,
    PreparedMessagesRequest, RetryAdvice, RetrySafety, classify_provider_error_with_safety,
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
pub const STANDARD_ANTHROPIC_EVENT_CAPACITY: NonZeroUsize = match NonZeroUsize::new(32) {
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
    #[allow(
        clippy::too_many_arguments,
        reason = "all executable fixture authority is explicit"
    )]
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
    let (kind, retryability) = if error.is_some_and(MessagesAttemptError::usage_limit)
        && !hosted_effects_possible
    {
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
}

impl MessagesHttpHooks for AnthropicHttpHooks {
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
        let mut headers = HeaderMap::new();
        let path = if oauth {
            headers.insert(AUTHORIZATION, credential);
            let beta = self.prepared.dialect().beta_header().ok_or_else(|| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth beta header was missing",
                ))
            })?;
            headers.insert(
                ANTHROPIC_BETA,
                HeaderValue::from_str(&beta).map_err(|_| {
                    protocol_source(&invalid_configuration(
                        "Claude OAuth beta header was invalid",
                    ))
                })?,
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
            let attempt_id = attempt.binding().attempt_id().to_string();
            let attempt_id = HeaderValue::from_str(&attempt_id).map_err(|_| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth attempt identity was invalid",
                ))
            })?;
            headers.insert(HeaderName::from_static("x-client-request-id"), attempt_id);
            let session_id = self.prepared.oauth_session_id().ok_or_else(|| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth request omitted its logical session identity",
                ))
            })?;
            let session_id = HeaderValue::from_str(&session_id.to_string()).map_err(|_| {
                protocol_source(&invalid_configuration(
                    "Claude OAuth logical session identity was invalid",
                ))
            })?;
            headers.insert(
                HeaderName::from_static("x-claude-code-session-id"),
                session_id,
            );
            crate::CLAUDE_OAUTH_MESSAGES_PATH
        } else {
            headers.insert(X_API_KEY, credential);
            ANTHROPIC_MESSAGES_PATH
        };
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
        let request = HttpRequest::new(
            dial_origin,
            Method::POST,
            path,
            headers,
            SensitiveBytes::new(operation.body().to_vec()).into_bytes(),
            client.limits(),
        )
        .map_err(|error| {
            protocol_source(&invalid_configuration(format!(
                "Anthropic HTTP request was invalid: {error}"
            )))
        })?;
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
        let error =
            if metadata.fable_included_limit && error.kind() == AnthropicErrorKind::RateLimited {
                error.with_notice("You've reached your Fable limit.")
            } else {
                error
            };
        protocol_source(&error)
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
        protocol_source(&crate::stream::map_stream_error(error))
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

fn anthropic_api_key_header(secret: &str) -> Result<HeaderValue, AnthropicError> {
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

fn retry_after_millis(headers: &plakous_http::Headers) -> Option<u64> {
    headers
        .get(&RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000)
        .filter(|value| *value <= 86_400_000)
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

pub(crate) fn network_failure(
    failure: plakous_http::Failure,
) -> Option<plakous_model::ModelNetworkFailure> {
    use plakous_http::{ErrorKind, Phase, TransmissionFact};
    use plakous_model::{ModelNetworkFailure, ModelNetworkTransmission, ModelRetryPhase};
    let phase = match failure.phase() {
        Phase::Resolution => ModelRetryPhase::Resolution,
        Phase::Connect => ModelRetryPhase::Connect,
        Phase::Tls => ModelRetryPhase::Tls,
        Phase::RequestWrite => ModelRetryPhase::RequestWrite,
        Phase::RequestWriteOrResponseHead => ModelRetryPhase::RequestWriteOrResponseHead,
        Phase::ResponseHead => ModelRetryPhase::ResponseHead,
        Phase::ResponseBody => ModelRetryPhase::ResponseBody,
        Phase::Cleanup => ModelRetryPhase::Cleanup,
        _ => return None,
    };
    Some(ModelNetworkFailure {
        phase,
        sent: match failure.transmission() {
            TransmissionFact::NotStarted => ModelNetworkTransmission::NotSent,
            TransmissionFact::RequestIncomplete => ModelNetworkTransmission::Incomplete,
            TransmissionFact::MayHaveTransmitted => ModelNetworkTransmission::MayHaveSent,
        },
        timed_out: failure.kind() == ErrorKind::DeadlineExceeded,
    })
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
) -> Result<(), MessagesAttemptProtocolError> {
    let header_name = HeaderName::from_static(name);
    let header_value = HeaderValue::from_str(value).map_err(|_| {
        protocol_source(&invalid_configuration(
            "Claude OAuth attribution header was invalid",
        ))
    })?;
    headers.insert(header_name, header_value);
    Ok(())
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
mod tests {
    use super::network_failure;
    use std::collections::BTreeMap;

    use plakous_messages::{
        MessagesAttemptError, MessagesAttemptProtocolError, ProviderAcceptance, TerminalFacts,
    };
    use plakous_model::{
        CapabilitySet, ModelAdapterProfileId, ModelErrorKind, ModelFamilyId, ModelHttpStatus,
        ModelId, ModelRetryDelay, ModelRetryability, ProtocolId, ResolvedModelDescriptor, RouteId,
    };

    use plakous_http::{
        ErrorKind as HttpErrorKind, Failure as HttpFailure, Phase as HttpPhase, TransmissionFact,
    };

    use super::{anthropic_model_error, classify_messages_attempt_failure, transport_error};

    #[test]
    fn user_cancel_after_accept_is_cancelled_not_outcome_unknown() {
        for (acceptance, message) in [
            (
                Some(ProviderAcceptance::Accepted),
                "Messages attempt cancelled",
            ),
            (
                Some(ProviderAcceptance::Accepted),
                "anthropic/Cancelled status=None request_id=None: cancelled",
            ),
            (
                Some(ProviderAcceptance::PossiblyAccepted),
                "Messages attempt cancelled",
            ),
            (
                Some(ProviderAcceptance::NotAccepted),
                "Messages attempt cancelled",
            ),
            (None, "Messages attempt cancelled"),
        ] {
            let (kind, retryability) = classify_messages_attempt_failure(
                acceptance,
                false,
                None,
                None,
                None,
                plakous_messages::ExchangeBreak::Protocol,
                message,
            );
            assert_eq!(kind, ModelErrorKind::Cancelled, "{acceptance:?} {message}");
            assert_eq!(retryability, ModelRetryability::Permanent);
        }
    }

    /// A break nobody named, after acceptance and with no hosted effect
    /// possible, is a definite failure of its own kind. The inference may have
    /// run, so nothing may replay it, but nothing is left to reconcile either.
    #[test]
    fn an_accepted_break_without_a_hosted_effect_is_definite() {
        use plakous_messages::ExchangeBreak;
        for acceptance in [
            ProviderAcceptance::Accepted,
            ProviderAcceptance::PossiblyAccepted,
        ] {
            for (exchange_break, expected) in [
                (ExchangeBreak::Transport, ModelErrorKind::Transport),
                (ExchangeBreak::Protocol, ModelErrorKind::Protocol),
            ] {
                let (kind, retryability) = classify_messages_attempt_failure(
                    Some(acceptance),
                    false,
                    None,
                    None,
                    None,
                    exchange_break,
                    "Messages stream omitted terminal response",
                );
                assert_eq!(kind, expected, "{acceptance:?} {exchange_break:?}");
                assert_eq!(
                    retryability,
                    ModelRetryability::Unsafe {
                        risk: plakous_model::ModelRetryRisk::DuplicateInferenceOrBilling,
                    }
                );
            }
        }
    }

    #[test]
    fn typed_auth_rejection_requires_definite_non_acceptance() {
        for (status, expected) in [
            (401, ModelErrorKind::Authentication),
            (403, ModelErrorKind::Authorization),
        ] {
            for acceptance in [
                ProviderAcceptance::NotAccepted,
                ProviderAcceptance::PossiblyAccepted,
                ProviderAcceptance::Accepted,
            ] {
                let (kind, retryability) = classify_messages_attempt_failure(
                    Some(acceptance),
                    false,
                    None,
                    Some(ModelHttpStatus::new(status).expect("status")),
                    None,
                    plakous_messages::ExchangeBreak::Protocol,
                    "opaque provider rejection",
                );
                if acceptance == ProviderAcceptance::NotAccepted {
                    assert_eq!(kind, expected);
                    assert_eq!(retryability, ModelRetryability::Permanent);
                } else {
                    assert_eq!(kind, ModelErrorKind::Protocol);
                    assert!(matches!(retryability, ModelRetryability::Unsafe { .. }));
                }
            }
        }
    }

    #[test]
    fn possible_hosted_effect_requires_explicit_recovery() {
        let (kind, retryability) = classify_messages_attempt_failure(
            Some(ProviderAcceptance::Accepted),
            true,
            None,
            None,
            None,
            plakous_messages::ExchangeBreak::Protocol,
            "Anthropic HTTP transport failed",
        );
        assert_eq!(kind, ModelErrorKind::OutcomeUnknown);
        assert_eq!(
            retryability,
            ModelRetryability::Unsafe {
                risk: plakous_model::ModelRetryRisk::ProviderHostedEffect,
            }
        );
    }

    #[test]
    fn transport_failure_message_names_the_http_phase() {
        let phases = [
            (HttpPhase::Resolution, "resolution"),
            (HttpPhase::Connect, "connect"),
            (HttpPhase::Tls, "tls"),
            (HttpPhase::RequestWrite, "request_write"),
            (
                HttpPhase::RequestWriteOrResponseHead,
                "request_write_or_response_head",
            ),
            (HttpPhase::ResponseHead, "response_head"),
            (HttpPhase::ResponseBody, "response_body"),
            (HttpPhase::Cleanup, "cleanup"),
        ];
        for (phase, name) in phases {
            let deadline = transport_error(
                HttpFailure::for_classifier_test(
                    phase,
                    TransmissionFact::MayHaveTransmitted,
                    HttpErrorKind::DeadlineExceeded,
                ),
                None,
            );
            let message = deadline.message();
            assert_eq!(
                message,
                network_failure(HttpFailure::for_classifier_test(
                    phase,
                    TransmissionFact::MayHaveTransmitted,
                    HttpErrorKind::DeadlineExceeded
                ))
                .expect("known phase")
                .to_string(),
                "{name}"
            );

            let transport = transport_error(
                HttpFailure::for_classifier_test(
                    phase,
                    TransmissionFact::NotStarted,
                    HttpErrorKind::Io,
                ),
                None,
            );
            let message = transport.message();
            assert_eq!(
                message,
                network_failure(HttpFailure::for_classifier_test(
                    phase,
                    TransmissionFact::NotStarted,
                    HttpErrorKind::Io
                ))
                .expect("known phase")
                .to_string(),
                "{name}"
            );
            assert!(
                !message.contains("deadline"),
                "a transport failure must not claim a deadline: {message}"
            );
        }
        let cancelled = transport_error(
            HttpFailure::for_classifier_test(
                HttpPhase::Connect,
                TransmissionFact::NotStarted,
                HttpErrorKind::Cancelled,
            ),
            None,
        );
        assert!(cancelled.message().contains("was cancelled during connect"));
        let invalid = transport_error(
            HttpFailure::for_classifier_test(
                HttpPhase::ResponseHead,
                TransmissionFact::MayHaveTransmitted,
                HttpErrorKind::InvalidResponse,
            ),
            None,
        );
        assert!(
            invalid
                .message()
                .contains("response was invalid during response_head")
        );
        let limited = transport_error(
            HttpFailure::for_classifier_test(
                HttpPhase::RequestWrite,
                TransmissionFact::RequestIncomplete,
                HttpErrorKind::LimitExceeded(plakous_http::LimitKind::RequestBodyBytes),
            ),
            None,
        );
        assert!(
            limited
                .message()
                .contains("exceeded a resource bound during request_write")
        );
        assert!(!limited.message().contains("deadline"));
    }

    /// A mid-stream `rate_limit_error` on an accepted stream escalated to
    /// `recovery_required` on both surfaces and blocked continuation. The
    /// stream was accepted, so the old classification stopped at acceptance;
    /// the provider had in fact named exactly why it produced nothing.
    #[test]
    fn a_named_refusal_that_produced_nothing_is_definite_however_it_was_accepted() {
        use plakous_messages::MessagesProviderRefusal;
        let retry_after = ModelRetryDelay::from_millis(1_000).expect("retry delay");
        for acceptance in [
            ProviderAcceptance::NotAccepted,
            ProviderAcceptance::PossiblyAccepted,
            ProviderAcceptance::Accepted,
        ] {
            for (refusal, expected) in [
                (
                    MessagesProviderRefusal::RateLimit,
                    ModelErrorKind::RateLimited,
                ),
                (
                    MessagesProviderRefusal::Overloaded,
                    ModelErrorKind::Unavailable,
                ),
                (MessagesProviderRefusal::Deadline, ModelErrorKind::Timeout),
            ] {
                let (kind, retryability) = classify_messages_attempt_failure(
                    Some(acceptance),
                    false,
                    Some(refusal),
                    None,
                    Some(retry_after),
                    plakous_messages::ExchangeBreak::Protocol,
                    "Anthropic provider error",
                );
                assert_eq!(
                    kind, expected,
                    "{refusal:?} under {acceptance:?} names its own outcome"
                );
                assert_ne!(
                    kind,
                    ModelErrorKind::OutcomeUnknown,
                    "a refusal that produced nothing leaves nothing to recover"
                );
                assert_eq!(
                    retryability,
                    if acceptance == ProviderAcceptance::NotAccepted {
                        ModelRetryability::Eligible {
                            retry_after: Some(retry_after),
                        }
                    } else {
                        ModelRetryability::Unsafe {
                            risk: plakous_model::ModelRetryRisk::DuplicateInferenceOrBilling,
                        }
                    }
                );
            }
        }
    }

    /// One column of the provider-error matrix.
    ///
    /// The rows are routes and the columns are the ways a provider can fail;
    /// this is the column, shared by every row so a new one is one line in the
    /// table below. Each names the condition and the terminal facts the adapter
    /// reports for it. Visible output is deliberately not among them: no
    /// classifier reads it any more, so a column carrying it would suggest a
    /// lever that does not exist.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ProviderFailureColumn {
        pub(crate) name: &'static str,
        pub(crate) acceptance: ProviderAcceptance,
        pub(crate) hosted_effects: bool,
        pub(crate) refusal: Option<plakous_messages::MessagesProviderRefusal>,
        pub(crate) http_status: Option<u16>,
        /// `None` means the column must be definite without pinning which kind.
        pub(crate) expect: Option<ModelErrorKind>,
    }

    /// Every way a provider fails that this matrix covers.
    ///
    /// Adding a column is one line. Each row of the matrix runs all of them and
    /// asserts the same property: a provider failure is a definite outcome
    /// unless a hosted effect may have run.
    pub(crate) fn provider_failure_columns() -> Vec<ProviderFailureColumn> {
        use plakous_messages::MessagesProviderRefusal as Refusal;
        let column = |name, acceptance, refusal, http_status, expect| ProviderFailureColumn {
            name,
            acceptance,
            hosted_effects: false,
            refusal,
            http_status,
            expect,
        };
        vec![
            column(
                "pre-stream 5xx",
                ProviderAcceptance::NotAccepted,
                Some(Refusal::Overloaded),
                Some(503),
                Some(ModelErrorKind::Unavailable),
            ),
            column(
                "pre-stream 429",
                ProviderAcceptance::NotAccepted,
                Some(Refusal::RateLimit),
                Some(429),
                Some(ModelErrorKind::RateLimited),
            ),
            column(
                "in-band error after partial stream",
                ProviderAcceptance::Accepted,
                Some(Refusal::Overloaded),
                None,
                Some(ModelErrorKind::Unavailable),
            ),
            column(
                "in-band rate limit after partial stream",
                ProviderAcceptance::Accepted,
                Some(Refusal::RateLimit),
                None,
                Some(ModelErrorKind::RateLimited),
            ),
            column(
                "transport timeout",
                ProviderAcceptance::NotAccepted,
                Some(Refusal::Deadline),
                None,
                Some(ModelErrorKind::Timeout),
            ),
            column(
                "operation deadline on an accepted stream",
                ProviderAcceptance::Accepted,
                Some(Refusal::Deadline),
                None,
                Some(ModelErrorKind::Timeout),
            ),
            column(
                "overloaded refusal on an accepted stream",
                ProviderAcceptance::PossiblyAccepted,
                Some(Refusal::Overloaded),
                Some(529),
                Some(ModelErrorKind::Unavailable),
            ),
        ]
    }

    /// The Anthropic row of the provider-error matrix.
    ///
    /// Anthropic API and Claude OAuth are one row: they differ in credential
    /// and route identity, and share `classify_messages_attempt_failure`
    /// exactly. Nothing in the classification reads the credential.
    #[test]
    fn the_anthropic_row_of_the_provider_failure_matrix_is_definite() {
        for column in provider_failure_columns() {
            let (kind, retryability) = classify_messages_attempt_failure(
                Some(column.acceptance),
                column.hosted_effects,
                column.refusal,
                column
                    .http_status
                    .and_then(|status| ModelHttpStatus::new(status).ok()),
                None,
                plakous_messages::ExchangeBreak::Protocol,
                "Anthropic provider error",
            );
            assert_ne!(
                kind,
                ModelErrorKind::OutcomeUnknown,
                "{}: a named provider failure with no hosted effect fences the session",
                column.name
            );
            assert_eq!(
                matches!(retryability, ModelRetryability::Unsafe { .. }),
                column.acceptance != ProviderAcceptance::NotAccepted,
                "{}: accepted inference cannot be replayed even when its terminal outcome is definite",
                column.name
            );
            if let Some(expected) = column.expect {
                assert_eq!(kind, expected, "{}", column.name);
            }
        }
    }

    /// The same columns with a hosted effect in flight stay undetermined, on
    /// every row. That is the one thing that may have happened outside what
    /// this client can see.
    #[test]
    fn every_column_with_a_hosted_effect_still_requires_recovery() {
        for column in provider_failure_columns() {
            let (kind, _) = classify_messages_attempt_failure(
                Some(column.acceptance),
                true,
                column.refusal,
                column
                    .http_status
                    .and_then(|status| ModelHttpStatus::new(status).ok()),
                None,
                plakous_messages::ExchangeBreak::Protocol,
                "Anthropic provider error",
            );
            assert_eq!(kind, ModelErrorKind::OutcomeUnknown, "{}", column.name);
        }
    }

    /// Only a hosted effect keeps a named refusal undetermined.
    ///
    /// Visible output used to keep it too, and that was wrong in a way the
    /// field proved: the live `OpenRouter` `520` arrived after a chunk had
    /// already streamed, so a guard on visible output would not have fired on
    /// the very case it was written for. Streamed text that is abandoned is
    /// discarded with the turn and leaves no state anything could fail to
    /// reconcile. A hosted effect is the one thing that would, and it is now
    /// the only fact this classifier reads for the question — visible output is
    /// no longer a parameter at all.
    #[test]
    fn only_a_hosted_effect_keeps_a_named_refusal_undetermined() {
        use plakous_messages::MessagesProviderRefusal;

        let (kind, _) = classify_messages_attempt_failure(
            Some(ProviderAcceptance::Accepted),
            false,
            Some(MessagesProviderRefusal::RateLimit),
            None,
            None,
            plakous_messages::ExchangeBreak::Protocol,
            "Anthropic provider error",
        );
        assert_eq!(
            kind,
            ModelErrorKind::RateLimited,
            "abandoned output does not make a named refusal unknown"
        );

        let (kind, _) = classify_messages_attempt_failure(
            Some(ProviderAcceptance::Accepted),
            true,
            Some(MessagesProviderRefusal::RateLimit),
            None,
            None,
            plakous_messages::ExchangeBreak::Protocol,
            "Anthropic provider error",
        );
        assert_eq!(
            kind,
            ModelErrorKind::OutcomeUnknown,
            "a hosted effect may have run outside what this client can see"
        );
    }

    #[test]
    fn typed_429_retains_retry_after_without_message_parsing() {
        let retry_after = ModelRetryDelay::from_millis(2_000).expect("retry delay");
        let (kind, retryability) = classify_messages_attempt_failure(
            Some(ProviderAcceptance::NotAccepted),
            false,
            None,
            Some(ModelHttpStatus::new(429).expect("status")),
            Some(retry_after),
            plakous_messages::ExchangeBreak::Protocol,
            "opaque provider rejection",
        );
        assert_eq!(kind, ModelErrorKind::RateLimited);
        assert_eq!(
            retryability,
            ModelRetryability::Eligible {
                retry_after: Some(retry_after),
            }
        );
    }

    #[test]
    fn typed_429_and_retry_after_reach_model_error_context() {
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("anthropic.messages").expect("protocol"),
            RouteId::new("anthropic.claude_oauth").expect("route"),
            ModelId::new("claude-sonnet-5").expect("model"),
            None,
            Some(ModelFamilyId::new("claude-5-sonnet").expect("family")),
            ModelAdapterProfileId::new("anthropic.claude.oauth.test").expect("profile"),
            CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
        );
        let status = ModelHttpStatus::new(429).expect("status");
        let retry_after = ModelRetryDelay::from_millis(2_000).expect("retry delay");
        let attempt = MessagesAttemptError::protocol(
            MessagesAttemptProtocolError::new("opaque rejection")
                .with_http_status(status)
                .with_retry_after(retry_after),
            TerminalFacts::preacceptance_test(),
        );
        let error = anthropic_model_error(&descriptor, Some(&attempt), "opaque rejection");
        assert_eq!(error.kind(), ModelErrorKind::RateLimited);
        let context = error.context().expect("model error context");
        assert_eq!(context.http_status(), Some(status));
        assert_eq!(
            context.retryability(),
            ModelRetryability::Eligible {
                retry_after: Some(retry_after),
            }
        );
    }
    #[tokio::test]
    async fn replay_strip_diagnostic_precedes_physical_operation_poll()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::*;
        use futures_util::{FutureExt, StreamExt};
        use plakous_model::{
            AttemptId, CancellationToken, Digest, DigestDomain, ModelCallContext, ModelDiagnostic,
            ModelEvent,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let binding = MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-diagnostic"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-diagnostic"])?,
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"operation-diagnostic"],
            )?,
            plakous_messages::ReplaySafety::CostOnly,
        );
        let context = ModelCallContext::new(
            AttemptId::try_from(uuid::Uuid::from_u128(45))?,
            CancellationToken::new(),
            None,
        );
        let (_attempt, events) = MessagesAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(1).ok_or("capacity")?,
        );
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("anthropic.messages")?,
            RouteId::new("anthropic.claude_oauth")?,
            ModelId::new("claude-sonnet-5")?,
            None,
            None,
            ModelAdapterProfileId::new("anthropic.claude.oauth.test")?,
            CapabilitySet::default(),
        );
        let polled = Arc::new(AtomicBool::new(false));
        let operation_polled = Arc::clone(&polled);
        let operation: AnthropicMessagesOperation = Box::pin(std::future::poll_fn(move |_| {
            operation_polled.store(true, Ordering::SeqCst);
            Poll::Pending
        }));
        let transition = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"transition"])?;
        let mut stream = AnthropicMessagesModelStream {
            pre_dispatch_diagnostics: VecDeque::from([
                ModelDiagnostic::new("incompatible Messages thinking replay was stripped"),
                ModelDiagnostic::historical_projection(transition, "configuration transition"),
            ]),
            events,
            operation: Some(operation),
            descriptor,
            pending_protocol: None,
            pending_error: None,
            done: false,
        };
        let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
            return Err("missing replay-strip diagnostic".into());
        };
        assert_eq!(
            diagnostic.message().as_str(),
            "incompatible Messages thinking replay was stripped"
        );
        assert_eq!(diagnostic.history_projection_identity(), None);
        assert!(!polled.load(Ordering::SeqCst));
        let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
            return Err("missing historical projection diagnostic".into());
        };
        assert_eq!(diagnostic.history_projection_identity(), Some(transition));
        assert!(!polled.load(Ordering::SeqCst));
        assert!(stream.next().now_or_never().is_none());
        assert!(polled.load(Ordering::SeqCst));
        Ok(())
    }

    /// The operation can enqueue observations and fail in the same poll. The
    /// stream delivers every queued observation in order, then the operation
    /// error exactly once, then ends.
    #[tokio::test]
    async fn queued_events_drain_in_order_before_the_operation_error()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::*;
        use futures_util::{FutureExt, StreamExt};
        use plakous_model::{
            AttemptId, CancellationToken, Digest, DigestDomain, ModelCallContext, ModelEvent,
            ModelRetry, ModelRetryCause,
        };

        let binding = MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-drain"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-drain"])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation-drain"])?,
            plakous_messages::ReplaySafety::CostOnly,
        );
        let context = ModelCallContext::new(
            AttemptId::try_from(uuid::Uuid::from_u128(46))?,
            CancellationToken::new(),
            None,
        );
        let (mut attempt, events) = MessagesAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(4).ok_or("capacity")?,
        );
        let descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("anthropic.messages")?,
            RouteId::new("anthropic.claude_oauth")?,
            ModelId::new("claude-sonnet-5")?,
            None,
            None,
            ModelAdapterProfileId::new("anthropic.claude.oauth.test")?,
            CapabilitySet::default(),
        );
        let retry = |next| {
            ModelRetry::new(
                next,
                4,
                Duration::ZERO,
                ModelRetryCause::RecordRejectedInTransit,
                false,
                "queued",
            )
        };
        let operation: AnthropicMessagesOperation = Box::pin(async move {
            for next in [2, 3] {
                attempt.events_mut().send_retry(retry(next)).await?;
            }
            Err(MessagesAttemptError::Unavailable)
        });
        let mut stream = AnthropicMessagesModelStream {
            pre_dispatch_diagnostics: VecDeque::new(),
            events,
            operation: Some(operation),
            descriptor,
            pending_protocol: None,
            pending_error: None,
            done: false,
        };
        for expected in [2, 3] {
            let Some(Some(Ok(ModelEvent::Retry(retry)))) = stream.next().now_or_never() else {
                return Err(format!("queued retry {expected} was not delivered").into());
            };
            assert_eq!(retry.next_physical(), expected);
        }
        let Some(Some(Err(error))) = stream.next().now_or_never() else {
            return Err("operation error was not delivered".into());
        };
        assert!(
            error.to_string().contains("Messages service unavailable"),
            "{error}"
        );
        assert!(matches!(stream.next().now_or_never(), Some(None)));
        Ok(())
    }
}
