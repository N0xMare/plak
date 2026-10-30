//! Thin provider-neutral model construction for the OpenAI Responses dialect.

mod execution;
mod platform;

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[cfg(all(feature = "native-auth", test))]
use std::num::NonZeroU32;

#[cfg(feature = "native-auth")]
use plakous_attempt::LedgerPolicy;
use plakous_model::RuntimeModelAuthorityError as PlatformRuntimeAuthorityError;
#[cfg(feature = "native-auth")]
use plakous_model::{
    Capability, CountCertificateKind, CountObservation, Digest, LanguageModel,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelRequest,
    PreparedCacheUse, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
    PreparedOutputLimit, PreparedTokenProjection, ProtocolId, ReplayKindId, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Support, ToolPresentationMode,
    ValidatedRequestPlan,
};
#[cfg(all(feature = "native-auth", test))]
use plakous_model::{
    CapacityCertificate, ContextContract, ContextEvidenceClass, ContextEvidenceSource,
    DigestDomain, EvidenceFreshness, MeasurementCertificate, SafeLimit,
};
use plakous_model::{
    CapacityRule, Clock, CompiledProviderConfiguration, ModelAdapterProfileId, ModelFamilyId,
    ModelId, ReasoningEffortDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    SelectedProviderConfiguration, SystemClock, Timestamp,
};
use plakous_responses::ResponsesStreamLimits;
#[cfg(feature = "native-auth")]
use plakous_responses::{
    PreparedResponsesModelExecution, ResponsesArtifactReplayBridge, ResponsesAttemptDialectData,
    ResponsesAttemptService, ResponsesAttemptTarget, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData, ResponsesProviderErrorPolicy,
    ResponsesReplayKind, ResponsesReplayProjection, ResponsesRequestAuthority,
    ResponsesResponseLimits, ResponsesStreamDialectData, canonical_local_tools,
    is_native_compaction_request, prepare_responses_attempt, start_prepared_responses_model_call,
};
use plakous_responses::{
    ResponsesProviderConfigurationError, ResponsesRequestPolicy, ResponsesStoragePolicy,
    compile_provider_configuration,
};
#[cfg(feature = "native-auth")]
use plakous_responses::{ResponsesSemanticRevisions, ResponsesStorageMode};
use thiserror::Error;

#[cfg(feature = "native-auth")]
mod usage_limit;

use crate::OPENAI_RESPONSES_ADAPTER_REVISION;
#[cfg(feature = "native-auth")]
use crate::config::OPENAI_RESPONSES_DIALECT_REVISION;
use crate::manifest::admit_reviewed_platform_model;
#[cfg(feature = "native-auth")]
use crate::native_auth::{
    CodexCatalogToolMode, CodexDispatchGuard, CodexModelResolutionEvidence, CodexReceiptRenewal,
    CodexRenewalFloor, CodexResponsesDialect, codex_credential_rejected, codex_responses_preflight,
    revalidate_codex_responses,
};
use crate::{
    ConfigError, CredentialReference, EndpointOrigin, OPENAI_PLATFORM_ORIGIN, OpenAiKeySource,
    OpenAiPlatformRouteConfig, OpenAiTarget, PlatformStoragePolicy,
    manifest::OPENAI_GPT_6_PLATFORM_PROFILE_ID, manifest::OPENAI_GPT_56_PLATFORM_PROFILE_ID,
};

const DEFAULT_OPENAI_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_OPENAI_EVENT_CHANNEL_CAPACITY: usize = 64;
#[cfg(feature = "native-auth")]
const CODEX_RESPONSES_PREFLIGHT_REVISION: u32 = 2;
#[cfg(feature = "native-auth")]
const CODEX_RESPONSES_FINAL_AUTHORIZATION_REVISION: u32 = 1;
#[cfg(feature = "native-auth")]
const RESPONSES_FAMILY_ID: &str = "responses-family";
#[cfg(feature = "native-auth")]
const RESPONSES_CANONICAL_PROJECTION_REVISION: &str = "responses-canonical-projection-v4";
#[cfg(feature = "native-auth")]
const RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION: &str = "responses-exact-wire-accounting-v3";
#[cfg(feature = "native-auth")]
/// Pinned Codex CLI estimates ordinary JSON items as `ceil(utf8_bytes / 4)`.
const CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN: u64 = 4;
#[cfg(all(feature = "native-auth", test))]
const TEST_MEASUREMENT_CERTIFICATE_ID: &str = "openai-test-complete-wire-byte-bound";
#[cfg(all(feature = "native-auth", test))]
const TEST_MEASUREMENT_CERTIFICATE_REVISION: &str = "fixture-v1";
#[cfg(all(feature = "native-auth", test))]
const TEST_OUTPUT_HARD_CAP: u32 = 64;

/// Installs exact synthetic context evidence on one test-only resolved descriptor.
#[cfg(all(feature = "native-auth", test))]
pub(crate) fn with_test_context_contract(
    descriptor: ResolvedModelDescriptor,
    observed_at: Timestamp,
) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error + Send + Sync>> {
    let expires_at = Timestamp::new(observed_at.seconds().saturating_add(3_600), 0)?;
    let freshness = EvidenceFreshness::new(observed_at, Some(expires_at))?;
    let corpus = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"openai-test-complete-wire-byte-bound-corpus-v1"],
    )?;
    let capacity = CapacityCertificate::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "openai-test-capacity",
        "fixture-v1",
        ContextEvidenceSource::ReleaseCertificate,
        CapacityRule::SharedWindowWithOutputCap {
            total: SafeLimit::Exact(10_000_000),
            output: SafeLimit::Exact(u64::from(TEST_OUTPUT_HARD_CAP)),
        },
        None,
        freshness,
    )?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        TEST_MEASUREMENT_CERTIFICATE_ID,
        TEST_MEASUREMENT_CERTIFICATE_REVISION,
        "complete-wire-byte-upper-bound",
        "fixture-v1",
        ContextEvidenceSource::ReleaseCertificate,
        CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
        None,
        freshness,
    )?;
    Ok(descriptor.with_context_contract(ContextContract::new(
        ProtocolId::new(RESPONSES_FAMILY_ID)?,
        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
        RESPONSES_CANONICAL_PROJECTION_REVISION,
        "openai-test-output-reservation-v1",
        capacity,
        vec![measurement],
    )?))
}

/// Immutable logical-operation and shared stream bounds for Codex Responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StandardOpenAiHttpConfig {
    operation_timeout: Duration,
    event_channel_capacity: usize,
    responses_limits: ResponsesStreamLimits,
}

impl Default for StandardOpenAiHttpConfig {
    fn default() -> Self {
        Self {
            operation_timeout: DEFAULT_OPENAI_OPERATION_TIMEOUT,
            event_channel_capacity: DEFAULT_OPENAI_EVENT_CHANNEL_CAPACITY,
            responses_limits: ResponsesStreamLimits::default(),
        }
    }
}

impl StandardOpenAiHttpConfig {
    /// Creates explicit nonzero operation, delivery, and family decoder bounds.
    pub fn new(
        operation_timeout: Duration,
        event_channel_capacity: usize,
        responses_limits: ResponsesStreamLimits,
    ) -> Result<Self, StandardOpenAiHttpBuildError> {
        if operation_timeout.is_zero() || event_channel_capacity == 0 {
            return Err(StandardOpenAiHttpBuildError::InvalidPolicy);
        }
        Ok(Self {
            operation_timeout,
            event_channel_capacity,
            responses_limits,
        })
    }

    /// Returns the dispatch timeout: it bounds preflight, retry waits and each
    /// exchange up to its response head, never a streaming body.
    #[must_use]
    pub const fn operation_timeout(self) -> Duration {
        self.operation_timeout
    }

    /// Returns the bounded live family-event channel capacity.
    #[must_use]
    pub const fn event_channel_capacity(self) -> usize {
        self.event_channel_capacity
    }

    /// Returns the shared family stream bounds.
    #[must_use]
    pub const fn responses_limits(self) -> ResponsesStreamLimits {
        self.responses_limits
    }
}

/// Failure to construct a strict shared-host OpenAI model.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum StandardOpenAiHttpBuildError {
    /// A duration or channel bound was zero.
    #[error("OpenAI shared-host policy bounds must be nonzero")]
    InvalidPolicy,
    /// The selected target is not an admitted Codex subscription target.
    #[error("OpenAI shared-host execution requires an admitted Codex target")]
    UnsupportedRoute,
    /// A shared Responses construction fact was invalid.
    #[error("OpenAI shared Responses construction failed")]
    SharedBinding,
}

/// Closed literal model selection admitted by the reviewed Platform table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiPlatformModelFacts {
    /// `gpt-6-sol`.
    Gpt6Sol,
    /// `gpt-6-luna`.
    Gpt6Luna,
    /// `gpt-5.6-sol`.
    Gpt56Sol,
    /// `gpt-5.6-terra`.
    Gpt56Terra,
    /// `gpt-5.6-luna`.
    Gpt56Luna,
}

impl OpenAiPlatformModelFacts {
    /// Every model the reviewed Platform table admits, in listing order.
    pub const ALL: [Self; 5] = [
        Self::Gpt6Luna,
        Self::Gpt6Sol,
        Self::Gpt56Luna,
        Self::Gpt56Sol,
        Self::Gpt56Terra,
    ];

    /// The row for one exact literal model id.
    #[must_use]
    pub fn from_model_id(model: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|facts| facts.model_id() == model)
    }

    /// Input window in tokens. `GET /v1/models` reports none; this is the
    /// `context_window` the authenticated Codex catalog reports for the same
    /// model literals (observed 2026-09-23), the vendor's own value for them.
    #[must_use]
    pub const fn context_window(self) -> u64 {
        272_000
    }

    /// Maximum output tokens from the reviewed Platform row.
    #[must_use]
    pub const fn max_output_tokens(self) -> u32 {
        crate::manifest::OPENAI_PLATFORM_MAX_OUTPUT_TOKENS
    }

    /// The reviewed Platform harness profile this row runs under.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        match self {
            Self::Gpt6Sol | Self::Gpt6Luna => OPENAI_GPT_6_PLATFORM_PROFILE_ID,
            Self::Gpt56Sol | Self::Gpt56Terra | Self::Gpt56Luna => {
                OPENAI_GPT_56_PLATFORM_PROFILE_ID
            }
        }
    }

    /// Returns the requested model.
    #[must_use]
    pub const fn model_id(self) -> &'static str {
        match self {
            Self::Gpt6Sol => "gpt-6-sol",
            Self::Gpt6Luna => "gpt-6-luna",
            Self::Gpt56Sol => "gpt-5.6-sol",
            Self::Gpt56Terra => "gpt-5.6-terra",
            Self::Gpt56Luna => "gpt-5.6-luna",
        }
    }

    fn identity(self) -> (ModelId, ModelFamilyId, ModelAdapterProfileId) {
        let model = ModelId::new(self.model_id())
            .unwrap_or_else(|error| unreachable!("static model ID is valid: {error}"));
        let family_id = match self {
            Self::Gpt6Sol | Self::Gpt6Luna => "gpt-6",
            Self::Gpt56Sol | Self::Gpt56Terra | Self::Gpt56Luna => "gpt-5.6",
        };
        let profile_id = self.profile_id();
        let family = ModelFamilyId::new(family_id)
            .unwrap_or_else(|error| unreachable!("static family ID is valid: {error}"));
        let profile = ModelAdapterProfileId::new(profile_id)
            .unwrap_or_else(|error| unreachable!("static profile ID is valid: {error}"));
        (model, family, profile)
    }
}

/// Failure to construct or bind exact Platform execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenAiPlatformResolveError {
    /// Retained route facts were invalid.
    #[error("OpenAI Platform route facts are invalid")]
    Configuration(#[from] ConfigError),
    /// Fresh runtime authority could not be validated.
    #[error("OpenAI Platform runtime authority is invalid")]
    Runtime(#[from] PlatformRuntimeAuthorityError),
    /// Configured execution outputs disagree.
    #[error("OpenAI Platform configured binding is invalid")]
    Binding(#[from] plakous_model::ProviderConfigurationError),
    /// Complete execution construction failed.
    #[error("OpenAI Platform execution construction failed")]
    Execution(#[from] crate::OpenAiPlatformBuildError),
}

/// Builder for exact Platform model, credential and context authority.
#[derive(Clone)]
pub struct OpenAiPlatformModelBuilder {
    credential: CredentialReference,
    source: Arc<OpenAiKeySource>,
    facts: OpenAiPlatformModelFacts,
    context_envelope: Option<std::num::NonZeroU64>,
    storage: PlatformStoragePolicy,
    project: crate::PlatformProjectPolicy,
    http: StandardOpenAiHttpConfig,
    clock: Arc<dyn Clock>,
    /// Opaque bytes the product admitted this session under. Appended to
    /// semantic evidence; empty leaves the previous identity unchanged.
    admitted_facts: Vec<u8>,
}

/// Sealed exact authority for Platform configuration and execution.
#[derive(Clone)]
pub struct OpenAiPlatformConfigurationAuthority {
    target: OpenAiTarget,
    http: StandardOpenAiHttpConfig,
    admitted_facts: Vec<u8>,
}

/// Codex subscription model whose execution is supplied only by the shared Responses host.
#[cfg(feature = "native-auth")]
///
/// The admitted descriptor is immutable. The credential receipt behind it is
/// renewed before dispatch when the shared store refreshed the grant.
pub struct OpenAiLanguageModel {
    target: std::sync::Mutex<Arc<CodexDispatchTarget>>,
    renewal: tokio::sync::Mutex<()>,
    renewal_floor: CodexRenewalFloor,
    receipt_renewal: CodexReceiptRenewal,
    admitted_from: Timestamp,
    admitted_until: Timestamp,
    client: plakous_http::Client,
    config: StandardOpenAiHttpConfig,
    attempt_data: ResponsesAttemptDialectData,
    descriptor: ResolvedModelDescriptor,
}

/// One exact credential receipt bound to the shared Responses host. A renewal
/// replaces the whole target; an undispatched prepared call reads the current
/// validated service only when its one-shot start is consumed.
#[cfg(feature = "native-auth")]
struct CodexDispatchTarget {
    target: OpenAiTarget,
    service: CodexDispatchService,
}

#[cfg(feature = "native-auth")]
type CodexDispatchService = Arc<ResponsesAttemptService<CodexResponsesDialect, CodexDispatchGuard>>;

#[cfg(feature = "native-auth")]
fn codex_authorization() -> Result<
    ResponsesAuthorizationAdapter<CodexResponsesDialect, CodexDispatchGuard>,
    StandardOpenAiHttpBuildError,
> {
    Ok(ResponsesAuthorizationAdapter::new(
        CODEX_RESPONSES_PREFLIGHT_REVISION,
        CODEX_RESPONSES_FINAL_AUTHORIZATION_REVISION,
        codex_responses_preflight,
        revalidate_codex_responses,
    )
    .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?
    .with_credential_rejected(codex_credential_rejected)
    .with_transport_failed(CodexDispatchGuard::transport_failed))
}

#[cfg(feature = "native-auth")]
fn codex_dispatch_service(
    target: &OpenAiTarget,
    client: &plakous_http::Client,
    attempt_data: &ResponsesAttemptDialectData,
) -> Result<CodexDispatchService, StandardOpenAiHttpBuildError> {
    let dialect = Arc::new(CodexResponsesDialect::new(target.clone(), client.clone()));
    Ok(Arc::new(
        ResponsesAttemptService::new(
            client.clone(),
            dialect,
            attempt_data.clone(),
            codex_authorization()?,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
    ))
}

#[cfg(feature = "native-auth")]
impl OpenAiLanguageModel {
    fn artifact_preparation_error(
        error: plakous_responses::ResponsesRequestError,
    ) -> ModelPreparationError {
        preparation_rejection(
            ModelPreparationErrorKind::ArtifactReuse,
            format!("Codex active Responses artifact is not reusable: {error}"),
        )
    }

    /// Binds an admitted native target to the provider-blind HTTP client.
    pub(crate) fn new_codex(
        target: OpenAiTarget,
        client: plakous_http::Client,
        config: StandardOpenAiHttpConfig,
        receipt_renewal: CodexReceiptRenewal,
    ) -> Result<Self, StandardOpenAiHttpBuildError> {
        if !target.route().is_native_codex() {
            return Err(StandardOpenAiHttpBuildError::UnsupportedRoute);
        }
        if config.operation_timeout.is_zero() || config.event_channel_capacity == 0 {
            return Err(StandardOpenAiHttpBuildError::InvalidPolicy);
        }
        let descriptor = target.descriptor().clone();
        let replay = ResponsesReplayProjection::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            ReplayKindId::new("responses.output_item")
                .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
            1,
            target
                .route()
                .account_binding()
                .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?
        .requiring_encrypted_reasoning_with_optional_status();
        let response_limits = ResponsesResponseLimits::default();
        // Keep the production Codex transport on the exact Codex stream
        // profile.  In particular, the reference client treats the first
        // valid typed terminal as the response-body boundary; buffered
        // transport trailers after it are not a second semantic event.
        let stream = ResponsesStreamDialectData::codex(descriptor.requested_model().as_str());
        let request_policy = codex_request_policy(&descriptor, target.codex_responses_lite());
        let authorization = codex_authorization()?;
        // The descriptor's route evidence is valid only inside the admitting
        // catalog's window; renewal never carries a target out of it.
        let (admitted_from, admitted_until) = target
            .native_codex_catalog_window()
            .ok_or(StandardOpenAiHttpBuildError::UnsupportedRoute)?;
        let revisions =
            codex_semantic_revisions().map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?;
        let attempt_target = ResponsesAttemptTarget::from_admitted_target(
            &descriptor,
            target
                .route()
                .account_binding()
                .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
            revisions,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?;
        let attempt_data = ResponsesAttemptDialectData::new(
            OPENAI_RESPONSES_DIALECT_REVISION,
            attempt_target,
            &client,
            &authorization,
            LedgerPolicy::OPENAI_STRICT,
            ResponsesAuthSlots::Secondary,
            request_policy,
            stream,
            config.responses_limits,
            replay,
            ResponsesStorageMode::Stateless,
            ResponsesProviderErrorPolicy::OpenAiCompatible,
            response_limits,
            Some(plakous_http::HeaderName::from_static("x-request-id")),
            true,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?
        .with_wrapped_overload();
        let service = codex_dispatch_service(&target, &client, &attempt_data)?;
        Ok(Self {
            target: std::sync::Mutex::new(Arc::new(CodexDispatchTarget { target, service })),
            renewal: tokio::sync::Mutex::new(()),
            renewal_floor: CodexRenewalFloor::default(),
            receipt_renewal,
            admitted_from,
            admitted_until,
            client,
            config,
            attempt_data,
            descriptor,
        })
    }

    /// The session's hosted web search side-request executor on this exact
    /// route, login, model and client.
    #[must_use]
    pub fn codex_retrieval(self: &Arc<Self>) -> crate::CodexRetrieval {
        crate::CodexRetrieval::new(Arc::clone(self))
    }

    /// The session's image generation executor on this exact route, login
    /// and client.
    #[must_use]
    pub fn codex_images(self: &Arc<Self>) -> crate::CodexImages {
        crate::CodexImages::new(Arc::clone(self))
    }

    pub(crate) const fn client(&self) -> &plakous_http::Client {
        &self.client
    }

    pub(crate) const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Revalidates admission exactly as before a model round (renewing a
    /// stale receipt), then authorizes one physical Codex search side
    /// request carrying `body` on the current receipt.
    pub(crate) async fn codex_side_request(
        self: &Arc<Self>,
        path: &'static str,
        body: &[u8],
        deadline: Instant,
        cancellation: &plakous_model::CancellationToken,
    ) -> Result<(plakous_http::Request, CodexDispatchGuard), &'static str> {
        let checked_at = self
            .current_target()
            .map_err(|_| "Codex admission is no longer current")?
            .target
            .clock()
            .now();
        Arc::clone(self)
            .revalidate_runtime_authority(checked_at)
            .await
            .map_err(|_| "Codex admission is no longer current")?;
        let current = self
            .current_target()
            .map_err(|_| "Codex target is unavailable")?;
        crate::native_auth::codex_side_request(
            &current.target,
            &self.client,
            path,
            body,
            deadline,
            cancellation,
        )
        .await
    }

    /// One claimed refresh of the session's login after a side request's
    /// HTTP 401, as a model round gets; `true` when the grant was renewed.
    pub(crate) async fn codex_side_request_rejected(&self) -> bool {
        let Ok(current) = self.current_target() else {
            return false;
        };
        crate::native_auth::codex_side_request_rejected(&current.target).await
    }

    fn current_target(&self) -> Result<Arc<CodexDispatchTarget>, RuntimeModelAuthorityError> {
        self.target
            .lock()
            .map(|target| Arc::clone(&target))
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)
    }

    async fn revalidate_target(
        &self,
        target: &CodexDispatchTarget,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        if checked_at >= self.admitted_until {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        target
            .target
            .revalidate_native_runtime_authority(checked_at)
            .await
    }

    /// Replaces a stale receipt before any dispatch: one claimed refresh when
    /// due and one exact catalog fetch of the same login selection, accepted
    /// only for the same route identity and admitted row. Concurrent callers
    /// note their `checked_at`, wait for one renewal and then check the
    /// receipt it installed, whose observation never postdates the earliest
    /// noted check.
    async fn renew(
        &self,
        stale: Arc<CodexDispatchTarget>,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        // Outside the admitted window no renewal can help: refuse before any
        // refresh, catalog request or wait.
        if checked_at < self.admitted_from || checked_at >= self.admitted_until {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        self.renewal_floor.note(checked_at);
        let _renewal = self.renewal.lock().await;
        // A renewal that failed while this caller waited cleared the floor.
        self.renewal_floor.note(checked_at);
        let current = self.current_target()?;
        if !Arc::ptr_eq(&current, &stale) {
            return self.revalidate_target(&current, checked_at).await;
        }
        let renewed = self
            .receipt_renewal
            .renew(&stale.target, self.admitted_until, &self.renewal_floor)
            .await;
        self.renewal_floor.clear();
        let target = renewed?;
        let service = codex_dispatch_service(&target, &self.client, &self.attempt_data)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let renewed = Arc::new(CodexDispatchTarget { target, service });
        let authority = self.revalidate_target(&renewed, checked_at).await?;
        *self
            .target
            .lock()
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)? = renewed;
        Ok(authority)
    }
}

/// At least the pinned client's estimate: whole serialized bytes / 4, with
/// encrypted reasoning and compaction items substituted by `estimate_reasoning_length`.
#[cfg(feature = "native-auth")]
pub(crate) fn codex_complete_input_token_upper_bound(wire_size: u64) -> Option<u64> {
    wire_size
        .checked_add(CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN - 1)
        .map(|rounded| rounded / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN)
}

/// Codex CLI `estimate_reasoning_length`: base64-decode heuristic, then bytes.
#[cfg(feature = "native-auth")]
fn codex_encrypted_item_visible_bytes(encrypted_len: u64) -> Option<u64> {
    encrypted_len
        .checked_mul(3)?
        .checked_div(4)
        .map(|decoded| decoded.saturating_sub(650))
}

/// Reference-client complete-input tokens for one serialized Codex body.
///
/// Reasoning and compaction items with encrypted content are substituted with
/// [`codex_encrypted_item_visible_bytes`]
/// before the ordinary `ceil(bytes/4)` bound. Missing/unparseable bodies stay
/// on the naive whole-body bound.
#[cfg(feature = "native-auth")]
pub(crate) fn codex_reference_client_input_tokens(body: &[u8]) -> Option<u64> {
    let wire_size = u64::try_from(body.len()).ok()?;
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return codex_complete_input_token_upper_bound(wire_size);
    };
    let Some(input) = value.get("input").and_then(serde_json::Value::as_array) else {
        return codex_complete_input_token_upper_bound(wire_size);
    };
    let mut json_bytes = 0_u64;
    let mut visible_bytes = 0_u64;
    for item in input {
        if !matches!(
            item.get("type").and_then(serde_json::Value::as_str),
            Some("compaction" | "reasoning")
        ) {
            continue;
        }
        let Some(encrypted) = item
            .get("encrypted_content")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let encoded = serde_json::to_vec(item).ok()?;
        json_bytes = json_bytes.checked_add(u64::try_from(encoded.len()).ok()?)?;
        visible_bytes = visible_bytes.checked_add(codex_encrypted_item_visible_bytes(
            u64::try_from(encrypted.len()).ok()?,
        )?)?;
    }
    let adjusted = wire_size
        .checked_sub(json_bytes)?
        .checked_add(visible_bytes)?;
    codex_complete_input_token_upper_bound(adjusted)
}

#[cfg(feature = "native-auth")]
fn preparation_rejection(
    kind: ModelPreparationErrorKind,
    message: impl AsRef<str>,
) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

#[cfg(feature = "native-auth")]
fn validate_codex_preparation_authority(
    target: &OpenAiTarget,
    authority: &RuntimeModelAuthority,
    prepared_at: Timestamp,
) -> Result<(), ModelPreparationError> {
    if authority.observed_at() > prepared_at {
        return Err(preparation_rejection(
            ModelPreparationErrorKind::InvalidConfiguration,
            "Codex runtime authority is newer than preparation time",
        ));
    }
    if authority
        .valid_until()
        .is_some_and(|valid_until| valid_until <= prepared_at)
    {
        return Err(preparation_rejection(
            ModelPreparationErrorKind::InvalidConfiguration,
            "Codex runtime authority expired before preparation",
        ));
    }
    authority.validate(target.descriptor(), prepared_at)?;
    let (credential_fence, _) = target
        .native_codex_credential_selection()
        .ok_or(RuntimeModelAuthorityError::Unavailable)?;
    if authority.account_binding() != target.account_binding()
        || authority.credential_authority_binding() != target.credential_authority_binding()
        || authority.credential_generation() != credential_fence.generation().get()
    {
        return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
    }
    Ok(())
}

#[cfg(feature = "native-auth")]
fn codex_artifact_replay_bridge(
    active: Option<&plakous_model::ActiveFamilyArtifact>,
    request: &ModelRequest,
    authority: ResponsesRequestAuthority,
    now: Timestamp,
) -> Result<Option<ResponsesArtifactReplayBridge>, plakous_responses::ResponsesRequestError> {
    let Some(artifact) = active else {
        return Ok(None);
    };
    match ResponsesArtifactReplayBridge::from_active_artifact(
        artifact,
        request,
        authority,
        ResponsesReplayKind::new("responses.output_item", 1),
        None,
        None,
        now,
    ) {
        Ok(bridge) => Ok(Some(bridge)),
        // Not this request's artifact: prepare fresh. See the xAI route for the
        // same rule and the live failures that motivated it.
        Err(error) if error.artifact_is_not_for_this_request() => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(feature = "native-auth")]
fn codex_request_authority(
    target: &OpenAiTarget,
    plan: &ValidatedRequestPlan,
    cache_affinity: Option<Digest>,
) -> Result<ResponsesRequestAuthority, ()> {
    let revisions = codex_semantic_revisions()?;
    ResponsesRequestAuthority::from_admitted_operation(
        target.descriptor(),
        plan,
        plan.request(),
        target.route().account_binding().map_err(|_| ())?,
        revisions,
        cache_affinity,
        ResponsesStoragePolicy::Stateless,
    )
    .map_err(|_| ())
}

#[cfg(feature = "native-auth")]
fn codex_semantic_revisions() -> Result<ResponsesSemanticRevisions, ()> {
    ResponsesSemanticRevisions::new(5, 3, 6).map_err(|_| ())
}

#[cfg(feature = "native-auth")]
/// Returns the exact Codex subscription request policy used by execution for
/// one resolved descriptor and authenticated catalog wire mode.
///
/// Provider admission and the Responses-family configuration compiler consume
/// this same table; callers must not reproduce its wire decisions.
fn codex_request_policy(
    descriptor: &ResolvedModelDescriptor,
    responses_lite: bool,
) -> ResponsesRequestPolicy {
    if responses_lite {
        return ResponsesRequestPolicy::openai_codex_lite(
            crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
        );
    }
    let parallel_tool_calls = descriptor
        .capabilities()
        .get(Capability::ParallelTools)
        .is_some_and(|fact| matches!(fact.support(), Support::Native | Support::Emulated));
    // The route wire carries `input_image`; whether a model accepts one is
    // the catalog's `ImageInput` fact, checked at admission.
    ResponsesRequestPolicy::openai_codex(
        true,
        parallel_tool_calls,
        crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
    )
}

/// Returns the exact request-wire policy for an admitted Platform target.
///
/// The target's explicit storage disposition selects `store`; the route wire
/// carries `input_image` (the catalog's `ImageInput` fact gates each model at
/// admission), and reviewed Platform rows exclude parallel function calls in
/// this adapter revision.
pub fn openai_platform_request_policy(
    authority: &OpenAiPlatformConfigurationAuthority,
) -> Result<ResponsesRequestPolicy, ResponsesProviderConfigurationError> {
    let target = &authority.target;
    if !matches!(
        target.route().access_mode(),
        crate::AccessMode::PlatformApi | crate::AccessMode::LocalFixture
    ) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    let storage = match target.route().storage() {
        crate::StorageMode::ProviderStored => ResponsesStoragePolicy::ProviderStored,
        crate::StorageMode::Stateless => ResponsesStoragePolicy::Stateless,
    };
    Ok(if authority.is_local_fixture() {
        ResponsesRequestPolicy::openai_platform(storage, false, false)
    } else {
        ResponsesRequestPolicy::openai_platform_stateless()
    })
}

/// Compiles a complete Platform selection against the same closed model and
/// request tables retained by its sealed target.
///
/// This creates semantic configuration authority only. It does not construct
/// an HTTP executor; [`OpenAiPlatformModelBuilder::resolve`] constructs that executor.
pub fn compile_openai_platform_configuration(
    selected: &SelectedProviderConfiguration,
    local: &OpenAiPlatformConfigurationAuthority,
    authority: ResolvedProviderAuthorityBinding,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let policy = validate_openai_platform_selection(selected, local)?;
    compile_provider_configuration(selected, local.descriptor().clone(), authority, policy)
}

pub(crate) fn validate_openai_platform_selection(
    selected: &SelectedProviderConfiguration,
    local: &OpenAiPlatformConfigurationAuthority,
) -> Result<ResponsesRequestPolicy, ResponsesProviderConfigurationError> {
    let target = &local.target;
    target
        .validate_current_route_evidence()
        .map_err(|_| ResponsesProviderConfigurationError::PolicyMismatch("descriptor"))?;
    let descriptor = target.descriptor().clone();
    let route = selected.route();
    if route.protocol().as_str() != crate::OPENAI_RESPONSES_PROTOCOL_ID
        || route.route().as_str() != crate::OPENAI_PLATFORM_ROUTE_ID
        || route.integration_profile() != descriptor.profile()
        || route.integration_revision().as_str() != OPENAI_RESPONSES_ADAPTER_REVISION
        || route.auth_profile().as_str() != crate::OPENAI_PLATFORM_AUTH_PROFILE_ID
        || route.auth_revision().as_str() != crate::OPENAI_PLATFORM_AUTH_REVISION
        || route.authority_class().as_str() != crate::OPENAI_PLATFORM_AUTHORITY_CLASS
        || route.credential_selection().as_str() != crate::OPENAI_PLATFORM_CREDENTIAL_SELECTION
        || route.requested_model() != descriptor.requested_model()
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    if matches!(
        selected.behavior().reasoning_effort,
        ReasoningEffortDisposition::Enabled(plakous_model::ReasoningEffort::Ultra)
    ) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    if !local.is_local_fixture()
        && !matches!(
            selected.behavior().reasoning_effort,
            ReasoningEffortDisposition::Enabled(
                plakous_model::ReasoningEffort::None
                    | plakous_model::ReasoningEffort::Low
                    | plakous_model::ReasoningEffort::Medium
                    | plakous_model::ReasoningEffort::High
                    | plakous_model::ReasoningEffort::XHigh
                    | plakous_model::ReasoningEffort::Max
            )
        )
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    if !local.is_local_fixture()
        && (!selected.behavior().hosted_tools.is_empty()
            || target.route().storage() != crate::StorageMode::Stateless)
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "hosted_tools_or_storage",
        ));
    }
    openai_platform_request_policy(local)
}

/// Compiles one explicit Codex subscription selection through the exact
/// catalog facts and request policy used by execution.
#[cfg(feature = "native-auth")]
pub fn compile_codex_subscription_configuration(
    selected: &SelectedProviderConfiguration,
    authority: &RuntimeModelAuthority,
    evidence: &CodexModelResolutionEvidence,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let descriptor = evidence
        .validated_descriptor()
        .map_err(ResponsesProviderConfigurationError::ProviderConfiguration)?
        .clone();
    let route = selected.route();
    if route.protocol().as_str() != crate::OPENAI_CODEX_RESPONSES_PROTOCOL_ID
        || route.route().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
        || route.integration_profile().as_str() != crate::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        || route.integration_revision().as_str() != OPENAI_RESPONSES_ADAPTER_REVISION
        || route.auth_profile().as_str() != crate::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        || route.auth_revision().as_str() != crate::OPENAI_CODEX_SOURCE_REVISION
        || route.authority_class().as_str() != "oauth-subscription"
        || route.credential_selection().as_str() != "primary"
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    let ReasoningEffortDisposition::Enabled(effort) = selected.behavior().reasoning_effort else {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    };
    if !evidence.supports_reasoning_effort(effort) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    validate_codex_catalog_tool_mode(selected, evidence.tool_mode())?;
    let semantic_evidence = evidence.semantic_provider_evidence()?;
    let stable_authority =
        ResolvedProviderAuthorityBinding::new(&descriptor, authority, semantic_evidence)?;
    let policy = codex_request_policy(&descriptor, evidence.uses_responses_lite());
    compile_provider_configuration(selected, descriptor, stable_authority, policy)
}

#[cfg(feature = "native-auth")]
fn validate_codex_catalog_tool_mode(
    selected: &SelectedProviderConfiguration,
    mode: Option<CodexCatalogToolMode>,
) -> Result<(), ResponsesProviderConfigurationError> {
    let selected_mode = selected.behavior().tool_mode;
    let names = selected
        .behavior()
        .tools
        .iter()
        .map(|tool| tool.definition().name().as_str())
        .collect::<Vec<_>>();
    if codex_tool_mode_accepts(mode, selected_mode, &names) {
        Ok(())
    } else {
        Err(ResponsesProviderConfigurationError::PolicyMismatch("tools"))
    }
}

#[cfg(feature = "native-auth")]
fn codex_tool_mode_accepts(
    mode: Option<CodexCatalogToolMode>,
    selected: ToolPresentationMode,
    names: &[&str],
) -> bool {
    let has_exec = names.contains(&"exec");
    let has_wait = names.contains(&"wait");
    let exact_code_mode_controls = names.len() == 2 && has_exec && has_wait;
    let hybrid_code_mode_surface =
        has_exec && has_wait && names.iter().any(|name| !matches!(*name, "exec" | "wait"));
    match mode {
        Some(CodexCatalogToolMode::CodeModeOnly) => {
            (selected == ToolPresentationMode::LocalCompositionOnly && exact_code_mode_controls)
                || (selected == ToolPresentationMode::Direct && !has_exec && !has_wait)
        }
        Some(CodexCatalogToolMode::Direct) => {
            selected == ToolPresentationMode::Direct && !has_exec && !has_wait
        }
        Some(CodexCatalogToolMode::CodeMode) => {
            selected == ToolPresentationMode::LocalComposition && hybrid_code_mode_surface
        }
        None => false,
    }
}

pub(crate) fn operation_deadline(
    clock: &dyn Clock,
    config: StandardOpenAiHttpConfig,
    requested: Option<Timestamp>,
) -> Instant {
    let now = Instant::now();
    let configured = now.checked_add(config.operation_timeout()).unwrap_or(now);
    let Some(requested) = requested else {
        return configured;
    };
    let wall_now = clock.now();
    if requested <= wall_now {
        return now;
    }
    let seconds = requested.seconds().saturating_sub(wall_now.seconds());
    now.checked_add(Duration::from_secs(seconds))
        .map_or(configured, |deadline| deadline.min(configured))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests may construct exact static authority fixtures"
)]
mod platform_tests;

#[cfg(all(test, feature = "native-auth"))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert exact artifact construction with expect and unwrap"
)]
mod tests;
