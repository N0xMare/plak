//! Closed admitted physical-attempt host for the Responses family.
//!
//! The host owns one exact request, one physical generation, one bounded HTTP
//! exchange, and one bounded Responses SSE terminal. Dialects supply immutable
//! execution data plus two ordinary function pointers: an asynchronous
//! credential/route preflight and a synchronous final revalidation fence. It
//! deliberately defines no plugin, auth-provider, retry, or callback trait.

use std::fmt;
use std::future::{Future, poll_fn};
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use plakous_attempt::{
    AdmissionFirstFuture, AttemptBinding, AuthReservationMask, FailurePoint, FailureRisk,
    FamilyAttemptCore, FamilyCoreError, Jitter, LedgerPolicy, LifecycleLedgerAdmission,
    PhysicalRequestBinding, PhysicalRequestOutcome, ReplaySafety, RetryClass, RetryPolicy,
    TerminalFacts, retry_after_seconds,
};
use plakous_exchange::{RetryDeclineInput, retry_decline, retry_phase};
use plakous_http::{
    Bytes, CancellationToken as HttpCancellationToken, Client, ErrorKind as HttpErrorKind,
    ExchangeControl, Failure as HttpFailure, HeaderName, Request, ResponseBody, StatusCode,
};
use plakous_model::ModelRetryDeclineReason as Decline;
use plakous_model::{
    AttemptId, CancellationToken, Digest, DigestDomain, ModelOperationRoot, ModelProviderRequestId,
    ModelRequest, ModelRetry, ModelRetryCause, ModelTransportFault, ResolvedModelDescriptor,
};
use plakous_responses::{ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamLimits};
use tokio::sync::mpsc;

use crate::CredentialGenerationFingerprint;
use crate::project_host::{
    ResponsesProjectError, ResponsesProviderRequestBinding, ResponsesReplayProjection,
    ResponsesTerminalProjection, project_terminal_response_with_model_binding,
};
use crate::request_host::{
    LoweredResponsesRequest, ResponsesPrefixBinding, ResponsesReplayContext,
    ResponsesRequestAuthority, ResponsesRequestConfig, ResponsesRequestError,
    ResponsesRequestPolicy, ResponsesRouterPolicy, ResponsesStoragePolicy, lower_responses_request,
    validate_authority_for_config,
};
use crate::response_host::{
    ResponsesContentPart, ResponsesOutputItem, ResponsesProviderError, ResponsesProviderErrorClass,
    ResponsesProviderErrorPolicy, ResponsesResponse, ResponsesResponseLimits,
    parse_provider_error_with_policy, parse_unique_json_value, provider_error_object,
};
use crate::stream_host::{
    ResponsesFamilySseDecoder, ResponsesFamilyStreamError, ResponsesFamilyStreamEvent,
    ResponsesStreamDialectData, ResponsesStreamEventKind,
};
use crate::wire_accounting::{ResponsesWireOccurrenceCounts, inspect_wire_occurrences};

mod authority;
mod dispatch;
mod identity;
mod retry;
pub use authority::{
    ResponsesAccountSlot, ResponsesAuthSlot, ResponsesAuthSlots, ResponsesCredentialAuthority,
    ResponsesHeaderBinding,
};
use authority::{header_name_requires_secret, validate_auth_slots, validate_header_authority};
#[cfg(test)]
use dispatch::{
    CollectorFailure, ObservationDelivery, RawTerminal, StreamCollector, classify_status,
};
use dispatch::{admit_attempt, event_starts_visible_output, execute_admitted, status_outcome};
use identity::{
    authority_target, dialect_execution_identity, host_behavior_identity, inspect_serialized_body,
    lowered_replay_safety, physical_request_digest, prepared_wire_digest, request_body_digest,
    target_identity,
};
use retry::{
    RetryStep, credential_rejection_retry_eligible, in_band_retryable, refresh_credential,
    refused_connection, retryable, wait_to_retry,
};

const REQUEST_DIGEST_REVISION: u32 = 2;
const PREPARED_WIRE_DIGEST_REVISION: u32 = 1;
const TERMINAL_RESPONSE_DIGEST_REVISION: u32 = 2;
const ATTEMPT_DIALECT_IDENTITY_REVISION: u32 = 3;
const HOST_BEHAVIOR_IDENTITY_REVISION: u32 = 11;
const ATTEMPT_TARGET_IDENTITY_REVISION: u32 = 2;

/// Exact non-secret target authority owned by one Responses attempt service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesAttemptTarget(Digest);

impl ResponsesAttemptTarget {
    /// Derives the route/account/model/revision authority admitted by a service.
    ///
    /// # Errors
    ///
    /// Rejects invalid account or descriptor binding identity.
    pub fn from_admitted_target(
        descriptor: &ResolvedModelDescriptor,
        account: Digest,
        revisions: ResponsesSemanticRevisions,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if account.domain() != DigestDomain::ModelBinding {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let semantic_model = descriptor
            .semantic_binding_digest()
            .map_err(|_| ResponsesAttemptBuildError::Binding)?;
        target_identity(
            descriptor.protocol().as_str(),
            descriptor.route().as_str(),
            account,
            descriptor.requested_model().as_str(),
            descriptor
                .model_family()
                .map_or("", plakous_model::ModelFamilyId::as_str),
            descriptor.profile().as_str(),
            semantic_model,
            revisions,
        )
        .map(Self)
    }

    /// Returns the exact target identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.0
    }
}

/// Exact immutable operation authority below the neutral runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesOperationBinding {
    request_authority: Digest,
    execution: Digest,
    request_body: Digest,
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
    replay_safety: ReplaySafety,
}

impl ResponsesOperationBinding {
    /// Returns the admitted request authority identity.
    #[must_use]
    pub const fn request_authority(&self) -> Digest {
        self.request_authority
    }

    /// Returns the exact dialect execution-data identity.
    #[must_use]
    pub const fn execution(&self) -> Digest {
        self.execution
    }

    /// Returns the digest of the exact credential-free request body.
    #[must_use]
    pub const fn request_body(&self) -> Digest {
        self.request_body
    }

    /// Returns the admitted physical-observation policy.
    #[must_use]
    pub const fn ledger_policy(&self) -> LedgerPolicy {
        self.ledger_policy
    }

    /// Returns authorization slots required for the physical generation.
    #[must_use]
    pub const fn auth_slots(&self) -> ResponsesAuthSlots {
        self.auth_slots
    }

    /// Returns what replaying the exact request body may cost.
    #[must_use]
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }

    /// The binding of one hosted-retrieval side request carrying `body`, a
    /// body the caller's per-tool allowlist already classified cost-only.
    ///
    /// A side request has no admitted model-request authority or dialect
    /// execution data; both are the fixed side-request identity, so the
    /// binding never matches a model attempt. The body is bound exactly.
    ///
    /// # Errors
    ///
    /// Fails only when a digest cannot be built.
    pub fn retrieval_side_request(
        body: &[u8],
        ledger_policy: LedgerPolicy,
        auth_slots: ResponsesAuthSlots,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        let side_request = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"responses-retrieval-side-request-v1"],
        )
        .map_err(|_| ResponsesAttemptBuildError::Binding)?;
        Ok(Self {
            request_authority: side_request,
            execution: side_request,
            request_body: request_body_digest(body)?,
            ledger_policy,
            auth_slots,
            replay_safety: ReplaySafety::CostOnly,
        })
    }
}

/// Exact logical-attempt identity.
pub type ResponsesAttemptBinding = AttemptBinding<ResponsesOperationBinding, AttemptId>;

/// Exact predicted physical generation supplied to preflight.
pub type ResponsesPhysicalRequestBinding = PhysicalRequestBinding<ResponsesAttemptBinding>;

/// Exact physical request, including the credentialized immutable HTTP value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesPhysicalDispatchBinding {
    physical: ResponsesPhysicalRequestBinding,
    route: Digest,
    credential_authority: ResponsesCredentialAuthority,
    auth_slots: ResponsesAuthSlots,
    request: Digest,
}

impl ResponsesPhysicalDispatchBinding {
    /// Returns the logical-attempt and physical-generation identity.
    #[must_use]
    pub const fn physical(&self) -> &ResponsesPhysicalRequestBinding {
        &self.physical
    }

    /// Returns the complete method/path/header/body request digest.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }

    /// Returns the host-derived normalized origin/path/execution identity.
    #[must_use]
    pub const fn route(&self) -> Digest {
        self.route
    }

    /// Returns the exact closed slot-to-generation/account authority supplied
    /// by the owning dialect authority.
    #[must_use]
    pub const fn credential_authority(&self) -> &ResponsesCredentialAuthority {
        &self.credential_authority
    }

    /// Returns the exact authorization slots reserved for this generation.
    #[must_use]
    pub const fn auth_slots(&self) -> ResponsesAuthSlots {
        self.auth_slots
    }
}

/// Failure to build an exact attempt or service configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesAttemptBuildError {
    /// Shared request serialization failed.
    #[error("Responses request serialization failed: {0}")]
    Request(#[from] ResponsesRequestError),
    /// An identity used the wrong digest domain or could not be constructed.
    #[error("Responses attempt binding is invalid")]
    Binding,
    /// The selected ledger cannot reserve every required authorization slot.
    #[error("Responses authorization slots are incompatible with the attempt ledger")]
    AuthSlots,
    /// The dialect behavior revision was zero or a closed datum was unsupported.
    #[error("Responses attempt dialect behavior identity is invalid")]
    DialectIdentity,
    /// Terminal replay projection identity construction failed.
    #[error("Responses replay projection identity is invalid")]
    Projection(#[from] ResponsesProjectError),
}

impl ResponsesAttemptBuildError {
    /// Classifies protected native history separately from ordinary projection failures.
    #[must_use]
    pub const fn preparation_error_kind(&self) -> plakous_model::ModelPreparationErrorKind {
        match self {
            Self::Request(ResponsesRequestError::Content(
                crate::ResponsesRequestContentError::CompactionWithoutReplay
                | crate::ResponsesRequestContentError::AssistantRefusalWithoutReplay
                | crate::ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                | crate::ResponsesRequestContentError::ProviderHostedCallWithoutReplay,
            )) => plakous_model::ModelPreparationErrorKind::ProtectedHistory,
            _ => plakous_model::ModelPreparationErrorKind::Projection,
        }
    }
}

/// Immutable credential-free request prepared before the attempt boundary.
///
/// Preparation owns every request-affecting operation: authority validation,
/// replay validation, Responses lowering, exact JSON serialization, body
/// binding, and hosted-effect classification. The value is deliberately
/// linear: consuming it starts one logical attempt, while any physical retry
/// remains inside that admitted family operation and reuses its retained body.
pub struct PreparedResponsesAttempt {
    authority: ResponsesRequestAuthority,
    body: Bytes,
    wire_digest: Digest,
    enforced_output_tokens: Option<NonZeroU32>,
    wire_occurrence_counts: Option<ResponsesWireOccurrenceCounts>,
    operation: ResponsesOperationBinding,
    retry_permitted: bool,
}

impl PreparedResponsesAttempt {
    /// Returns the exact current-operation request authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        &self.authority
    }

    /// Returns a shared handle to the exact serialized request body.
    #[must_use]
    pub fn body(&self) -> Bytes {
        self.body.clone()
    }

    /// Returns the exact credential-free serialized body digest used by model
    /// preparation evidence.
    #[must_use]
    pub const fn wire_digest(&self) -> Digest {
        self.wire_digest
    }

    /// Returns the exact credential-free serialized body size.
    #[must_use]
    pub fn wire_size(&self) -> usize {
        self.body.len()
    }

    /// Returns the output maximum actually present in the exact serialized
    /// body, when the admitted dialect emitted one.
    #[must_use]
    pub const fn enforced_output_tokens(&self) -> Option<NonZeroU32> {
        self.enforced_output_tokens
    }

    /// Returns the checked closed-grammar occurrence vector derived while
    /// inspecting this exact serialized body, when its wire grammar is in the
    /// currently certified family subset.
    #[must_use]
    pub const fn wire_occurrence_counts(&self) -> Option<&ResponsesWireOccurrenceCounts> {
        self.wire_occurrence_counts.as_ref()
    }

    /// Returns every immutable operation fact derived during preparation.
    #[must_use]
    pub const fn operation(&self) -> &ResponsesOperationBinding {
        &self.operation
    }
}

impl fmt::Debug for PreparedResponsesAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedResponsesAttempt")
            .field("authority", &self.authority)
            .field("body_bytes", &self.body.len())
            .field("wire_digest", &self.wire_digest)
            .field("enforced_output_tokens", &self.enforced_output_tokens)
            .field("wire_occurrence_counts", &self.wire_occurrence_counts)
            .field("operation", &self.operation)
            .field("retry_permitted", &self.retry_permitted)
            .finish()
    }
}

/// Prepares one exact credential-free Responses request before attempt start.
///
/// This is the sole lowering and body-serialization entry point for
/// [`ResponsesAttempt`]. Attempt construction accepts only this sealed value
/// plus cancellation/deadline controls.
///
/// # Errors
///
/// Returns [`ResponsesAttemptBuildError`] when authority, target, replay, auth
/// slots, lowering, serialization, or derived request facts fail closed.
pub fn prepare_responses_attempt(
    authority: ResponsesRequestAuthority,
    request: &ModelRequest,
    replay: Option<ResponsesReplayContext<'_>>,
    data: &ResponsesAttemptDialectData,
) -> Result<PreparedResponsesAttempt, ResponsesAttemptBuildError> {
    prepare_responses_attempt_with(
        authority,
        request,
        replay,
        data,
        lower_responses_request,
        LoweredResponsesRequest::body_bytes,
    )
}

fn prepare_responses_attempt_with<L, S>(
    authority: ResponsesRequestAuthority,
    request: &ModelRequest,
    replay: Option<ResponsesReplayContext<'_>>,
    data: &ResponsesAttemptDialectData,
    mut lower: L,
    mut serialize: S,
) -> Result<PreparedResponsesAttempt, ResponsesAttemptBuildError>
where
    L: for<'a> FnMut(
        &ModelRequest,
        ResponsesRequestConfig<'a>,
    ) -> Result<LoweredResponsesRequest, ResponsesRequestError>,
    S: FnMut(&LoweredResponsesRequest) -> Result<Vec<u8>, ResponsesRequestError>,
{
    if authority.identity().domain() != DigestDomain::ModelBinding
        || data.execution.domain() != DigestDomain::ModelBinding
    {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    ResponsesPrefixBinding::for_request(request, 0, &authority)?;
    if authority_target(&authority)? != data.target {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let admitted = authority.artifact_binding();
    if !data.replay.matches_target(
        admitted.dialect(),
        admitted.model_family(),
        admitted.model(),
    ) {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    validate_auth_slots(data.ledger_policy, data.auth_slots)?;
    let target = authority.artifact_binding();
    let cache_affinity = authority.cache_affinity();
    let account = target.account();
    let request_config = ResponsesRequestConfig::new(
        target.dialect(),
        target.model_family(),
        target.model(),
        data.request_policy,
        replay,
    )
    .with_cache_affinity(cache_affinity.as_ref())
    .with_account_binding(&account);
    validate_authority_for_config(request, request_config, &authority)
        .map_err(ResponsesRequestError::from)?;
    let lowered = lower(request, request_config)?;
    let lowered = match &data.router_policy {
        Some(policy) => lowered.with_router_policy(policy)?,
        None => lowered,
    };
    let body = Bytes::from(serialize(&lowered)?);
    let inspection = inspect_serialized_body(
        &body,
        data.request_policy
            .requires_closed_wire_occurrence_accounting(),
    )?;
    let wire_digest = prepared_wire_digest(&body)?;
    let request_body = request_body_digest(&body)?;
    let replay_safety = lowered_replay_safety(&lowered)?;
    let operation = ResponsesOperationBinding {
        request_authority: authority.identity(),
        execution: data.execution,
        request_body,
        ledger_policy: data.ledger_policy,
        auth_slots: data.auth_slots,
        replay_safety,
    };
    let retry_permitted = matches!(request.metadata().root(), ModelOperationRoot::Turn(_));
    Ok(PreparedResponsesAttempt {
        authority,
        body,
        wire_digest,
        enforced_output_tokens: inspection.enforced_output_tokens,
        wire_occurrence_counts: inspection.wire_occurrence_counts,
        operation,
        retry_permitted,
    })
}

/// One item on a logical attempt's live observation channel.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "events are the hot variant; boxing them would allocate per event for a rare retry marker"
)]
pub enum ResponsesObservation {
    /// A typed stream event of the current physical request.
    Event(ResponsesFamilyStreamEvent),
    /// The logical attempt will replay in a new physical request; ordered
    /// after every event of the failed one.
    Retry(ModelRetry),
    /// Content-free progress, including output suppressed by canonical projection.
    Activity(plakous_model::ModelStreamActivity),
}

/// What one physical request's stream had shown before it ended.
#[derive(Clone, Copy, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts that the replay classifier reads separately"
)]
struct StreamProgress {
    started: bool,
    /// The stream broke and its generation was closed for a replay.
    interrupted: bool,
    delivered: bool,
    /// The stream ended in the provider's own rate-limit or overload refusal
    /// and its generation was closed for a replay.
    refused_in_band: Option<ModelTransportFault>,
    loss: plakous_model::ModelRetryLoss,
    physical_started: Option<Instant>,
    retry_disabled: bool,
    last_semantic_event: Option<Instant>,
    activity: plakous_model::ModelStreamActivity,
    comment_line_open: bool,
    semantic_bound_active: bool,
    last_activity_emission: Option<(Instant, plakous_model::ModelStreamActivity)>,
}

impl StreamProgress {
    fn observe_semantic_event(&mut self, event: &ResponsesFamilyStreamEvent, now: Instant) {
        self.observe_gap(now);
        if matches!(event, ResponsesStreamEventKind::Unknown { .. }) {
            self.activity.keepalive_count = self.activity.keepalive_count.saturating_add(1);
            return;
        }
        if event_starts_visible_output(event) {
            self.activity.output_started = true;
            self.semantic_bound_active = true;
        } else if matches!(event, ResponsesStreamEventKind::OutputItemAdded { .. }) {
            self.semantic_bound_active = false;
        }
        self.last_semantic_event = Some(now);
        self.activity.semantic_gap_ms = 0;
        self.activity.last_semantic_event_elapsed_ms =
            Some(self.physical_started.map_or(0, |started| {
                milliseconds(now.saturating_duration_since(started))
            }));
    }

    fn observe_gap(&mut self, now: Instant) {
        if let Some(last) = self.last_semantic_event {
            self.activity.semantic_gap_ms = milliseconds(now.saturating_duration_since(last));
            if self.semantic_bound_active {
                self.activity.max_semantic_gap_ms = self
                    .activity
                    .max_semantic_gap_ms
                    .max(self.activity.semantic_gap_ms);
            }
        }
    }

    fn observe_comments(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if !self.comment_line_open && *byte == b':' {
                self.activity.keepalive_count = self.activity.keepalive_count.saturating_add(1);
            }
            self.comment_line_open = *byte != b'\n';
        }
    }

    fn activity_to_emit(
        &mut self,
        now: Instant,
        force: bool,
    ) -> Option<plakous_model::ModelStreamActivity> {
        if !force && !self.activity.output_started && self.activity.keepalive_count == 0 {
            return None;
        }
        if !force
            && let Some((last, activity)) = self.last_activity_emission
            && activity.output_started == self.activity.output_started
            && (now.saturating_duration_since(last) < Duration::from_secs(1)
                || activity.keepalive_count == self.activity.keepalive_count)
        {
            return None;
        }
        self.last_activity_emission = Some((now, self.activity));
        Some(self.activity)
    }

    fn semantic_deadline(&self) -> Option<Instant> {
        self.semantic_bound_active
            .then(|| {
                self.last_semantic_event?
                    .checked_add(Duration::from_secs(300))
            })
            .flatten()
    }
}

fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn monotonic_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// One credential-free logical Responses attempt.
pub struct ResponsesAttempt {
    authority: ResponsesRequestAuthority,
    body: Bytes,
    binding: ResponsesAttemptBinding,
    cancellation: CancellationToken,
    deadline: Instant,
    dispatch_timeout: Duration,
    next_dispatch_started: Option<Instant>,
    hard_deadline: Option<Instant>,
    long_stream_replays: plakous_attempt::LongStreamReplayBudget,
    unreachable: plakous_attempt::UnreachableRideOut,
    observations: Option<mpsc::Sender<ResponsesObservation>>,
    physical: Option<ResponsesPhysicalDispatchBinding>,
    stream: StreamProgress,
    core: FamilyAttemptCore,
    retry_permitted: bool,
    request_log: Option<(String, String, usize)>,
    fresh_connection: bool,
}

impl ResponsesAttempt {
    fn reset_stream(&mut self) {
        let activity = self.stream.activity;
        let last_activity_emission = self.stream.last_activity_emission;
        self.stream = StreamProgress::default();
        self.stream.last_activity_emission = last_activity_emission;
        self.stream.activity.keepalive_count = activity.keepalive_count;
        self.stream.activity.max_semantic_gap_ms = activity.max_semantic_gap_ms;
    }
    /// Starts one logical attempt from immutable request facts plus controls.
    #[must_use]
    pub fn from_prepared(
        prepared: PreparedResponsesAttempt,
        attempt_id: AttemptId,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Self {
        let PreparedResponsesAttempt {
            authority,
            body,
            wire_digest: _,
            enforced_output_tokens: _,
            wire_occurrence_counts: _,
            operation,
            retry_permitted,
        } = prepared;
        let ledger_policy = operation.ledger_policy();
        Self {
            authority,
            body,
            binding: AttemptBinding::new(operation, attempt_id),
            cancellation,
            deadline,
            dispatch_timeout: deadline.saturating_duration_since(monotonic_now()),
            hard_deadline: None,
            next_dispatch_started: None,
            long_stream_replays: plakous_attempt::LongStreamReplayBudget::default(),
            unreachable: plakous_attempt::UnreachableRideOut::default(),
            observations: None,
            physical: None,
            stream: StreamProgress::default(),
            core: FamilyAttemptCore::new_with_policy(ledger_policy),
            retry_permitted,
            request_log: None,
            fresh_connection: false,
        }
    }

    /// Attaches a caller-owned bounded live observation channel.
    ///
    /// Non-terminal events use bounded nonblocking delivery. Saturation or
    /// receiver loss records a gap without delaying or changing the
    /// authoritative terminal result. A retry marker waits for capacity during
    /// the retry delay, so a receiver always learns which events to discard.
    #[must_use]
    pub fn with_observations(mut self, observations: mpsc::Sender<ResponsesObservation>) -> Self {
        self.observations = Some(observations);
        self
    }

    /// Separates each generation's dispatch budget from a caller's hard bound.
    #[must_use]
    pub(crate) fn with_dispatch_deadlines(
        mut self,
        timeout: Duration,
        hard_deadline: Option<Instant>,
    ) -> Self {
        self.dispatch_timeout = timeout;
        self.hard_deadline = hard_deadline;
        self
    }

    /// Returns the exact logical-attempt binding.
    #[must_use]
    pub const fn binding(&self) -> &ResponsesAttemptBinding {
        &self.binding
    }

    /// Returns the exact current-operation request authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        &self.authority
    }

    /// Returns the cooperative logical-operation cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns this generation's dispatch deadline, never a retry-wait bound.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    fn facts(&self) -> TerminalFacts {
        self.core.terminal_facts()
    }

    fn physical(&self) -> Option<Box<ResponsesPhysicalDispatchBinding>> {
        self.physical.clone().map(Box::new)
    }
}

impl fmt::Debug for ResponsesAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesAttempt")
            .field("binding", &self.binding)
            .field("body_bytes", &self.body.len())
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("deadline", &"<bounded>")
            .field("replay_safety", &self.binding.operation().replay_safety())
            .field(
                "observations",
                &self.observations.as_ref().map(|_| "<bounded>"),
            )
            .field("physical", &self.physical)
            .finish_non_exhaustive()
    }
}

/// Borrowed, exact preflight inputs for the next physical generation.
pub struct ResponsesPreflightRequest<'a> {
    authority: &'a ResponsesRequestAuthority,
    operation: &'a ResponsesOperationBinding,
    physical: ResponsesPhysicalRequestBinding,
    body: &'a Bytes,
    cancellation: &'a CancellationToken,
    deadline: Instant,
}

impl ResponsesPreflightRequest<'_> {
    /// Returns the exact current request authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        self.authority
    }

    /// Returns the exact operation binding.
    #[must_use]
    pub const fn operation(&self) -> &ResponsesOperationBinding {
        self.operation
    }

    /// Returns the predicted next physical generation.
    #[must_use]
    pub const fn physical(&self) -> &ResponsesPhysicalRequestBinding {
        &self.physical
    }

    /// Returns a shared handle to the exact credential-free JSON body.
    #[must_use]
    pub fn body(&self) -> Bytes {
        self.body.clone()
    }

    /// Borrows the exact credential-free JSON body for inspection.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        self.body.as_ref()
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        self.cancellation
    }

    /// Returns the absolute dispatch deadline: it bounds preflight, retry
    /// waits and each exchange up to its response head, never a streaming body.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// Credentialized immutable request returned by dialect preflight.
pub struct ResponsesPreparedPhysicalRequest<G> {
    physical: ResponsesPhysicalRequestBinding,
    request: Request,
    credential_authority: ResponsesCredentialAuthority,
    header_bindings: Vec<ResponsesHeaderBinding>,
    response_head_guard: G,
}

impl<G> ResponsesPreparedPhysicalRequest<G> {
    /// Tags one immutable request with the exact predicted generation.
    #[must_use]
    pub fn new(
        physical: ResponsesPhysicalRequestBinding,
        request: Request,
        credential_authority: ResponsesCredentialAuthority,
        header_bindings: Vec<ResponsesHeaderBinding>,
        response_head_guard: G,
    ) -> Self {
        Self {
            physical,
            request,
            credential_authority,
            header_bindings,
            response_head_guard,
        }
    }
}

impl<G> fmt::Debug for ResponsesPreparedPhysicalRequest<G> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesPreparedPhysicalRequest")
            .field("physical", &self.physical)
            .field("request", &"<credentialized immutable request>")
            .field("credential_authority", &self.credential_authority)
            .field("header_bindings", &self.header_bindings.len())
            .field(
                "response_head_guard",
                &"<opaque lease retained through response head>",
            )
            .finish_non_exhaustive()
    }
}

/// Exact final fence inputs; this function runs synchronously immediately
/// before the one physical exchange.
pub struct ResponsesRevalidationRequest<'a, G> {
    authority: &'a ResponsesRequestAuthority,
    binding: &'a ResponsesPhysicalDispatchBinding,
    request: &'a Request,
    cancellation: &'a CancellationToken,
    deadline: Instant,
    response_head_guard: &'a mut G,
}

impl<G> ResponsesRevalidationRequest<'_, G> {
    /// Returns the exact current request authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        self.authority
    }

    /// Returns the exact physical/request binding.
    #[must_use]
    pub const fn binding(&self) -> &ResponsesPhysicalDispatchBinding {
        self.binding
    }

    /// Returns the immutable credentialized request for dialect validation.
    #[must_use]
    pub const fn request(&self) -> &Request {
        self.request
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        self.cancellation
    }

    /// Returns the absolute dispatch deadline: it bounds preflight, retry
    /// waits and each exchange up to its response head, never a streaming body.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Borrows the concrete dialect lease for its one-shot final authorization.
    ///
    /// This request is issued only after all host-side fences and immediately
    /// before the one transport exchange.
    #[must_use]
    pub const fn response_head_guard_mut(&mut self) -> &mut G {
        self.response_head_guard
    }
}

/// Closed reason returned by a dialect-owned preflight/revalidation function.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesDispatchRejection {
    /// Route/model/account/operation authority differed.
    Binding,
    /// Credential acquisition or freshness validation failed.
    Authentication,
    /// Current route/account authority denied dispatch.
    Authorization,
    /// A bounded admission or subscription gate denied dispatch.
    Admission,
    /// The subscription inference class exhausted the conservative
    /// input-liability budget of the current turn scope.
    InferenceInput,
    /// A required authority was temporarily unavailable.
    Unavailable,
    /// Subscription inference class exhausted its content-free reservation counter.
    InferenceClass {
        /// Reservations already charged, including abandoned permits.
        reserved: u32,
        /// Immutable inference-class allowance.
        allowance: u32,
    },
}

/// Future returned by one narrow asynchronous preflight function.
pub type ResponsesPreflightFuture<'a, G> = Pin<
    Box<
        dyn Future<Output = Result<ResponsesPreparedPhysicalRequest<G>, ResponsesDispatchRejection>>
            + Send
            + 'a,
    >,
>;

/// Closed async preflight function shape; no dialect trait or callback object.
pub type ResponsesPreflightFn<D, G> =
    for<'a> fn(&'a D, ResponsesPreflightRequest<'a>) -> ResponsesPreflightFuture<'a, G>;

/// Closed synchronous final revalidation function shape.
pub type ResponsesRevalidateFn<D, G> = for<'a> fn(
    &'a D,
    ResponsesRevalidationRequest<'a, G>,
) -> Result<(), ResponsesDispatchRejection>;

/// Trusted typed dialect adapter for the two authorization boundaries.
///
/// The semantic revisions, rather than unstable function addresses, are bound
/// into every operation identity. A dialect adapter is trusted to increment
/// the applicable revision whenever either function's dispatch-affecting
/// semantics change. The service never accepts an unbound replacement hook.
pub struct ResponsesAuthorizationAdapter<D, G> {
    preflight_revision: u32,
    final_authorization_revision: u32,
    preflight: ResponsesPreflightFn<D, G>,
    revalidate: ResponsesRevalidateFn<D, G>,
    credential_rejected: Option<ResponsesCredentialRejectedFn<D>>,
    transport_failed: Option<fn(&mut G, HttpFailure)>,
}

/// Closed async hook run once after an HTTP 401 head, before the single
/// refresh-and-retry re-dispatch. It must force a credential refresh; the
/// next preflight then acquires the renewed grant. Not part of the host
/// behavior identity: it changes no wire byte of any dispatch.
pub type ResponsesCredentialRejectedFn<D> = for<'a> fn(
    &'a D,
) -> Pin<
    Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + 'a>,
>;

impl<D, G> ResponsesAuthorizationAdapter<D, G> {
    /// Binds exact semantic revisions to one typed function pair.
    ///
    /// # Errors
    ///
    /// Rejects zero, which is reserved for an unversioned/untrusted adapter.
    pub const fn new(
        preflight_revision: u32,
        final_authorization_revision: u32,
        preflight: ResponsesPreflightFn<D, G>,
        revalidate: ResponsesRevalidateFn<D, G>,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if preflight_revision == 0 || final_authorization_revision == 0 {
            return Err(ResponsesAttemptBuildError::DialectIdentity);
        }
        Ok(Self {
            preflight_revision,
            final_authorization_revision,
            preflight,
            revalidate,
            credential_rejected: None,
            transport_failed: None,
        })
    }

    /// Settles the retained dispatch guard using trusted transport evidence.
    #[must_use]
    pub const fn with_transport_failed(mut self, settle: fn(&mut G, HttpFailure)) -> Self {
        self.transport_failed = Some(settle);
        self
    }

    /// Installs the credential-rejection hook that enables one
    /// refresh-and-retry after an HTTP 401 head (see
    /// [`ResponsesAttemptService::execute`]).
    #[must_use]
    pub const fn with_credential_rejected(
        mut self,
        credential_rejected: ResponsesCredentialRejectedFn<D>,
    ) -> Self {
        self.credential_rejected = Some(credential_rejected);
        self
    }

    /// Returns the semantic revision of credential/route preflight.
    #[must_use]
    pub const fn preflight_revision(&self) -> u32 {
        self.preflight_revision
    }

    /// Returns the semantic revision of final dispatch authorization.
    #[must_use]
    pub const fn final_authorization_revision(&self) -> u32 {
        self.final_authorization_revision
    }
}

impl<D, G> Clone for ResponsesAuthorizationAdapter<D, G> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D, G> Copy for ResponsesAuthorizationAdapter<D, G> {}

impl<D, G> fmt::Debug for ResponsesAuthorizationAdapter<D, G> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesAuthorizationAdapter")
            .field("preflight_revision", &self.preflight_revision)
            .field(
                "final_authorization_revision",
                &self.final_authorization_revision,
            )
            .finish_non_exhaustive()
    }
}

/// Immutable execution data shared by one admitted dialect service.
#[derive(Clone, Debug)]
pub struct ResponsesAttemptDialectData {
    target: ResponsesAttemptTarget,
    execution: Digest,
    host_behavior: Digest,
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
    request_policy: ResponsesRequestPolicy,
    stream: ResponsesStreamDialectData,
    stream_limits: ResponsesStreamLimits,
    replay: ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_error_policy: ResponsesProviderErrorPolicy,
    error_limits: ResponsesResponseLimits,
    provider_request_id_header: Option<HeaderName>,
    allow_missing_sse_content_type: bool,
    router_policy: Option<ResponsesRouterPolicy>,
}

impl ResponsesAttemptDialectData {
    /// Builds one exact, provider-name-free execution table.
    ///
    /// # Errors
    ///
    /// This host admits requests whose provider authority is complete at the
    /// final revalidation fence immediately before transport.
    #[allow(
        clippy::too_many_arguments,
        reason = "closed Responses attempt data table"
    )]
    pub fn new<D, G>(
        dialect_revision: u32,
        target: ResponsesAttemptTarget,
        client: &Client,
        authorization: &ResponsesAuthorizationAdapter<D, G>,
        ledger_policy: LedgerPolicy,
        auth_slots: ResponsesAuthSlots,
        request_policy: ResponsesRequestPolicy,
        stream: ResponsesStreamDialectData,
        stream_limits: ResponsesStreamLimits,
        replay: ResponsesReplayProjection,
        storage: ResponsesStorageMode,
        provider_error_policy: ResponsesProviderErrorPolicy,
        error_limits: ResponsesResponseLimits,
        provider_request_id_header: Option<HeaderName>,
        allow_missing_sse_content_type: bool,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if dialect_revision == 0 {
            return Err(ResponsesAttemptBuildError::DialectIdentity);
        }
        if !request_and_terminal_storage_agree(request_policy.storage(), storage) {
            return Err(ResponsesAttemptBuildError::DialectIdentity);
        }
        validate_auth_slots(ledger_policy, auth_slots)?;
        let host_behavior = host_behavior_identity(client, authorization)?;
        let execution = dialect_execution_identity(
            dialect_revision,
            target,
            host_behavior,
            ledger_policy,
            auth_slots,
            request_policy,
            &stream,
            stream_limits,
            &replay,
            storage,
            provider_error_policy,
            error_limits,
            provider_request_id_header.as_ref(),
            allow_missing_sse_content_type,
        )?;
        Ok(Self {
            target,
            execution,
            host_behavior,
            ledger_policy,
            auth_slots,
            request_policy,
            stream,
            stream_limits,
            replay,
            storage,
            provider_error_policy,
            error_limits,
            provider_request_id_header,
            allow_missing_sse_content_type,
            router_policy: None,
        })
    }

    /// Attaches one strict route-evidenced router request extension.
    ///
    /// # Errors
    ///
    /// Rejects an invalid router execution identity or digest construction.
    pub fn with_router_policy(
        mut self,
        policy: ResponsesRouterPolicy,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        let router = policy
            .execution_identity()
            .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)?;
        self.execution = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"responses-dialect-router-extension-v1",
                self.execution.identity_bytes().as_slice(),
                router.identity_bytes().as_slice(),
            ],
        )
        .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)?;
        self.router_policy = Some(policy);
        Ok(self)
    }

    /// Returns the exact identity callers bind into [`ResponsesAttempt`].
    #[must_use]
    pub const fn execution(&self) -> Digest {
        self.execution
    }

    /// Returns the required physical-observation policy.
    #[must_use]
    pub const fn ledger_policy(&self) -> LedgerPolicy {
        self.ledger_policy
    }

    /// Returns authorization slots required for each physical generation.
    #[must_use]
    pub const fn auth_slots(&self) -> ResponsesAuthSlots {
        self.auth_slots
    }
}

const fn request_and_terminal_storage_agree(
    request: ResponsesStoragePolicy,
    terminal: ResponsesStorageMode,
) -> bool {
    matches!(
        (request, terminal),
        (
            ResponsesStoragePolicy::Stateless,
            ResponsesStorageMode::Stateless
        ) | (
            ResponsesStoragePolicy::ProviderStored,
            ResponsesStorageMode::ProviderStored
        )
    )
}

/// One concrete single-exchange Responses attempt service.
pub struct ResponsesAttemptService<D, G> {
    client: Client,
    dialect: Arc<D>,
    data: ResponsesAttemptDialectData,
    authorization: ResponsesAuthorizationAdapter<D, G>,
}

impl<D, G> ResponsesAttemptService<D, G>
where
    D: Send + Sync + 'static,
    G: Send + 'static,
{
    /// Binds a strict provider-blind HTTP client and closed dialect functions.
    ///
    /// `G` is a subscription/unmetered request lease retained through response
    /// head. It carries no external billing or dispatch authority.
    ///
    /// # Errors
    ///
    /// Rejects a client or authorization adapter whose exact bound behavior
    /// differs from the admitted dialect data.
    pub fn new(
        client: Client,
        dialect: Arc<D>,
        data: ResponsesAttemptDialectData,
        authorization: ResponsesAuthorizationAdapter<D, G>,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if host_behavior_identity(&client, &authorization)? != data.host_behavior {
            return Err(ResponsesAttemptBuildError::DialectIdentity);
        }
        Ok(Self {
            client,
            dialect,
            data,
            authorization,
        })
    }

    /// Admits and runs one bound logical operation.
    ///
    /// Service admission is sealed before the inner future is dropped, even on
    /// cancellation or panic. The host performs no redirect, route fallback,
    /// or credential fallback. When the attempt permits retry (an ordinary
    /// turn), the same logical attempt is re-sent under
    /// [`RetryPolicy::TURN`]: after a refusal or an unsent request, after a
    /// failure past send or a stream cut, including a begun local tool call, when the
    /// request is cost-only, and after one HTTP 401 head through the
    /// credential-rejection hook, which forces one refresh. Every re-send
    /// shares the policy's physical budget; a hosted effect or a second 401
    /// ends the attempt as-is. Session workflows retry only exact transport
    /// evidence that the request was not fully sent, never an HTTP response.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
    pub fn execute(&self, attempt: ResponsesAttempt) -> ResponsesAttemptFuture {
        if attempt.binding.operation().execution() != self.data.execution
            || attempt.binding.operation().ledger_policy() != self.data.ledger_policy
            || attempt.binding.operation().auth_slots() != self.data.auth_slots
        {
            attempt.core.mark_failed();
            let facts = attempt.facts();
            let physical = attempt.physical();
            return Box::pin(
                async move { Err(ResponsesAttemptError::Binding { physical, facts }) },
            );
        }
        if let Err(source) = admit_attempt(&attempt) {
            attempt.core.mark_failed();
            let facts = attempt.facts();
            let physical = attempt.physical();
            return Box::pin(async move {
                Err(ResponsesAttemptError::Observation {
                    physical,
                    source,
                    facts,
                })
            });
        }
        let admission =
            LifecycleLedgerAdmission::new(attempt.core.lifecycle(), attempt.core.observations());
        let retry_permitted = attempt.retry_permitted;
        let hook = self
            .authorization
            .credential_rejected
            .filter(|_| retry_permitted);
        let client = self.client.clone();
        let dialect = Arc::clone(&self.dialect);
        let data = self.data.clone();
        let authorization = self.authorization;
        let future = async move {
            let mut attempt = attempt;
            // The 401 refresh runs at most once; every re-send shares the
            // policy's physical budget.
            let mut hook = hook;
            let mut waited = Duration::ZERO;
            loop {
                let now = attempt
                    .next_dispatch_started
                    .take()
                    .unwrap_or_else(monotonic_now);
                attempt.deadline = now.checked_add(attempt.dispatch_timeout).unwrap_or(now);
                if let Some(hard) = attempt.hard_deadline {
                    attempt.deadline = attempt.deadline.min(hard);
                }
                attempt.stream.physical_started = Some(now);
                let error = match execute_admitted(
                    client.clone(),
                    Arc::clone(&dialect),
                    data.clone(),
                    authorization,
                    &mut attempt,
                    true,
                )
                .await
                {
                    Ok(response) => {
                        let Some(fault) = attempt.stream.refused_in_band else {
                            return Ok(response);
                        };
                        let failure = in_band_retryable(fault, attempt.stream);
                        match wait_to_retry(&mut attempt, failure, &mut waited).await {
                            RetryStep::Send => {
                                attempt.physical = None;
                                attempt.reset_stream();
                                continue;
                            }
                            RetryStep::Cancelled => {
                                attempt.core.mark_failed();
                                return Err(ResponsesAttemptError::Cancelled {
                                    physical: attempt.physical(),
                                    facts: attempt.facts(),
                                });
                            }
                            RetryStep::Deadline | RetryStep::End => {
                                attempt.core.mark_failed();
                                return Ok(ResponsesAttemptResponse {
                                    facts: attempt.facts(),
                                    ..response
                                });
                            }
                        }
                    }
                    Err(error) => error,
                };
                if matches!(
                    &error,
                    ResponsesAttemptError::HttpStatus {
                        retry_disabled: true,
                        ..
                    }
                ) {
                    attempt.stream.retry_disabled = true;
                }
                let step = if retry_permitted
                    && credential_rejection_retry_eligible(&error, &attempt.unreachable)
                {
                    match hook.take() {
                        Some(hook) => refresh_credential(hook, &dialect, &mut attempt).await,
                        None => RetryStep::End,
                    }
                } else if let Some(failure) = retryable(&error, attempt.stream) {
                    wait_to_retry(&mut attempt, failure, &mut waited).await
                } else {
                    RetryStep::End
                };
                match step {
                    RetryStep::Send => {
                        attempt.physical = None;
                        attempt.reset_stream();
                    }
                    RetryStep::Cancelled => {
                        attempt.core.mark_failed();
                        return Err(ResponsesAttemptError::Cancelled {
                            physical: attempt.physical(),
                            facts: attempt.facts(),
                        });
                    }
                    RetryStep::Deadline => {
                        attempt.core.mark_failed();
                        return Err(ResponsesAttemptError::Deadline {
                            physical: attempt.physical(),
                            facts: attempt.facts(),
                        });
                    }
                    RetryStep::End => {
                        attempt.core.mark_failed();
                        return Err(
                            error.with_summary(attempt.core.observations_ref().attempt_summary())
                        );
                    }
                }
            }
        };
        Box::pin(AdmissionFirstFuture::new(future, Some(admission)))
    }
}

impl<D, G> fmt::Debug for ResponsesAttemptService<D, G> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesAttemptService")
            .field("client", &self.client)
            .field("dialect", &"<closed dialect data>")
            .field("data", &self.data)
            .field("authorization", &self.authorization)
            .finish()
    }
}

/// Owned service future for one admitted logical operation.
pub type ResponsesAttemptFuture =
    Pin<Box<dyn Future<Output = Result<ResponsesAttemptResponse, ResponsesAttemptError>> + Send>>;

/// Closed typed terminal returned by one successfully decoded Responses stream.
#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub enum ResponsesAttemptTerminal {
    /// Successful completed aggregate.
    Completed(ResponsesTerminalProjection),
    /// Explicit incomplete/truncated aggregate.
    Incomplete(ResponsesTerminalProjection),
    /// Explicit failed aggregate.
    Failed(ResponsesTerminalProjection),
    /// Terminal in-stream provider error without a response aggregate.
    ProviderError(ResponsesProviderError),
}

impl ResponsesAttemptTerminal {
    /// Reports whether the provider operation completed successfully.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }
}

/// One exact authoritative terminal plus observation-delivery facts.
#[derive(Debug, PartialEq)]
pub struct ResponsesAttemptResponse {
    binding: ResponsesAttemptBinding,
    physical: ResponsesPhysicalDispatchBinding,
    terminal: ResponsesAttemptTerminal,
    facts: TerminalFacts,
    observations_delivered: usize,
    observations_dropped: usize,
}

impl ResponsesAttemptResponse {
    /// Returns the exact logical-attempt binding.
    #[must_use]
    pub const fn binding(&self) -> &ResponsesAttemptBinding {
        &self.binding
    }

    /// Returns the exact host-verified physical dispatch binding.
    #[must_use]
    pub const fn physical(&self) -> &ResponsesPhysicalDispatchBinding {
        &self.physical
    }

    /// Returns the number of non-terminal events accepted by the live channel.
    ///
    /// This count is observational only; saturation and receiver loss do not
    /// alter the terminal classification.
    #[must_use]
    pub const fn observations_delivered(&self) -> usize {
        self.observations_delivered
    }

    /// Returns non-terminal observations dropped by a full or closed channel.
    #[must_use]
    pub const fn observations_dropped(&self) -> usize {
        self.observations_dropped
    }

    /// Returns the one typed terminal classification.
    #[must_use]
    pub const fn terminal(&self) -> &ResponsesAttemptTerminal {
        &self.terminal
    }

    /// Returns conservative physical-attempt facts.
    #[must_use]
    pub const fn facts(&self) -> TerminalFacts {
        self.facts
    }

    /// Consumes the response without cloning native aggregate material.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ResponsesAttemptBinding,
        ResponsesPhysicalDispatchBinding,
        ResponsesAttemptTerminal,
        TerminalFacts,
        usize,
        usize,
    ) {
        (
            self.binding,
            self.physical,
            self.terminal,
            self.facts,
            self.observations_delivered,
            self.observations_dropped,
        )
    }
}

/// Stable classification for a non-successful HTTP response head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesHttpStatusClass {
    /// HTTP 401.
    Authentication,
    /// HTTP 403.
    Authorization,
    /// HTTP 429.
    RateLimited,
    /// HTTP 402.
    Quota,
    /// HTTP 413.
    ContextLimit,
    /// Other 4xx response (acceptance remains ambiguous without dialect proof).
    Rejected,
    /// 5xx ambiguous server outcome.
    Server,
    /// Other unsupported/ambiguous status.
    Unknown,
}

/// Terminal host failure with conservative retry/effect facts.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesAttemptError {
    /// Service and attempt execution data differed.
    #[error("Responses attempt does not match the admitted service")]
    Binding {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Attempt/physical observation ordering failed closed.
    #[error("Responses attempt observation failed")]
    Observation {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Shared lifecycle/observation failure.
        source: FamilyCoreError,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Dialect preflight rejected before a physical request.
    #[error("Responses preflight rejected dispatch")]
    Preflight {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Closed dialect-owned denial.
        rejection: ResponsesDispatchRejection,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Dialect revalidation rejected the exact physical request.
    #[error("Responses final revalidation rejected dispatch")]
    Revalidation {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Closed dialect-owned denial.
        rejection: ResponsesDispatchRejection,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Cooperative cancellation was acknowledged.
    #[error("Responses attempt was cancelled")]
    Cancelled {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Admission-sensitive terminal facts.
        facts: TerminalFacts,
    },
    /// The dispatch deadline or the stream idle bound elapsed.
    #[error("Responses attempt deadline elapsed")]
    Deadline {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Admission-sensitive terminal facts.
        facts: TerminalFacts,
    },
    /// The preflight request body or physical generation was substituted.
    #[error("Responses preflight substituted the bound physical request")]
    RequestBinding {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Provider-blind transport failed.
    #[error("Responses HTTP transport failed")]
    Transport {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Redacted phase/transmission failure.
        source: HttpFailure,
        /// Conservative physical-attempt facts.
        facts: TerminalFacts,
    },
    /// A non-success HTTP status was classified without fallback.
    #[error("Responses provider rejected or ambiguously failed the HTTP request")]
    HttpStatus {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Exact non-success HTTP status.
        status: StatusCode,
        /// Stable family classification.
        class: ResponsesHttpStatusClass,
        /// Optional bounded typed provider error.
        provider: Option<ResponsesProviderError>,
        /// Provider `Retry-After` advice in whole seconds, when present.
        retry_after: Option<Duration>,
        /// Provider advice or a body-classified entitlement failure forbids replay.
        retry_disabled: bool,
        /// Conservative physical-attempt facts.
        facts: TerminalFacts,
    },
    /// Successful HTTP status did not carry the admitted SSE media type.
    #[error("Responses success did not use the SSE media type")]
    ContentType {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Successful response head carried invalid request-attribution metadata.
    #[error("Responses response request attribution is invalid")]
    ResponseMetadata {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Shared family SSE/lifecycle decoding failed.
    #[error("Responses stream decoding failed")]
    Stream {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Shared family decoder failure.
        source: ResponsesFamilyStreamError,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// More than one typed terminal escaped the shared stream kernel.
    #[error("Responses stream produced more than one typed terminal")]
    MultipleTerminals {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Terminal projection/artifact preparation failed.
    #[error("Responses terminal projection failed")]
    Projection {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Terminal projection/artifact failure.
        source: ResponsesProjectError,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
}

impl ResponsesAttemptError {
    fn with_summary(mut self, summary: plakous_attempt::AttemptSummary) -> Self {
        match &mut self {
            Self::Binding { facts, .. }
            | Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Cancelled { facts, .. }
            | Self::Deadline { facts, .. }
            | Self::RequestBinding { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. }
            | Self::ContentType { facts, .. }
            | Self::ResponseMetadata { facts, .. }
            | Self::Stream { facts, .. }
            | Self::MultipleTerminals { facts, .. }
            | Self::Projection { facts, .. } => *facts = facts.with_summary(summary),
        }
        self
    }

    /// Returns conservative physical-attempt facts.
    #[must_use]
    pub const fn facts(&self) -> TerminalFacts {
        match self {
            Self::Binding { facts, .. }
            | Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Cancelled { facts, .. }
            | Self::Deadline { facts, .. }
            | Self::RequestBinding { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. }
            | Self::ContentType { facts, .. }
            | Self::ResponseMetadata { facts, .. }
            | Self::Stream { facts, .. }
            | Self::MultipleTerminals { facts, .. }
            | Self::Projection { facts, .. } => *facts,
        }
    }

    /// Returns the exact physical dispatch binding for every post-start result.
    #[must_use]
    pub fn physical(&self) -> Option<&ResponsesPhysicalDispatchBinding> {
        match self {
            Self::Binding { physical, .. }
            | Self::Observation { physical, .. }
            | Self::Preflight { physical, .. }
            | Self::Revalidation { physical, .. }
            | Self::Cancelled { physical, .. }
            | Self::Deadline { physical, .. }
            | Self::RequestBinding { physical, .. }
            | Self::Transport { physical, .. }
            | Self::HttpStatus { physical, .. }
            | Self::ContentType { physical, .. }
            | Self::ResponseMetadata { physical, .. }
            | Self::Stream { physical, .. }
            | Self::MultipleTerminals { physical, .. }
            | Self::Projection { physical, .. } => physical.as_deref(),
        }
    }

    /// Returns the strongest safe retry classification. The host never retries
    /// itself. Deterministic local cancellation/limit failures remain `Never`
    /// even when the separately exposed acceptance facts retain duplicate-risk
    /// uncertainty after final authorization.
    #[must_use]
    pub const fn retry_class(&self) -> RetryClass {
        match self {
            Self::Cancelled { .. }
            | Self::Deadline { .. }
            | Self::Binding { .. }
            | Self::RequestBinding { .. }
            | Self::ContentType { .. }
            | Self::ResponseMetadata { .. }
            | Self::Stream { .. }
            | Self::MultipleTerminals { .. }
            | Self::Projection { .. } => RetryClass::Never,
            Self::Transport { source, facts, .. }
                if matches!(
                    source.kind(),
                    HttpErrorKind::Cancelled | HttpErrorKind::LimitExceeded(_)
                ) =>
            {
                let _ = facts;
                RetryClass::Never
            }
            Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. } => facts.retry_class(),
        }
    }
}

#[cfg(test)]
mod tests;
