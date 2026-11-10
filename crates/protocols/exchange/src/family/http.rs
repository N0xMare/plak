//! One-exchange HTTP/SSE host for one protocol family.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

use crate::{
    RetryDeclineInput, cancellable, read_bounded_body, retry_cause_refusal, retry_decline,
    retry_phase, time_until,
};
use plakous_attempt::{ExchangeBreak, FailurePoint, Jitter, PhysicalRequestOutcome, RetryPolicy};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, ExchangeControl, Failure as HttpFailure,
    Origin, Request as HttpRequest, Response, ResponseBody, StatusCode,
};
use plakous_model::ModelRetryDeclineReason as Decline;
use plakous_model::{
    Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelResponse, ModelRetry,
    ModelRetryCause, ModelTransportFault,
};
use tower_service::Service;

use super::{
    FamilyAttempt, FamilyAttemptError, FamilyAttemptProtocolError, FamilyAttemptResponse,
    FamilyOperationBinding, FamilyWire,
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
/// turn request; the redispatch hook enables [`RetryPolicy::TURN`]. Both share its
/// physical budget.
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

    fn call(&mut self, mut attempt: FamilyAttempt<W>) -> Self::Future {
        let client = self.client.clone();
        let dial_origin = self.dial_origin.clone();
        let operation = self.operation.clone();
        let hooks = self.hooks.clone();
        let clock = Arc::clone(&self.clock);
        let ids = Arc::clone(&self.ids);
        let timeout = self.timeout;
        let max_error_body_bytes = self.max_error_body_bytes;
        let credential_rejected = self.credential_rejected;
        let redispatch = self.redispatch;
        let turn_rooted = self.turn_rooted;
        Box::pin(async move {
            attempt.ensure_service_admitted()?;
            if attempt.binding().operation() != operation.binding() {
                return Err(FamilyAttemptError::<W>::Binding {
                    facts: attempt.terminal_facts(),
                });
            }
            execute(
                &client,
                dial_origin,
                &operation,
                hooks,
                clock,
                ids,
                timeout,
                max_error_body_bytes,
                credential_rejected,
                redispatch,
                turn_rooted,
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
async fn execute<W: FamilyWire, H: FamilyHttpHooks<W>>(
    client: &Client,
    dial_origin: Origin,
    operation: &PreparedFamilyHttpOperation<W>,
    hooks: H,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    timeout: Duration,
    max_error_body_bytes: usize,
    credential_rejected: Option<FamilyCredentialRejectedFn<W, H>>,
    redispatch: Option<FamilyRedispatchFn<W, H>>,
    turn_rooted: bool,
    attempt: &mut FamilyAttempt<W>,
) -> Result<FamilyAttemptResponse<W>, FamilyAttemptError<W>> {
    let mut hooks = hooks;
    // The 401 renewal runs at most once. Every re-send, renewal or policy
    // retry, shares the policy's physical budget; each generation gets its own
    // exchange deadline so a backoff never eats the stream time of the attempt
    // that succeeds.
    let mut credential_retry = credential_rejected;
    let mut provider_deadline_replayed = false;
    let mut waited = Duration::ZERO;
    let mut long_stream_replays = plakous_attempt::LongStreamReplayBudget::default();
    let mut unreachable = plakous_attempt::UnreachableRideOut::default();
    let timeout = attempt
        .context()
        .dispatch_timeout()
        .map_or(timeout, |budget| timeout.min(budget));
    let mut next_dispatch_started = None;
    let mut fresh_connection = false;
    let hard_deadline = attempt
        .context()
        .deadline()
        .map(|deadline| Instant::now() + time_until(deadline, clock.now()));
    loop {
        let physical_started = next_dispatch_started.take().unwrap_or_else(Instant::now);
        if attempt.context().cancellation().is_cancelled() {
            return Err(FamilyAttemptError::<W>::Cancelled {
                facts: Some(attempt.terminal_facts()),
            });
        }
        let physical = attempt.observe_physical_request_started()?;
        let reserved = attempt.reserve_authorization()?;
        if physical != reserved {
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
        attempt.validate_authorization(&reserved)?;
        let request = match hooks.build_request(client, dial_origin.clone(), attempt, operation) {
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
        let response = match exchanged.value {
            Ok(response) => response,
            Err(failure) => {
                hooks.transport_failed(failure);
                fresh_connection = failure.cause() == Some("peer_rejected_record");
                let transmission = failure.transmission();
                let outcome = if transmission.peer_cannot_have_acted() {
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
                attempt.observe_physical_request_finished(outcome)?;
                let Some(retryable) = retryable else {
                    return emit_error(attempt, error).await;
                };
                let step = retry_step(
                    attempt,
                    RetryContext {
                        hooks: &hooks,
                        redispatch,
                        turn_rooted,
                        clock: &clock,
                        hard_deadline,
                        hook_budget: timeout,
                        request: &request_log,
                    },
                    retryable
                        .with_phase(failure.phase())
                        .with_detail(format!("{failure:?}")),
                    RetryProgress {
                        waited: &mut waited,
                        long_stream_replays: &mut long_stream_replays,
                        unreachable: &mut unreachable,
                    },
                    physical_started.elapsed(),
                )
                .await;
                hooks = match step {
                    RetryStep::Send(refreshed, started) => {
                        next_dispatch_started = Some(started);
                        refreshed
                    }
                    RetryStep::Cancelled => return Err(cancelled(attempt)),
                    RetryStep::End => return emit_error(attempt, error).await,
                };
                continue;
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(&plakous_http::HeaderName::from_static("retry-after"))
            .and_then(|value| value.to_str().ok())
            .and_then(plakous_attempt::retry_after_seconds);
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
                let outcome = head.outcome;
                let error = hooks.classify_error_response(head.metadata, &body, safety(attempt));
                let usage_limit = plakous_attempt::body_reports_usage_limit(&body);
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
                let step = if error.usage_limit()
                    || (retry_disabled && !W::STATUS_RETRY_DECLINE_NOTICE)
                {
                    RetryStep::End
                } else if status == StatusCode::UNAUTHORIZED
                    && refused
                    && !retry_disabled
                    && turn_rooted
                {
                    // The 401 generation is closed without emitting an
                    // observation: the caller sees one logical attempt.
                    match credential_retry.take() {
                        Some(renew) => {
                            renew_credential(renew, &hooks, attempt, &clock, timeout, &unreachable)
                                .await
                        }
                        None => RetryStep::End,
                    }
                } else if let Some(point) = RetryPolicy::status_failure_point(status.as_u16()) {
                    // A refusal status the provider did not prove refused is
                    // as ambiguous as any other failure after the send.
                    let point = if refused {
                        point
                    } else {
                        FailurePoint::AfterSend
                    };
                    let mut retryable = Retryable::new(
                        point,
                        ModelRetryCause::HttpStatus {
                            status: status.as_u16(),
                        },
                    )
                    .with_detail(error.message());
                    retryable.retry_after = error
                        .retry_after()
                        .map(|advice| Duration::from_millis(advice.as_millis()))
                        .or(retry_after);
                    if retry_disabled {
                        retryable.loss.declined_reason =
                            Some(plakous_model::ModelRetryDeclineReason::RetryDisabled);
                    }
                    retry_step(
                        attempt,
                        RetryContext {
                            hooks: &hooks,
                            redispatch,
                            turn_rooted,
                            clock: &clock,
                            hard_deadline,
                            hook_budget: timeout,
                            request: &request_log,
                        },
                        retryable,
                        RetryProgress {
                            waited: &mut waited,
                            long_stream_replays: &mut long_stream_replays,
                            unreachable: &mut unreachable,
                        },
                        physical_started.elapsed(),
                    )
                    .await
                } else {
                    RetryStep::End
                };
                hooks = match step {
                    RetryStep::Send(refreshed, started) => {
                        next_dispatch_started = Some(started);
                        refreshed
                    }
                    RetryStep::Cancelled => return Err(cancelled(attempt)),
                    RetryStep::End => return emit_error(attempt, error).await,
                };
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
                    &hooks,
                    request_id,
                    Arc::clone(&clock),
                    Arc::clone(&ids),
                    cancellation,
                    attempt,
                )
                .await
                {
                    StreamOutcome::<W>::Finished(result) => return result,
                    StreamOutcome::<W>::Interrupted(interrupted) => interrupted,
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
                if W::PROVIDER_DEADLINE_REPLAY_ONCE
                    && matches!(
                        interrupted
                            .error
                            .http_status()
                            .map(plakous_model::ModelHttpStatus::get),
                        Some(408 | 504)
                    )
                    && interrupted.error.refusal()
                        == Some(plakous_attempt::ProviderRefusal::Deadline)
                {
                    retryable.provider_refusal = Some(plakous_attempt::ProviderRefusal::Deadline);
                    if provider_deadline_replayed {
                        retryable.loss.declined_reason = Some(Decline::Budget);
                    }
                    provider_deadline_replayed = true;
                }
                if retry_disabled {
                    retryable.loss.declined_reason =
                        Some(plakous_model::ModelRetryDeclineReason::RetryDisabled);
                }
                retryable
                    .loss
                    .transport_phase
                    .get_or_insert(plakous_model::ModelRetryPhase::ResponseBody);
                let step = retry_step(
                    attempt,
                    RetryContext {
                        hooks: &hooks,
                        redispatch,
                        turn_rooted,
                        clock: &clock,
                        hard_deadline,
                        hook_budget: timeout,
                        request: &request_log,
                    },
                    retryable,
                    RetryProgress {
                        waited: &mut waited,
                        long_stream_replays: &mut long_stream_replays,
                        unreachable: &mut unreachable,
                    },
                    physical_started.elapsed(),
                )
                .await;
                hooks = match step {
                    RetryStep::Send(refreshed, started) => {
                        next_dispatch_started = Some(started);
                        refreshed
                    }
                    RetryStep::Cancelled => return Err(cancelled(attempt)),
                    RetryStep::End => return emit_error(attempt, interrupted.error).await,
                };
            }
        }
    }
}

/// What a refused generation leads to.
enum RetryStep<H> {
    /// Send the next generation with these (possibly re-authorized) hooks.
    Send(H, Instant),
    /// The caller cancelled while waiting.
    Cancelled,
    /// End the logical attempt with the refusal it has.
    End,
}

fn cancelled<W: FamilyWire>(attempt: &FamilyAttempt<W>) -> FamilyAttemptError<W> {
    FamilyAttemptError::<W>::Cancelled {
        facts: Some(attempt.terminal_facts()),
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
    provider_refusal: Option<plakous_attempt::ProviderRefusal>,
}

impl Retryable {
    const fn new(point: FailurePoint, cause: ModelRetryCause) -> Self {
        Self {
            point,
            cause,
            retry_after: None,
            output_discarded: false,
            detail: String::new(),
            provider_refusal: None,
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

struct RetryContext<'a, W: FamilyWire, H: FamilyHttpHooks<W>> {
    hooks: &'a H,
    redispatch: Option<FamilyRedispatchFn<W, H>>,
    turn_rooted: bool,
    clock: &'a Arc<dyn Clock>,
    hard_deadline: Option<Instant>,
    hook_budget: Duration,
    request: &'a (String, String, usize),
}

struct RetryProgress<'a> {
    waited: &'a mut Duration,
    long_stream_replays: &'a mut plakous_attempt::LongStreamReplayBudget,
    unreachable: &'a mut plakous_attempt::UnreachableRideOut,
}

/// Applies [`RetryPolicy::TURN`] to a failed generation: announces the retry
/// as a typed event, waits the policy's delay, then re-authorizes the next
/// generation. A workflow retries only on typed evidence that the request
/// was not fully sent; every resend requires a revalidation hook.
async fn retry_step<W: FamilyWire, H: FamilyHttpHooks<W>>(
    attempt: &mut FamilyAttempt<W>,
    context: RetryContext<'_, W, H>,
    failure: Retryable,
    progress: RetryProgress<'_>,
    elapsed: Duration,
) -> RetryStep<H> {
    let RetryContext {
        hooks,
        redispatch,
        turn_rooted,
        clock,
        hard_deadline,
        hook_budget,
        request,
    } = context;
    let RetryProgress {
        waited,
        long_stream_replays,
        unreachable,
    } = progress;
    let facts = attempt.terminal_facts();
    let advice_beyond_budget =
        unreachable.advice_beyond_budget(facts, failure.point, failure.retry_after, *waited);
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
            redispatch_available: redispatch.is_some(),
            turn_retry: turn_rooted,
            cause: failure.cause,
            point: failure.point,
            facts,
            elapsed,
            delay,
            advice_beyond_budget,
        },
        long_stream_replays,
    );
    let phase = failure.loss.transport_phase;
    let unsent = failure.point == FailurePoint::Unreachable;
    attempt.record_failure(
        failure.point,
        phase.map(plakous_model::ModelRetryPhase::as_str),
        failure
            .provider_refusal
            .or_else(|| retry_cause_refusal(failure.cause)),
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
        return RetryStep::End;
    }
    if attempt.events_mut().send_retry(retry).await.is_err() {
        return RetryStep::Cancelled;
    }
    let cancellation = attempt.context().cancellation().clone();
    tokio::select! {
        biased;
        () = cancellation.cancelled() => return RetryStep::Cancelled,
        () = tokio::time::sleep(delay) => {}
    }
    *waited = waited.saturating_add(delay);
    match redispatch {
        Some(redispatch) => run_hook(redispatch, hooks, attempt, clock, hook_budget).await,
        None => RetryStep::End,
    }
}

/// Forces one credential renewal after a 401, within the same physical budget.
async fn renew_credential<W: FamilyWire, H: FamilyHttpHooks<W>>(
    renew: FamilyCredentialRejectedFn<W, H>,
    hooks: &H,
    attempt: &FamilyAttempt<W>,
    clock: &Arc<dyn Clock>,
    hook_budget: Duration,
    unreachable: &plakous_attempt::UnreachableRideOut,
) -> RetryStep<H> {
    let facts = attempt.terminal_facts();
    if facts.failure_risk() != plakous_attempt::FailureRisk::None
        || facts.replay_safety() == plakous_attempt::ReplaySafety::ServerEffect
        || unreachable.ordinary_physical(facts) >= RetryPolicy::TURN.max_physical()
    {
        return RetryStep::End;
    }
    run_hook(renew, hooks, attempt, clock, hook_budget).await
}

/// Runs a re-authorization hook, bounded by one exchange budget and the
/// caller's deadline. The claimed-refresh store persists Unknown before any
/// provider I/O. If cancellation or the bound drops the hook, that durable
/// fence remains conservative; no detached refresh or late inference survives
/// the logical attempt.
async fn run_hook<W: FamilyWire, H: FamilyHttpHooks<W>>(
    hook: FamilyRedispatchFn<W, H>,
    hooks: &H,
    attempt: &FamilyAttempt<W>,
    clock: &Arc<dyn Clock>,
    budget: Duration,
) -> RetryStep<H> {
    let started = Instant::now();
    let cancellation = attempt.context().cancellation().clone();
    let bound = attempt.context().deadline().map_or(budget, |deadline| {
        time_until(deadline, clock.now()).min(budget)
    });
    if bound.is_zero() {
        return RetryStep::End;
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => RetryStep::Cancelled,
        () = tokio::time::sleep(bound) => RetryStep::End,
        result = hook(hooks) => match result {
            Ok(refreshed) if !cancellation.is_cancelled() => RetryStep::Send(refreshed, started),
            Ok(_) => RetryStep::Cancelled,
            Err(_) => RetryStep::End,
        },
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
        Some(plakous_attempt::ProviderRefusal::Overloaded) => Some(ModelTransportFault::Overloaded),
        Some(plakous_attempt::ProviderRefusal::RateLimit) => Some(ModelTransportFault::RateLimited),
        Some(plakous_attempt::ProviderRefusal::Deadline)
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
            != plakous_attempt::ProviderAcceptance::NotAccepted,
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
