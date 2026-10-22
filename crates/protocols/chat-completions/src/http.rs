//! One-exchange HTTP/SSE host for the Chat family.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

use plakous_attempt::{
    ExchangeBreak, FailurePoint, Jitter, PhysicalRequestOutcome, RetryPolicy, retry_after_seconds,
};
use plakous_exchange::{
    RetryDeclineInput, cancellable, read_bounded_body, retry_cause_refusal, retry_decline,
    retry_phase, time_until,
};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, ExchangeControl, Failure as HttpFailure,
    HeaderName, Origin, Request as HttpRequest, Response, ResponseBody,
};
use plakous_model::ModelRetryDeclineReason as Decline;
use plakous_model::{
    Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelRetry, ModelRetryCause,
    ModelTransportFault,
};
use tower_service::Service;

use crate::{
    ChatAttempt, ChatAttemptError, ChatAttemptProtocolError, ChatAttemptResponse,
    ChatOperationBinding, ChatPhysicalRequestBinding, ChatStreamError, ChatStreamProjector,
    LoweredChatRequest, PreparedChatStreamBinding,
};

const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");

/// Credential-free, exact input to one Chat HTTP operation.
#[derive(Clone)]
pub struct PreparedChatHttpOperation {
    binding: ChatOperationBinding,
    request: LoweredChatRequest,
    stream: PreparedChatStreamBinding,
}

impl PreparedChatHttpOperation {
    /// Creates a complete family operation.
    #[must_use]
    pub fn new(
        binding: ChatOperationBinding,
        request: LoweredChatRequest,
        stream: PreparedChatStreamBinding,
    ) -> Self {
        Self {
            binding,
            request,
            stream,
        }
    }
    /// Returns exact attempt authority.
    #[must_use]
    pub const fn binding(&self) -> &ChatOperationBinding {
        &self.binding
    }
    /// Returns credential-free request bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        self.request.body()
    }
    /// Returns exact stream grammar authority.
    #[must_use]
    pub const fn stream(&self) -> &PreparedChatStreamBinding {
        &self.stream
    }
}

impl fmt::Debug for PreparedChatHttpOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedChatHttpOperation")
            .field("binding", &self.binding)
            .field("request", &"<redacted>")
            .field("stream", &self.stream)
            .finish()
    }
}

/// Retry/effect facts exposed narrowly to provider error classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatHttpSafety {
    /// Provider acceptance is possible or proven.
    pub provider_accepted: bool,
    /// Caller-visible output was delivered.
    pub visible_output: bool,
    /// Provider-hosted effects may have occurred.
    pub hosted_effects_possible: bool,
}

/// Provider-owned interpretation of a non-success response head.
pub struct ChatHttpErrorHead<M> {
    /// Provider metadata retained for body classification.
    pub metadata: M,
    /// Conservative physical outcome for the status.
    pub outcome: PhysicalRequestOutcome,
    /// Whether the status itself proves acceptance.
    pub provider_accepted: bool,
}

/// Provider-owned interpretation of a response head.
pub enum ChatHttpResponseHead<M> {
    /// Accepted SSE response with validated provider request attribution.
    Stream(Option<ModelProviderRequestId>),
    /// Non-success response requiring bounded body classification.
    Error(ChatHttpErrorHead<M>),
}

/// Narrow provider dialect hooks around the shared Chat exchange host.
pub trait ChatHttpHooks: Clone + Send + Sync + 'static {
    /// Provider metadata retained between response-head and error-body handling.
    type ErrorMetadata: Send + 'static;

    /// Builds the exact authorized request after private attempt admission.
    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        physical: &ChatPhysicalRequestBinding,
        operation: &PreparedChatHttpOperation,
    ) -> Result<HttpRequest, ChatAttemptProtocolError>;

    /// Interprets status, headers, provider request ID, and content type.
    fn classify_response_head(
        &self,
        response: &Response,
    ) -> Result<ChatHttpResponseHead<Self::ErrorMetadata>, ChatAttemptProtocolError>;

    /// Interprets a bounded provider error body.
    fn classify_error_response(
        &self,
        metadata: Self::ErrorMetadata,
        body: &[u8],
        safety: ChatHttpSafety,
    ) -> ChatAttemptProtocolError;

    /// Interprets a provider-blind transport failure.
    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        request_id: Option<&str>,
    ) -> ChatAttemptProtocolError;

    /// Supplies the provider-specific cancellation diagnostic.
    fn cancellation_error(&self) -> ChatAttemptProtocolError;

    /// Interprets family grammar errors that contain provider-owned payloads.
    fn classify_stream_error(&self, error: ChatStreamError) -> ChatAttemptProtocolError;
}

/// Shared exchange service. It has no reconnect or fallback path; with
/// [`ChatHttpAttemptService::with_turn_retry`] it re-sends a
/// failed turn request under [`RetryPolicy::TURN`].
pub struct ChatHttpAttemptService<H> {
    client: Client,
    dial_origin: Origin,
    operation: PreparedChatHttpOperation,
    hooks: H,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    timeout: Duration,
    max_error_body_bytes: usize,
    retry: bool,
}

impl<H: ChatHttpHooks> ChatHttpAttemptService<H> {
    /// Creates a fresh service with explicit bounded transport policy.
    ///
    /// `timeout` bounds each physical exchange up to its response head; the
    /// streamed body is bounded by the client's per-pull idle limit and the
    /// context's hard deadline.
    #[allow(
        clippy::too_many_arguments,
        reason = "all executable family authority is explicit"
    )]
    #[must_use]
    pub fn new(
        client: Client,
        dial_origin: Origin,
        operation: PreparedChatHttpOperation,
        hooks: H,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        timeout: Duration,
        max_error_body_bytes: usize,
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
            retry: false,
        }
    }

    /// Enables [`RetryPolicy::TURN`] for an ordinary turn. Without it,
    /// workflows retry only unsent requests, re-authorizing in `build_request`.
    #[must_use]
    pub const fn with_turn_retry(mut self) -> Self {
        self.retry = true;
        self
    }
}

impl<H: ChatHttpHooks> Service<ChatAttempt> for ChatHttpAttemptService<H> {
    type Response = ChatAttemptResponse;
    type Error = ChatAttemptError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut attempt: ChatAttempt) -> Self::Future {
        let client = self.client.clone();
        let dial_origin = self.dial_origin.clone();
        let operation = self.operation.clone();
        let hooks = self.hooks.clone();
        let clock = Arc::clone(&self.clock);
        let ids = Arc::clone(&self.ids);
        let timeout = self.timeout;
        let max_error_body_bytes = self.max_error_body_bytes;
        let retry = self.retry;
        Box::pin(async move {
            attempt.ensure_service_admitted()?;
            if attempt.binding().operation() != operation.binding() {
                return Err(ChatAttemptError::Binding {
                    facts: attempt.terminal_facts(),
                });
            }
            execute(
                &client,
                dial_origin,
                &operation,
                &hooks,
                clock,
                ids,
                timeout,
                max_error_body_bytes,
                retry,
                &mut attempt,
            )
            .await
        })
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one physical exchange keeps lifecycle facts adjacent"
)]
async fn execute<H: ChatHttpHooks>(
    client: &Client,
    dial_origin: Origin,
    operation: &PreparedChatHttpOperation,
    hooks: &H,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    timeout: Duration,
    max_error_body_bytes: usize,
    retry: bool,
    attempt: &mut ChatAttempt,
) -> Result<ChatAttemptResponse, ChatAttemptError> {
    let mut waited = Duration::ZERO;
    let mut fresh_connection = false;
    let mut long_stream_replays = plakous_attempt::LongStreamReplayBudget::default();
    let mut unreachable = plakous_attempt::UnreachableRideOut::default();
    let timeout = attempt
        .context()
        .dispatch_timeout()
        .map_or(timeout, |budget| timeout.min(budget));
    let hard_deadline = attempt
        .context()
        .deadline()
        .map(|deadline| Instant::now() + time_until(deadline, clock.now()));
    loop {
        let physical_started = Instant::now();
        let physical = attempt.observe_physical_request_started()?;
        let reserved = attempt.reserve_authorization()?;
        if physical != reserved {
            return finish_error(
                attempt,
                PhysicalRequestOutcome::DefiniteRejection,
                ChatAttemptProtocolError::new("Chat authorization generation mismatch"),
            )
            .await;
        }
        attempt.validate_authorization(&reserved)?;
        let request = match hooks.build_request(client, dial_origin.clone(), &physical, operation) {
            Ok(request) => request,
            Err(error) => {
                return finish_error(attempt, PhysicalRequestOutcome::DefiniteRejection, error)
                    .await;
            }
        };
        let request_log = (
            request.origin().host().to_owned(),
            request.path_and_query().path().to_owned(),
            request.body().len(),
        );
        let cancellation = HttpCancellationToken::new();
        let deadline = physical_started
            .checked_add(timeout)
            .unwrap_or(physical_started);
        let deadline = hard_deadline.map_or(deadline, |hard| deadline.min(hard));
        let exchanged = cancellable(
            client.execute(
                request,
                ExchangeControl::streaming(deadline.into_std(), cancellation.clone())
                    .with_body_deadline(hard_deadline.map(Instant::into_std))
                    .with_fresh_connection(std::mem::take(&mut fresh_connection)),
            ),
            attempt.context().cancellation(),
            &cancellation,
        )
        .await;
        let retry_disabled = exchanged
            .value
            .as_ref()
            .is_ok_and(|response| plakous_http::retry_disabled(response.headers()));
        let (retryable, error) = match exchanged.value {
            Ok(response) => match hooks.classify_response_head(&response) {
                Err(error) => {
                    let _ = response.into_body().close().await;
                    attempt.observe_provider_accepted()?;
                    return finish_error(
                        attempt,
                        PhysicalRequestOutcome::AcceptedDisconnect,
                        error,
                    )
                    .await;
                }
                Ok(ChatHttpResponseHead::Error(head)) => {
                    if head.provider_accepted {
                        attempt.observe_provider_accepted()?;
                    }
                    let status = response.status().as_u16();
                    let retry_after = response
                        .headers()
                        .get(&RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(retry_after_seconds);
                    let body = read_bounded_body(
                        response.into_body(),
                        attempt.context().cancellation(),
                        &cancellation,
                        max_error_body_bytes,
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
                    let error =
                        hooks.classify_error_response(head.metadata, &body, safety(attempt));
                    let usage_limit = plakous_attempt::body_reports_usage_limit(&body);
                    let error = if usage_limit {
                        error.with_usage_limit()
                    } else {
                        error
                    };
                    attempt.observe_physical_request_finished(head.outcome)?;
                    let refused = head.outcome == PhysicalRequestOutcome::DefiniteRejection
                        && !head.provider_accepted;
                    // The status ends this request whether or not a retry
                    // follows; a retry evaluation overwrites it with its decision.
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
                    let retryable = RetryPolicy::status_failure_point(status)
                        .filter(|_| !retry_disabled && !usage_limit)
                        .map(|point| {
                            // A refusal status the provider did not prove refused is
                            // as ambiguous as any other failure after the send.
                            let point = if refused {
                                point
                            } else {
                                FailurePoint::AfterSend
                            };
                            let mut retryable =
                                Retryable::new(point, ModelRetryCause::HttpStatus { status })
                                    .with_detail(error.message());
                            retryable.retry_after = retry_after;
                            retryable
                        });
                    (retryable, error)
                }
                Ok(ChatHttpResponseHead::Stream(request_id)) => {
                    attempt.observe_provider_accepted()?;
                    let interrupted = match stream_response(
                        response.into_body(),
                        operation,
                        hooks,
                        request_id,
                        Arc::clone(&clock),
                        Arc::clone(&ids),
                        cancellation,
                        attempt,
                    )
                    .await
                    {
                        StreamOutcome::Finished(result) => return result,
                        StreamOutcome::Interrupted(interrupted) => interrupted,
                    };
                    attempt.observe_physical_request_finished(
                        PhysicalRequestOutcome::AcceptedDisconnect,
                    )?;
                    let mut retryable = Retryable::new(
                        FailurePoint::MidStream,
                        ModelRetryCause::StreamInterrupted {
                            fault: interrupted.fault,
                        },
                    )
                    .with_detail(interrupted.detail);
                    retryable.output_discarded = interrupted.delivered;
                    retryable.loss = interrupted.loss;
                    retryable
                        .loss
                        .transport_phase
                        .get_or_insert(plakous_model::ModelRetryPhase::ResponseBody);
                    (Some(retryable), interrupted.error)
                }
            },
            Err(failure) => {
                fresh_connection = failure.cause() == Some("peer_rejected_record");
                let transmission = failure.transmission();
                let outcome = if transmission.peer_cannot_have_acted() {
                    PhysicalRequestOutcome::DefiniteRejection
                } else {
                    PhysicalRequestOutcome::AmbiguousPostSend
                };
                if exchanged.cancelled {
                    return finish_error(attempt, outcome, hooks.cancellation_error()).await;
                }
                let error = hooks
                    .classify_transport_failure(failure, None)
                    .with_exchange_break(ExchangeBreak::Transport);
                attempt.observe_physical_request_finished(outcome)?;
                let retryable = if failure.cause() == Some("peer_rejected_record") {
                    Some(Retryable::new(
                        FailurePoint::RecordRejectedInTransit,
                        ModelRetryCause::RecordRejectedInTransit,
                    ))
                } else if transmission == plakous_http::TransmissionFact::RequestIncomplete
                    || (transmission == plakous_http::TransmissionFact::NotStarted
                        && matches!(
                            failure.phase(),
                            plakous_http::Phase::Resolution
                                | plakous_http::Phase::Connect
                                | plakous_http::Phase::Tls
                        ))
                {
                    Some(Retryable::new(
                        if transmission == plakous_http::TransmissionFact::NotStarted {
                            FailurePoint::Unreachable
                        } else {
                            FailurePoint::Refused
                        },
                        ModelRetryCause::Connect,
                    ))
                } else {
                    transport_fault(failure).map(|fault| {
                        Retryable::new(
                            FailurePoint::AfterSend,
                            ModelRetryCause::TransportAfterSend { fault },
                        )
                    })
                };
                (
                    retryable.map(|retryable| {
                        retryable
                            .with_phase(failure.phase())
                            .with_detail(format!("{failure:?}"))
                    }),
                    error,
                )
            }
        };
        match retryable {
            Some(mut retryable) => {
                if retry_disabled {
                    retryable.loss.declined_reason = Some(Decline::RetryDisabled);
                }
                if wait_to_retry(
                    attempt,
                    hard_deadline,
                    retry,
                    retryable,
                    &mut waited,
                    &mut long_stream_replays,
                    &mut unreachable,
                    physical_started.elapsed(),
                    &request_log,
                )
                .await?
                {
                    continue;
                }
                return emit_error(attempt, error).await;
            }
            _ => return emit_error(attempt, error).await,
        }
    }
}

/// One failed physical request the turn retry policy may replay.
struct Retryable {
    point: FailurePoint,
    cause: ModelRetryCause,
    retry_after: Option<Duration>,
    output_discarded: bool,
    detail: String,
    loss: plakous_model::ModelRetryLoss,
}

impl Retryable {
    const fn new(point: FailurePoint, cause: ModelRetryCause) -> Self {
        Self {
            point,
            cause,
            retry_after: None,
            output_discarded: false,
            detail: String::new(),
            loss: plakous_model::ModelRetryLoss {
                assistant_text_bytes: 0,
                reasoning_bytes: 0,
                completed_items: 0,
                elapsed_ms: 0,
                declined_reason: None,
                server_effect: false,
                transport_phase: Some(match cause {
                    ModelRetryCause::Connect => plakous_model::ModelRetryPhase::Connect,
                    ModelRetryCause::StreamInterrupted { .. } => {
                        plakous_model::ModelRetryPhase::ResponseBody
                    }
                    _ => plakous_model::ModelRetryPhase::ResponseHead,
                }),
            },
        }
    }

    fn with_phase(mut self, phase: plakous_http::Phase) -> Self {
        self.loss.transport_phase = retry_phase(phase);
        self
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// The transport faults a turn may replay; cancellation, local limits, an
/// invalid response and internal failures are not transient.
const fn transport_fault(failure: HttpFailure) -> Option<ModelTransportFault> {
    match failure.kind() {
        plakous_http::ErrorKind::Io => Some(ModelTransportFault::Io),
        plakous_http::ErrorKind::Protocol => Some(ModelTransportFault::Protocol),
        plakous_http::ErrorKind::Tls => Some(ModelTransportFault::Tls),
        plakous_http::ErrorKind::DeadlineExceeded => Some(ModelTransportFault::Timeout),
        _ => None,
    }
}

/// Applies [`RetryPolicy::TURN`] to a failed generation: returns `true` after
/// announcing the retry as a typed event and waiting its delay, and `false`
/// when the attempt must end with the failure it has.
async fn wait_to_retry(
    attempt: &mut ChatAttempt,
    hard_deadline: Option<Instant>,
    turn_retry: bool,
    failure: Retryable,
    waited: &mut Duration,
    long_stream_replays: &mut plakous_attempt::LongStreamReplayBudget,
    unreachable: &mut plakous_attempt::UnreachableRideOut,
    elapsed: Duration,
    request: &(String, String, usize),
) -> Result<bool, ChatAttemptError> {
    let facts = attempt.terminal_facts();
    let delay = unreachable.next_delay(
        facts,
        failure.point,
        failure.retry_after,
        *waited,
        Jitter::random(),
    );
    let reason = retry_decline(
        &RetryDeclineInput {
            deadline_remaining: hard_deadline
                .map(|hard| hard.saturating_duration_since(Instant::now())),
            established: failure.loss.declined_reason,
            redispatch_available: true,
            turn_retry,
            cause: failure.cause,
            point: failure.point,
            facts,
            elapsed,
            delay,
        },
        long_stream_replays,
    );
    let phase = failure.loss.transport_phase;
    let unsent = failure.point == FailurePoint::Unreachable;
    attempt.record_failure(
        failure.point,
        phase.map(plakous_model::ModelRetryPhase::as_str),
        retry_cause_refusal(failure.cause),
        unsent,
        reason.map(Decline::as_str),
    );
    let delay = delay.unwrap_or_default();
    let retry = ModelRetry::new(
        facts.physical_attempts().saturating_add(1),
        unreachable.max_physical(facts, failure.point),
        if reason.is_some() {
            Duration::ZERO
        } else {
            delay
        },
        failure.cause,
        failure.output_discarded && reason.is_none(),
        failure.detail,
    )
    .with_loss(plakous_model::ModelRetryLoss {
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        server_effect: facts.replay_safety() == plakous_attempt::ReplaySafety::ServerEffect,
        declined_reason: reason,
        ..failure.loss
    })
    .with_request(&request.0, &request.1, request.2);
    if reason.is_some() {
        let _ = attempt.events_mut().send_retry(retry).await;
        return Ok(false);
    }
    attempt.events_mut().send_retry(retry).await?;
    let cancellation = attempt.context().cancellation().clone();
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(ChatAttemptError::Cancelled {
                facts: Some(attempt.terminal_facts()),
            });
        }
        () = tokio::time::sleep(delay) => {}
    }
    *waited = waited.saturating_add(delay);
    Ok(true)
}

/// How one accepted response stream ended.
#[allow(
    clippy::large_enum_variant,
    reason = "returned once per physical stream and matched immediately; never stored"
)]
enum StreamOutcome {
    /// The logical attempt ended, successfully or not.
    Finished(Result<ChatAttemptResponse, ChatAttemptError>),
    /// The stream broke transiently; the physical request is still open.
    Interrupted(Interrupted),
}

/// A transient break of an accepted response stream.
struct Interrupted {
    error: ChatAttemptProtocolError,
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

enum StreamBreak {
    /// Close the body, then end the attempt with this failure.
    Closing(ChatAttemptProtocolError),
    Failed(ChatAttemptError),
    Interrupted {
        error: ChatAttemptProtocolError,
        fault: ModelTransportFault,
        detail: String,
    },
}

impl From<ChatAttemptError> for StreamBreak {
    fn from(error: ChatAttemptError) -> Self {
        Self::Failed(error)
    }
}

/// A provider error naming a transient refusal mid-stream.
const fn stream_refusal_fault(error: &ChatAttemptProtocolError) -> Option<ModelTransportFault> {
    match error.refusal() {
        Some(plakous_attempt::ProviderRefusal::Overloaded) => Some(ModelTransportFault::Overloaded),
        Some(plakous_attempt::ProviderRefusal::RateLimit) => Some(ModelTransportFault::RateLimited),
        _ => None,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "stream owner receives exact exchange state"
)]
async fn stream_response<H: ChatHttpHooks>(
    mut body: ResponseBody,
    operation: &PreparedChatHttpOperation,
    hooks: &H,
    request_id: Option<ModelProviderRequestId>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    cancellation: HttpCancellationToken,
    attempt: &mut ChatAttempt,
) -> StreamOutcome {
    let mut progress = StreamProgress::default();
    let finished = stream_body(
        &mut body,
        operation,
        hooks,
        request_id,
        clock,
        ids,
        &cancellation,
        attempt,
        &mut progress,
    )
    .await;
    match finished {
        Ok(response) => StreamOutcome::Finished(Ok(response)),
        Err(StreamBreak::Failed(error)) => StreamOutcome::Finished(Err(error)),
        Err(StreamBreak::Closing(error)) => {
            let _ = body.close().await;
            StreamOutcome::Finished(
                finish_error(attempt, PhysicalRequestOutcome::AcceptedDisconnect, error).await,
            )
        }
        Err(StreamBreak::Interrupted {
            error,
            fault,
            detail,
        }) => {
            let _ = body.close().await;
            StreamOutcome::Interrupted(Interrupted {
                error,
                fault,
                detail,
                delivered: progress.delivered,
                loss: progress.loss,
            })
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "stream owner receives exact exchange state"
)]
async fn stream_body<H: ChatHttpHooks>(
    body: &mut ResponseBody,
    operation: &PreparedChatHttpOperation,
    hooks: &H,
    request_id: Option<ModelProviderRequestId>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    cancellation: &HttpCancellationToken,
    attempt: &mut ChatAttempt,
    progress: &mut StreamProgress,
) -> Result<ChatAttemptResponse, StreamBreak> {
    let mut projector =
        ChatStreamProjector::new(operation.stream().clone(), request_id.clone(), clock, ids);
    loop {
        let next = cancellable(
            body.next_chunk(),
            attempt.context().cancellation(),
            cancellation,
        )
        .await;
        if next.cancelled {
            return Err(StreamBreak::Closing(hooks.cancellation_error()));
        }
        match next.value {
            Some(Ok(chunk)) => {
                let pushed = projector.push(&chunk);
                let events = match pushed {
                    Ok(events) => events,
                    Err(error) => {
                        let usage_limit = error.usage_limit();
                        let error = hooks.classify_stream_error(error);
                        let error = if usage_limit {
                            error.with_usage_limit()
                        } else {
                            error
                        };
                        if let Some(fault) = stream_refusal_fault(&error) {
                            return Err(StreamBreak::Interrupted {
                                detail: error.message().to_owned(),
                                error,
                                fault,
                            });
                        }
                        // A refusal no retry rule replays still names the
                        // class the terminal is typed as.
                        if let Some(refusal) = error.refusal() {
                            attempt.record_failure(
                                FailurePoint::MidStream,
                                Some(plakous_model::ModelRetryPhase::ResponseBody.as_str()),
                                Some(refusal),
                                false,
                                None,
                            );
                        }
                        return Err(StreamBreak::Closing(error));
                    }
                };
                deliver(attempt, events, progress).await?;
            }
            Some(Err(failure)) => {
                progress.loss.transport_phase = retry_phase(failure.phase());
                let error = hooks
                    .classify_transport_failure(
                        failure,
                        request_id.as_ref().map(ModelProviderRequestId::as_str),
                    )
                    .with_exchange_break(ExchangeBreak::Transport);
                if let Some(fault) = transport_fault(failure) {
                    return Err(StreamBreak::Interrupted {
                        detail: format!("{failure:?}"),
                        error,
                        fault,
                    });
                }
                return finish_error(attempt, PhysicalRequestOutcome::AcceptedDisconnect, error)
                    .await
                    .map_err(StreamBreak::from);
            }
            None => break,
        }
    }
    // The body ended; a projector that cannot finish was cut short.
    let projection = match projector.finish() {
        Ok(projection) => projection,
        Err(error) => {
            let error = hooks
                .classify_stream_error(error)
                .with_exchange_break(ExchangeBreak::Transport);
            return Err(StreamBreak::Interrupted {
                detail: error.message().to_owned(),
                error,
                fault: ModelTransportFault::EndOfStream,
            });
        }
    };
    let sealed = ChatAttemptResponse::new(attempt, projection)?;
    attempt
        .events_mut()
        .send_terminal_response(sealed.response().clone())
        .await?;
    Ok(sealed)
}

async fn deliver(
    attempt: &mut ChatAttempt,
    events: Vec<ModelEvent>,
    progress: &mut StreamProgress,
) -> Result<(), ChatAttemptError> {
    for event in events {
        progress.loss.observe(&event);
        match event {
            ModelEvent::ResponseCompleted(_) => {
                return Err(protocol(
                    attempt,
                    ChatAttemptProtocolError::new("Chat projector emitted terminal authority"),
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

async fn finish_error<T>(
    attempt: &mut ChatAttempt,
    outcome: PhysicalRequestOutcome,
    source: ChatAttemptProtocolError,
) -> Result<T, ChatAttemptError> {
    attempt.observe_physical_request_finished(outcome)?;
    emit_error(attempt, source).await
}

/// Emits one terminal observation for an already-finished physical request.
async fn emit_error<T>(
    attempt: &mut ChatAttempt,
    source: ChatAttemptProtocolError,
) -> Result<T, ChatAttemptError> {
    let source = source.with_terminal_facts(attempt.terminal_facts());
    match attempt.events_mut().send_error(source.clone()).await {
        Ok(()) => Err(ChatAttemptError::protocol(source, attempt.terminal_facts())),
        Err(error) => Err(error),
    }
}

fn protocol(attempt: &ChatAttempt, source: ChatAttemptProtocolError) -> ChatAttemptError {
    ChatAttemptError::protocol(source, attempt.terminal_facts())
}

fn safety(attempt: &ChatAttempt) -> ChatHttpSafety {
    let facts = attempt.terminal_facts();
    ChatHttpSafety {
        provider_accepted: facts.provider_acceptance()
            != plakous_attempt::ProviderAcceptance::NotAccepted,
        visible_output: facts.visible_output(),
        hosted_effects_possible: facts.hosted_effects_possible(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "focused loopback fixtures use infallible constants and assertion extraction"
)]
mod tests {
    use super::*;

    use std::error::Error;
    use std::io;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::StreamExt;
    use plakous_attempt::LedgerPolicy;
    use plakous_http::{Bytes, HeaderMap, Limits, Method, Request, StatusCode, Version};
    use plakous_model::{
        AttemptId, CancellationToken, Digest, DigestDomain, EffectiveProviderConfigurationId,
        FixedClock, ModelCallContext, ModelId, ProtocolId, ReasoningPolicy,
        SelectedProviderConfigurationId, SequenceIdGenerator, Timestamp, Usage,
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use uuid::Uuid;

    use crate::{
        CHAT_FAMILY_LEDGER_POLICY, ChatAttemptEventReceiver, ChatReasoningReplay,
        ChatReplayTargetV1, ChatStreamLimits, ReasoningAliasPolicy, call_chat_operation,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    #[derive(Clone, Default)]
    struct FakeHooks {
        physical: Arc<Mutex<Option<ChatPhysicalRequestBinding>>>,
    }

    impl ChatHttpHooks for FakeHooks {
        type ErrorMetadata = ();

        fn build_request(
            &self,
            client: &Client,
            dial_origin: Origin,
            physical: &ChatPhysicalRequestBinding,
            operation: &PreparedChatHttpOperation,
        ) -> Result<HttpRequest, ChatAttemptProtocolError> {
            *self.physical.lock().expect("physical capture") = Some(physical.clone());
            Request::new(
                dial_origin,
                Method::POST,
                "/chat",
                HeaderMap::new(),
                Bytes::copy_from_slice(operation.body()),
                client.limits(),
            )
            .map_err(|_| ChatAttemptProtocolError::new("fake request construction failed"))
        }

        fn classify_response_head(
            &self,
            response: &Response,
        ) -> Result<ChatHttpResponseHead<Self::ErrorMetadata>, ChatAttemptProtocolError> {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Ok(ChatHttpResponseHead::Error(ChatHttpErrorHead {
                    metadata: (),
                    outcome: PhysicalRequestOutcome::DefiniteRejection,
                    provider_accepted: false,
                }));
            }
            if response.status() != StatusCode::OK || response.version() != Version::HTTP_11 {
                return Err(ChatAttemptProtocolError::new("fake response head mismatch"));
            }
            Ok(ChatHttpResponseHead::Stream(Some(
                ModelProviderRequestId::new("head-request-exact")
                    .map_err(|_| ChatAttemptProtocolError::new("fake request id failed"))?,
            )))
        }

        fn classify_error_response(
            &self,
            (): (),
            _body: &[u8],
            _safety: ChatHttpSafety,
        ) -> ChatAttemptProtocolError {
            ChatAttemptProtocolError::new("fake status refusal")
        }

        fn classify_transport_failure(
            &self,
            failure: HttpFailure,
            _request_id: Option<&str>,
        ) -> ChatAttemptProtocolError {
            ChatAttemptProtocolError::new(format!("fake transport failure: {:?}", failure.kind()))
        }

        fn cancellation_error(&self) -> ChatAttemptProtocolError {
            ChatAttemptProtocolError::new("fake cancellation")
        }

        fn classify_stream_error(&self, error: ChatStreamError) -> ChatAttemptProtocolError {
            ChatAttemptProtocolError::new(error.message())
        }
    }

    fn digest(domain: DigestDomain, label: &[u8]) -> TestResult<Digest> {
        Ok(Digest::sha256(domain, 1, &[label])?)
    }

    fn configuration_id<T>(domain: DigestDomain, label: &[u8]) -> TestResult<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let digest = Digest::sha256(domain, 5, &[label])?;
        Ok(serde_json::from_value(serde_json::to_value(digest)?)?)
    }

    fn fixture_operation(label: &[u8]) -> TestResult<PreparedChatHttpOperation> {
        let plan = digest(DigestDomain::RequestPlan, label)?;
        let protocol = ProtocolId::new("chat-completions.test")?;
        let model = ModelId::new("model-literal")?;
        let selected: SelectedProviderConfigurationId =
            configuration_id(DigestDomain::SelectedProviderConfiguration, b"selected")?;
        let effective: EffectiveProviderConfigurationId =
            configuration_id(DigestDomain::EffectiveProviderConfiguration, b"effective")?;
        let replay = ChatReplayTargetV1::new(
            protocol.clone(),
            model.clone(),
            digest(DigestDomain::ModelBinding, b"account")?,
            selected,
            effective,
        );
        let stream = PreparedChatStreamBinding::new(
            plan,
            protocol,
            model,
            replay,
            ReasoningPolicy::Disabled,
            ChatReasoningReplay::VisibleOnly,
            ReasoningAliasPolicy::Reasoning,
            ChatStreamLimits::default(),
        );
        let lowered_digest = digest(DigestDomain::ProviderOperation, label)?;
        let lowered = LoweredChatRequest::test_fixture(b"{}".to_vec(), lowered_digest);
        let binding = ChatOperationBinding::new(
            digest(DigestDomain::ModelBinding, label)?,
            plan,
            lowered_digest,
            lowered.replay_safety(),
        );
        Ok(PreparedChatHttpOperation::new(binding, lowered, stream))
    }

    fn fixture_attempt(
        operation: ChatOperationBinding,
    ) -> TestResult<(ChatAttempt, ChatAttemptEventReceiver)> {
        let context = ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(100))?,
            CancellationToken::new(),
            None,
        );
        Ok(ChatAttempt::with_event_channel(
            operation,
            context,
            NonZeroUsize::new(32).expect("nonzero"),
        ))
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(FixedClock::new(
            Timestamp::new(1_700_000_000, 0).expect("timestamp"),
        ))
    }

    fn ids() -> Arc<dyn IdGenerator> {
        Arc::new(SequenceIdGenerator::new((1_u128..=8).map(Uuid::from_u128)).expect("sequence ids"))
    }

    async fn loopback(response_body: Vec<u8>) -> TestResult<(Client, Origin)> {
        let (client, origin, _) = loopback_responses(vec![(200, response_body, false)]).await?;
        Ok((client, origin))
    }

    async fn loopback_responses(
        responses: Vec<(u16, Vec<u8>, bool)>,
    ) -> TestResult<(Client, Origin, Arc<AtomicUsize>)> {
        loopback_responses_with_hold(responses, None).await
    }

    async fn loopback_responses_with_hold(
        responses: Vec<(u16, Vec<u8>, bool)>,
        mut release: Option<tokio::sync::mpsc::Receiver<()>>,
    ) -> TestResult<(Client, Origin, Arc<AtomicUsize>)> {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
        let certificate = cert.der().clone();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate.to_vec()))?;
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let body_timeout = if release.is_some() {
            Duration::from_secs(600)
        } else {
            Limits::default().response_body_timeout
        };
        let dispatches = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&dispatches);
        tokio::spawn(async move {
            for (status, response_body, incomplete) in responses {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).await?;
                    if read == 0 {
                        return Err::<(), io::Error>(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "request ended before its body",
                        ));
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n")
                        && request.ends_with(b"{}")
                    {
                        break;
                    }
                }
                seen.fetch_add(1, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                    response_body.len() + usize::from(incomplete)
                );
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&response_body).await?;
                if let Some(release) = &mut release {
                    let _ = release.recv().await;
                }
                stream.shutdown().await?;
            }
            Ok(())
        });
        Ok((
            Client::with_tls_config(
                Limits {
                    response_body_timeout: body_timeout,
                    ..Limits::default()
                },
                client_config,
            )?,
            origin,
            dispatches,
        ))
    }

    fn successful_sse() -> Vec<u8> {
        let prefix = |choices: &str| {
            format!(
                "data: {{\"id\":\"response-exact\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":{choices}}}\n\n"
            )
        };
        [
            prefix(r#"[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]"#),
            prefix(r#"[{"index":0,"delta":{"content":"exact answer"},"finish_reason":null}]"#),
            prefix(r#"[{"index":0,"delta":{},"finish_reason":"stop"}]"#),
            String::from(
                "data: {\"id\":\"response-exact\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":4,\"total_tokens\":11,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
            ),
            String::from("data: [DONE]\n\n"),
        ]
        .concat()
        .into_bytes()
    }

    #[derive(Clone, Copy)]
    enum SinkFailure {
        Never,
        Chunk,
        Terminal,
    }

    struct CaptureObserver {
        required: bool,
        failure: SinkFailure,
    }

    impl plakous_http::ExchangeObserver for CaptureObserver {
        fn required(&self) -> bool {
            self.required
        }

        fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
            Some(Box::new(CaptureSink {
                failure: self.failure,
                failed: false,
            }))
        }
    }

    struct CaptureSink {
        failure: SinkFailure,
        failed: bool,
    }

    impl plakous_http::ExchangeObservation for CaptureSink {
        fn failed(&self) -> bool {
            self.failed
        }

        fn response_head(&mut self, _: u16) {}

        fn response_chunk(&mut self, _: &[u8]) {
            self.failed |= matches!(self.failure, SinkFailure::Chunk);
        }

        fn terminal(&mut self, _: &plakous_http::ExchangeTerminal) {
            self.failed |= matches!(self.failure, SinkFailure::Terminal);
        }
    }

    #[tokio::test]
    async fn a_long_chat_stream_failure_allows_only_one_replay() -> TestResult {
        let operation = fixture_operation(b"long-chat-cap")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let complete = String::from_utf8(successful_sse())?;
        let boundary = complete
            .match_indices("\n\n")
            .nth(1)
            .ok_or("SSE boundary")?
            .0
            + 2;
        let prefix = complete.as_bytes()[..boundary].to_vec();
        let (release, holds) = tokio::sync::mpsc::channel(1);
        let (client, origin, dispatches) = loopback_responses_with_hold(
            vec![(200, prefix.clone(), true), (200, prefix, true)],
            Some(holds),
        )
        .await?;
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        )
        .with_turn_retry();
        let execution =
            tokio::spawn(async move { call_chat_operation(&mut service, attempt).await });
        for physical in 1..=2 {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(5), receiver.next())
                    .await?
                    .ok_or("output channel closed")??;
                if matches!(event, ModelEvent::TextDelta(_)) {
                    break;
                }
            }
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(if physical == 1 { 301 } else { 1 })).await;
            tokio::time::resume();
            release.send(()).await?;
            let retry = loop {
                let event = tokio::time::timeout(Duration::from_secs(5), receiver.next())
                    .await?
                    .ok_or("retry channel closed")??;
                if let ModelEvent::Retry(retry) = event {
                    break retry;
                }
            };
            assert_eq!(retry.next_physical(), physical + 1);
            if physical == 1 {
                assert!(retry.loss().elapsed_ms > 300_000);
                assert!(retry.output_discarded());
                assert_eq!(retry.loss().declined_reason, None);
            } else {
                assert!(retry.loss().elapsed_ms < 300_000);
                assert!(!retry.output_discarded());
                assert_eq!(retry.delay(), Duration::ZERO);
                assert_eq!(retry.loss().declined_reason, Some(Decline::Budget));
            }
        }
        let error = tokio::time::timeout(Duration::from_secs(5), execution)
            .await??
            .expect_err("second long failure must be terminal");
        assert_eq!(
            error
                .facts()
                .ok_or("admitted failure facts")?
                .physical_attempts(),
            2
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn required_capture_failure_on_429_ends_without_retry() -> TestResult {
        // Exercise chunk failure, clean and truncated EOF terminal failure, and
        // early-close failure. The healthy response exposes any retry.
        for (failure, limit, incomplete) in [
            (SinkFailure::Chunk, 1024, false),
            (SinkFailure::Terminal, 1024, false),
            (SinkFailure::Terminal, 1024, true),
            (SinkFailure::Terminal, 1, false),
        ] {
            let operation = fixture_operation(b"capture-refusal")?;
            let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
            let (client, origin, dispatches) = loopback_responses(vec![
                (429, b"optional error body".to_vec(), incomplete),
                (200, successful_sse(), false),
            ])
            .await?;
            let client = client.with_observer(Some(Arc::new(CaptureObserver {
                required: true,
                failure,
            })));
            let mut service = ChatHttpAttemptService::new(
                client,
                origin,
                operation,
                FakeHooks::default(),
                clock(),
                ids(),
                Duration::from_secs(5),
                limit,
            )
            .with_turn_retry();
            let error = call_chat_operation(&mut service, attempt)
                .await
                .expect_err("required capture must abort a refused attempt");
            let ChatAttemptError::Protocol { source, facts } = error else {
                panic!("capture failure must use the terminal protocol path");
            };
            assert_eq!(source.message(), "fake transport failure: Internal");
            assert_eq!(source.exchange_break(), ExchangeBreak::Transport);
            assert_eq!(facts.physical_attempts(), 1);
            assert_eq!(dispatches.load(Ordering::SeqCst), 1);
            let observed = receiver
                .next()
                .await
                .expect("one terminal error, not a retry notice")
                .expect_err("capture error observation");
            assert_eq!(observed.message(), source.message());
            assert!(receiver.next().await.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn optional_429_body_faults_and_passive_capture_still_allow_retry() -> TestResult {
        for (required, failure, limit, incomplete) in [
            (true, SinkFailure::Never, 1024, false),
            (false, SinkFailure::Chunk, 1024, false),
            (false, SinkFailure::Terminal, 1, false),
            (true, SinkFailure::Never, 1, false),
            (true, SinkFailure::Never, 1024, true),
        ] {
            let operation = fixture_operation(b"capture-control")?;
            let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
            let (client, origin, dispatches) = loopback_responses(vec![
                (429, b"{malformed optional json".to_vec(), incomplete),
                (200, successful_sse(), false),
            ])
            .await?;
            let client =
                client.with_observer(Some(Arc::new(CaptureObserver { required, failure })));
            let mut service = ChatHttpAttemptService::new(
                client,
                origin,
                operation,
                FakeHooks::default(),
                clock(),
                ids(),
                Duration::from_secs(5),
                limit,
            )
            .with_turn_retry();
            let response = call_chat_operation(&mut service, attempt).await?;
            assert_eq!(response.facts().physical_attempts(), 2);
            assert_eq!(dispatches.load(Ordering::SeqCst), 2);
            let mut notices = 0;
            while let Some(event) = receiver.next().await {
                if matches!(event?, ModelEvent::Retry(_)) {
                    notices += 1;
                }
            }
            assert_eq!(notices, 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn record_rejected_resends_immediately_on_a_fresh_connection() -> TestResult {
        let operation = fixture_operation(b"record-rejected")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let (client, origin, dispatches) =
            loopback_responses(vec![(200, successful_sse(), false)]).await?;
        let client = client
            .pooled()
            .with_faults(Some(plakous_http::FaultPlan::parse(
                "fault=record-rejected,nth=1",
            )?));
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        )
        .with_turn_retry();
        let response = call_chat_operation(&mut service, attempt).await?;
        assert_eq!(response.facts().physical_attempts(), 2);
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        let mut retries = Vec::new();
        while let Some(event) = receiver.next().await {
            if let ModelEvent::Retry(retry) = event? {
                retries.push(retry);
            }
        }
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].cause().kind(), "record_rejected_in_transit");
        assert_eq!(
            retries[0].cause().to_string(),
            "request corrupted in transit (TLS record rejected)"
        );
        assert_eq!(retries[0].delay(), Duration::ZERO);
        assert_eq!(retries[0].request(), Some(("127.0.0.1", "/chat", 2)));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn chat_unreachable_ride_out_excludes_incomplete_requests() -> TestResult {
        for (fault, expected) in [("connect", 12), ("write-reset", 4), ("record-rejected", 4)] {
            let operation = fixture_operation(b"ride-out")?;
            let (attempt, _receiver) = fixture_attempt(operation.binding().clone())?;
            let client = Client::new(Limits::default())?.with_faults(Some(
                plakous_http::FaultPlan::parse(&format!("fault={fault}"))?,
            ));
            let mut service = ChatHttpAttemptService::new(
                client,
                Origin::parse("https://127.0.0.1:9")?,
                operation,
                FakeHooks::default(),
                clock(),
                ids(),
                Duration::from_secs(5),
                1024,
            )
            .with_turn_retry();
            let started = Instant::now();
            let error = call_chat_operation(&mut service, attempt)
                .await
                .expect_err("injected outage");
            assert_eq!(
                error.facts().ok_or("facts")?.physical_attempts(),
                expected,
                "{fault}"
            );
            assert_eq!(
                error.facts().ok_or("facts")?.summary().ride_out_physical,
                if fault == "connect" { expected } else { 0 }
            );
            assert!(started.elapsed() <= Duration::from_secs(240));
        }
        Ok(())
    }

    #[tokio::test]
    async fn loopback_success_seals_exact_head_metadata_before_one_terminal() -> TestResult {
        assert_eq!(
            CHAT_FAMILY_LEDGER_POLICY,
            LedgerPolicy {
                require_service_admitted_for_dispatch: true,
                require_active_request_always: false,
                dual_auth_slots: false,
                clear_service_on_terminal_success: true,
            }
        );
        let operation = fixture_operation(b"success")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let expected_physical = ChatPhysicalRequestBinding::new(attempt.binding().clone(), 1);
        let hooks = FakeHooks::default();
        let captured = Arc::clone(&hooks.physical);
        let (client, origin) = loopback(successful_sse()).await?;
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            hooks,
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        );
        let drain = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = receiver.next().await {
                events.push(event);
            }
            events
        });
        let response = call_chat_operation(&mut service, attempt).await?;
        let events = drain.await?;

        assert_eq!(
            captured.lock().expect("captured").as_ref(),
            Some(&expected_physical)
        );
        assert_eq!(response.request_id().as_str(), "head-request-exact");
        assert_eq!(
            response.response().provider_request_id(),
            Some("head-request-exact")
        );
        assert_eq!(
            response.response().actual_model().map(ModelId::as_str),
            Some("model-literal")
        );
        assert_eq!(response.response().usage(), Some(Usage::new(7, 4, 2, 3, 0)));
        assert_eq!(response.finish().wire(), "stop");
        assert!(response.facts().provider_accepted());
        assert_eq!(
            response.facts().retry_class(),
            plakous_attempt::RetryClass::Never
        );
        assert!(events.iter().all(Result::is_ok));
        let started = events
            .iter()
            .find_map(|event| match event {
                Ok(ModelEvent::ResponseStarted(started)) => Some(started),
                _ => None,
            })
            .expect("response started");
        assert_eq!(started.provider_request_id(), Some("head-request-exact"));
        assert_eq!(
            started.actual_model().map(ModelId::as_str),
            Some("model-literal")
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
                .count(),
            1
        );
        let terminal = events.last().expect("terminal").as_ref().expect("success");
        let ModelEvent::ResponseCompleted(terminal) = terminal else {
            panic!("last event must be terminal");
        };
        assert_eq!(terminal, response.response());
        assert_eq!(terminal.provider_request_id(), Some("head-request-exact"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_foreign_operation_before_building_a_physical_request() -> TestResult {
        let operation = fixture_operation(b"service")?;
        let foreign = fixture_operation(b"foreign")?;
        let (attempt, _receiver) = fixture_attempt(foreign.binding().clone())?;
        let hooks = FakeHooks::default();
        let captured = Arc::clone(&hooks.physical);
        let (client, origin) = loopback(successful_sse()).await?;
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            hooks,
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        );
        let error = call_chat_operation(&mut service, attempt)
            .await
            .expect_err("foreign binding");
        assert!(matches!(error, ChatAttemptError::Binding { .. }));
        assert!(captured.lock().expect("captured").is_none());
        assert_eq!(
            error.facts().expect("admitted facts").physical_attempts(),
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn accepted_malformed_stream_error_retains_outcome_unknown_facts() -> TestResult {
        let operation = fixture_operation(b"malformed")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let (client, origin) = loopback(b"data: {bad}\n\n".to_vec()).await?;
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        );

        let operation_error = call_chat_operation(&mut service, attempt)
            .await
            .expect_err("malformed accepted stream");
        assert_eq!(
            operation_error.provider_acceptance(),
            Some(plakous_attempt::ProviderAcceptance::Accepted)
        );
        let observation_error = receiver
            .next()
            .await
            .expect("terminal observation")
            .expect_err("protocol error observation");
        assert_eq!(
            observation_error.provider_acceptance(),
            Some(plakous_attempt::ProviderAcceptance::Accepted)
        );
        assert!(receiver.next().await.is_none());
        Ok(())
    }
}
