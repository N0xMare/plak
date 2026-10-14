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
}

/// Closed authorization-slot requirement for one Responses dialect route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAuthSlots {
    /// One primary platform/single-slot authorization.
    Primary,
    /// One secondary subscription authorization (for example native Codex).
    Secondary,
    /// Both independent authorizations are required for the same generation.
    PrimaryAndSecondary,
}

impl ResponsesAuthSlots {
    const fn mask(self) -> AuthReservationMask {
        match self {
            Self::Primary => AuthReservationMask::PRIMARY,
            Self::Secondary => AuthReservationMask::SECONDARY,
            Self::PrimaryAndSecondary => {
                AuthReservationMask::PRIMARY.union(AuthReservationMask::SECONDARY)
            }
        }
    }

    const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary",
            Self::Secondary => b"secondary",
            Self::PrimaryAndSecondary => b"primary+secondary",
        }
    }
}

/// One credential authorization slot used by a secret header binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAuthSlot {
    /// Primary platform/single-slot credential.
    Primary,
    /// Secondary subscription credential.
    Secondary,
}

impl ResponsesAuthSlot {
    const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary",
            Self::Secondary => b"secondary",
        }
    }
}

/// One account-authority slot used by an account-secret header binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAccountSlot {
    /// Primary account authority.
    Primary,
    /// Secondary/subscription account authority.
    Secondary,
}

/// Exact non-secret authority for all credential and account-secret slots in
/// one immutable physical request.
///
/// Credential bytes never enter this value. Each present authorization slot
/// carries only its owning authority's generation fingerprint and, when the
/// route emits an account-secret header, its non-secret account authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesCredentialAuthority {
    primary: Option<ResponsesSlotAuthority>,
    secondary: Option<ResponsesSlotAuthority>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResponsesSlotAuthority {
    generation: CredentialGenerationFingerprint,
    account: Option<Digest>,
}

impl ResponsesCredentialAuthority {
    /// Builds an exact single-primary-slot authority.
    #[must_use]
    pub const fn primary(generation: CredentialGenerationFingerprint) -> Self {
        Self {
            primary: Some(ResponsesSlotAuthority {
                generation,
                account: None,
            }),
            secondary: None,
        }
    }

    /// Builds an exact single-secondary-slot authority.
    #[must_use]
    pub const fn secondary(generation: CredentialGenerationFingerprint) -> Self {
        Self {
            primary: None,
            secondary: Some(ResponsesSlotAuthority {
                generation,
                account: None,
            }),
        }
    }

    /// Builds exact independent primary and secondary authorities.
    #[must_use]
    pub const fn primary_and_secondary(
        primary: CredentialGenerationFingerprint,
        secondary: CredentialGenerationFingerprint,
    ) -> Self {
        Self {
            primary: Some(ResponsesSlotAuthority {
                generation: primary,
                account: None,
            }),
            secondary: Some(ResponsesSlotAuthority {
                generation: secondary,
                account: None,
            }),
        }
    }

    /// Adds the exact non-secret authority for one required account-secret
    /// header slot.
    ///
    /// # Errors
    ///
    /// Rejects a missing credential slot, a duplicate account authority, or an
    /// invalid authority digest.
    pub fn with_account(
        mut self,
        slot: ResponsesAccountSlot,
        authority: Digest,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if authority.version() == 0 {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let target = match slot {
            ResponsesAccountSlot::Primary => self.primary.as_mut(),
            ResponsesAccountSlot::Secondary => self.secondary.as_mut(),
        }
        .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
        if target.account.replace(authority).is_some() {
            return Err(ResponsesAttemptBuildError::AuthSlots);
        }
        Ok(self)
    }

    /// Returns the exact authorization slots represented by this authority.
    #[must_use]
    pub const fn auth_slots(&self) -> ResponsesAuthSlots {
        match (&self.primary, &self.secondary) {
            (Some(_), None) => ResponsesAuthSlots::Primary,
            (None, Some(_)) => ResponsesAuthSlots::Secondary,
            (Some(_), Some(_)) => ResponsesAuthSlots::PrimaryAndSecondary,
            (None, None) => unreachable!(),
        }
    }

    /// Returns the exact generation fingerprint for a named slot.
    #[must_use]
    pub const fn generation(
        &self,
        slot: ResponsesAuthSlot,
    ) -> Option<CredentialGenerationFingerprint> {
        match slot {
            ResponsesAuthSlot::Primary => match &self.primary {
                Some(authority) => Some(authority.generation),
                None => None,
            },
            ResponsesAuthSlot::Secondary => match &self.secondary {
                Some(authority) => Some(authority.generation),
                None => None,
            },
        }
    }

    const fn account(&self, slot: ResponsesAccountSlot) -> Option<Digest> {
        let authority = match slot {
            ResponsesAccountSlot::Primary => &self.primary,
            ResponsesAccountSlot::Secondary => &self.secondary,
        };
        match authority {
            Some(authority) => authority.account,
            None => None,
        }
    }
}

impl ResponsesAccountSlot {
    const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary-account",
            Self::Secondary => b"secondary-account",
        }
    }
}

/// Complete closed semantic classification for one immutable request header.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesHeaderBinding {
    /// A non-secret wire value pinned exactly by the dialect.
    PublicExact {
        /// Exact header name.
        name: HeaderName,
        /// Exact canonical public bytes expected on the request.
        value: Vec<u8>,
    },
    /// A non-secret exact routing marker whose presence is authorized by one
    /// credential slot. This covers provider headers whose names look secret
    /// (for example, contain `token`) while their pinned value is public.
    CredentialMarkerExact {
        /// Exact header name.
        name: HeaderName,
        /// Exact canonical public bytes expected on the request.
        value: Vec<u8>,
        /// Credential slot that authorizes emitting the marker.
        slot: ResponsesAuthSlot,
    },
    /// A credential-bearing value bound without hashing credential bytes.
    CredentialSecret {
        /// Exact header name.
        name: HeaderName,
        /// Authorization slot whose generation fingerprint binds the value.
        slot: ResponsesAuthSlot,
    },
    /// An account-bearing value bound by a distinct non-secret authority digest.
    AccountSecret {
        /// Exact header name.
        name: HeaderName,
        /// Account authority slot.
        slot: ResponsesAccountSlot,
        /// Non-secret exact account-authority fingerprint.
        authority: Digest,
    },
}

impl ResponsesHeaderBinding {
    const fn name(&self) -> &HeaderName {
        match self {
            Self::PublicExact { name, .. }
            | Self::CredentialMarkerExact { name, .. }
            | Self::CredentialSecret { name, .. }
            | Self::AccountSecret { name, .. } => name,
        }
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

/// What a refused generation leads to.
enum RetryStep {
    /// Send the next generation.
    Send,
    /// The caller cancelled while waiting.
    Cancelled,
    /// The operation deadline passed while waiting.
    Deadline,
    /// End the logical attempt with the refusal it has.
    End,
}

/// A connection that could not be established sent nothing, and one the peer
/// broke before the request's final byte was written delivered no request.
/// Any other local failure after final authorization stays may-have-transmitted.
fn refused_connection(source: HttpFailure) -> bool {
    use plakous_http::{Phase, TransmissionFact};
    match source.transmission() {
        TransmissionFact::NotStarted => matches!(
            source.phase(),
            Phase::Resolution | Phase::Connect | Phase::Tls
        ),
        TransmissionFact::RequestIncomplete => true,
        TransmissionFact::MayHaveTransmitted => false,
    }
}

/// Whether a refused dispatch may be re-dispatched after a forced credential
/// refresh: only an HTTP 401 head, within the physical budget, while no
/// generation was possibly accepted and no hosted effect is possible.
fn credential_rejection_retry_eligible(
    error: &ResponsesAttemptError,
    unreachable: &plakous_attempt::UnreachableRideOut,
) -> bool {
    matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            class: ResponsesHttpStatusClass::Authentication,
            facts,
            retry_disabled: false,
            ..
        } if unreachable.ordinary_physical(*facts) < RetryPolicy::TURN.max_physical()
            && facts.failure_risk() == FailureRisk::None
    )
}

/// How long a retry marker may wait for observation capacity beyond the
/// retry delay itself.
const RETRY_MARKER_GRACE: Duration = Duration::from_secs(1);

/// One failed physical request the turn retry policy may replay.
struct Retryable {
    point: FailurePoint,
    cause: ModelRetryCause,
    retry_after: Option<Duration>,
    output_discarded: bool,
    detail: String,
    phase: Option<plakous_model::ModelRetryPhase>,
}

/// Classifies a failed physical request for [`RetryPolicy::TURN`]. A failure
/// after the response head is mid-stream; its partial output is discarded.
fn in_band_retryable(fault: ModelTransportFault, stream: StreamProgress) -> Retryable {
    Retryable {
        point: FailurePoint::MidStream,
        cause: ModelRetryCause::StreamInterrupted { fault },
        retry_after: None,
        output_discarded: stream.delivered,
        detail: format!("in-band refusal ({fault})"),
        phase: Some(plakous_model::ModelRetryPhase::ResponseBody),
    }
}

fn retryable(error: &ResponsesAttemptError, stream: StreamProgress) -> Option<Retryable> {
    let interrupted = |fault: ModelTransportFault, detail: String| Retryable {
        point: FailurePoint::MidStream,
        cause: ModelRetryCause::StreamInterrupted { fault },
        retry_after: None,
        output_discarded: stream.delivered,
        phase: Some(plakous_model::ModelRetryPhase::ResponseBody),
        detail,
    };
    match error {
        ResponsesAttemptError::HttpStatus {
            status,
            retry_after,
            ..
        } => {
            let point = match RetryPolicy::status_failure_point(status.as_u16())? {
                // Only a definite rejection proves the request was refused.
                FailurePoint::Refused
                    if status_outcome(*status) != PhysicalRequestOutcome::DefiniteRejection =>
                {
                    FailurePoint::AfterSend
                }
                point => point,
            };
            Some(Retryable {
                point,
                cause: ModelRetryCause::HttpStatus {
                    status: status.as_u16(),
                },
                retry_after: *retry_after,
                output_discarded: false,
                detail: format!("HTTP {}", status.as_u16()),
                phase: Some(plakous_model::ModelRetryPhase::ResponseHead),
            })
        }
        ResponsesAttemptError::Transport { source, .. } if !stream.started => {
            if refused_connection(*source) {
                return Some(Retryable {
                    point: if source.cause() == Some("peer_rejected_record") {
                        FailurePoint::RecordRejectedInTransit
                    } else if source.transmission() == plakous_http::TransmissionFact::NotStarted {
                        FailurePoint::Unreachable
                    } else {
                        FailurePoint::Refused
                    },
                    cause: if source.cause() == Some("peer_rejected_record") {
                        ModelRetryCause::RecordRejectedInTransit
                    } else {
                        ModelRetryCause::Connect
                    },
                    retry_after: None,
                    output_discarded: false,
                    detail: format!("{source:?}"),
                    phase: retry_phase(source.phase()),
                });
            }
            Some(Retryable {
                point: FailurePoint::AfterSend,
                cause: ModelRetryCause::TransportAfterSend {
                    fault: transport_fault(*source)?,
                },
                retry_after: None,
                output_discarded: false,
                detail: format!("{source:?}"),
                phase: retry_phase(source.phase()),
            })
        }
        _ if !stream.interrupted => None,
        ResponsesAttemptError::Transport { source, .. } => Some(Retryable {
            phase: retry_phase(source.phase()),
            ..interrupted(transport_fault(*source)?, format!("{source:?}"))
        }),
        ResponsesAttemptError::Deadline { .. } => Some(interrupted(
            ModelTransportFault::Timeout,
            "stream idle timeout".to_owned(),
        )),
        ResponsesAttemptError::Stream {
            source:
                source @ (ResponsesFamilyStreamError::Truncated
                | ResponsesFamilyStreamError::MissingTerminal),
            ..
        } => Some(interrupted(
            ModelTransportFault::EndOfStream,
            format!("{source:?}"),
        )),
        _ => None,
    }
}

/// The transport faults a turn may replay; cancellation, local limits, an
/// invalid response and internal failures are not transient.
const fn transport_fault(failure: HttpFailure) -> Option<ModelTransportFault> {
    match failure.kind() {
        HttpErrorKind::Io => Some(ModelTransportFault::Io),
        HttpErrorKind::Protocol => Some(ModelTransportFault::Protocol),
        HttpErrorKind::Tls => Some(ModelTransportFault::Tls),
        HttpErrorKind::DeadlineExceeded => Some(ModelTransportFault::Timeout),
        _ => None,
    }
}

/// Forces the credential refresh after a 401, bounded by the operation
/// deadline and cancellation.
async fn refresh_credential<D: Send + Sync + 'static>(
    hook: ResponsesCredentialRejectedFn<D>,
    dialect: &Arc<D>,
    attempt: &mut ResponsesAttempt,
) -> RetryStep {
    let started = monotonic_now();
    attempt.next_dispatch_started = Some(started);
    attempt.deadline = started
        .checked_add(attempt.dispatch_timeout)
        .unwrap_or(started);
    if let Some(hard) = attempt.hard_deadline {
        attempt.deadline = attempt.deadline.min(hard);
    }
    tokio::select! {
        biased;
        () = attempt.cancellation.cancelled() => RetryStep::Cancelled,
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(attempt.deadline)) => {
            RetryStep::Deadline
        }
        refreshed = hook(dialect.as_ref()) => match refreshed {
            Ok(()) => RetryStep::Send,
            Err(_) => RetryStep::End,
        },
    }
}

/// Applies [`RetryPolicy::TURN`]: announces the retry on the observation
/// channel, ordered after the failed request's events, then waits its delay
/// when the wait fits before the operation deadline.
async fn wait_to_retry(
    attempt: &mut ResponsesAttempt,
    failure: Retryable,
    waited: &mut Duration,
) -> RetryStep {
    let facts = attempt.facts();
    let elapsed = attempt
        .stream
        .physical_started
        .map_or(Duration::ZERO, |started| {
            monotonic_now().saturating_duration_since(started)
        });
    let delay = attempt.unreachable.next_delay(
        facts,
        failure.point,
        failure.retry_after,
        *waited,
        Jitter::random(),
    );
    let reason = if attempt
        .hard_deadline
        .is_some_and(|hard| monotonic_now() >= hard)
    {
        Some(Decline::Deadline)
    } else if attempt.stream.retry_disabled {
        Some(Decline::RetryDisabled)
    } else if !attempt.retry_permitted
        && !matches!(
            failure.cause,
            ModelRetryCause::Connect | ModelRetryCause::RecordRejectedInTransit
        )
    {
        Some(Decline::Workflow)
    } else if facts.hosted_effects_possible()
        || (facts.replay_safety() == ReplaySafety::ServerEffect
            && !matches!(
                failure.point,
                FailurePoint::Unreachable
                    | FailurePoint::Refused
                    | FailurePoint::RecordRejectedInTransit
            ))
    {
        Some(Decline::ServerEffect)
    } else if !attempt.long_stream_replays.permits(
        facts.physical_attempts(),
        failure.point,
        elapsed,
    ) || delay.is_none()
    {
        Some(Decline::Budget)
    } else if attempt.hard_deadline.is_some_and(|hard| {
        monotonic_now()
            .checked_add(delay.unwrap_or_default())
            .is_none_or(|resume| resume >= hard)
    }) {
        Some(Decline::Deadline)
    } else {
        None
    };
    let unsent = failure.point == FailurePoint::Unreachable;
    attempt.core.observations_ref().record_failure(
        failure.point,
        failure.phase.map(plakous_model::ModelRetryPhase::as_str),
        unsent,
        reason.map(Decline::as_str),
    );
    let delay = delay.unwrap_or_default();
    let output_discarded = failure.output_discarded && reason.is_none();
    let mut retry = ModelRetry::new(
        facts.physical_attempts().saturating_add(1),
        attempt.unreachable.max_physical(facts, failure.point),
        if reason.is_some() {
            Duration::ZERO
        } else {
            delay
        },
        failure.cause,
        output_discarded,
        failure.detail,
    )
    .with_loss(plakous_model::ModelRetryLoss {
        elapsed_ms: milliseconds(elapsed),
        server_effect: facts.replay_safety() == ReplaySafety::ServerEffect,
        transport_phase: failure.phase,
        declined_reason: reason,
        ..attempt.stream.loss
    });
    if let Some((host, route, bytes)) = &attempt.request_log {
        retry = retry.with_request(host, route, *bytes);
    }
    if reason.is_some() {
        if let Some(observations) = &attempt.observations {
            let _ = observations.try_send(ResponsesObservation::Retry(retry));
        }
        return RetryStep::End;
    }
    attempt.fresh_connection = failure.cause == ModelRetryCause::RecordRejectedInTransit;
    let resume = tokio::time::Instant::now() + delay;
    let pause = async {
        let announced = match &attempt.observations {
            None => true,
            Some(observations) => {
                let send = observations.send(ResponsesObservation::Retry(retry));
                // A closed receiver only loses a notice nobody reads.
                tokio::time::timeout(delay.max(RETRY_MARKER_GRACE), send)
                    .await
                    .is_ok()
            }
        };
        tokio::time::sleep_until(resume).await;
        announced
    };
    let announced = tokio::select! {
        biased;
        () = attempt.cancellation.cancelled() => return RetryStep::Cancelled,
        () = async {
            match attempt.hard_deadline {
                Some(hard) => tokio::time::sleep_until(tokio::time::Instant::from_std(hard)).await,
                None => std::future::pending().await,
            }
        } => return RetryStep::Deadline,
        announced = pause => announced,
    };
    // A receiver that never learns to discard the failed request's output
    // would join it to the replay's, so that replay must not happen.
    if !announced && output_discarded {
        return RetryStep::End;
    }
    *waited = waited.saturating_add(delay);
    RetryStep::Send
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

fn admit_attempt(attempt: &ResponsesAttempt) -> Result<(), FamilyCoreError> {
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
async fn execute_admitted<D, G>(
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
async fn decode_body(
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

enum PreflightFailure {
    Rejected(ResponsesDispatchRejection),
    Cancelled,
    Deadline,
}

async fn run_preflight<'a, D, G>(
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

struct StreamCollector {
    decoder: ResponsesFamilySseDecoder,
    terminal: Option<RawTerminal>,
}

impl StreamCollector {
    fn new(data: ResponsesStreamDialectData, limits: ResponsesStreamLimits) -> Self {
        Self {
            decoder: ResponsesFamilySseDecoder::new(data, limits),
            terminal: None,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<ResponsesFamilyStreamEvent>, CollectorFailure> {
        let events = self.decoder.push(bytes).map_err(CollectorFailure::Stream)?;
        self.accept(events)
    }

    fn finish(
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

struct ObservationDelivery {
    sender: Option<mpsc::Sender<ResponsesObservation>>,
    observations_delivered: usize,
    observations_dropped: usize,
    visible_observation: bool,
}

impl ObservationDelivery {
    const fn new(sender: Option<mpsc::Sender<ResponsesObservation>>) -> Self {
        Self {
            sender,
            observations_delivered: 0,
            observations_dropped: 0,
            visible_observation: false,
        }
    }

    fn deliver(&mut self, events: Vec<ResponsesFamilyStreamEvent>) {
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
enum CollectorFailure {
    Stream(ResponsesFamilyStreamError),
    MultipleTerminals,
}

enum RawTerminal {
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

    fn native(&self) -> &serde_json::Value {
        match self {
            Self::Completed(response) | Self::Incomplete(response) | Self::Failed(response) => {
                response.native()
            }
            Self::ProviderError(error) => error.native(),
        }
    }
}

fn project_terminal(
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
fn in_band_refusal(terminal: &RawTerminal) -> Option<ModelTransportFault> {
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
fn deliver(
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

fn emit_activity(attempt: &mut ResponsesAttempt, force: bool) {
    if let Some(activity) = attempt.stream.activity_to_emit(monotonic_now(), force)
        && let Some(observations) = &attempt.observations
    {
        let _ = observations.try_send(ResponsesObservation::Activity(activity));
    }
}

/// Closes a generation whose stream broke. A logical attempt that may retry
/// stays open for the replay; the retry loop fails it if it ends.
fn close_interrupted_stream(attempt: &mut ResponsesAttempt, retain: bool) {
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
fn fail_decoded_stream(
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

fn fail_stream(attempt: &mut ResponsesAttempt, failure: CollectorFailure) -> ResponsesAttemptError {
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

fn fail_observation(
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

fn fail_definite_rejection(
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

fn finish_before_dispatch(attempt: &mut ResponsesAttempt) {
    attempt.core.mark_failed();
}

fn finish_definite_rejection(attempt: &mut ResponsesAttempt) {
    finish_physical(attempt, PhysicalRequestOutcome::DefiniteRejection);
}

fn finish_after_authorization(attempt: &mut ResponsesAttempt) {
    finish_physical(attempt, PhysicalRequestOutcome::AmbiguousPostSend);
}

/// Closes a failed generation. When the logical attempt may still retry it
/// stays open; the retry loop fails it if it ends.
#[allow(
    clippy::result_large_err,
    reason = "closed attempt errors retain exact physical dispatch and terminal facts"
)]
fn retain_or_finish(
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

fn finish_physical(attempt: &mut ResponsesAttempt, outcome: PhysicalRequestOutcome) {
    let _ = attempt.core.physical_request_finished(outcome);
    attempt.core.mark_failed();
}

#[allow(
    clippy::result_large_err,
    reason = "closed attempt errors retain exact physical dispatch and terminal facts"
)]
fn ensure_running(attempt: &mut ResponsesAttempt) -> Result<(), ResponsesAttemptError> {
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
fn ensure_running_after_start(attempt: &mut ResponsesAttempt) -> Result<(), ResponsesAttemptError> {
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
async fn next_body_chunk(
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

enum BodyPull {
    Next(Option<Result<Bytes, HttpFailure>>),
    Cancelled,
    Deadline,
}

async fn read_error_body(
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

fn classify_status(status: StatusCode) -> ResponsesHttpStatusClass {
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
fn status_outcome(status: StatusCode) -> PhysicalRequestOutcome {
    if status == StatusCode::UNAUTHORIZED
        || RetryPolicy::status_failure_point(status.as_u16()) == Some(FailurePoint::Refused)
    {
        PhysicalRequestOutcome::DefiniteRejection
    } else {
        PhysicalRequestOutcome::AmbiguousPostSend
    }
}

fn response_request_id(
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

fn is_event_stream(headers: &plakous_http::Headers, allow_missing: bool) -> bool {
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

fn event_starts_visible_output(event: &ResponsesFamilyStreamEvent) -> bool {
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

fn event_is_visible(event: &ResponsesFamilyStreamEvent) -> bool {
    !matches!(
        event,
        ResponsesStreamEventKind::ResponseCreated { .. }
            | ResponsesStreamEventKind::ResponseInProgress { .. }
            | ResponsesStreamEventKind::Opaque { .. }
            | ResponsesStreamEventKind::Unknown { .. }
    )
}

fn request_body_digest(bytes: &[u8]) -> Result<Digest, ResponsesAttemptBuildError> {
    Digest::sha256(
        DigestDomain::ProviderOperation,
        REQUEST_DIGEST_REVISION,
        &[b"responses-request-body-v2", bytes],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

fn prepared_wire_digest(bytes: &[u8]) -> Result<Digest, ResponsesAttemptBuildError> {
    Digest::sha256(
        DigestDomain::Blob,
        PREPARED_WIRE_DIGEST_REVISION,
        &[b"responses-prepared-wire-body-v1", bytes],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

struct PreparedWireInspection {
    enforced_output_tokens: Option<NonZeroU32>,
    wire_occurrence_counts: Option<ResponsesWireOccurrenceCounts>,
}

fn inspect_serialized_body(
    bytes: &[u8],
    require_wire_occurrences: bool,
) -> Result<PreparedWireInspection, ResponsesAttemptBuildError> {
    let body = std::str::from_utf8(bytes)
        .ok()
        .and_then(|body| parse_unique_json_value(body).ok())
        .ok_or(ResponsesAttemptBuildError::Binding)?;
    let wire_occurrence_counts = if require_wire_occurrences {
        Some(inspect_wire_occurrences(&body).map_err(|_| ResponsesAttemptBuildError::Binding)?)
    } else {
        None
    };
    Ok(PreparedWireInspection {
        enforced_output_tokens: exact_serialized_output_limit(&body)?,
        wire_occurrence_counts,
    })
}

fn exact_serialized_output_limit(
    body: &serde_json::Value,
) -> Result<Option<NonZeroU32>, ResponsesAttemptBuildError> {
    if !body.is_object() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let Some(value) = body.get("max_output_tokens") else {
        return Ok(None);
    };
    let tokens = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .and_then(NonZeroU32::new)
        .ok_or(ResponsesAttemptBuildError::Binding)?;
    Ok(Some(tokens))
}

fn authority_target(
    authority: &ResponsesRequestAuthority,
) -> Result<ResponsesAttemptTarget, ResponsesAttemptBuildError> {
    let binding = authority.artifact_binding();
    target_identity(
        binding.dialect().as_str(),
        binding.route().as_str(),
        binding.account(),
        binding.model().as_str(),
        binding
            .model_family()
            .map_or("", plakous_model::ModelFamilyId::as_str),
        binding.adapter().as_str(),
        binding.semantic_model(),
        binding.revisions(),
    )
    .map(ResponsesAttemptTarget)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the target identity is the complete closed admitted service tuple"
)]
fn target_identity(
    protocol: &str,
    route: &str,
    account: Digest,
    model: &str,
    family: &str,
    adapter: &str,
    semantic_model: Digest,
    revisions: ResponsesSemanticRevisions,
) -> Result<Digest, ResponsesAttemptBuildError> {
    if account.domain() != DigestDomain::ModelBinding
        || semantic_model.domain() != DigestDomain::ModelBinding
    {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let family_kernel = revisions.family_kernel().to_be_bytes();
    let dialect = revisions.dialect().to_be_bytes();
    let adapter_revision = revisions.adapter().to_be_bytes();
    let account = account.identity_bytes();
    let semantic_model = semantic_model.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        ATTEMPT_TARGET_IDENTITY_REVISION,
        &[
            b"responses-attempt-target-v2",
            protocol.as_bytes(),
            route.as_bytes(),
            account.as_slice(),
            model.as_bytes(),
            family.as_bytes(),
            adapter.as_bytes(),
            semantic_model.as_slice(),
            &family_kernel,
            &dialect,
            &adapter_revision,
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

#[allow(
    clippy::too_many_arguments,
    reason = "canonical identity covers the complete closed dialect behavior table"
)]
fn dialect_execution_identity(
    dialect_revision: u32,
    target: ResponsesAttemptTarget,
    host_behavior: Digest,
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
    request_policy: ResponsesRequestPolicy,
    stream: &ResponsesStreamDialectData,
    stream_limits: ResponsesStreamLimits,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_error_policy: ResponsesProviderErrorPolicy,
    error_limits: ResponsesResponseLimits,
    provider_request_id_header: Option<&HeaderName>,
    allow_missing_sse_content_type: bool,
) -> Result<Digest, ResponsesAttemptBuildError> {
    let stream = stream
        .execution_identity()
        .ok_or(ResponsesAttemptBuildError::DialectIdentity)?;
    let replay = replay.execution_identity()?;
    let revision = dialect_revision.to_be_bytes();
    let target = target.identity().identity_bytes();
    let host_behavior = host_behavior.identity_bytes();
    let ledger = [
        u8::from(ledger_policy.require_service_admitted_for_dispatch),
        u8::from(ledger_policy.require_active_request_always),
        u8::from(ledger_policy.dual_auth_slots),
        u8::from(ledger_policy.clear_service_on_terminal_success),
    ];
    let request_policy = request_policy
        .execution_identity()
        .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)?
        .identity_bytes();
    let frame_bytes = stream_limits.max_frame_bytes().to_be_bytes();
    let total_bytes = stream_limits.max_total_bytes().to_be_bytes();
    let events = stream_limits.max_events().to_be_bytes();
    let storage = match storage {
        ResponsesStorageMode::Stateless => b"stateless".as_slice(),
        ResponsesStorageMode::ProviderStored => b"provider-stored".as_slice(),
    };
    let provider_error_policy = match provider_error_policy {
        ResponsesProviderErrorPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
        ResponsesProviderErrorPolicy::OpenRouter => b"openrouter".as_slice(),
        ResponsesProviderErrorPolicy::Grok => b"grok".as_slice(),
    };
    let response_bytes = error_limits.max_response_bytes().to_be_bytes();
    let output_items = error_limits.max_output_items().to_be_bytes();
    let unknown_bytes = error_limits.max_unknown_bytes().to_be_bytes();
    let request_id_header = provider_request_id_header.map_or("", HeaderName::as_str);
    let missing_content_type = [u8::from(allow_missing_sse_content_type)];
    Digest::sha256(
        DigestDomain::ModelBinding,
        ATTEMPT_DIALECT_IDENTITY_REVISION,
        &[
            b"responses-attempt-dialect-v3",
            revision.as_slice(),
            target.as_slice(),
            host_behavior.as_slice(),
            ledger.as_slice(),
            auth_slots.identity_label(),
            request_policy.as_slice(),
            &stream.identity_bytes(),
            frame_bytes.as_slice(),
            total_bytes.as_slice(),
            events.as_slice(),
            &replay.identity_bytes(),
            storage,
            provider_error_policy,
            response_bytes.as_slice(),
            output_items.as_slice(),
            unknown_bytes.as_slice(),
            request_id_header.as_bytes(),
            missing_content_type.as_slice(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)
}

fn host_behavior_identity<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
) -> Result<Digest, ResponsesAttemptBuildError> {
    let transport = client
        .behavior_identity()
        .ok_or(ResponsesAttemptBuildError::DialectIdentity)?
        .as_bytes();
    let preflight = authorization.preflight_revision.to_be_bytes();
    let final_authorization = authorization.final_authorization_revision.to_be_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        HOST_BEHAVIOR_IDENTITY_REVISION,
        &[
            b"responses-attempt-host-behavior-v11",
            transport.as_slice(),
            preflight.as_slice(),
            final_authorization.as_slice(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)
}

/// Top-level Responses fields whose replay can only cost duplicate inference.
/// `store` and `previous_response_id` only persist or read the response
/// itself; `conversation`, `background` and every unlisted field may have an
/// effect beyond inference.
const COST_ONLY_FIELDS: &[&str] = &[
    "include",
    "input",
    "instructions",
    "max_output_tokens",
    "metadata",
    "model",
    "parallel_tool_calls",
    "plugins",
    "previous_response_id",
    "prompt_cache_key",
    "provider",
    "reasoning",
    "service_tier",
    "session_id",
    "store",
    "stream",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
];

/// Classifies the exact lowered body by allowlist: only `function` and
/// `custom` tools, disabled gateway plugins, and the fields above are
/// cost-only. A hosted tool or any unclassified field is a server effect.
fn lowered_replay_safety(
    lowered: &LoweredResponsesRequest,
) -> Result<ReplaySafety, ResponsesAttemptBuildError> {
    let serde_json::Value::Object(fields) = lowered.body() else {
        return Err(ResponsesAttemptBuildError::Binding);
    };
    let mut safety = ReplaySafety::CostOnly;
    for (name, value) in fields {
        let cost_only = match name.as_str() {
            "tools" => {
                let tools = value
                    .as_array()
                    .ok_or(ResponsesAttemptBuildError::Binding)?;
                let mut client_only = true;
                for tool in tools {
                    let kind = tool
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(ResponsesAttemptBuildError::Binding)?;
                    client_only &= matches!(kind, "function" | "custom");
                }
                client_only
            }
            "plugins" => value.as_array().is_some_and(|plugins| {
                plugins
                    .iter()
                    .all(|plugin| plugin.get("enabled") == Some(&serde_json::Value::Bool(false)))
            }),
            name => COST_ONLY_FIELDS.contains(&name),
        };
        if !cost_only {
            safety = ReplaySafety::ServerEffect;
        }
    }
    Ok(safety)
}

fn validate_auth_slots(
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
) -> Result<(), ResponsesAttemptBuildError> {
    if matches!(
        auth_slots,
        ResponsesAuthSlots::Secondary | ResponsesAuthSlots::PrimaryAndSecondary
    ) && !ledger_policy.dual_auth_slots
    {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    Ok(())
}

fn physical_request_digest(
    physical: &ResponsesPhysicalRequestBinding,
    credential_authority: &ResponsesCredentialAuthority,
    request: &Request,
    header_bindings: &[ResponsesHeaderBinding],
) -> Result<(Digest, Digest), ResponsesAttemptBuildError> {
    if physical.attempt().operation().auth_slots() != credential_authority.auth_slots() {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    validate_header_authority(header_bindings, credential_authority)?;
    if request.headers().len() != header_bindings.len() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let mut headers = Vec::with_capacity(header_bindings.len());
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in request.headers().iter() {
        if !seen.insert(name.as_str()) {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let mut matching = header_bindings
            .iter()
            .filter(|binding| binding.name() == name);
        let binding = matching.next().ok_or(ResponsesAttemptBuildError::Binding)?;
        if matching.next().is_some() {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let mut field = Vec::new();
        append_framed(&mut field, name.as_str().as_bytes())?;
        match binding {
            ResponsesHeaderBinding::PublicExact {
                value: expected, ..
            } => {
                if header_name_requires_secret(name) || value.as_bytes() != expected.as_slice() {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"public-exact")?;
                append_framed(&mut field, value.as_bytes())?;
            }
            ResponsesHeaderBinding::CredentialMarkerExact {
                value: expected,
                slot,
                ..
            } => {
                if !header_name_requires_secret(name)
                    || value.as_bytes() != expected.as_slice()
                    || credential_authority.generation(*slot).is_none()
                {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"credential-marker-exact")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, value.as_bytes())?;
            }
            ResponsesHeaderBinding::CredentialSecret { slot, .. } => {
                if !header_name_requires_secret(name) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                let generation = credential_authority
                    .generation(*slot)
                    .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
                append_framed(&mut field, b"credential-secret")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, generation.as_bytes())?;
            }
            ResponsesHeaderBinding::AccountSecret {
                slot, authority, ..
            } => {
                if !header_name_requires_secret(name) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                if credential_authority.account(*slot) != Some(*authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"account-secret")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, &authority.identity_bytes())?;
            }
        }
        headers.push(field);
    }
    if seen.len() != header_bindings.len() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    headers.sort();
    let operation = physical.attempt().operation();
    let authority = operation.request_authority.identity_bytes();
    let execution = operation.execution.identity_bytes();
    let attempt_id = physical.attempt().attempt_id_ref().to_string();
    let generation = physical.generation().to_be_bytes();
    let route = Digest::sha256(
        DigestDomain::ModelBinding,
        REQUEST_DIGEST_REVISION,
        &[
            b"responses-physical-route-v2",
            execution.as_slice(),
            request.origin().binding_authority().as_bytes(),
            request.path_and_query().as_str().as_bytes(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)?;
    let route_bytes = route.identity_bytes();
    let hosted_effect = [u8::from(
        operation.replay_safety() == ReplaySafety::ServerEffect,
    )];
    let mut parts = vec![
        b"responses-physical-request-v2".as_slice(),
        authority.as_slice(),
        execution.as_slice(),
        route_bytes.as_slice(),
        attempt_id.as_bytes(),
        generation.as_slice(),
        credential_authority.auth_slots().identity_label(),
        hosted_effect.as_slice(),
        request.method().as_str().as_bytes(),
        request.path_and_query().as_str().as_bytes(),
        request.body().as_ref(),
    ];
    parts.extend(headers.iter().map(Vec::as_slice));
    let request = Digest::sha256(
        DigestDomain::ProviderOperation,
        REQUEST_DIGEST_REVISION,
        &parts,
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)?;
    Ok((route, request))
}

fn validate_header_authority(
    header_bindings: &[ResponsesHeaderBinding],
    authority: &ResponsesCredentialAuthority,
) -> Result<(), ResponsesAttemptBuildError> {
    let mut primary_credential = 0_u8;
    let mut secondary_credential = 0_u8;
    let mut primary_account = 0_u8;
    let mut secondary_account = 0_u8;
    for binding in header_bindings {
        let count = match binding {
            ResponsesHeaderBinding::CredentialSecret {
                slot: ResponsesAuthSlot::Primary,
                ..
            } => Some(&mut primary_credential),
            ResponsesHeaderBinding::CredentialSecret {
                slot: ResponsesAuthSlot::Secondary,
                ..
            } => Some(&mut secondary_credential),
            ResponsesHeaderBinding::AccountSecret {
                slot: ResponsesAccountSlot::Primary,
                authority: binding_authority,
                ..
            } => {
                if authority.account(ResponsesAccountSlot::Primary) != Some(*binding_authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                Some(&mut primary_account)
            }
            ResponsesHeaderBinding::AccountSecret {
                slot: ResponsesAccountSlot::Secondary,
                authority: binding_authority,
                ..
            } => {
                if authority.account(ResponsesAccountSlot::Secondary) != Some(*binding_authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                Some(&mut secondary_account)
            }
            ResponsesHeaderBinding::PublicExact { .. }
            | ResponsesHeaderBinding::CredentialMarkerExact { .. } => None,
        };
        if let Some(count) = count {
            *count = (*count)
                .checked_add(1)
                .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
        }
    }
    let exact = primary_credential
        == u8::from(authority.generation(ResponsesAuthSlot::Primary).is_some())
        && secondary_credential
            == u8::from(authority.generation(ResponsesAuthSlot::Secondary).is_some())
        && primary_account == u8::from(authority.account(ResponsesAccountSlot::Primary).is_some())
        && secondary_account
            == u8::from(authority.account(ResponsesAccountSlot::Secondary).is_some());
    if !exact {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    Ok(())
}

fn append_framed(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ResponsesAttemptBuildError> {
    let length = u32::try_from(value.len()).map_err(|_| ResponsesAttemptBuildError::Binding)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

fn header_name_requires_secret(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.contains("authorization")
        || name.contains("api-key")
        || name.contains("apikey")
        || name.contains("token")
        || name.contains("cookie")
        || name.contains("account")
}

fn retry_phase(phase: plakous_http::Phase) -> Option<plakous_model::ModelRetryPhase> {
    use plakous_http::Phase;
    use plakous_model::ModelRetryPhase as RetryPhase;
    Some(match phase {
        Phase::Resolution => RetryPhase::Resolution,
        Phase::Connect => RetryPhase::Connect,
        Phase::Tls => RetryPhase::Tls,
        Phase::RequestWrite => RetryPhase::RequestWrite,
        Phase::RequestWriteOrResponseHead => RetryPhase::RequestWriteOrResponseHead,
        Phase::ResponseHead => RetryPhase::ResponseHead,
        Phase::ResponseBody => RetryPhase::ResponseBody,
        Phase::Cleanup => RetryPhase::Cleanup,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "attempt_host_tests.rs"]
mod tests;
