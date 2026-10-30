//! One-shot provider-model lifecycle for prepared Responses operations.
//!
//! Dialects prepare exact request authority, replay, wire bytes, and canonical
//! projection data before constructing this value. This host owns the common
//! start admission, joined operation/observation stream, terminal projection,
//! family-artifact sealing, error mapping, cancellation, and drop behavior.
//!
//! Incremental [`plakous_model::ModelEvent`] values emitted by this host are
//! bounded, lossy observations. Consumers must publish or display them as
//! observations only: they must not require them to reconstruct the terminal
//! response. The sole `ResponseCompleted` value is independently projected
//! from complete terminal family authority, is self-validating, and is the
//! only model-result authority.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::{CanonicalIdSources, ResponsesArtifactBinding};
use futures_core::Stream;
use plakous_attempt::{FailureRisk, RetryClass, TerminalFacts};
use plakous_exchange::network_failure;
use plakous_model::{
    CanonicalItemPrefixIdentity, Clock, ModelCallContext, ModelDispatchRejection,
    ModelDispatchRejectionClass, ModelDispatchRejectionPhase, ModelError, ModelErrorContext,
    ModelErrorKind, ModelErrorPhase, ModelHttpStatus, ModelProviderCode, ModelProviderRequestId,
    ModelRetryRisk, ModelRetryability, ModelStream, ResolvedModelDescriptor, StartModelCall,
    ValidatedRequestPlan,
};
use tokio::sync::mpsc;

use crate::{
    PreparedResponsesAttempt, ResponsesAttempt, ResponsesAttemptError, ResponsesAttemptFuture,
    ResponsesAttemptResponse, ResponsesAttemptService, ResponsesAttemptTerminal,
    ResponsesCanonicalDialectData, ResponsesCanonicalObservationProjector,
    ResponsesCanonicalTerminal, ResponsesContentPart, ResponsesFamilyStreamError,
    ResponsesFamilyStreamEvent, ResponsesHostedProgramContext, ResponsesHttpStatusClass,
    ResponsesObservation, ResponsesOutputItem, ResponsesTerminalProjection,
    project_canonical_terminal_with_hosted_programs, seal_responses_family_artifact,
};

/// Optional dialect hook for error facts not normalized by the family host.
///
/// Returning `None` delegates to the complete common Responses mapping. The
/// hook must remain pure: it receives only the closed attempt error and model
/// descriptor after the operation has terminalized.
pub type ResponsesModelErrorHook =
    fn(&ResponsesAttemptError, &ResolvedModelDescriptor) -> Option<ModelError>;

impl ResponsesAttemptError {
    /// Returns the default contextual HTTP error, or `None` for other failures.
    ///
    /// Dialect hooks can reuse its kind and context when customizing only the
    /// operator-facing message, without duplicating attribution or retry policy.
    pub fn contextual_http_error(
        &self,
        descriptor: &ResolvedModelDescriptor,
    ) -> Option<ModelError> {
        let Self::HttpStatus {
            status,
            class,
            provider,
            ..
        } = self
        else {
            return None;
        };
        let retryability = retryability_from_facts(self.facts(), self.retry_class());
        let kind = match class {
            _ if status.as_u16() == 426
                && provider.as_ref().is_some_and(|error| {
                    error.class() == crate::ResponsesProviderErrorClass::ClientVersion
                }) =>
            {
                ModelErrorKind::InvalidConfiguration
            }
            ResponsesHttpStatusClass::Authentication => ModelErrorKind::Authentication,
            ResponsesHttpStatusClass::Authorization
                if provider.as_ref().is_some_and(|provider| {
                    provider.class() == crate::ResponsesProviderErrorClass::ContentRefusal
                }) =>
            {
                ModelErrorKind::Policy
            }
            ResponsesHttpStatusClass::Authorization => ModelErrorKind::Authorization,
            ResponsesHttpStatusClass::RateLimited => ModelErrorKind::RateLimited,
            ResponsesHttpStatusClass::Quota => ModelErrorKind::QuotaExceeded,
            ResponsesHttpStatusClass::ContextLimit => ModelErrorKind::ContextLimit,
            ResponsesHttpStatusClass::Rejected
                if provider.as_ref().is_some_and(|provider| {
                    plakous_model::provider_rejection_is_context_overflow(
                        provider.code(),
                        provider.message().unwrap_or_default(),
                    )
                }) =>
            {
                ModelErrorKind::ContextLimit
            }
            ResponsesHttpStatusClass::Rejected => ModelErrorKind::InvalidRequest,
            ResponsesHttpStatusClass::Server | ResponsesHttpStatusClass::WrappedOverload => {
                ModelErrorKind::Unavailable
            }
            ResponsesHttpStatusClass::Unknown => ModelErrorKind::Protocol,
        };
        // A terminal credential rejection needs new credentials, contract
        // drift a new build, and a content refusal a different request.
        let retryability = if matches!(
            kind,
            ModelErrorKind::Authentication
                | ModelErrorKind::InvalidConfiguration
                | ModelErrorKind::Policy
        ) && retryability == ModelRetryability::Exhausted
        {
            ModelRetryability::Permanent
        } else {
            retryability
        };
        Some(contextual_http_error(
            descriptor,
            kind,
            retryability,
            status.as_u16(),
            provider.clone(),
        ))
    }
}

/// Invalid shared model-lifecycle construction.
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesModelBuildError {
    /// Prepared request, plan, descriptor, or artifact binding differed.
    #[error("prepared Responses model execution binding is invalid")]
    Binding,
    /// The bounded observation channel capacity was zero.
    #[error("prepared Responses model observation capacity must be nonzero")]
    InvalidObservationCapacity,
}

/// Exact provider-specific projection facts surrounding one prepared family attempt.
///
/// The provider supplies only already validated values. No request lowering,
/// serialization, replay reconstruction, or authorization operation is
/// available through this type after construction.
pub struct PreparedResponsesModelExecution {
    attempt: PreparedResponsesAttempt,
    canonical: ResponsesCanonicalDialectData,
    terminal_ids: CanonicalIdSources,
    observation_ids: CanonicalIdSources,
    hosted_programs: ResponsesHostedProgramContext,
    plan: ValidatedRequestPlan,
    artifact_binding: ResponsesArtifactBinding,
    clock: Arc<dyn Clock>,
    descriptor: ResolvedModelDescriptor,
    observation_capacity: usize,
    observation_event_limit: usize,
    observation_byte_limit: usize,
    error_hook: Option<ResponsesModelErrorHook>,
    content_filter_retry: Option<bool>,
}

impl PreparedResponsesModelExecution {
    /// Couples one exact prepared attempt to its canonical terminal projection.
    ///
    /// # Errors
    ///
    /// Rejects descriptor, plan or artifact drift or a zero observation bound.
    #[allow(
        clippy::too_many_arguments,
        reason = "the constructor binds the closed exact terminal projection fact set"
    )]
    pub fn new(
        attempt: PreparedResponsesAttempt,
        canonical: ResponsesCanonicalDialectData,
        terminal_ids: CanonicalIdSources,
        observation_ids: CanonicalIdSources,
        hosted_programs: ResponsesHostedProgramContext,
        plan: ValidatedRequestPlan,
        artifact_binding: ResponsesArtifactBinding,
        clock: Arc<dyn Clock>,
        descriptor: ResolvedModelDescriptor,
        observation_capacity: usize,
        error_hook: Option<ResponsesModelErrorHook>,
    ) -> Result<Self, ResponsesModelBuildError> {
        if observation_capacity == 0
            || descriptor.validate_plan_binding(&plan).is_err()
            || attempt.authority().artifact_binding() != &artifact_binding
            || canonical
                .validate_tool_owner_projection(plan.request())
                .is_err()
            || canonical.expected_model()
                != descriptor
                    .reported_model()
                    .unwrap_or(descriptor.requested_model())
        {
            return Err(if observation_capacity == 0 {
                ResponsesModelBuildError::InvalidObservationCapacity
            } else {
                ResponsesModelBuildError::Binding
            });
        }
        let observation_event_limit = canonical
            .limits()
            .max_events
            .min(observation_capacity.saturating_mul(16))
            .max(1);
        let observation_byte_limit = canonical
            .limits()
            .max_accumulated_bytes
            .min(observation_capacity.saturating_mul(64 * 1024))
            .max(1);
        Ok(Self {
            attempt,
            canonical,
            terminal_ids,
            observation_ids,
            hosted_programs,
            plan,
            artifact_binding,
            clock,
            descriptor,
            observation_capacity,
            observation_event_limit,
            observation_byte_limit,
            error_hook,
            content_filter_retry: None,
        })
    }

    /// Enrolls the sampling-only content-filter retry classification.
    #[must_use]
    pub const fn with_content_filter_retry(mut self, sampling: bool) -> Self {
        self.content_filter_retry = Some(sampling);
        self
    }
}

impl std::fmt::Debug for PreparedResponsesModelExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedResponsesModelExecution")
            .field("attempt", &self.attempt)
            .field("canonical", &self.canonical)
            .field("plan", &self.plan.digest())
            .field("artifact_binding", &self.artifact_binding.identity())
            .field("descriptor", &"<exact resolved model>")
            .field("observation_capacity", &self.observation_capacity)
            .finish_non_exhaustive()
    }
}

/// Applies attempt-only controls and starts one shared Responses service.
///
/// The returned future first-polls the admitted service exactly once before it
/// returns the joined model stream. All lowering, JSON serialization, replay
/// reconstruction, and terminal configuration were consumed by
/// [`PreparedResponsesModelExecution::new`] before this function is callable.
/// Nonterminal events in the returned stream are explicitly observational;
/// callers must validate the self-contained `ResponseCompleted` aggregate
/// directly instead of gating it on incremental-event assembly.
pub fn start_prepared_responses_model_call<D, G>(
    prepared: PreparedResponsesModelExecution,
    service: Arc<ResponsesAttemptService<D, G>>,
    context: ModelCallContext,
    deadline: Instant,
) -> StartModelCall
where
    D: Send + Sync + 'static,
    G: Send + 'static,
{
    Box::pin(async move {
        let PreparedResponsesModelExecution {
            attempt,
            canonical,
            terminal_ids,
            observation_ids,
            hosted_programs,
            plan,
            artifact_binding,
            clock,
            descriptor,
            observation_capacity,
            observation_event_limit,
            observation_byte_limit,
            error_hook,
            content_filter_retry,
        } = prepared;
        let progress = StreamProgress::new(Instant::now());
        let cancellation = context.cancellation().clone();
        let (sender, mut receiver) = mpsc::channel(observation_capacity);
        let attempt = ResponsesAttempt::from_prepared(
            attempt,
            context.attempt_id(),
            cancellation.clone(),
            deadline,
        )
        .with_dispatch_deadlines(
            context.dispatch_timeout().map_or_else(
                || deadline.saturating_duration_since(Instant::now()),
                |budget| budget.min(deadline.saturating_duration_since(Instant::now())),
            ),
            context.deadline().map(|hard| {
                let now = clock.now();
                let nanos = |stamp: plakous_model::Timestamp| {
                    u128::from(stamp.seconds()) * 1_000_000_000 + u128::from(stamp.nanoseconds())
                };
                Instant::now()
                    + Duration::from_nanos(
                        u64::try_from(nanos(hard).saturating_sub(nanos(now))).unwrap_or(u64::MAX),
                    )
            }),
        )
        .with_observations(sender);
        let mut operation = service.execute(attempt);
        let first =
            poll_fn(|poll_context| Poll::Ready(operation.as_mut().poll(poll_context))).await;
        let (operation, outcome) = match first {
            Poll::Pending => (Some(operation), None),
            Poll::Ready(outcome) => (None, Some(outcome)),
        };
        if outcome.is_some() {
            receiver.close();
        }
        let stream: ModelStream = Box::pin(ResponsesJoinedModelStream {
            operation,
            outcome,
            progress,
            receiver,
            observation_state: ObservationState::Open,
            queued: VecDeque::new(),
            observation: ResponsesCanonicalObservationProjector::new_with_hosted_programs(
                canonical.clone(),
                plan.digest(),
                None,
                observation_ids.clone(),
                hosted_programs.clone(),
            ),
            observation_ids,
            hosted_programs,
            canonical,
            terminal_ids,
            plan,
            artifact_binding,
            clock,
            descriptor,
            error_hook,
            content_filter_retry,
            observation_event_limit,
            observation_byte_limit,
            queued_observation_bytes: 0,
            cancellation,
            state: JoinedState::Running,
        });
        Ok(stream)
    })
}

struct ResponsesJoinedModelStream {
    progress: StreamProgress,
    operation: Option<ResponsesAttemptFuture>,
    outcome: Option<Result<ResponsesAttemptResponse, ResponsesAttemptError>>,
    receiver: mpsc::Receiver<ResponsesObservation>,
    observation_state: ObservationState,
    queued: VecDeque<QueuedModelEvent>,
    observation: ResponsesCanonicalObservationProjector,
    observation_ids: CanonicalIdSources,
    hosted_programs: ResponsesHostedProgramContext,
    canonical: ResponsesCanonicalDialectData,
    terminal_ids: CanonicalIdSources,
    plan: ValidatedRequestPlan,
    artifact_binding: ResponsesArtifactBinding,
    clock: Arc<dyn Clock>,
    descriptor: ResolvedModelDescriptor,
    error_hook: Option<ResponsesModelErrorHook>,
    content_filter_retry: Option<bool>,
    observation_event_limit: usize,
    observation_byte_limit: usize,
    queued_observation_bytes: usize,
    cancellation: plakous_model::CancellationToken,
    state: JoinedState,
}

// This aggregate sees only the bounded, lossy observation channel. It neither
// retains content nor participates in stream acceptance, dispatch, or cleanup.
struct StreamProgress {
    started: Instant,
    last_progress: Option<Instant>,
    phase: ProgressPhase,
    events: u64,
    argument_deltas: u64,
    argument_completions: u64,
}

#[derive(Clone, Copy, Debug)]
enum ProgressPhase {
    Waiting,
    Envelope,
    Arguments,
    ArgumentsDone,
    Content,
    Item,
    Unknown,
}

impl StreamProgress {
    fn new(started: Instant) -> Self {
        Self {
            started,
            last_progress: None,
            phase: ProgressPhase::Waiting,
            events: 0,
            argument_deltas: 0,
            argument_completions: 0,
        }
    }

    fn observe(&mut self, event: &ResponsesFamilyStreamEvent, now: Instant) {
        use crate::ResponsesStreamEventKind as Event;
        self.events = self.events.saturating_add(1);
        self.last_progress = Some(now);
        self.phase = match event {
            Event::FunctionCallArgumentsDelta { .. } | Event::CustomToolCallInputDelta { .. } => {
                self.argument_deltas = self.argument_deltas.saturating_add(1);
                ProgressPhase::Arguments
            }
            Event::FunctionCallArgumentsDone { .. } | Event::CustomToolCallInputDone { .. } => {
                self.argument_completions = self.argument_completions.saturating_add(1);
                ProgressPhase::ArgumentsDone
            }
            Event::ResponseCreated { .. } | Event::ResponseInProgress { .. } => {
                ProgressPhase::Envelope
            }
            Event::OutputItemAdded { .. } | Event::OutputItemDone { .. } => ProgressPhase::Item,
            Event::OutputTextDelta { .. }
            | Event::OutputTextDone { .. }
            | Event::RefusalDelta { .. }
            | Event::RefusalDone { .. }
            | Event::ReasoningTextDelta { .. }
            | Event::ReasoningTextDone { .. }
            | Event::ReasoningSummaryTextDelta { .. }
            | Event::ReasoningSummaryTextDone { .. }
            | Event::ContentPartAdded { .. }
            | Event::ContentPartDone { .. }
            | Event::ReasoningSummaryPartAdded { .. }
            | Event::ReasoningSummaryPartDone { .. }
            | Event::OutputTextAnnotationAdded { .. } => ProgressPhase::Content,
            _ => ProgressPhase::Unknown,
        };
    }

    fn deadline_message(&self, now: Instant) -> String {
        let detail = if matches!(self.phase, ProgressPhase::Arguments) {
            "incomplete tool argument stream observed"
        } else {
            "no authoritative completion received"
        };
        let last_progress_ms = self.last_progress.map_or_else(
            || "none".to_owned(),
            |last| now.saturating_duration_since(last).as_millis().to_string(),
        );
        format!(
            "Responses operation deadline elapsed; {detail}; observed_phase={:?} observed_events={} observed_argument_deltas={} observed_argument_completions={} elapsed_ms={} last_observation_age_ms={last_progress_ms} authoritative_completion=false cleanup=joined (observations may be dropped)",
            self.phase,
            self.events,
            self.argument_deltas,
            self.argument_completions,
            now.saturating_duration_since(self.started).as_millis(),
        )
    }
}

struct QueuedModelEvent {
    event: plakous_model::ModelEvent,
    retained_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationState {
    Open,
    Suppressed,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JoinedState {
    Running,
    TerminalQueued,
    Finished,
}

impl ResponsesJoinedModelStream {
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
    fn terminalize(&mut self) -> Result<(), ModelError> {
        let outcome = self.outcome.take().ok_or_else(|| {
            model_error(
                &self.descriptor,
                ModelErrorKind::IncompleteResponse,
                ModelErrorPhase::Terminal,
                ModelRetryability::Permanent,
                "Responses operation ended without an authoritative outcome",
            )
        })?;
        let response = outcome.map_err(|error| {
            model_error_from_attempt(error, &self.descriptor, self.error_hook, &self.progress)
        })?;
        let (_, _, terminal, facts, _, _) = response.into_parts();
        let terminal_retryability = retryability_from_facts(facts, RetryClass::Never);
        let projection = match terminal {
            ResponsesAttemptTerminal::Completed(projection) => projection,
            ResponsesAttemptTerminal::Incomplete(projection)
                if self.canonical.policy().allows_grok_length_text()
                    && grok_length_text_terminal(&projection) =>
            {
                projection
            }
            ResponsesAttemptTerminal::Incomplete(projection) => {
                if self.content_filter_retry.is_some()
                    && facts.failure_risk() != FailureRisk::HostedEffect
                    && projection
                        .response()
                        .incomplete_details()
                        .and_then(|details| details.reason())
                        == Some("content_filter")
                {
                    return Err(model_error(
                        &self.descriptor,
                        if self.content_filter_retry == Some(true) {
                            ModelErrorKind::ContentFilteredIncomplete
                        } else {
                            ModelErrorKind::Policy
                        },
                        ModelErrorPhase::Terminal,
                        terminal_retryability,
                        "stream disconnected before completion: Incomplete response returned, reason: content_filter",
                    ));
                }
                let (kind, message) = incomplete_terminal_failure(&projection);
                return Err(model_error(
                    &self.descriptor,
                    kind,
                    ModelErrorPhase::Terminal,
                    retryability_from_facts(facts, RetryClass::Never),
                    message,
                ));
            }
            ResponsesAttemptTerminal::Failed(projection) => {
                return Err(provider_terminal_error(
                    &self.descriptor,
                    projection.response().error(),
                    facts,
                ));
            }
            ResponsesAttemptTerminal::ProviderError(error) => {
                return Err(provider_terminal_error(
                    &self.descriptor,
                    Some(&error),
                    facts,
                ));
            }
        };
        let canonical = project_canonical_terminal_with_hosted_programs(
            &projection,
            &self.canonical,
            self.plan.digest(),
            &self.terminal_ids,
            self.hosted_programs.clone(),
        )
        .map_err(|error| {
            let diagnostic = match error {
                crate::ResponsesCanonicalProjectError::InvalidConfiguration(message)
                | crate::ResponsesCanonicalProjectError::InvalidTerminal(message)
                | crate::ResponsesCanonicalProjectError::Unsupported(message)
                | crate::ResponsesCanonicalProjectError::ResourceLimit(message) => message,
                crate::ResponsesCanonicalProjectError::Family(_) => {
                    "Responses canonical family projection rejected terminal structure"
                }
                crate::ResponsesCanonicalProjectError::Content(_) => {
                    "Responses canonical terminal content was invalid"
                }
                crate::ResponsesCanonicalProjectError::Identity(_) => {
                    "Responses canonical terminal identity failed"
                }
                crate::ResponsesCanonicalProjectError::Json(_) => {
                    "Responses canonical opaque terminal serialization failed"
                }
            };
            model_error(
                &self.descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                terminal_retryability,
                diagnostic,
            )
        })?;
        let events = seal_terminal_events(
            projection,
            canonical,
            &self.plan,
            self.artifact_binding.clone(),
            self.clock.as_ref(),
            &self.descriptor,
            terminal_retryability,
        )?;
        let terminal = events.into_iter().find_map(|event| match event {
            plakous_model::ModelEvent::ResponseCompleted(response) => Some(response),
            _ => None,
        });
        let terminal = terminal.ok_or_else(|| {
            model_error(
                &self.descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                terminal_retryability,
                "Responses terminal projection omitted completion",
            )
        })?;
        terminal
            .validate_output_format(self.plan.request().output().format())
            .map_err(|_| {
                model_error(
                    &self.descriptor,
                    ModelErrorKind::InvalidEvent,
                    ModelErrorPhase::Terminal,
                    terminal_retryability,
                    "Responses structured assistant output failed local validation",
                )
            })?;
        self.queued.push_back(QueuedModelEvent {
            event: plakous_model::ModelEvent::ResponseCompleted(terminal),
            retained_bytes: 0,
        });
        Ok(())
    }

    fn poll_one_observation(&mut self, context: &mut Context<'_>) -> bool {
        if self.observation_state == ObservationState::Closed {
            return false;
        }
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(ResponsesObservation::Activity(activity))) => {
                self.queued.push_back(QueuedModelEvent {
                    event: plakous_model::ModelEvent::StreamActivity(activity),
                    retained_bytes: 0,
                });
                true
            }
            Poll::Ready(Some(ResponsesObservation::Retry(retry))) => {
                self.restart_observation(retry);
                true
            }
            Poll::Ready(Some(ResponsesObservation::Event(observation))) => {
                self.progress.observe(&observation, Instant::now());
                if self.observation_state == ObservationState::Open {
                    let events = self.observation.project(observation);
                    let encoded = events
                        .into_iter()
                        .map(|event| {
                            serde_json::to_vec(&event)
                                .ok()
                                .map(|bytes| QueuedModelEvent {
                                    event,
                                    retained_bytes: bytes.len(),
                                })
                        })
                        .collect::<Option<Vec<_>>>();
                    let encoded_bytes = encoded.as_ref().and_then(|events| {
                        events.iter().try_fold(0_usize, |total, event| {
                            total.checked_add(event.retained_bytes)
                        })
                    });
                    let fits =
                        encoded
                            .as_ref()
                            .zip(encoded_bytes)
                            .is_some_and(|(events, bytes)| {
                                self.queued.len().saturating_add(events.len())
                                    <= self.observation_event_limit
                                    && self.queued_observation_bytes.saturating_add(bytes)
                                        <= self.observation_byte_limit
                            });
                    if fits {
                        self.queued_observation_bytes = self
                            .queued_observation_bytes
                            .saturating_add(encoded_bytes.unwrap_or_default());
                        self.queued.extend(encoded.unwrap_or_default());
                    } else {
                        self.observation_state = ObservationState::Suppressed;
                    }
                }
                true
            }
            Poll::Ready(None) => {
                self.observation_state = ObservationState::Closed;
                true
            }
            Poll::Pending => false,
        }
    }

    /// Announces a replay. When the failed request's output is discarded the
    /// replay projects from a fresh projector, as a new response.
    fn restart_observation(&mut self, retry: plakous_model::ModelRetry) {
        if retry.output_discarded() {
            self.observation = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
                self.canonical.clone(),
                self.plan.digest(),
                None,
                self.observation_ids.clone(),
                self.hosted_programs.clone(),
            );
            self.observation_state = ObservationState::Open;
        }
        // The marker is never suppressed: the consumer must learn which
        // events to discard.
        self.queued.push_back(QueuedModelEvent {
            event: plakous_model::ModelEvent::Retry(retry),
            retained_bytes: 0,
        });
    }

    fn pop_queued(&mut self) -> Option<plakous_model::ModelEvent> {
        let queued = self.queued.pop_front()?;
        self.queued_observation_bytes = self
            .queued_observation_bytes
            .saturating_sub(queued.retained_bytes);
        Some(queued.event)
    }
}

/// Classifies an incomplete terminal the operation cannot accept.
///
/// Stopping at the output bound is its own kind: the runtime can tell the
/// model its call was cut off and let it retry with less output, which is not
/// true of any other incomplete reason.
fn incomplete_terminal_failure(
    projection: &ResponsesTerminalProjection,
) -> (ModelErrorKind, &'static str) {
    if projection
        .response()
        .incomplete_details()
        .and_then(|details| details.reason())
        == Some("max_output_tokens")
    {
        (
            ModelErrorKind::OutputLimit,
            "Responses provider stopped at the output bound before the response was complete",
        )
    } else {
        (
            ModelErrorKind::IncompleteResponse,
            "Responses provider returned an incomplete terminal",
        )
    }
}

fn grok_length_text_terminal(projection: &ResponsesTerminalProjection) -> bool {
    let response = projection.response();
    if response
        .incomplete_details()
        .and_then(|details| details.reason())
        != Some("max_output_tokens")
    {
        return false;
    }
    let Some((last, preceding)) = response.output().split_last() else {
        return false;
    };
    let text_message = matches!(last,
        ResponsesOutputItem::Message { status: Some(status), role, content, .. }
            if status == "incomplete"
                && role == "assistant"
                && matches!(content.as_slice(),
                    [ResponsesContentPart::OutputText { text, annotations }]
                        if !text.is_empty() && annotations.is_empty())
    );
    text_message
        && preceding.iter().all(|item| {
            matches!(item,
            ResponsesOutputItem::Reasoning { status: Some(status), .. }
                if status == "completed")
        })
}

impl Stream for ResponsesJoinedModelStream {
    type Item = Result<plakous_model::ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.state == JoinedState::Finished {
            return Poll::Ready(None);
        }
        if let Some(event) = self.pop_queued() {
            return Poll::Ready(Some(Ok(event)));
        }
        if self.state == JoinedState::TerminalQueued {
            self.state = JoinedState::Finished;
            return Poll::Ready(None);
        }
        let observed_before_operation = self.poll_one_observation(context);
        if let Some(event) = self.pop_queued() {
            return Poll::Ready(Some(Ok(event)));
        }
        if self.outcome.is_none()
            && let Some(operation) = self.operation.as_mut()
            && let Poll::Ready(outcome) = operation.as_mut().poll(context)
        {
            self.operation = None;
            self.outcome = Some(outcome);
            self.receiver.close();
        }
        let observed_after_operation = self.outcome.is_some() && self.poll_one_observation(context);
        if let Some(event) = self.pop_queued() {
            return Poll::Ready(Some(Ok(event)));
        }
        if self.outcome.is_some() && self.observation_state == ObservationState::Closed {
            if let Err(error) = self.terminalize() {
                self.state = JoinedState::Finished;
                return Poll::Ready(Some(Err(error)));
            }
            self.state = JoinedState::TerminalQueued;
            if let Some(event) = self.pop_queued() {
                return Poll::Ready(Some(Ok(event)));
            }
            self.state = JoinedState::Finished;
            return Poll::Ready(None);
        }
        if observed_before_operation || observed_after_operation {
            context.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl Drop for ResponsesJoinedModelStream {
    fn drop(&mut self) {
        if self.state != JoinedState::Finished {
            self.cancellation.cancel();
        }
        self.operation = None;
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
fn seal_terminal_events(
    projection: ResponsesTerminalProjection,
    canonical: ResponsesCanonicalTerminal,
    plan: &ValidatedRequestPlan,
    artifact_binding: ResponsesArtifactBinding,
    clock: &dyn Clock,
    descriptor: &ResolvedModelDescriptor,
    retryability: ModelRetryability,
) -> Result<Vec<plakous_model::ModelEvent>, ModelError> {
    let canonical_output = canonical.canonical_output().to_vec();
    let covered_projection =
        covered_projection_after_output(plan, &canonical_output).map_err(|_| {
            model_error(
                descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                retryability,
                "Responses canonical output coverage could not be sealed",
            )
        })?;
    let paired = projection
        .pair_canonical(plan.request(), canonical_output)
        .map_err(|error| {
            model_error(
                descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                retryability,
                format!("Responses raw output disagreed with canonical semantics: {error}"),
            )
        })?;
    if descriptor.integration_class() == plakous_model::IntegrationClass::Custom {
        let events = canonical.into_events();
        if !matches!(
            events.last(),
            Some(plakous_model::ModelEvent::ResponseCompleted(_))
        ) {
            return Err(model_error(
                descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                retryability,
                "Responses terminal projection omitted completion",
            ));
        }
        return Ok(events);
    }
    let (_, _, _, paired_artifact) = paired.into_parts();
    let paired_artifact = paired_artifact.ok_or_else(|| {
        model_error(
            descriptor,
            ModelErrorKind::InvalidEvent,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses canonical output omitted its artifact candidate",
        )
    })?;
    let sealed = paired_artifact
        .seal(plan, covered_projection)
        .map_err(|_| {
            model_error(
                descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                retryability,
                "Responses reconstructive artifact coverage could not be sealed",
            )
        })?;
    let candidate = match seal_responses_family_artifact(
        sealed,
        plan,
        covered_projection,
        artifact_binding,
        clock.now(),
    ) {
        Ok(candidate) => Some(candidate),
        // A plain canonical response may contain no provider-native replay,
        // continuation, or cache fact. In that case the correct family
        // artifact is absence; canonical terminal authority remains complete.
        Err(crate::ResponsesProjectError::Artifact(
            crate::ResponsesArtifactError::EmptyArtifact,
        )) => None,
        Err(error) => {
            let diagnostic = match error {
                crate::ResponsesProjectError::InvalidTerminal(_) => {
                    "Responses terminal artifact rejected terminal projection"
                }
                crate::ResponsesProjectError::InvalidReplayVersion => {
                    "Responses terminal artifact rejected replay version"
                }
                crate::ResponsesProjectError::InvalidProviderRequestBinding => {
                    "Responses terminal artifact rejected physical request binding"
                }
                crate::ResponsesProjectError::InvalidArtifactCoverage => {
                    "Responses terminal artifact rejected committed coverage"
                }
                crate::ResponsesProjectError::Artifact(error) => match error {
                    crate::ResponsesArtifactError::IncompleteReplayCoverage => {
                        "Responses terminal artifact had incomplete native replay coverage"
                    }
                    crate::ResponsesArtifactError::SecretReplayMaterial { .. } => {
                        "Responses terminal artifact contained forbidden raw secret replay"
                    }
                    crate::ResponsesArtifactError::ReplayBindingMismatch { .. } => {
                        "Responses terminal artifact replay binding disagreed"
                    }
                    crate::ResponsesArtifactError::ReplayPurposeKindMismatch { .. } => {
                        "Responses terminal artifact replay kind disagreed"
                    }
                    crate::ResponsesArtifactError::ProductionBindingMismatch => {
                        "Responses terminal artifact production binding disagreed"
                    }
                    crate::ResponsesArtifactError::InvalidCoordinates => {
                        "Responses terminal artifact production coordinates disagreed"
                    }
                    _ => "Responses terminal artifact rejected family-local facts",
                },
                crate::ResponsesProjectError::FamilyArtifact(_) => {
                    "Responses terminal artifact rejected neutral producer facts"
                }
                crate::ResponsesProjectError::OutputIndex => {
                    "Responses terminal artifact exceeded output index"
                }
                crate::ResponsesProjectError::Json(_) => {
                    "Responses terminal artifact replay serialization failed"
                }
                crate::ResponsesProjectError::Replay(_) => {
                    "Responses terminal artifact replay envelope failed"
                }
                crate::ResponsesProjectError::CanonicalPair(_) => {
                    "Responses terminal artifact rejected canonical pairing"
                }
                _ => "Responses terminal artifact could not be sealed",
            };
            return Err(model_error(
                descriptor,
                ModelErrorKind::InvalidEvent,
                ModelErrorPhase::Terminal,
                retryability,
                diagnostic,
            ));
        }
    };
    let mut events = canonical.into_events();
    let Some(plakous_model::ModelEvent::ResponseCompleted(response)) = events.last_mut() else {
        return Err(model_error(
            descriptor,
            ModelErrorKind::InvalidEvent,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses terminal projection omitted completion",
        ));
    };
    if let Some(candidate) = candidate {
        *response = response
            .clone()
            .with_family_artifact_candidate(candidate)
            .map_err(|_| {
                model_error(
                    descriptor,
                    ModelErrorKind::InvalidEvent,
                    ModelErrorPhase::Terminal,
                    retryability,
                    "Responses artifact candidate did not match its terminal",
                )
            })?;
    }
    Ok(events)
}

fn covered_projection_after_output(
    plan: &ValidatedRequestPlan,
    output: &[plakous_model::ConversationItem],
) -> Result<CanonicalItemPrefixIdentity, plakous_model::HistoryError> {
    plan.request()
        .input()
        .staged_canonical_item_prefix_identity(output)
}

/// Binds a credential-free probe to the executor's exact origin and route.
#[must_use]
pub fn bind_reachability_probe(
    client: plakous_http::Client,
    origin: plakous_http::Origin,
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

fn model_error_from_attempt(
    error: ResponsesAttemptError,
    descriptor: &ResolvedModelDescriptor,
    hook: Option<ResponsesModelErrorHook>,
    progress: &StreamProgress,
) -> ModelError {
    let summary = error.facts().summary();
    model_error_from_attempt_without_summary(error, descriptor, hook, progress)
        .with_attempt_summary(summary)
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
fn model_error_from_attempt_without_summary(
    error: ResponsesAttemptError,
    descriptor: &ResolvedModelDescriptor,
    hook: Option<ResponsesModelErrorHook>,
    progress: &StreamProgress,
) -> ModelError {
    if let Some(error) = hook.and_then(|hook| hook(&error, descriptor)) {
        return error;
    }
    if let Some(error) = error.contextual_http_error(descriptor) {
        return error;
    }
    let facts = error.facts();
    let retryability = retryability_from_facts(facts, error.retry_class());
    match error {
        ResponsesAttemptError::Cancelled { .. } => model_error(
            descriptor,
            ModelErrorKind::Cancelled,
            ModelErrorPhase::Operation,
            retryability,
            "Responses operation was cancelled",
        ),
        ResponsesAttemptError::Deadline { .. } => model_error(
            descriptor,
            ModelErrorKind::Timeout,
            ModelErrorPhase::Operation,
            retryability,
            progress.deadline_message(Instant::now()),
        ),
        ResponsesAttemptError::Preflight { rejection, .. } => dispatch_rejection_error(
            descriptor,
            rejection,
            ModelDispatchRejectionPhase::Preflight,
            retryability,
        ),
        ResponsesAttemptError::Revalidation { rejection, .. } => dispatch_rejection_error(
            descriptor,
            rejection,
            ModelDispatchRejectionPhase::Revalidation,
            retryability,
        ),
        ResponsesAttemptError::Transport { source, .. } => model_error(
            descriptor,
            ModelErrorKind::Transport,
            ModelErrorPhase::Stream,
            retryability,
            format!(
                "Responses transport failed: {}",
                network_failure(source).map_or_else(
                    || "network exchange failed".to_owned(),
                    |cause| cause.to_string()
                )
            ),
        )
        .with_source(source.to_string()),
        ResponsesAttemptError::ContentType { .. } => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::FirstEvent,
            retryability,
            "Responses success media type failed closed",
        ),
        ResponsesAttemptError::ResponseMetadata { .. } => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::FirstEvent,
            retryability,
            "Responses attribution metadata failed closed",
        ),
        ResponsesAttemptError::Stream {
            source: ResponsesFamilyStreamError::OutputBoundReached,
            ..
        } => model_error(
            descriptor,
            ModelErrorKind::OutputLimit,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses provider stopped at the output bound before its output was complete",
        ),
        ResponsesAttemptError::Stream { source, .. } => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::Stream,
            retryability,
            stream_failure_message(source),
        ),
        ResponsesAttemptError::MultipleTerminals { .. } => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses terminal cardinality failed closed",
        ),
        ResponsesAttemptError::Projection { .. } => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses terminal projection failed closed",
        ),
        _ => model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::Terminal,
            retryability,
            "Responses family contract failed closed",
        ),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned value matches the dialect hook and caller ownership"
)]
fn dispatch_rejection_error(
    descriptor: &ResolvedModelDescriptor,
    rejection: crate::ResponsesDispatchRejection,
    phase: ModelDispatchRejectionPhase,
    retryability: ModelRetryability,
) -> ModelError {
    let (kind, class, message) = match rejection {
        crate::ResponsesDispatchRejection::Binding => (
            ModelErrorKind::InvalidConfiguration,
            ModelDispatchRejectionClass::Binding,
            "Responses fence-binding".to_owned(),
        ),
        crate::ResponsesDispatchRejection::Authentication => (
            ModelErrorKind::Authentication,
            ModelDispatchRejectionClass::Authentication,
            "Responses fence-authentication".to_owned(),
        ),
        crate::ResponsesDispatchRejection::Authorization => (
            ModelErrorKind::Authorization,
            ModelDispatchRejectionClass::Authorization,
            "Responses fence-authorization".to_owned(),
        ),
        crate::ResponsesDispatchRejection::Admission => (
            ModelErrorKind::ResourceLimit,
            ModelDispatchRejectionClass::Admission,
            "Responses fence-admission".to_owned(),
        ),
        crate::ResponsesDispatchRejection::InferenceInput => (
            ModelErrorKind::ResourceLimit,
            ModelDispatchRejectionClass::Admission,
            "Responses fence-admission: the turn's inference input-liability budget is exhausted"
                .to_owned(),
        ),
        crate::ResponsesDispatchRejection::Unavailable => (
            ModelErrorKind::Unavailable,
            ModelDispatchRejectionClass::Unavailable,
            "Responses fence-unavailable".to_owned(),
        ),
        crate::ResponsesDispatchRejection::InferenceClass {
            reserved,
            allowance,
        } => (
            ModelErrorKind::Unavailable,
            ModelDispatchRejectionClass::Unavailable,
            format!("Responses fence-unavailable inference {reserved}/{allowance}"),
        ),
    };
    model_error(
        descriptor,
        kind,
        ModelErrorPhase::Readiness,
        retryability,
        message,
    )
    .with_dispatch_rejection(ModelDispatchRejection::new(class, phase))
    .unwrap_or_else(|_| {
        model_error(
            descriptor,
            ModelErrorKind::Protocol,
            ModelErrorPhase::Readiness,
            ModelRetryability::Permanent,
            "Responses dispatch rejection evidence was internally inconsistent",
        )
    })
}

fn stream_failure_message(error: ResponsesFamilyStreamError) -> String {
    match error {
        ResponsesFamilyStreamError::Lifecycle(reason) => {
            format!("Responses stream invalid lifecycle: {reason}")
        }
        other => other.to_string(),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned value matches the dialect hook and caller ownership"
)]
fn contextual_http_error(
    descriptor: &ResolvedModelDescriptor,
    kind: ModelErrorKind,
    retryability: ModelRetryability,
    status: u16,
    provider: Option<crate::ResponsesProviderError>,
) -> ModelError {
    let message = if status == 426
        && provider
            .as_ref()
            .is_some_and(|error| error.class() == crate::ResponsesProviderErrorClass::ClientVersion)
    {
        "provider rejected the compiled client version; this build's subscription contract is out of date; install a newer build".to_owned()
    } else {
        responses_http_error_message(provider.as_ref())
    };
    let Some(mut context) = ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::FirstEvent,
        retryability,
    )
    .ok() else {
        return ModelError::new(kind, message);
    };
    if let Ok(status) = ModelHttpStatus::new(status) {
        context = context.with_http_status(status);
    }
    if let Some(provider) = provider.as_ref() {
        if let Some(code) = provider
            .code()
            .and_then(|value| ModelProviderCode::new(value).ok())
        {
            context = context.with_provider_code(code);
        }
        if let Some(request_id) = provider
            .request_id()
            .and_then(|value| ModelProviderRequestId::new(value).ok())
        {
            context = context.with_provider_request_id(request_id);
        }
    }
    ModelError::contextual(kind, message.clone(), context)
        .unwrap_or_else(|_| ModelError::new(kind, message))
}

fn responses_http_error_message(provider: Option<&crate::ResponsesProviderError>) -> String {
    match provider.and_then(operator_provider_detail) {
        Some(detail) => format!("Responses HTTP request failed: {detail}"),
        None => "Responses HTTP request failed".to_owned(),
    }
}

fn operator_provider_detail(provider: &crate::ResponsesProviderError) -> Option<String> {
    if let Some(detail) = provider.usage_limit_detail() {
        return Some(detail);
    }
    let raw = provider.message()?;
    let collapsed: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

fn provider_terminal_error(
    descriptor: &ResolvedModelDescriptor,
    provider: Option<&crate::ResponsesProviderError>,
    facts: TerminalFacts,
) -> ModelError {
    let kind = match provider.map(crate::ResponsesProviderError::class) {
        Some(crate::ResponsesProviderErrorClass::Authentication) => ModelErrorKind::Authentication,
        Some(crate::ResponsesProviderErrorClass::Quota) => ModelErrorKind::QuotaExceeded,
        Some(crate::ResponsesProviderErrorClass::RateLimited) => ModelErrorKind::RateLimited,
        Some(crate::ResponsesProviderErrorClass::Server) => ModelErrorKind::Unavailable,
        Some(crate::ResponsesProviderErrorClass::ContentRefusal) => ModelErrorKind::Policy,
        Some(crate::ResponsesProviderErrorClass::Unknown)
            if provider.is_some_and(|provider| {
                [provider.code(), provider.kind()]
                    .into_iter()
                    .flatten()
                    .any(|label| {
                        matches!(
                            label,
                            "content_policy_violation"
                                | "content_filter"
                                | "cyber_safety_violation"
                                | "cyber_policy"
                        )
                    })
            }) =>
        {
            ModelErrorKind::Policy
        }
        Some(
            crate::ResponsesProviderErrorClass::Unknown
            | crate::ResponsesProviderErrorClass::ClientVersion,
        )
        | None => ModelErrorKind::Protocol,
    };
    let retryability = retryability_from_facts(facts, RetryClass::Never);
    let message = match provider.and_then(operator_provider_detail) {
        Some(detail) => format!("Responses provider returned a typed terminal error: {detail}"),
        None => "Responses provider returned a typed terminal error".to_owned(),
    };
    let Some(mut context) = ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::Terminal,
        retryability,
    )
    .ok() else {
        return ModelError::new(kind, message);
    };
    if let Some(code) = provider
        .and_then(crate::ResponsesProviderError::code)
        .and_then(|code| ModelProviderCode::new(code).ok())
    {
        context = context.with_provider_code(code);
    }
    if let Some(request_id) = provider
        .and_then(crate::ResponsesProviderError::request_id)
        .and_then(|request_id| ModelProviderRequestId::new(request_id).ok())
    {
        context = context.with_provider_request_id(request_id);
    }
    ModelError::contextual(kind, message.clone(), context)
        .unwrap_or_else(|_| ModelError::new(kind, message))
}

/// States the risk a failure leaves behind. Only a hosted effect makes the
/// agent fence the session; any accepted inference, with or without visible
/// output, may have been billed and is never replayed.
fn retryability_from_facts(facts: TerminalFacts, retry: RetryClass) -> ModelRetryability {
    match facts.failure_risk() {
        FailureRisk::HostedEffect => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
        FailureRisk::DuplicateInferenceOrBilling => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
        FailureRisk::None if retry == RetryClass::BeforeAcceptance => ModelRetryability::Exhausted,
        FailureRisk::None => ModelRetryability::Permanent,
    }
}

fn model_error(
    descriptor: &ResolvedModelDescriptor,
    kind: ModelErrorKind,
    phase: ModelErrorPhase,
    retryability: ModelRetryability,
    message: impl Into<String>,
) -> ModelError {
    let message = message.into();
    ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        phase,
        retryability,
    )
    .ok()
    .and_then(|context| ModelError::contextual(kind, message.clone(), context).ok())
    .unwrap_or_else(|| ModelError::new(kind, message))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod progress_tests;
