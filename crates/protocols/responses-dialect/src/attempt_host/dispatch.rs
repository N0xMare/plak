//! Physical dispatch of one admitted attempt: preflight, send, body decode and stream collection.

use super::{
    Arc, Bytes, Client, Digest, DigestDomain, ExchangeControl, FailurePoint, FamilyCoreError,
    HeaderName, HttpCancellationToken, HttpErrorKind, HttpFailure, Instant, ModelProviderRequestId,
    ModelTransportFault, PhysicalRequestBinding, PhysicalRequestOutcome, Poll, ReplaySafety,
    ResponseBody, ResponsesAttempt, ResponsesAttemptDialectData, ResponsesAttemptError,
    ResponsesAttemptResponse, ResponsesAttemptTerminal, ResponsesAuthorizationAdapter,
    ResponsesContentPart, ResponsesDispatchRejection, ResponsesFamilySseDecoder,
    ResponsesFamilyStreamError, ResponsesFamilyStreamEvent, ResponsesHttpStatusClass,
    ResponsesObservation, ResponsesOutputItem, ResponsesPhysicalDispatchBinding,
    ResponsesPreflightFn, ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest,
    ResponsesProjectError, ResponsesProviderError, ResponsesProviderErrorClass,
    ResponsesProviderErrorPolicy, ResponsesProviderRequestBinding, ResponsesReplayProjection,
    ResponsesResponse, ResponsesResponseLimits, ResponsesRevalidationRequest, ResponsesStorageMode,
    ResponsesStreamDialectData, ResponsesStreamEventKind, ResponsesStreamLimits, RetryPolicy,
    StatusCode, TERMINAL_RESPONSE_DIGEST_REVISION, monotonic_now, mpsc,
    parse_provider_error_with_policy, physical_request_digest, poll_fn,
    project_terminal_response_with_model_binding, provider_error_object, refused_connection,
    request_body_digest, retry_after_seconds,
};

pub(super) fn admit_attempt(attempt: &ResponsesAttempt) -> Result<(), FamilyCoreError> {
    attempt.core.mark_admitted()?;
    attempt.core.mark_service_admitted()?;
    if attempt.binding.operation().replay_safety() == ReplaySafety::ServerEffect {
        attempt.core.declare_server_effect()?;
    }
    Ok(())
}

#[allow(
    clippy::result_large_err,
    reason = "the attempt error is returned once per attempt from a future that already holds it"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub(super) async fn execute_admitted<D, G>(
    client: Client,
    dialect: Arc<D>,
    data: ResponsesAttemptDialectData,
    authorization: ResponsesAuthorizationAdapter<D, G>,
    attempt: &mut ResponsesAttempt,
    retain: bool,
) -> Result<ResponsesAttemptResponse, ResponsesAttemptError>
where
    D: Send + Sync + 'static,
    G: Send + 'static,
{
    let Some(generation) = attempt
        .core
        .observation_snapshot()
        .physical_attempts
        .checked_add(1)
    else {
        return Err(fail_observation(attempt, FamilyCoreError::InvalidAdmission));
    };
    let expected = PhysicalRequestBinding::new(attempt.binding.clone(), generation);
    let request = ResponsesPreflightRequest {
        authority: &attempt.authority,
        operation: attempt.binding.operation(),
        physical: expected.clone(),
        body: &attempt.body,
        cancellation: &attempt.cancellation,
        deadline: attempt.deadline,
    };
    let prepared = run_preflight(dialect.as_ref(), authorization.preflight, request)
        .await
        .map_err(|error| {
            finish_before_dispatch(attempt);
            match error {
                PreflightFailure::Rejected(rejection) => ResponsesAttemptError::Preflight {
                    physical: attempt.physical(),
                    rejection,
                    facts: attempt.facts(),
                },
                PreflightFailure::Cancelled => ResponsesAttemptError::Cancelled {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                },
                PreflightFailure::Deadline => ResponsesAttemptError::Deadline {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                },
            }
        })?;
    if prepared.physical != expected
        || prepared.request.body().as_ref() != attempt.body.as_ref()
        || request_body_digest(prepared.request.body().as_ref()).ok()
            != Some(attempt.binding.operation().request_body())
    {
        finish_before_dispatch(attempt);
        return Err(ResponsesAttemptError::RequestBinding {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    ensure_running(attempt)?;
    let (route_digest, request_digest) = physical_request_digest(
        &expected,
        &prepared.credential_authority,
        &prepared.request,
        &prepared.header_bindings,
    )
    .map_err(|_| {
        finish_before_dispatch(attempt);
        ResponsesAttemptError::RequestBinding {
            physical: attempt.physical(),
            facts: attempt.facts(),
        }
    })?;
    let dispatch_binding = ResponsesPhysicalDispatchBinding {
        physical: expected,
        route: route_digest,
        credential_authority: prepared.credential_authority.clone(),
        auth_slots: data.auth_slots,
        request: request_digest,
    };
    attempt.physical = Some(dispatch_binding.clone());
    attempt
        .core
        .physical_request_started()
        .map_err(|source| fail_observation(attempt, source))?;
    let auth_mask = data.auth_slots.mask();
    let observed_generation = attempt
        .core
        .observations_ref()
        .reserve_auth(auth_mask)
        .map_err(FamilyCoreError::from)
        .map_err(|source| fail_definite_rejection(attempt, source))?;
    if observed_generation != generation {
        finish_definite_rejection(attempt);
        return Err(ResponsesAttemptError::RequestBinding {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    attempt
        .core
        .observations_ref()
        .validate_auth(generation, auth_mask)
        .map_err(FamilyCoreError::from)
        .map_err(|source| fail_definite_rejection(attempt, source))?;
    ensure_running_after_start(attempt)?;

    let ResponsesPreparedPhysicalRequest {
        request,
        mut response_head_guard,
        ..
    } = prepared;
    attempt.request_log = Some((
        request.origin().host().to_owned(),
        request.path_and_query().path().to_owned(),
        request.body().len(),
    ));
    let http_cancellation = HttpCancellationToken::new();
    let control = ExchangeControl::streaming(attempt.deadline, http_cancellation.clone())
        .with_fresh_connection(std::mem::take(&mut attempt.fresh_connection));
    let revalidation = ResponsesRevalidationRequest {
        authority: &attempt.authority,
        binding: &dispatch_binding,
        request: &request,
        cancellation: &attempt.cancellation,
        deadline: attempt.deadline,
        response_head_guard: &mut response_head_guard,
    };
    if let Err(rejection) = (authorization.revalidate)(&dialect, revalidation) {
        finish_definite_rejection(attempt);
        if attempt.cancellation.is_cancelled() {
            return Err(ResponsesAttemptError::Cancelled {
                physical: attempt.physical(),
                facts: attempt.facts(),
            });
        }
        if monotonic_now() >= attempt.deadline {
            return Err(ResponsesAttemptError::Deadline {
                physical: attempt.physical(),
                facts: attempt.facts(),
            });
        }
        return Err(ResponsesAttemptError::Revalidation {
            physical: attempt.physical(),
            rejection,
            facts: attempt.facts(),
        });
    }
    // The dialect's final function may irreversibly authorize the opaque
    // lease. The exact transport future is constructed and synchronously
    // polled once before cancellation/deadline can win. From this point a
    // cancelled or expired exchange is conservatively may-have-transmitted;
    // only a transport failure that reports its write never started (a
    // refused connection) is a definite rejection. A retry spends a fresh
    // preflight lease, never this one.
    let exchange = client.execute(request, control);
    tokio::pin!(exchange);
    let first_poll = poll_fn(|context| Poll::Ready(exchange.as_mut().poll(context))).await;
    let response = match first_poll {
        Poll::Ready(response) => response,
        Poll::Pending => tokio::select! {
            biased;
            () = attempt.cancellation.cancelled() => {
                http_cancellation.cancel();
                let _ = exchange.await;
                finish_after_authorization(attempt);
                return Err(ResponsesAttemptError::Cancelled {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                });
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(attempt.deadline)) => {
                http_cancellation.cancel();
                let _ = exchange.await;
                finish_after_authorization(attempt);
                return Err(ResponsesAttemptError::Deadline {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                });
            }
            response = &mut exchange => response,
        },
    };
    // The credential/authorization lease is deliberately retained until the
    // exact response-head outcome (including transport failure) is known.
    if let Err(failure) = &response
        && let Some(settle) = authorization.transport_failed
    {
        settle(&mut response_head_guard, *failure);
    }
    drop(response_head_guard);
    let response = match response {
        Ok(response) => response,
        Err(source) => {
            let outcome = if refused_connection(source) {
                PhysicalRequestOutcome::DefiniteRejection
            } else {
                PhysicalRequestOutcome::AmbiguousPostSend
            };
            retain_or_finish(attempt, outcome, retain)?;
            return Err(ResponsesAttemptError::Transport {
                physical: attempt.physical(),
                source,
                facts: attempt.facts(),
            });
        }
    };

    attempt.stream.retry_disabled = plakous_http::retry_disabled(response.headers());
    let status = response.status();
    if status != StatusCode::OK {
        let retry_disabled = plakous_http::retry_disabled(response.headers());
        let mut class = classify_status(status);
        let outcome = status_outcome(status);
        let retry_after = response
            .headers()
            .get(&HeaderName::from_static("retry-after"))
            .and_then(|value| value.to_str().ok())
            .and_then(retry_after_seconds);
        let mut body = response.into_body();
        let provider = read_error_body(
            &mut body,
            attempt,
            &http_cancellation,
            data.provider_error_policy,
            data.error_limits,
        )
        .await;
        let closed = body.close().await;
        // Required observation failures must not become retryable status refusals.
        let provider = match provider.and_then(|provider| match closed {
            Err(source) if source.kind() == HttpErrorKind::Internal => Err(source),
            _ => Ok(provider),
        }) {
            Ok(provider) => provider,
            Err(source) => {
                retain_or_finish(attempt, outcome, retain)?;
                return Err(ResponsesAttemptError::Transport {
                    physical: attempt.physical(),
                    source,
                    facts: attempt.facts(),
                });
            }
        };
        retain_or_finish(attempt, outcome, retain)?;
        if provider
            .as_ref()
            .is_some_and(|error| error.class() == ResponsesProviderErrorClass::Quota)
        {
            class = ResponsesHttpStatusClass::Quota;
        }
        let retry_disabled = retry_disabled
            || provider
                .as_ref()
                .is_some_and(|error| error.class() == ResponsesProviderErrorClass::Quota);
        // The status ends this request whether or not a retry follows; a
        // retry evaluation overwrites it with its decision. Exhausted quota
        // names no refusal class.
        attempt.core.observations_ref().record_failure(
            if outcome == PhysicalRequestOutcome::DefiniteRejection {
                FailurePoint::Refused
            } else {
                FailurePoint::AfterSend
            },
            Some(plakous_model::ModelRetryPhase::ResponseHead.as_str()),
            (class != ResponsesHttpStatusClass::Quota)
                .then(|| plakous_attempt::ProviderRefusal::from_http_status(status.as_u16()))
                .flatten(),
            false,
            None,
        );
        return Err(ResponsesAttemptError::HttpStatus {
            physical: attempt.physical(),
            status,
            class,
            provider,
            retry_after,
            retry_disabled,
            facts: attempt.facts(),
        });
    }

    attempt
        .core
        .observe_provider_accepted()
        .map_err(|source| fail_observation(attempt, source))?;
    let provider_request_id =
        response_request_id(response.headers(), data.provider_request_id_header.as_ref()).map_err(
            |()| {
                finish_physical(attempt, PhysicalRequestOutcome::AcceptedDisconnect);
                ResponsesAttemptError::ResponseMetadata {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                }
            },
        )?;
    if !is_event_stream(response.headers(), data.allow_missing_sse_content_type) {
        let _ = response.into_body().close().await;
        finish_physical(attempt, PhysicalRequestOutcome::AcceptedDisconnect);
        return Err(ResponsesAttemptError::ContentType {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    attempt
        .core
        .begin_decoding()
        .map_err(|source| fail_observation(attempt, source))?;
    attempt.stream.started = true;
    let mut body = Some(response.into_body());
    let result = decode_body(
        &mut body,
        &http_cancellation,
        data,
        dispatch_binding.request(),
        provider_request_id,
        attempt,
        retain,
    )
    .await;
    attempt.stream.observe_gap(monotonic_now());
    emit_activity(attempt, true);
    if result.is_err()
        && let Some(body) = body
    {
        let _ = body.close().await;
    }
    result
}

#[allow(
    clippy::result_large_err,
    reason = "the attempt error is returned once per attempt from a future that already holds it"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub(super) async fn decode_body(
    body_slot: &mut Option<ResponseBody>,
    http_cancellation: &HttpCancellationToken,
    data: ResponsesAttemptDialectData,
    physical_request: Digest,
    provider_request_id: Option<ModelProviderRequestId>,
    attempt: &mut ResponsesAttempt,
    retain: bool,
) -> Result<ResponsesAttemptResponse, ResponsesAttemptError> {
    let model_binding = data.stream.model_binding();
    let stop_at_typed_terminal = data.stream.typed_terminal_cutoff();
    let mut collector = StreamCollector::new(data.stream, data.stream_limits);
    let mut delivery = ObservationDelivery::new(attempt.observations.clone());
    let Some(body) = body_slot.as_mut() else {
        return Err(fail_observation(attempt, FamilyCoreError::InvalidAdmission));
    };
    loop {
        let deadline = match (attempt.stream.semantic_deadline(), attempt.hard_deadline) {
            (Some(semantic), Some(hard)) => Some(semantic.min(hard)),
            (semantic, hard) => semantic.or(hard),
        };
        match next_body_chunk(body, attempt, deadline, http_cancellation).await {
            BodyPull::Next(Some(Ok(bytes))) => {
                attempt.stream.observe_comments(&bytes);
                let events = collector
                    .push(&bytes)
                    .map_err(|failure| fail_decoded_stream(attempt, failure, retain))?;
                deliver(attempt, &mut delivery, events);
                if stop_at_typed_terminal && collector.terminal.is_some() {
                    break;
                }
            }
            BodyPull::Next(Some(Err(source))) => {
                return Err(match source.kind() {
                    HttpErrorKind::Cancelled => {
                        let _ = attempt.core.claim_and_close_protocol_failure();
                        ResponsesAttemptError::Cancelled {
                            physical: attempt.physical(),
                            facts: attempt.facts(),
                        }
                    }
                    HttpErrorKind::DeadlineExceeded => {
                        close_interrupted_stream(attempt, retain);
                        ResponsesAttemptError::Deadline {
                            physical: attempt.physical(),
                            facts: attempt.facts(),
                        }
                    }
                    _ => {
                        close_interrupted_stream(attempt, retain);
                        ResponsesAttemptError::Transport {
                            physical: attempt.physical(),
                            source,
                            facts: attempt.facts(),
                        }
                    }
                });
            }
            BodyPull::Next(None) => break,
            BodyPull::Cancelled => {
                let _ = attempt.core.claim_and_close_protocol_failure();
                return Err(ResponsesAttemptError::Cancelled {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                });
            }
            BodyPull::Deadline => {
                close_interrupted_stream(attempt, retain);
                return Err(ResponsesAttemptError::Deadline {
                    physical: attempt.physical(),
                    facts: attempt.facts(),
                });
            }
        }
    }
    let (residual, terminal) = collector
        .finish()
        .map_err(|failure| fail_decoded_stream(attempt, failure, retain))?;
    deliver(attempt, &mut delivery, residual);
    if model_binding == plakous_responses::ResponseModelBinding::ExactRequest
        && terminal
            .response_model()
            .is_some_and(|model| model != attempt.authority.artifact_binding().model().as_str())
    {
        return Err(fail_stream(
            attempt,
            CollectorFailure::Stream(ResponsesFamilyStreamError::InvalidField("response.model")),
        ));
    }
    let refused_in_band = if retain {
        in_band_refusal(&terminal)
    } else {
        None
    };
    // Observation may name protocol completion only after aggregate and model
    // binding validation. Other early closes retain incomplete evidence.
    if stop_at_typed_terminal
        && let Some(body) = body_slot.take()
        && let Err(source) = body.close_after_protocol_terminal().await
    {
        let _ = attempt.core.claim_and_close_protocol_failure();
        return Err(ResponsesAttemptError::Transport {
            physical: attempt.physical(),
            source,
            facts: attempt.facts(),
        });
    }
    if refused_in_band.is_none() {
        attempt
            .core
            .mark_decoded()
            .map_err(|source| fail_observation(attempt, source))?;
    }
    let terminal_bytes = serde_json::to_vec(terminal.native())
        .map_err(|_| fail_observation(attempt, FamilyCoreError::InvalidAdmission))?;
    let terminal_response = Digest::sha256(
        DigestDomain::ProviderOperation,
        TERMINAL_RESPONSE_DIGEST_REVISION,
        &[b"responses-terminal-native-json-v2", &terminal_bytes],
    )
    .map_err(|_| fail_observation(attempt, FamilyCoreError::InvalidAdmission))?;
    let provider_request = ResponsesProviderRequestBinding::new(
        physical_request,
        terminal_response,
        provider_request_id,
    )
    .map_err(|source| {
        finish_physical(attempt, PhysicalRequestOutcome::AcceptedDisconnect);
        ResponsesAttemptError::Projection {
            physical: attempt.physical(),
            source,
            facts: attempt.facts(),
        }
    })?;
    let terminal = project_terminal(
        terminal,
        &data.replay,
        data.storage,
        provider_request,
        model_binding,
    )
    .map_err(|source| {
        finish_physical(attempt, PhysicalRequestOutcome::AcceptedDisconnect);
        ResponsesAttemptError::Projection {
            physical: attempt.physical(),
            source,
            facts: attempt.facts(),
        }
    })?;
    if terminal.is_completed() {
        attempt
            .core
            .terminal_success()
            .map_err(|source| fail_observation(attempt, source))?;
        attempt
            .core
            .mark_terminal()
            .map_err(|source| fail_observation(attempt, source))?;
    } else if let Some(fault) = refused_in_band {
        // The retry loop replays it, or ends it with this terminal.
        close_interrupted_stream(attempt, true);
        if attempt.stream.interrupted {
            attempt.stream.refused_in_band = Some(fault);
        }
    } else {
        finish_physical(attempt, PhysicalRequestOutcome::AcceptedDisconnect);
    }
    let Some(physical) = attempt.physical.clone() else {
        return Err(fail_observation(attempt, FamilyCoreError::InvalidAdmission));
    };
    Ok(ResponsesAttemptResponse {
        binding: attempt.binding.clone(),
        physical,
        terminal,
        facts: attempt.facts(),
        observations_delivered: delivery.observations_delivered,
        observations_dropped: delivery.observations_dropped,
    })
}

pub(super) enum PreflightFailure {
    Rejected(ResponsesDispatchRejection),
    Cancelled,
    Deadline,
}

pub(super) async fn run_preflight<'a, D, G>(
    dialect: &'a D,
    preflight: ResponsesPreflightFn<D, G>,
    request: ResponsesPreflightRequest<'a>,
) -> Result<ResponsesPreparedPhysicalRequest<G>, PreflightFailure>
where
    D: Send + Sync + 'static,
    G: Send + 'static,
{
    let cancellation = request.cancellation.clone();
    let deadline = request.deadline;
    if cancellation.is_cancelled() {
        return Err(PreflightFailure::Cancelled);
    }
    if monotonic_now() >= deadline {
        return Err(PreflightFailure::Deadline);
    }
    let future = preflight(dialect, request);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(PreflightFailure::Cancelled),
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            Err(PreflightFailure::Deadline)
        }
        result = future => result.map_err(PreflightFailure::Rejected),
    }
}

pub(super) struct StreamCollector {
    decoder: ResponsesFamilySseDecoder,
    terminal: Option<RawTerminal>,
}

impl StreamCollector {
    pub(super) fn new(data: ResponsesStreamDialectData, limits: ResponsesStreamLimits) -> Self {
        Self {
            decoder: ResponsesFamilySseDecoder::new(data, limits),
            terminal: None,
        }
    }

    pub(super) fn push(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, CollectorFailure> {
        let events = self.decoder.push(bytes).map_err(CollectorFailure::Stream)?;
        self.accept(events)
    }

    pub(super) fn finish(
        &mut self,
    ) -> Result<(Vec<ResponsesFamilyStreamEvent>, RawTerminal), CollectorFailure> {
        let residual = self.decoder.finish().map_err(CollectorFailure::Stream)?;
        let observations = self.accept(residual)?;
        let terminal = self.terminal.take().ok_or(CollectorFailure::Stream(
            ResponsesFamilyStreamError::MissingTerminal,
        ))?;
        Ok((observations, terminal))
    }

    fn accept(
        &mut self,
        events: Vec<ResponsesFamilyStreamEvent>,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, CollectorFailure> {
        let mut observations = Vec::with_capacity(events.len());
        for event in events {
            let terminal = match event {
                ResponsesStreamEventKind::ResponseCompleted(response) => {
                    Some(RawTerminal::Completed(response))
                }
                ResponsesStreamEventKind::ResponseIncomplete(response) => {
                    Some(RawTerminal::Incomplete(response))
                }
                ResponsesStreamEventKind::ResponseFailed(response) => {
                    Some(RawTerminal::Failed(response))
                }
                ResponsesStreamEventKind::Error(error) => Some(RawTerminal::ProviderError(error)),
                other => {
                    observations.push(other);
                    None
                }
            };
            if let Some(terminal) = terminal
                && self.terminal.replace(terminal).is_some()
            {
                return Err(CollectorFailure::MultipleTerminals);
            }
        }
        Ok(observations)
    }
}

pub(super) struct ObservationDelivery {
    sender: Option<mpsc::Sender<ResponsesObservation>>,
    pub(super) observations_delivered: usize,
    pub(super) observations_dropped: usize,
    visible_observation: bool,
}

impl ObservationDelivery {
    pub(super) const fn new(sender: Option<mpsc::Sender<ResponsesObservation>>) -> Self {
        Self {
            sender,
            observations_delivered: 0,
            observations_dropped: 0,
            visible_observation: false,
        }
    }

    pub(super) fn deliver(&mut self, events: Vec<ResponsesFamilyStreamEvent>) {
        for event in events {
            self.visible_observation |= event_is_visible(&event);
            let Some(sender) = self.sender.clone() else {
                self.observations_dropped = self.observations_dropped.saturating_add(1);
                continue;
            };
            match sender.try_send(ResponsesObservation::Event(event)) {
                Ok(()) => {
                    self.observations_delivered = self.observations_delivered.saturating_add(1);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.observations_dropped = self.observations_dropped.saturating_add(1);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.observations_dropped = self.observations_dropped.saturating_add(1);
                    self.sender = None;
                }
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum CollectorFailure {
    Stream(ResponsesFamilyStreamError),
    MultipleTerminals,
}

pub(super) enum RawTerminal {
    Completed(ResponsesResponse),
    Incomplete(ResponsesResponse),
    Failed(ResponsesResponse),
    ProviderError(ResponsesProviderError),
}

impl RawTerminal {
    fn response_model(&self) -> Option<&str> {
        match self {
            Self::Completed(response) | Self::Incomplete(response) | Self::Failed(response) => {
                Some(response.model())
            }
            Self::ProviderError(_) => None,
        }
    }

    pub(super) fn native(&self) -> &serde_json::Value {
        match self {
            Self::Completed(response) | Self::Incomplete(response) | Self::Failed(response) => {
                response.native()
            }
            Self::ProviderError(error) => error.native(),
        }
    }
}

pub(super) fn project_terminal(
    terminal: RawTerminal,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_request: ResponsesProviderRequestBinding,
    model_binding: plakous_responses::ResponseModelBinding,
) -> Result<ResponsesAttemptTerminal, ResponsesProjectError> {
    Ok(match terminal {
        RawTerminal::Completed(response) => {
            ResponsesAttemptTerminal::Completed(project_terminal_response_with_model_binding(
                response,
                replay,
                storage,
                provider_request,
                model_binding,
            )?)
        }
        RawTerminal::Incomplete(response) => {
            ResponsesAttemptTerminal::Incomplete(project_terminal_response_with_model_binding(
                response,
                replay,
                storage,
                provider_request,
                model_binding,
            )?)
        }
        RawTerminal::Failed(response) => {
            ResponsesAttemptTerminal::Failed(project_terminal_response_with_model_binding(
                response,
                replay,
                storage,
                provider_request,
                model_binding,
            )?)
        }
        RawTerminal::ProviderError(error) => ResponsesAttemptTerminal::ProviderError(error),
    })
}

/// An accepted stream that ended in the provider's own rate-limit or overload
/// refusal. Whether replaying it can only cost inference is the retry
/// policy's decision.
pub(super) fn in_band_refusal(terminal: &RawTerminal) -> Option<ModelTransportFault> {
    let error = match terminal {
        RawTerminal::Failed(response) => response.error()?,
        RawTerminal::ProviderError(error) => error,
        RawTerminal::Completed(_) | RawTerminal::Incomplete(_) => return None,
    };
    if error.class() == ResponsesProviderErrorClass::RateLimited {
        return Some(ModelTransportFault::RateLimited);
    }
    (error.code() == Some("server_is_overloaded")).then_some(ModelTransportFault::Overloaded)
}

/// Delivers one batch of events and records what the stream has shown.
pub(super) fn deliver(
    attempt: &mut ResponsesAttempt,
    delivery: &mut ObservationDelivery,
    events: Vec<ResponsesFamilyStreamEvent>,
) {
    let now = monotonic_now();
    attempt.stream.observe_gap(now);
    for event in &events {
        attempt.stream.observe_semantic_event(event, now);
        match event {
            ResponsesStreamEventKind::OutputTextDelta { delta, .. }
            | ResponsesStreamEventKind::RefusalDelta { delta, .. } => {
                attempt.stream.loss.assistant_text_bytes = attempt
                    .stream
                    .loss
                    .assistant_text_bytes
                    .saturating_add(delta.len() as u64);
            }
            ResponsesStreamEventKind::ReasoningTextDelta { delta, .. }
            | ResponsesStreamEventKind::ReasoningSummaryTextDelta { delta, .. } => {
                attempt.stream.loss.reasoning_bytes = attempt
                    .stream
                    .loss
                    .reasoning_bytes
                    .saturating_add(delta.len() as u64);
            }
            ResponsesStreamEventKind::OutputItemDone { .. } => {
                attempt.stream.loss.completed_items =
                    attempt.stream.loss.completed_items.saturating_add(1);
            }
            _ => {}
        }
    }
    attempt.stream.delivered |= !events.is_empty();
    delivery.deliver(events);
    emit_activity(attempt, false);
    attempt
        .core
        .observe_after_emit(delivery.visible_observation);
}

pub(super) fn emit_activity(attempt: &mut ResponsesAttempt, force: bool) {
    if let Some(activity) = attempt.stream.activity_to_emit(monotonic_now(), force)
        && let Some(observations) = &attempt.observations
    {
        let _ = observations.try_send(ResponsesObservation::Activity(activity));
    }
}

/// Closes a generation whose stream broke. A logical attempt that may retry
/// stays open for the replay; the retry loop fails it if it ends.
pub(super) fn close_interrupted_stream(attempt: &mut ResponsesAttempt, retain: bool) {
    attempt.stream.observe_gap(monotonic_now());
    if !retain {
        let _ = attempt.core.claim_and_close_protocol_failure();
    } else if attempt.core.interrupt_stream().is_ok() {
        attempt.stream.interrupted = true;
    } else {
        attempt.core.mark_failed();
    }
}

/// A stream that ended early is interrupted; any other stream failure is a
/// protocol failure.
pub(super) fn fail_decoded_stream(
    attempt: &mut ResponsesAttempt,
    failure: CollectorFailure,
    retain: bool,
) -> ResponsesAttemptError {
    let CollectorFailure::Stream(
        source @ (ResponsesFamilyStreamError::Truncated
        | ResponsesFamilyStreamError::MissingTerminal),
    ) = failure
    else {
        return fail_stream(attempt, failure);
    };
    close_interrupted_stream(attempt, retain);
    ResponsesAttemptError::Stream {
        physical: attempt.physical(),
        source,
        facts: attempt.facts(),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned value matches the dialect hook and caller ownership"
)]
pub(super) fn fail_stream(
    attempt: &mut ResponsesAttempt,
    failure: CollectorFailure,
) -> ResponsesAttemptError {
    let _ = attempt.core.claim_and_close_protocol_failure();
    match failure {
        CollectorFailure::Stream(source) => ResponsesAttemptError::Stream {
            physical: attempt.physical(),
            source,
            facts: attempt.facts(),
        },
        CollectorFailure::MultipleTerminals => ResponsesAttemptError::MultipleTerminals {
            physical: attempt.physical(),
            facts: attempt.facts(),
        },
    }
}

pub(super) fn fail_observation(
    attempt: &mut ResponsesAttempt,
    source: FamilyCoreError,
) -> ResponsesAttemptError {
    attempt.core.mark_failed();
    ResponsesAttemptError::Observation {
        physical: attempt.physical(),
        source,
        facts: attempt.facts(),
    }
}

pub(super) fn fail_definite_rejection(
    attempt: &mut ResponsesAttempt,
    source: FamilyCoreError,
) -> ResponsesAttemptError {
    finish_definite_rejection(attempt);
    ResponsesAttemptError::Observation {
        physical: attempt.physical(),
        source,
        facts: attempt.facts(),
    }
}

pub(super) fn finish_before_dispatch(attempt: &mut ResponsesAttempt) {
    attempt.core.mark_failed();
}

pub(super) fn finish_definite_rejection(attempt: &mut ResponsesAttempt) {
    finish_physical(attempt, PhysicalRequestOutcome::DefiniteRejection);
}

pub(super) fn finish_after_authorization(attempt: &mut ResponsesAttempt) {
    finish_physical(attempt, PhysicalRequestOutcome::AmbiguousPostSend);
}

/// Closes a failed generation. When the logical attempt may still retry it
/// stays open; the retry loop fails it if it ends.
#[allow(
    clippy::result_large_err,
    reason = "closed attempt errors retain exact physical dispatch and terminal facts"
)]
pub(super) fn retain_or_finish(
    attempt: &mut ResponsesAttempt,
    outcome: PhysicalRequestOutcome,
    retain: bool,
) -> Result<(), ResponsesAttemptError> {
    if !retain {
        finish_physical(attempt, outcome);
        return Ok(());
    }
    attempt
        .core
        .physical_request_finished(outcome)
        .map_err(|source| fail_observation(attempt, source))
}

pub(super) fn finish_physical(attempt: &mut ResponsesAttempt, outcome: PhysicalRequestOutcome) {
    let _ = attempt.core.physical_request_finished(outcome);
    attempt.core.mark_failed();
}

#[allow(
    clippy::result_large_err,
    reason = "closed attempt errors retain exact physical dispatch and terminal facts"
)]
pub(super) fn ensure_running(attempt: &mut ResponsesAttempt) -> Result<(), ResponsesAttemptError> {
    if attempt.cancellation.is_cancelled() {
        finish_before_dispatch(attempt);
        return Err(ResponsesAttemptError::Cancelled {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    if monotonic_now() >= attempt.deadline {
        finish_before_dispatch(attempt);
        return Err(ResponsesAttemptError::Deadline {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    Ok(())
}

#[allow(
    clippy::result_large_err,
    reason = "closed attempt errors retain exact physical dispatch and terminal facts"
)]
pub(super) fn ensure_running_after_start(
    attempt: &mut ResponsesAttempt,
) -> Result<(), ResponsesAttemptError> {
    if attempt.cancellation.is_cancelled() {
        finish_definite_rejection(attempt);
        return Err(ResponsesAttemptError::Cancelled {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    if monotonic_now() >= attempt.deadline {
        finish_definite_rejection(attempt);
        return Err(ResponsesAttemptError::Deadline {
            physical: attempt.physical(),
            facts: attempt.facts(),
        });
    }
    Ok(())
}

/// Pulls one body chunk under the logical attempt's cancellation. A success
/// stream passes no `deadline`: past its response head it is bounded only by
/// the transport's per-pull idle bound. A bounded error body keeps the
/// attempt deadline.
pub(super) async fn next_body_chunk(
    body: &mut ResponseBody,
    attempt: &ResponsesAttempt,
    deadline: Option<Instant>,
    http_cancellation: &HttpCancellationToken,
) -> BodyPull {
    let mut pull = Box::pin(body.next_chunk());
    let expired = async {
        match deadline {
            Some(deadline) => {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = attempt.cancellation.cancelled() => {
            http_cancellation.cancel();
            let _ = pull.await;
            BodyPull::Cancelled
        }
        () = expired => {
            http_cancellation.cancel();
            let _ = pull.await;
            BodyPull::Deadline
        }
        result = &mut pull => BodyPull::Next(result),
    }
}

pub(super) enum BodyPull {
    Next(Option<Result<Bytes, HttpFailure>>),
    Cancelled,
    Deadline,
}

pub(super) async fn read_error_body(
    body: &mut ResponseBody,
    attempt: &ResponsesAttempt,
    http_cancellation: &HttpCancellationToken,
    policy: ResponsesProviderErrorPolicy,
    limits: ResponsesResponseLimits,
) -> Result<Option<ResponsesProviderError>, HttpFailure> {
    let mut bytes = Vec::new();
    loop {
        match next_body_chunk(body, attempt, Some(attempt.deadline), http_cancellation).await {
            BodyPull::Next(Some(Ok(chunk))) => bytes.extend_from_slice(&chunk),
            BodyPull::Next(Some(Err(source))) if source.kind() == HttpErrorKind::Internal => {
                return Err(source);
            }
            BodyPull::Next(Some(Err(_))) | BodyPull::Cancelled | BodyPull::Deadline => {
                return Ok(None);
            }
            BodyPull::Next(None) => break,
        }
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
    Ok(parse_provider_error_with_policy(provider_error_object(&value), limits, policy).ok())
}

pub(super) fn classify_status(status: StatusCode) -> ResponsesHttpStatusClass {
    match status {
        StatusCode::UNAUTHORIZED => ResponsesHttpStatusClass::Authentication,
        StatusCode::FORBIDDEN => ResponsesHttpStatusClass::Authorization,
        StatusCode::TOO_MANY_REQUESTS => ResponsesHttpStatusClass::RateLimited,
        StatusCode::PAYMENT_REQUIRED => ResponsesHttpStatusClass::Quota,
        StatusCode::PAYLOAD_TOO_LARGE => ResponsesHttpStatusClass::ContextLimit,
        status if status.is_client_error() => ResponsesHttpStatusClass::Rejected,
        status if status.is_server_error() => ResponsesHttpStatusClass::Server,
        _ => ResponsesHttpStatusClass::Unknown,
    }
}

/// A `401` is the provider's authentication rejection, and `429`, `503`
/// and `529` refuse the request before acceptance; every other status may
/// follow work the provider started.
pub(super) fn status_outcome(status: StatusCode) -> PhysicalRequestOutcome {
    if status == StatusCode::UNAUTHORIZED
        || RetryPolicy::status_failure_point(status.as_u16()) == Some(FailurePoint::Refused)
    {
        PhysicalRequestOutcome::DefiniteRejection
    } else {
        PhysicalRequestOutcome::AmbiguousPostSend
    }
}

pub(super) fn response_request_id(
    headers: &plakous_http::Headers,
    name: Option<&HeaderName>,
) -> Result<Option<ModelProviderRequestId>, ()> {
    let Some(name) = name else {
        return Ok(None);
    };
    let mut values = headers
        .iter()
        .filter_map(|(candidate, value)| (candidate == name).then_some(value));
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    ModelProviderRequestId::new(value).map(Some).map_err(|_| ())
}

pub(super) fn is_event_stream(headers: &plakous_http::Headers, allow_missing: bool) -> bool {
    headers
        .get(&HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .map_or(allow_missing, |value| {
            value
                .split(';')
                .next()
                .map(str::trim)
                .is_some_and(|media| media.eq_ignore_ascii_case("text/event-stream"))
        })
}

pub(super) fn event_starts_visible_output(event: &ResponsesFamilyStreamEvent) -> bool {
    match event {
        ResponsesStreamEventKind::OutputItemAdded { item, .. }
        | ResponsesStreamEventKind::OutputItemDone { item, .. } => match item {
            ResponsesOutputItem::FunctionCall { .. }
            | ResponsesOutputItem::CustomToolCall { .. } => true,
            ResponsesOutputItem::Message { content, .. } => content.iter().any(|part| {
                matches!(
                    part,
                    ResponsesContentPart::OutputText { .. } | ResponsesContentPart::Refusal { .. }
                )
            }),
            _ => false,
        },
        ResponsesStreamEventKind::ContentPartAdded { part, .. }
        | ResponsesStreamEventKind::ContentPartDone { part, .. } => matches!(
            part.get("type").and_then(serde_json::Value::as_str),
            Some("output_text" | "refusal")
        ),
        ResponsesStreamEventKind::OutputTextDelta { .. }
        | ResponsesStreamEventKind::RefusalDelta { .. }
        | ResponsesStreamEventKind::FunctionCallArgumentsDelta { .. }
        | ResponsesStreamEventKind::CustomToolCallInputDelta { .. } => true,
        _ => false,
    }
}

pub(super) fn event_is_visible(event: &ResponsesFamilyStreamEvent) -> bool {
    !matches!(
        event,
        ResponsesStreamEventKind::ResponseCreated { .. }
            | ResponsesStreamEventKind::ResponseInProgress { .. }
            | ResponsesStreamEventKind::Opaque { .. }
            | ResponsesStreamEventKind::Unknown { .. }
    )
}
