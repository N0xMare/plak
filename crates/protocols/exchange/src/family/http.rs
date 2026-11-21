//! One-exchange HTTP/SSE host for one protocol family.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

use crate::attempt::{ExchangeBreak, FailurePoint, PhysicalRequestOutcome};
use crate::{cancellable, read_bounded_body, retry_phase, time_until};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, Failure as HttpFailure, Origin,
    Request as HttpRequest, Response, ResponseBody, StatusCode,
};
use plakous_model::ModelRetryDeclineReason as Decline;
use plakous_model::{
    Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelResponse, ModelRetry,
    ModelTransportFault,
};
use tower_service::Service;

use super::{
    FamilyAttempt, FamilyAttemptError, FamilyAttemptProtocolError, FamilyAttemptResponse,
    FamilyExchange, FamilyExchangeHooks, FamilyOperationBinding, FamilyWire, Generation,
    GenerationEnd, GenerationTransport, Reauthorization, RetryDelivery, RetryFailure,
    TransportRejection, transport_fault,
};

/// Credential-free, exact input to one family HTTP operation.
#[derive(Clone)]
pub struct PreparedFamilyHttpOperation<W: FamilyWire> {
    binding: FamilyOperationBinding,
    body: Arc<[u8]>,
    stream: W::StreamBinding,
}

impl<W: FamilyWire> PreparedFamilyHttpOperation<W> {
    /// Creates a complete family operation.
    #[must_use]
    pub fn new(
        binding: FamilyOperationBinding,
        body: impl Into<Arc<[u8]>>,
        stream: W::StreamBinding,
    ) -> Self {
        Self {
            binding,
            body: body.into(),
            stream,
        }
    }
    /// Returns exact attempt authority.
    #[must_use]
    pub const fn binding(&self) -> &FamilyOperationBinding {
        &self.binding
    }
    /// Returns credential-free request bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    /// Returns exact stream grammar authority.
    #[must_use]
    pub const fn stream(&self) -> &W::StreamBinding {
        &self.stream
    }
}

impl<W: FamilyWire> fmt::Debug for PreparedFamilyHttpOperation<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedFamilyHttpOperation<W>")
            .field("binding", &self.binding)
            .field("body", &"<redacted>")
            .field("stream", &self.stream)
            .finish()
    }
}

/// Retry/effect facts exposed narrowly to provider error classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FamilyHttpSafety {
    /// Provider acceptance is possible or proven.
    pub provider_accepted: bool,
    /// Caller-visible output was delivered.
    pub visible_output: bool,
    /// Provider-hosted effects may have occurred.
    pub hosted_effects_possible: bool,
}

/// Provider-owned interpretation of a non-success response head.
pub struct FamilyHttpErrorHead<M> {
    /// Provider metadata retained for body classification.
    pub metadata: M,
    /// Conservative physical outcome for the status.
    pub outcome: PhysicalRequestOutcome,
    /// Whether the status itself proves acceptance.
    pub provider_accepted: bool,
}

/// Provider-owned interpretation of a response head.
pub enum FamilyHttpResponseHead<M> {
    /// Accepted SSE response with validated provider request attribution.
    Stream(ModelProviderRequestId),
    /// Accepted SSE response whose selected dialect derives request attribution in-band.
    StreamWithoutRequestId,
    /// Non-success response requiring bounded body classification.
    Error(FamilyHttpErrorHead<M>),
}

/// Narrow provider dialect hooks around the shared family exchange host.
pub trait FamilyHttpHooks<W: FamilyWire>: Clone + Send + Sync + 'static {
    /// Provider metadata retained between response-head and error-body handling.
    type ErrorMetadata: Send + 'static;

    /// Builds the exact authorized request after private attempt admission.
    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        attempt: &FamilyAttempt<W>,
        operation: &PreparedFamilyHttpOperation<W>,
    ) -> Result<HttpRequest, FamilyAttemptProtocolError<W>>;

    /// Interprets status, headers, provider request ID, and content type.
    fn classify_response_head(
        &self,
        response: &Response,
    ) -> Result<FamilyHttpResponseHead<Self::ErrorMetadata>, FamilyAttemptProtocolError<W>>;

    /// Interprets a bounded provider error body.
    fn classify_error_response(
        &self,
        metadata: Self::ErrorMetadata,
        body: &[u8],
        safety: FamilyHttpSafety,
    ) -> FamilyAttemptProtocolError<W>;

    /// Interprets a provider-blind transport failure.
    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        request_id: Option<&str>,
    ) -> FamilyAttemptProtocolError<W>;

    /// Settles dispatch accounting from the exact exchange's transport proof.
    fn transport_failed(&self, _failure: HttpFailure) {}

    /// Supplies the provider-specific cancellation diagnostic.
    fn cancellation_error(&self) -> FamilyAttemptProtocolError<W>;

    /// Interprets family grammar errors that contain provider-owned payloads.
    fn classify_stream_error(&self, error: W::StreamError) -> FamilyAttemptProtocolError<W>;
}

/// Closed async hook run once after an HTTP 401 head, before the single
/// refresh-and-retry re-dispatch. It must force a credential refresh and
/// return the dialect hooks carrying the renewed grant. Not part of the host
/// behavior identity: it changes no wire byte of any dispatch.
pub type FamilyCredentialRejectedFn<W, H> = for<'a> fn(
    &'a H,
) -> Pin<
    Box<dyn Future<Output = Result<H, FamilyAttemptProtocolError<W>>> + Send + 'a>,
>;

/// Re-authorizes the same sealed route and credential before each retried
/// physical request. Workflows retry only requests proven unsent.
pub type FamilyRedispatchFn<W, H> = for<'a> fn(
    &'a H,
) -> Pin<
    Box<dyn Future<Output = Result<H, FamilyAttemptProtocolError<W>>> + Send + 'a>,
>;

/// Shared exchange service. A provider may install a 401 renewal hook and a
/// redispatch hook. The renewal re-sends only a definitely refused
/// turn request; the redispatch hook enables [`crate::attempt::RetryPolicy::TURN`]. Both share its
/// physical budget.
#[derive(Clone)]
pub struct FamilyHttpAttemptService<W: FamilyWire, H> {
    client: Client,
    dial_origin: Origin,
    operation: PreparedFamilyHttpOperation<W>,
    hooks: H,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    timeout: Duration,
    max_error_body_bytes: usize,
    credential_rejected: Option<FamilyCredentialRejectedFn<W, H>>,
    redispatch: Option<FamilyRedispatchFn<W, H>>,
    turn_rooted: bool,
}

impl<W: FamilyWire, H: FamilyHttpHooks<W>> FamilyHttpAttemptService<W, H> {
    /// Creates a fresh service with explicit bounded transport policy.
    ///
    /// `timeout` bounds each physical exchange up to its response head; the
    /// streamed body is bounded only by the client's per-pull idle limit.
    #[allow(
        clippy::too_many_arguments,
        reason = "all executable family authority is explicit"
    )]
    #[must_use]
    pub fn new(
        client: Client,
        dial_origin: Origin,
        operation: PreparedFamilyHttpOperation<W>,
        hooks: H,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        timeout: Duration,
        max_error_body_bytes: usize,
        turn_rooted: bool,
    ) -> Self {
        Self {
            client,
            dial_origin,
            operation,
            hooks,
            clock,
            ids,
            timeout,
            max_error_body_bytes,
            credential_rejected: None,
            redispatch: if W::BUILD_REVALIDATES_RETRY {
                Some(unchanged_hooks::<W, H>)
            } else {
                None
            },
            turn_rooted,
        }
    }

    /// Enables ordinary-turn retry when `build_request` revalidates every generation.
    #[must_use]
    pub const fn with_turn_retry(mut self) -> Self {
        self.turn_rooted = true;
        self
    }

    /// Installs the credential-rejection hook that enables one
    /// refresh-and-retry after an HTTP 401 head.
    #[must_use]
    pub const fn with_credential_rejected(
        mut self,
        credential_rejected: FamilyCredentialRejectedFn<W, H>,
    ) -> Self {
        self.credential_rejected = Some(credential_rejected);
        self
    }

    /// Enables revalidated retries, restricted to unsent requests for workflows.
    #[must_use]
    pub const fn with_retry(mut self, redispatch: FamilyRedispatchFn<W, H>) -> Self {
        self.redispatch = Some(redispatch);
        self
    }
}

impl<W: FamilyWire, H: FamilyHttpHooks<W>> Service<FamilyAttempt<W>>
    for FamilyHttpAttemptService<W, H>
{
    type Response = FamilyAttemptResponse<W>;
    type Error = FamilyAttemptError<W>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, attempt: FamilyAttempt<W>) -> Self::Future {
        let service = self.clone();
        Box::pin(async move {
            attempt.ensure_service_admitted()?;
            if attempt.binding().operation() != service.operation.binding() {
                return Err(FamilyAttemptError::<W>::Binding {
                    facts: attempt.terminal_facts(),
                });
            }
            let timeout = attempt
                .context()
                .dispatch_timeout()
                .map_or(service.timeout, |budget| service.timeout.min(budget));
            let hard_deadline = attempt
                .context()
                .deadline()
                .map(|deadline| Instant::now() + time_until(deadline, service.clock.now()));
            let host = FamilyExchange {
                timeout,
                hard_deadline,
                turn_rooted: service.turn_rooted,
                redispatch_available: service.redispatch.is_some(),
                retry_delivery: RetryDelivery::BeforeDelay,
            };
            let cancellation = attempt.context().cancellation().clone();
            host.run(
                &mut HttpGeneration {
                    service,
                    attempt,
                    timeout,
                    hard_deadline,
                    provider_deadline_replayed: false,
                },
                &cancellation,
            )
            .await
        })
    }
}

struct HttpGeneration<W: FamilyWire, H> {
    service: FamilyHttpAttemptService<W, H>,
    attempt: FamilyAttempt<W>,
    timeout: Duration,
    hard_deadline: Option<Instant>,
    provider_deadline_replayed: bool,
}

type HttpResult<W> = Result<FamilyAttemptResponse<W>, FamilyAttemptError<W>>;

#[allow(
    clippy::large_enum_variant,
    reason = "one terminal is moved out without allocating the successful response"
)]
enum HttpTerminal<W: FamilyWire> {
    Finished(HttpResult<W>),
    Deferred(FamilyAttemptProtocolError<W>),
}

impl<W: FamilyWire, H: FamilyHttpHooks<W>> FamilyExchangeHooks for HttpGeneration<W, H> {
    type Result = HttpResult<W>;
    type Terminal = HttpTerminal<W>;

    fn ledger(&self) -> &crate::attempt::PhysicalRequestLedger {
        self.attempt.ledger()
    }

    async fn exchange(&mut self, generation: Generation) -> GenerationEnd<Self::Terminal> {
        match self.exchange_one(generation).await {
            Ok(end) => end,
            Err(error) => GenerationEnd::Finished(HttpTerminal::Finished(Err(error))),
        }
    }

    async fn announce_retry(&mut self, retry: ModelRetry) -> bool {
        self.attempt.events_mut().send_retry(retry).await.is_ok()
    }

    async fn reauthorize(&mut self, renew: bool) -> Reauthorization {
        let service = &mut self.service;
        let hook = if renew {
            service.credential_rejected
        } else {
            service.redispatch
        };
        let Some(hook) = hook else {
            return Reauthorization::End;
        };
        let started = Instant::now();
        let cancellation = self.attempt.context().cancellation();
        let bound = self
            .attempt
            .context()
            .deadline()
            .map_or(self.timeout, |deadline| {
                time_until(deadline, service.clock.now()).min(self.timeout)
            });
        if bound.is_zero() {
            return Reauthorization::End;
        }
        // Dropping the bounded refresh leaves the claimed-refresh store's
        // durable Unknown fence in place and never detaches provider I/O.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Reauthorization::Cancelled,
            () = tokio::time::sleep(bound) => Reauthorization::End,
            result = hook(&service.hooks) => match result {
                Ok(refreshed) if !cancellation.is_cancelled() => {
                    service.hooks = refreshed;
                    Reauthorization::Send(Some(started))
                }
                Ok(_) => Reauthorization::Cancelled,
                Err(_) => Reauthorization::End,
            },
        }
    }

    async fn finish(&mut self, terminal: Self::Terminal, _deferred: bool) -> Self::Result {
        match terminal {
            HttpTerminal::Finished(result) => result,
            HttpTerminal::Deferred(error) => emit_error(&mut self.attempt, error).await,
        }
    }

    fn cancelled(&mut self) -> Self::Result {
        Err(FamilyAttemptError::Cancelled {
            facts: Some(self.attempt.terminal_facts()),
        })
    }

    async fn deadline(&mut self, terminal: Self::Terminal) -> Self::Result {
        self.finish(terminal, true).await
    }
}

impl<W: FamilyWire, H: FamilyHttpHooks<W>> HttpGeneration<W, H> {
    #[allow(
        clippy::too_many_lines,
        reason = "one generation has ordered authorization, response-head and stream phases"
    )]
    async fn exchange_one(
        &mut self,
        generation: Generation,
    ) -> Result<GenerationEnd<HttpTerminal<W>>, FamilyAttemptError<W>> {
        let service = &self.service;
        let attempt = &mut self.attempt;
        let client = &service.client;
        let hooks = &service.hooks;
        let operation = &service.operation;
        let dial_origin = &service.dial_origin;
        if attempt.context().cancellation().is_cancelled() {
            return Err(FamilyAttemptError::<W>::Cancelled {
                facts: Some(attempt.terminal_facts()),
            });
        }
        let physical = attempt.observe_physical_request_started()?;
        if !attempt.authorize_generation(&physical)? {
            return finish_error(
                attempt,
                PhysicalRequestOutcome::DefiniteRejection,
                FamilyAttemptProtocolError::<W>::new(format!(
                    "{} authorization generation mismatch",
                    W::NAME
                )),
            )
            .await;
        }
        let request = match hooks.build_request(client, dial_origin.clone(), attempt, operation) {
            Ok(request) => request,
            Err(error) => {
                return finish_error(attempt, PhysicalRequestOutcome::DefiniteRejection, error)
                    .await;
            }
        };
        let request_log = RetryFailure::request_log(&request);
        let transport = GenerationTransport::new(&generation, self.hard_deadline);
        let cancellation = transport.cancellation().clone();
        let exchanged = transport
            .send(client, request, attempt.context().cancellation())
            .await;
        let response = match exchanged.value {
            Ok(response) => response,
            Err(failure) => {
                hooks.transport_failed(failure);
                let outcome = if failure.transmission().peer_cannot_have_acted() {
                    PhysicalRequestOutcome::DefiniteRejection
                } else {
                    PhysicalRequestOutcome::AmbiguousPostSend
                };
                if exchanged.cancelled && outcome == PhysicalRequestOutcome::DefiniteRejection {
                    attempt.observe_physical_request_finished(outcome)?;
                    return Err(FamilyAttemptError::<W>::Cancelled {
                        facts: Some(attempt.terminal_facts()),
                    });
                }
                if exchanged.cancelled {
                    return finish_error(attempt, outcome, hooks.cancellation_error()).await;
                }
                let error = hooks
                    .classify_transport_failure(failure, None)
                    .with_exchange_break(ExchangeBreak::Transport);
                attempt.observe_physical_request_finished(outcome)?;
                return Ok(retry_or_end(
                    error,
                    RetryFailure::transport(failure, TransportRejection::from_failure(failure)),
                    request_log,
                ));
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(&plakous_http::HeaderName::from_static("retry-after"))
            .and_then(|value| value.to_str().ok())
            .and_then(crate::attempt::retry_after_seconds);
        let retry_disabled = plakous_http::retry_disabled(response.headers());
        match hooks.classify_response_head(&response) {
            Err(error) => {
                let _ = response.into_body().close().await;
                attempt.observe_provider_accepted()?;
                return finish_error(attempt, PhysicalRequestOutcome::AcceptedDisconnect, error)
                    .await;
            }
            Ok(FamilyHttpResponseHead::Error(head)) => {
                if head.provider_accepted {
                    attempt.observe_provider_accepted()?;
                }
                let body = read_bounded_body(
                    response.into_body(),
                    attempt.context().cancellation(),
                    &cancellation,
                    service.max_error_body_bytes,
                )
                .await;
                let body = match body {
                    Ok(body) => body,
                    Err(failure) => {
                        return finish_error(
                            attempt,
                            head.outcome,
                            hooks
                                .classify_transport_failure(failure, None)
                                .with_exchange_break(ExchangeBreak::Transport),
                        )
                        .await;
                    }
                };
                let outcome = head.outcome;
                let error = hooks.classify_error_response(head.metadata, &body, safety(attempt));
                let usage_limit = crate::attempt::body_reports_usage_limit(&body);
                let error = if usage_limit {
                    error.with_usage_limit()
                } else {
                    error
                };
                let refused =
                    outcome == PhysicalRequestOutcome::DefiniteRejection && !head.provider_accepted;
                attempt.observe_physical_request_finished(outcome)?;
                // The status ends this request whether or not a retry follows;
                // a retry evaluation overwrites it with its decision.
                attempt.record_failure(
                    if refused {
                        FailurePoint::Refused
                    } else {
                        FailurePoint::AfterSend
                    },
                    Some(plakous_model::ModelRetryPhase::ResponseHead.as_str()),
                    error.refusal(),
                    false,
                    None,
                );
                if error.usage_limit() || (retry_disabled && !W::STATUS_RETRY_DECLINE_NOTICE) {
                    return Ok(GenerationEnd::Finished(HttpTerminal::Deferred(error)));
                }
                if status == StatusCode::UNAUTHORIZED
                    && refused
                    && !retry_disabled
                    && service.turn_rooted
                {
                    return Ok(
                        if attempt.terminal_facts().replay_safety()
                            == crate::attempt::ReplaySafety::ServerEffect
                        {
                            GenerationEnd::Finished(HttpTerminal::Deferred(error))
                        } else {
                            GenerationEnd::Renew(HttpTerminal::Deferred(error))
                        },
                    );
                }
                let advice = error
                    .retry_after()
                    .map(|advice| Duration::from_millis(advice.as_millis()))
                    .or(retry_after);
                let failure = RetryFailure::status(
                    status.as_u16(),
                    refused,
                    advice,
                    error.message().to_owned(),
                )
                .map(|failure| {
                    if retry_disabled {
                        failure.with_decline(Decline::RetryDisabled)
                    } else {
                        failure
                    }
                });
                Ok(retry_or_end(error, failure, request_log))
            }
            Ok(
                head @ (FamilyHttpResponseHead::Stream(_)
                | FamilyHttpResponseHead::StreamWithoutRequestId),
            ) => {
                attempt.observe_provider_accepted()?;
                let request_id = match head {
                    FamilyHttpResponseHead::Stream(request_id) => Some(request_id),
                    _ => None,
                };
                let interrupted = match stream_response(
                    response.into_body(),
                    operation,
                    hooks,
                    request_id,
                    Arc::clone(&service.clock),
                    Arc::clone(&service.ids),
                    cancellation,
                    attempt,
                )
                .await
                {
                    StreamOutcome::<W>::Finished(result) => {
                        return Ok(GenerationEnd::Finished(HttpTerminal::Finished(result)));
                    }
                    StreamOutcome::<W>::Interrupted(interrupted) => interrupted,
                };
                attempt.observe_physical_request_finished(
                    PhysicalRequestOutcome::AcceptedDisconnect,
                )?;
                let mut failure = RetryFailure::interrupted(
                    interrupted.fault,
                    interrupted.detail,
                    interrupted.delivered,
                    interrupted.loss,
                )
                .with_phase(
                    interrupted
                        .loss
                        .transport_phase
                        .or(Some(plakous_model::ModelRetryPhase::ResponseBody)),
                );
                if W::PROVIDER_DEADLINE_REPLAY_ONCE
                    && matches!(
                        interrupted
                            .error
                            .http_status()
                            .map(plakous_model::ModelHttpStatus::get),
                        Some(408 | 504)
                    )
                    && interrupted.error.refusal()
                        == Some(crate::attempt::ProviderRefusal::Deadline)
                {
                    failure = failure.with_refusal(crate::attempt::ProviderRefusal::Deadline);
                    if self.provider_deadline_replayed {
                        failure = failure.with_decline(Decline::Budget);
                    }
                    self.provider_deadline_replayed = true;
                }
                if retry_disabled {
                    failure = failure.with_decline(Decline::RetryDisabled);
                }
                Ok(retry_or_end(interrupted.error, Some(failure), request_log))
            }
        }
    }
}

fn retry_or_end<W: FamilyWire>(
    error: FamilyAttemptProtocolError<W>,
    failure: Option<RetryFailure>,
    request_log: (String, String, usize),
) -> GenerationEnd<HttpTerminal<W>> {
    let terminal = HttpTerminal::Deferred(error);
    match failure {
        Some(failure) => GenerationEnd::Retry {
            terminal,
            failure: failure.with_request(request_log),
        },
        None => GenerationEnd::Finished(terminal),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "stream owner receives exact exchange state"
)]
async fn stream_response<W: FamilyWire, H: FamilyHttpHooks<W>>(
    mut body: ResponseBody,
    operation: &PreparedFamilyHttpOperation<W>,
    hooks: &H,
    request_id: Option<ModelProviderRequestId>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    cancellation: HttpCancellationToken,
    attempt: &mut FamilyAttempt<W>,
) -> StreamOutcome<W> {
    let mut progress = StreamProgress::default();
    let finished = stream_body(
        &mut body,
        operation,
        hooks,
        request_id.as_ref(),
        clock,
        ids,
        &cancellation,
        attempt,
        &mut progress,
    )
    .await;
    match finished {
        Ok(response) => StreamOutcome::<W>::Finished(Ok(response)),
        Err(StreamBreak::<W>::Failed(error)) => StreamOutcome::<W>::Finished(Err(error)),
        Err(StreamBreak::<W>::Closing(error)) => {
            let _ = body.close().await;
            StreamOutcome::<W>::Finished(
                finish_error(attempt, PhysicalRequestOutcome::AcceptedDisconnect, error).await,
            )
        }
        Err(StreamBreak::<W>::Interrupted {
            error,
            fault,
            detail,
        }) => {
            let _ = body.close().await;
            StreamOutcome::<W>::Interrupted(Interrupted {
                detail,
                error,
                fault,
                delivered: progress.delivered,
                loss: progress.loss,
            })
        }
    }
}

/// How one accepted response stream ended.
#[allow(
    clippy::large_enum_variant,
    reason = "returned once per physical stream and matched immediately; never stored"
)]
enum StreamOutcome<W: FamilyWire> {
    /// The logical attempt ended, successfully or not.
    Finished(Result<FamilyAttemptResponse<W>, FamilyAttemptError<W>>),
    /// The stream broke transiently; the physical request is still open.
    Interrupted(Interrupted<W>),
}

/// A transient break of an accepted response stream.
struct Interrupted<W: FamilyWire> {
    error: FamilyAttemptProtocolError<W>,
    fault: ModelTransportFault,
    detail: String,
    delivered: bool,
    loss: plakous_model::ModelRetryLoss,
}

/// What the stream of one physical request has delivered so far.
#[derive(Default)]
struct StreamProgress {
    delivered: bool,
    loss: plakous_model::ModelRetryLoss,
}

enum StreamBreak<W: FamilyWire> {
    /// Close the body, then end the attempt with this failure.
    Closing(FamilyAttemptProtocolError<W>),
    Failed(FamilyAttemptError<W>),
    Interrupted {
        error: FamilyAttemptProtocolError<W>,
        fault: ModelTransportFault,
        detail: String,
    },
}

impl<W: FamilyWire> From<FamilyAttemptError<W>> for StreamBreak<W> {
    fn from(error: FamilyAttemptError<W>) -> Self {
        Self::Failed(error)
    }
}

/// A provider error event naming a transient refusal mid-stream.
const fn stream_refusal_fault<W: FamilyWire>(
    error: &FamilyAttemptProtocolError<W>,
) -> Option<ModelTransportFault> {
    match error.refusal() {
        Some(crate::attempt::ProviderRefusal::Overloaded) => Some(ModelTransportFault::Overloaded),
        Some(crate::attempt::ProviderRefusal::RateLimit) => Some(ModelTransportFault::RateLimited),
        Some(crate::attempt::ProviderRefusal::Deadline)
            if W::PROVIDER_DEADLINE_REPLAY_ONCE
                && match error.http_status() {
                    Some(status) => matches!(status.get(), 408 | 504),
                    None => false,
                } =>
        {
            Some(ModelTransportFault::Timeout)
        }
        _ => None,
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "stream owner receives exact exchange state"
)]
async fn stream_body<W: FamilyWire, H: FamilyHttpHooks<W>>(
    body: &mut ResponseBody,
    operation: &PreparedFamilyHttpOperation<W>,
    hooks: &H,
    request_id: Option<&ModelProviderRequestId>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    cancellation: &HttpCancellationToken,
    attempt: &mut FamilyAttempt<W>,
    progress: &mut StreamProgress,
) -> Result<FamilyAttemptResponse<W>, StreamBreak<W>> {
    let mut projector =
        match W::projector(operation.stream().clone(), request_id.cloned(), clock, ids) {
            Ok(projector) => projector,
            Err(error) => {
                return finish_error(
                    attempt,
                    PhysicalRequestOutcome::AcceptedDisconnect,
                    hooks.classify_stream_error(error),
                )
                .await
                .map_err(StreamBreak::<W>::from);
            }
        };
    let mut terminal = None;
    loop {
        let next = cancellable(
            body.next_chunk(),
            attempt.context().cancellation(),
            cancellation,
        )
        .await;
        if next.cancelled {
            return Err(StreamBreak::<W>::Closing(hooks.cancellation_error()));
        }
        match next.value {
            Some(Ok(chunk)) => {
                let events = match W::push(&mut projector, &chunk) {
                    Ok(events) => events,
                    Err(error) => {
                        let usage_limit = W::usage_limit(&error);
                        let error = hooks.classify_stream_error(error);
                        let error = if usage_limit {
                            error.with_usage_limit()
                        } else {
                            error
                        };
                        if let Some(fault) = stream_refusal_fault(&error) {
                            return Err(StreamBreak::<W>::Interrupted {
                                detail: error.message().to_owned(),
                                error,
                                fault,
                            });
                        }
                        if let Some(refusal) = error.refusal() {
                            attempt.record_failure(
                                FailurePoint::MidStream,
                                Some(plakous_model::ModelRetryPhase::ResponseBody.as_str()),
                                Some(refusal),
                                false,
                                None,
                            );
                        }
                        return Err(StreamBreak::<W>::Closing(error));
                    }
                };
                deliver(attempt, events, &mut terminal, progress).await?;
                if let Some(completed_items) = W::completed_items(&projector) {
                    progress.loss.completed_items = completed_items;
                }
            }
            Some(Err(failure)) => {
                progress.loss.transport_phase = retry_phase(failure.phase());
                let error = hooks
                    .classify_transport_failure(
                        failure,
                        request_id.map(ModelProviderRequestId::as_str),
                    )
                    .with_exchange_break(ExchangeBreak::Transport);
                if let Some(fault) = transport_fault(failure) {
                    return Err(StreamBreak::<W>::Interrupted {
                        detail: format!("{failure:?}"),
                        error,
                        fault,
                    });
                }
                return finish_error(attempt, PhysicalRequestOutcome::AcceptedDisconnect, error)
                    .await
                    .map_err(StreamBreak::<W>::from);
            }
            None => break,
        }
    }
    // The body ended; a projector that cannot finish was cut short.
    let events = match W::finish(&mut projector) {
        Ok(events) => events,
        Err(error) => {
            let error = hooks
                .classify_stream_error(error)
                .with_exchange_break(ExchangeBreak::Transport);
            return Err(StreamBreak::<W>::Interrupted {
                detail: error.message().to_owned(),
                error,
                fault: ModelTransportFault::EndOfStream,
            });
        }
    };
    deliver(attempt, events, &mut terminal, progress).await?;
    let Some(request_id) = W::request_id(&projector) else {
        return finish_error(
            attempt,
            PhysicalRequestOutcome::AcceptedDisconnect,
            FamilyAttemptProtocolError::<W>::new(format!(
                "{} stream omitted provider request attribution",
                W::NAME
            )),
        )
        .await
        .map_err(StreamBreak::<W>::from);
    };
    let projection = match W::projection(projector) {
        Ok(projection) => projection,
        Err(error) => {
            return finish_error(
                attempt,
                PhysicalRequestOutcome::AcceptedDisconnect,
                hooks.classify_stream_error(error),
            )
            .await
            .map_err(StreamBreak::<W>::from);
        }
    };
    if W::PROJECTOR_TERMINAL && terminal.is_none() {
        return Err(StreamBreak::Interrupted {
            error: FamilyAttemptProtocolError::new(format!(
                "{} stream omitted terminal response",
                W::NAME
            ))
            .with_exchange_break(ExchangeBreak::Transport),
            fault: ModelTransportFault::EndOfStream,
            detail: format!("{} stream omitted terminal response", W::NAME),
        });
    }
    let sealed = FamilyAttemptResponse::new(attempt, projection, Some(request_id))?;
    attempt.publish_terminal(&sealed).await?;
    Ok(sealed)
}

async fn deliver<W: FamilyWire>(
    attempt: &mut FamilyAttempt<W>,
    events: Vec<ModelEvent>,
    terminal: &mut Option<ModelResponse>,
    progress: &mut StreamProgress,
) -> Result<(), FamilyAttemptError<W>> {
    for event in events {
        progress.loss.observe(&event);
        match event {
            ModelEvent::ResponseCompleted(response) => {
                if !W::PROJECTOR_TERMINAL {
                    return Err(protocol(
                        attempt,
                        FamilyAttemptProtocolError::new(format!(
                            "{} projector emitted terminal authority",
                            W::NAME
                        )),
                    ));
                }
                if terminal.replace(response).is_some() {
                    return Err(protocol(
                        attempt,
                        FamilyAttemptProtocolError::<W>::new(format!(
                            "{} stream emitted two terminals",
                            W::NAME
                        )),
                    ));
                }
            }
            _ if terminal.is_some() => {
                return Err(protocol(
                    attempt,
                    FamilyAttemptProtocolError::<W>::new(format!(
                        "{} event followed terminal",
                        W::NAME
                    )),
                ));
            }
            event => {
                attempt.events_mut().send(event).await?;
                progress.delivered = true;
            }
        }
    }
    Ok(())
}

async fn finish_error<W: FamilyWire, T>(
    attempt: &mut FamilyAttempt<W>,
    outcome: PhysicalRequestOutcome,
    source: FamilyAttemptProtocolError<W>,
) -> Result<T, FamilyAttemptError<W>> {
    attempt.observe_physical_request_finished(outcome)?;
    emit_error(attempt, source).await
}

/// Emits one terminal observation for an already-finished physical request.
async fn emit_error<W: FamilyWire, T>(
    attempt: &mut FamilyAttempt<W>,
    source: FamilyAttemptProtocolError<W>,
) -> Result<T, FamilyAttemptError<W>> {
    let facts = attempt.terminal_facts();
    let source = source.with_terminal_facts(facts);
    match attempt.events_mut().send_error(source.clone()).await {
        Ok(()) => Err(FamilyAttemptError::<W>::Protocol { source, facts }),
        Err(error) => Err(error),
    }
}

fn protocol<W: FamilyWire>(
    attempt: &FamilyAttempt<W>,
    source: FamilyAttemptProtocolError<W>,
) -> FamilyAttemptError<W> {
    FamilyAttemptError::<W>::protocol(source, attempt.terminal_facts())
}

fn safety<W: FamilyWire>(attempt: &FamilyAttempt<W>) -> FamilyHttpSafety {
    let facts = attempt.terminal_facts();
    FamilyHttpSafety {
        provider_accepted: facts.provider_acceptance()
            != crate::attempt::ProviderAcceptance::NotAccepted,
        visible_output: facts.visible_output(),
        hosted_effects_possible: facts.hosted_effects_possible(),
    }
}

fn unchanged_hooks<W: FamilyWire, H: FamilyHttpHooks<W>>(
    hooks: &H,
) -> Pin<Box<dyn Future<Output = Result<H, FamilyAttemptProtocolError<W>>> + Send + '_>> {
    Box::pin(std::future::ready(Ok(hooks.clone())))
}

#[cfg(test)]
mod tests;
