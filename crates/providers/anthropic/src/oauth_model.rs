//! Production Claude OAuth LanguageModel: identity-first Messages + HTTP factory.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;

use plakous_auth::subscription_limit::SubscriptionRequestPermit;
use plakous_http::{Client, Origin};
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule, Clock,
    ContextContract, ContextEvidenceSource, CountCertificateKind, CountObservation, Digest,
    DigestDomain, EvidenceFreshness, FactProvenance, FactSource, FeatureSupport, LanguageModel,
    MeasurementCertificate, ModelAdapterProfileId, ModelCallContext, ModelError, ModelErrorKind,
    ModelFamilyId, ModelId, ModelOperationRoot, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId,
    ResolvedModelDescriptor, RuntimeModelAuthority, RuntimeModelAuthorityError,
    RuntimeModelAuthorityOperation, SessionLineage, Support, Timestamp, ValidatedRequestPlan,
};

use crate::http::AnthropicDispatchAuthorization;
use crate::native_auth::SubscriptionClock;
use crate::oauth::{
    CLAUDE_OAUTH_ADAPTER_REVISION, CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
    CLAUDE_OAUTH_EXPIRES_AT_UNIX, CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_SOURCE_REVISION,
    claude_max_output_tokens, claude_oauth_reasoning_effort_options,
};
use crate::{
    AnthropicAccountIdentity, AnthropicError, AnthropicErrorKind, AnthropicErrorPhase,
    AnthropicHttpDeadline, AnthropicLocalHttpServiceFactory, AnthropicMessagesDialect,
    AnthropicModel, AnthropicPlatformRoute, AnthropicRetryGrant, ApiKey, ClaudeCredentialRenewal,
    ClaudeNativeAuthError, ClaudeNativeAuthManager, ClaudeProfile, MessagesRequestTranslator,
    ResolvedAnthropicMessages, ThinkingSupport, ToolOwnership, start_anthropic_messages_model_call,
};

const WIRE_DOMAIN: &[u8] = b"plakous-anthropic/claude-oauth-messages-wire-v1";
const LOWERING_REVISION: &str = "anthropic.claude.oauth.lowering.2026-10-09.v12";
const PROJECTION_REVISION: &str = "messages.projection.v3";
const ACCOUNTING_REVISION: &str = "messages.accounting.v1";
// Linux aarch64 is admitted so the Harbor eval plane can build the same
// product for native arm64 cells; it is an eval build target, not a supported
// product platform (chapter 06 owns that list).
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
compile_error!(
    "Claude OAuth envelope pin is Apple Silicon macOS, Linux x86_64 or Linux aarch64 only"
);
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_ID: &str = "claude-code-2.1.284-darwin-aarch64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const REFERENCE_ARTIFACT_ID: &str = "claude-code-2.1.284-linux-x86_64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_ID: &str = "claude-code-2.1.284-linux-aarch64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_REVISION: &str = "2.1.284-darwin-aarch64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const REFERENCE_ARTIFACT_REVISION: &str = "2.1.284-linux-x86_64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_REVISION: &str = "2.1.284-linux-aarch64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_SHA256: &str =
    "50a14c2f50f56668380fdda490167f1d3630d5cc18fb8aed3073c2c7ea7314fe";
/// Claude Code 2.1.284 Linux x86_64 executable identity from the official manifest.
/// reference-client evidence for Plak's Linux product.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const REFERENCE_ARTIFACT_SHA256: &str =
    "5cd90aabd83f8a15136c35aa37bb1d92b348993573316643dc3fe4e04afbf88f";
/// Claude Code 2.1.284 Linux arm64 executable identity from the official manifest.
/// reference-client evidence for the eval plane's native arm64 cells.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const REFERENCE_ARTIFACT_SHA256: &str =
    "3dd0f96d7ada463152d20300186f6cfc6ab94b57e218f49e3ac86db42ac695a6";
const REFERENCE_REVISION: &str = "claude-code-2.1.284-context-reference-v1";
const REFERENCE_CAPACITY_REVISION: &str = "claude-code-2.1.284-context-envelope-v1";
const REFERENCE_MEASUREMENT_ID: &str = "claude-complete-messages-wire-byte-upper-bound";
const REFERENCE_MEASUREMENT_REVISION: &str = "claude-code-2.1.284-wire-bytes-v1";
/// Share of the reported context window admitted as prepared input.
///
/// The reference client compacts against the model's full window (its
/// auto-compact fence sits a fixed buffer under the window, not a fifth of it),
/// so the envelope is the whole window and the capacity-derived scheduled
/// compaction trigger leaves room for a summary and further observations. An 80% envelope reported a
/// 1M model as 800k and compacted at 680k — far too early, since the measured
/// count is already a byte upper bound over the lowered request.
const REFERENCE_INPUT_PERCENT: u64 = 100;
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Executable Claude OAuth Messages model.
pub struct ClaudeOAuthLanguageModel {
    identity: crate::ClaudeOAuthRequestIdentity,
    resolved: ResolvedAnthropicMessages,
    client: Client,
    authority: RuntimeModelAuthority,
    credentials: ClaudeNativeAuthManager,
    observation: Mutex<AuthorityObservation>,
    login_renewed: Arc<AtomicBool>,
    parked_credential: Mutex<Option<crate::ClaudeSubscriptionStatus>>,
    #[cfg(test)]
    test_dial_origin: Option<Origin>,
}

struct AuthorityObservation {
    authority: RuntimeModelAuthority,
    grant: Option<plakous_auth::AccountBinding>,
}

impl ClaudeOAuthLanguageModel {
    fn new(
        resolved: ResolvedAnthropicMessages,
        client: Client,
        authority: RuntimeModelAuthority,
        credentials: ClaudeNativeAuthManager,
        identity: crate::ClaudeOAuthRequestIdentity,
    ) -> Self {
        let grant = credentials
            .identity_credentials()
            .ok()
            .map(|((_, _, grant), _)| grant);
        Self {
            identity,
            resolved,
            client,
            observation: Mutex::new(AuthorityObservation {
                authority: authority.clone(),
                grant,
            }),
            login_renewed: Arc::new(AtomicBool::new(false)),
            parked_credential: Mutex::new(None),
            authority,
            credentials,
            #[cfg(test)]
            test_dial_origin: None,
        }
    }
}

/// Builds the one Claude OAuth request translator, for dispatch and for tests.
///
/// Claude OAuth drives the same Messages contract as the API key and holds the
/// same replay discipline. Without `require_exact_thinking_replay` a reasoning
/// block that reaches lowering without its signature is sent as assistant text,
/// and Opus reads its own chain of thought coming back as extraction. Both
/// callers go through here so that flag cannot be dropped from one of them.
pub(crate) fn oauth_request_translator(
    resolved: ResolvedAnthropicMessages,
    identity: crate::ClaudeOAuthRequestIdentity,
    session_id: Option<plakous_model::SessionId>,
) -> MessagesRequestTranslator {
    MessagesRequestTranslator::new(resolved)
        .require_exact_thinking_replay()
        .with_oauth_identity(identity)
        .with_session_id(session_id)
}

impl ClaudeOAuthLanguageModel {
    /// Revalidates admission and authority exactly as before a model round,
    /// reserves and authorizes one inference permit, and returns the current
    /// access token with the identity it belongs to.
    pub(crate) async fn side_request_credential(
        self: &Arc<Self>,
        input_bound: u64,
    ) -> Result<(String, crate::ClaudeOAuthRequestIdentity), AnthropicError> {
        let checked_at = SubscriptionClock.now();
        self.resolved
            .ensure_current_admission(checked_at)
            .map_err(|_| oauth_retry_error())?;
        Arc::clone(self)
            .revalidate_runtime_authority(checked_at)
            .await
            .map_err(|_| oauth_retry_error())?;
        let mut permit = refresh_then_reserve_inference(&self.credentials, input_bound)
            .await
            .map_err(|_| oauth_retry_error())?;
        let ((access_token, _, _), identity) = self
            .credentials
            .identity_credentials()
            .map_err(|_| oauth_retry_error())?;
        let account_binding = digest_binding(
            b"claude-oauth-provider-account",
            identity.account_uuid().as_str().as_bytes(),
        )
        .map_err(|_| oauth_retry_error())?;
        if identity != self.identity || account_binding != self.authority.account_binding() {
            return Err(oauth_retry_error());
        }
        permit
            .authorize_dispatch()
            .map_err(|_| oauth_retry_error())?;
        Ok((access_token, identity))
    }

    pub(crate) const fn resolved(&self) -> &ResolvedAnthropicMessages {
        &self.resolved
    }

    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) const fn identity(&self) -> &crate::ClaudeOAuthRequestIdentity {
        &self.identity
    }
}

impl LanguageModel for ClaudeOAuthLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = Origin::parse(self.resolved.route().origin()).ok()?;
        #[cfg(test)]
        let origin = self.test_dial_origin.clone().unwrap_or(origin);
        Some(crate::http::bind_reachability_probe(
            self.client.clone(),
            origin,
            self.resolved.descriptor().route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let descriptor = self.resolved.descriptor().clone();
        Box::pin(async move {
            if let Err(error) = self.credentials.refresh_if_needed().await {
                let status = self
                    .credentials
                    .async_status()
                    .await
                    .map_err(credential_authority_error)?;
                if !status
                    .as_ref()
                    .is_some_and(|status| status.status() == plakous_auth::CredentialStatus::Ready)
                {
                    *self
                        .parked_credential
                        .lock()
                        .map_err(|_| RuntimeModelAuthorityError::Unavailable)? = status;
                    return Err(RuntimeModelAuthorityError::AwaitingCredential);
                }
                return Err(credential_authority_error(error));
            }
            let ((_, generation, grant), current_identity) =
                match self.credentials.identity_credentials() {
                    Err(ClaudeNativeAuthError::AccountIdentityUnavailable) => {
                        self.credentials
                            .ensure_account_identity()
                            .await
                            .map_err(credential_authority_error)?;
                        self.credentials
                            .identity_credentials()
                            .map_err(credential_authority_error)?
                    }
                    result => result.map_err(credential_authority_error)?,
                };
            if current_identity.account_uuid() != self.identity.account_uuid() {
                return Err(RuntimeModelAuthorityError::AccountChanged);
            }
            if current_identity != self.identity {
                return Err(RuntimeModelAuthorityError::ForeignAuthority);
            }
            let mut observation = self
                .observation
                .lock()
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
            let authority = &observation.authority;
            if generation < authority.credential_generation() {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            // This receipt observes the resolved admission, not the order in
            // which callers sample clocks before asynchronous revalidation.
            let checked_at = checked_at.max(authority.observed_at());
            let account = digest_binding(
                b"claude-oauth-provider-account",
                current_identity.account_uuid().as_str().as_bytes(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            let credential_authority = digest_binding(
                b"claude-oauth-provider-account-credential",
                current_identity.account_uuid().as_str().as_bytes(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            if account != authority.account_binding()
                || credential_authority != authority.credential_authority_binding()
            {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            if generation == authority.credential_generation() {
                authority.validate(&descriptor, checked_at)?;
                return Ok(authority.clone());
            }
            let refreshed = RuntimeModelAuthority::new(
                &descriptor,
                account,
                credential_authority,
                generation,
                authority.observed_at(),
                authority.valid_until(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            refreshed.validate(&descriptor, checked_at)?;
            if observation
                .grant
                .as_ref()
                .is_some_and(|previous| previous != &grant)
            {
                self.login_renewed.store(true, Ordering::Release);
            }
            observation.authority = refreshed.clone();
            observation.grant = Some(grant);
            Ok(refreshed)
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous_model::RuntimeModelAuthorityWaitOperation {
        Box::pin(async move {
            let parked = self
                .parked_credential
                .lock()
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)?
                .clone();
            self.credentials
                .wait_for_ready(parked)
                .await
                .map_err(credential_authority_error)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Seal the Claude OAuth wire, context certificate and replay binding before any dispatch is possible."
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let prepared = oauth_request_translator(
            self.resolved.clone(),
            self.identity.clone(),
            context
                .cache_affinity()
                .map(plakous_model::ModelCacheAffinity::session),
        )
        .with_completed_history_prefix(context.completed_history_prefix())
        .with_continuation(context.previous_response_id(), context.turn_position())
        .translate_with_provenance(
            &plan,
            ToolOwnership::default(),
            context.active_family_artifact(),
            context.previous_response_metadata(),
        )
        .map_err(|error| rejection(error.preparation_error_kind(), error))?;
        let wire_size = u64::try_from(prepared.body().len()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "Claude OAuth Messages request exceeded the size bound",
            )
        })?;
        let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[WIRE_DOMAIN, prepared.body()])
            .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let max_tokens = NonZeroU32::new(prepared.max_tokens()).ok_or_else(|| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Claude OAuth max_tokens was missing or zero",
            )
        })?;
        let observation = reference_client_wire_observation(self.resolved.descriptor(), wire_size)?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, Some(observation))
                .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?
                .with_input_parts(plakous_model::ContextInputParts::measure_wire(
                    prepared.body(),
                ))
                .with_wire_input(
                    prepared.body(),
                    Some(crate::model::CLAUDE_IMAGE_MAX_TOKENS),
                    &context,
                )?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            max_tokens,
            "max_tokens",
            wire_digest,
            LOWERING_REVISION,
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let protocol = ProtocolId::new("messages")
            .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let projection = PreparedModelProjection::new(
            self.resolved.descriptor(),
            &plan,
            &context,
            protocol,
            LOWERING_REVISION,
            PROJECTION_REVISION,
            ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            Some(output_limit),
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let record = PreparedCallRecord::for_prepared(
            self.resolved.descriptor(),
            &plan,
            projection,
            &context,
        )?;
        let deadline =
            AnthropicHttpDeadline::new(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .map_err(|error| {
                    rejection(ModelPreparationErrorKind::InvalidConfiguration, error)
                })?;
        let descriptor = self.resolved.descriptor().clone();
        let credentials = self.credentials.clone();
        let client = self.client.clone();
        let route = self.resolved.route().clone();
        let clock = Arc::new(SubscriptionClock);
        let ids = Arc::new(plakous_model::SystemIdGenerator);
        let input_bound = wire_size.max(1);
        let expected_account_binding = self.authority.account_binding();
        let identity = self.identity.clone();
        let turn_rooted = matches!(
            plan.request().metadata().root(),
            ModelOperationRoot::Turn(_)
        );
        let retry_model = Arc::clone(&self);
        let login_renewed = Arc::clone(&self.login_renewed);
        #[cfg(test)]
        let test_dial_origin = self.test_dial_origin.clone();
        Ok(PreparedModelCall::new(
            record,
            move |call_context: ModelCallContext| {
                Box::pin(async move {
                    let permit = refresh_then_reserve_inference(&credentials, input_bound)
                        .await
                        .map_err(|error| contextual_refresh_error(error, &descriptor))?;
                    let ((access_token, _, _), current_identity) =
                        credentials.identity_credentials().map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Authentication,
                                "Claude OAuth dispatch credentials were unavailable",
                            )
                        })?;
                    let account_binding = digest_binding(
                        b"claude-oauth-provider-account",
                        current_identity.account_uuid().as_str().as_bytes(),
                    )
                    .map_err(map_credential_error)?;
                    if current_identity != identity || account_binding != expected_account_binding {
                        return Err(ModelError::new(
                            ModelErrorKind::Authentication,
                            "Claude OAuth account identity changed",
                        ));
                    }
                    let route = route
                        .with_oauth_access_token(access_token)
                        .map_err(|error| {
                            ModelError::new(ModelErrorKind::Authentication, error.to_string())
                        })?;
                    #[cfg(test)]
                    let factory = match test_dial_origin {
                        Some(origin) => AnthropicLocalHttpServiceFactory::with_runtime(
                            client, origin, route, prepared, clock, ids, deadline,
                        ),
                        None => AnthropicLocalHttpServiceFactory::for_route(
                            client, route, prepared, clock, ids, deadline,
                        ),
                    };
                    #[cfg(not(test))]
                    let factory = AnthropicLocalHttpServiceFactory::for_route(
                        client, route, prepared, clock, ids, deadline,
                    );
                    let factory = factory
                        .map_err(|error| {
                            ModelError::new(ModelErrorKind::InvalidRequest, error.to_string())
                        })?
                        .with_dispatch_authorization(permit_authorization(permit));
                    let factory = if turn_rooted {
                        factory.with_credential_renewal(Arc::new(ClaudeForcedRenewal {
                            model: Arc::clone(&retry_model),
                            credentials: credentials.clone(),
                            identity,
                            expected_account_binding,
                            input_bound,
                        }))
                    } else {
                        factory
                    };
                    let factory = {
                        let credentials = credentials.clone();
                        factory.with_retry(Arc::new(move |route| {
                            let credentials = credentials.clone();
                            let retry_model = Arc::clone(&retry_model);
                            let identity = retry_model.identity.clone();
                            Box::pin(async move {
                                let checked_at = SubscriptionClock.now();
                                retry_model
                                    .resolved
                                    .ensure_current_admission(checked_at)
                                    .map_err(|_| oauth_retry_error())?;
                                Arc::clone(&retry_model)
                                    .revalidate_runtime_authority(checked_at)
                                    .await
                                    .map_err(|_| oauth_retry_error())?;
                                let permit =
                                    refresh_then_reserve_inference(&credentials, input_bound)
                                        .await
                                        .map_err(|_| oauth_retry_error())?;
                                let checked_at = SubscriptionClock.now();
                                retry_model
                                    .resolved
                                    .ensure_current_admission(checked_at)
                                    .map_err(|_| oauth_retry_error())?;
                                Arc::clone(&retry_model)
                                    .revalidate_runtime_authority(checked_at)
                                    .await
                                    .map_err(|_| oauth_retry_error())?;
                                let ((access_token, _, _), current_identity) = credentials
                                    .identity_credentials()
                                    .map_err(|_| oauth_retry_error())?;
                                let account_binding = digest_binding(
                                    b"claude-oauth-provider-account",
                                    current_identity.account_uuid().as_str().as_bytes(),
                                )
                                .map_err(|_| oauth_retry_error())?;
                                if current_identity != identity
                                    || account_binding != expected_account_binding
                                {
                                    return Err(oauth_retry_error());
                                }
                                let route = route.with_oauth_access_token(access_token)?;
                                Ok(AnthropicRetryGrant {
                                    route,
                                    dispatch_authorization: Some(permit_authorization(permit)),
                                })
                            })
                        }))
                    };
                    let stream =
                        start_anthropic_messages_model_call(factory, descriptor, call_context)
                            .await?;
                    let notice = login_renewed.swap(false, Ordering::AcqRel).then(|| {
                        Ok(plakous_model::ModelEvent::Diagnostic(
                            plakous_model::ModelDiagnostic::new(
                                "Claude login renewed; continuing on the same account",
                            ),
                        ))
                    });
                    Ok(Box::pin(futures_util::stream::iter(notice).chain(stream))
                        as plakous_model::ModelStream)
                })
            },
        ))
    }
}

fn credential_authority_error(error: ClaudeNativeAuthError) -> RuntimeModelAuthorityError {
    match error {
        ClaudeNativeAuthError::NotLoggedIn
        | ClaudeNativeAuthError::NotReady
        | ClaudeNativeAuthError::RefreshQuarantined => RuntimeModelAuthorityError::Unavailable,
        ClaudeNativeAuthError::AccountIdentityUnavailable
        | ClaudeNativeAuthError::InvalidStoredCredential => RuntimeModelAuthorityError::Invalid,
        _ => RuntimeModelAuthorityError::Unavailable,
    }
}

fn oauth_retry_error() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::Authentication,
        AnthropicErrorPhase::Connect,
        "Claude OAuth safe retry authority was unavailable",
    )
}

fn permit_authorization(
    permit: Box<dyn SubscriptionRequestPermit>,
) -> AnthropicDispatchAuthorization {
    let permit = Arc::new(Mutex::new(permit));
    let retained = Arc::clone(&permit);
    AnthropicDispatchAuthorization {
        authorize: Arc::new(move || {
            permit
                .lock()
                .map_err(|_| oauth_retry_error())?
                .authorize_dispatch()
                .map_err(|_| oauth_retry_error())
        }),
        transport_failed: Arc::new(move |failure| {
            if failure.transmission() == plakous_http::TransmissionFact::NotStarted
                && let Ok(mut permit) = retained.lock()
            {
                let _ = permit.refund_not_started();
            }
        }),
    }
}

impl ClaudeNativeAuthManager {
    /// Resolves one enrolled Claude OAuth coding model for live Messages dispatch.
    ///
    /// # Errors
    ///
    /// Requires a Ready selected credential and an enrolled model id.
    pub async fn resolve_model(
        &self,
        requested_model: &str,
        client: Client,
        lineage: SessionLineage,
    ) -> Result<
        (
            Arc<dyn LanguageModel>,
            RuntimeModelAuthority,
            ResolvedModelDescriptor,
        ),
        ClaudeNativeAuthError,
    > {
        let (model, authority, descriptor) = self
            .resolve_oauth_model(requested_model, client, lineage)
            .await?;
        Ok((model, authority, descriptor))
    }

    /// Resolves the model as [`Self::resolve_model`] does, with the
    /// hosted-retrieval side-request port on the same route, account,
    /// credential and model, for the logical session `session_id`.
    ///
    /// # Errors
    ///
    /// Requires a Ready selected credential and an enrolled model id.
    pub async fn resolve_model_with_retrieval(
        &self,
        requested_model: &str,
        client: Client,
        lineage: SessionLineage,
        session_id: plakous_model::SessionId,
    ) -> Result<
        (
            Arc<dyn LanguageModel>,
            RuntimeModelAuthority,
            ResolvedModelDescriptor,
            crate::AnthropicRetrieval,
        ),
        ClaudeNativeAuthError,
    > {
        let (model, authority, descriptor) = self
            .resolve_oauth_model(requested_model, client, lineage)
            .await?;
        let retrieval = crate::AnthropicRetrieval::claude_oauth(Arc::clone(&model), session_id)
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        Ok((model, authority, descriptor, retrieval))
    }

    async fn resolve_oauth_model(
        &self,
        requested_model: &str,
        client: Client,
        lineage: SessionLineage,
    ) -> Result<
        (
            Arc<ClaudeOAuthLanguageModel>,
            RuntimeModelAuthority,
            ResolvedModelDescriptor,
        ),
        ClaudeNativeAuthError,
    > {
        if !crate::oauth::claude_oauth_enrolled_model(requested_model) {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        self.refresh_if_needed().await?;
        self.ensure_account_identity().await?;
        let ((access_token, generation, _), identity) = self.identity_credentials()?;
        let key = ApiKey::new(access_token)
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let account = AnthropicAccountIdentity::new(identity.account_uuid().as_str())
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let route = AnthropicPlatformRoute::claude_oauth(key, account)
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let now = SubscriptionClock.now();
        let expires_at = pin_expires_at(now)?;
        let account_binding = digest_binding(
            b"claude-oauth-provider-account",
            identity.account_uuid().as_str().as_bytes(),
        )?;
        let credential_authority_binding = digest_binding(
            b"claude-oauth-provider-account-credential",
            identity.account_uuid().as_str().as_bytes(),
        )?;
        let context_authority_binding = RuntimeModelAuthority::authority_class_for(
            account_binding,
            credential_authority_binding,
        )
        .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let resolved = oauth_resolved_target(
            route,
            requested_model,
            context_authority_binding,
            now,
            expires_at,
        )
        .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?
        .with_session_lineage(lineage);
        let authority = RuntimeModelAuthority::new(
            resolved.descriptor(),
            account_binding,
            credential_authority_binding,
            generation,
            now,
            Some(expires_at),
        )
        .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let descriptor = resolved.descriptor().clone();
        let model = ClaudeOAuthLanguageModel::new(
            resolved,
            client,
            authority.clone(),
            self.clone(),
            identity,
        );
        Ok((Arc::new(model), authority, descriptor))
    }
}

async fn refresh_then_reserve_inference(
    credentials: &ClaudeNativeAuthManager,
    input_bound: u64,
) -> Result<Box<dyn SubscriptionRequestPermit>, ModelError> {
    credentials
        .refresh_if_needed()
        .await
        .map_err(map_credential_error)?;
    let gate = credentials.request_gate().ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::ResourceLimit,
            "Claude OAuth inference request gate is required",
        )
    })?;
    let permit = gate.acquire_inference(input_bound).map_err(|_| {
        ModelError::new(
            ModelErrorKind::ResourceLimit,
            "Claude OAuth inference request exceeded its classified cap",
        )
    })?;
    credentials
        .dispatch_credentials()
        .map_err(map_credential_error)?;
    Ok(permit)
}

/// Forced Claude OAuth renewal for the single post-401 Messages re-dispatch.
struct ClaudeForcedRenewal {
    model: Arc<ClaudeOAuthLanguageModel>,
    expected_account_binding: Digest,
    identity: crate::ClaudeOAuthRequestIdentity,
    credentials: ClaudeNativeAuthManager,
    input_bound: u64,
}

impl ClaudeCredentialRenewal for ClaudeForcedRenewal {
    fn renew_after_rejection<'a>(
        &'a self,
        route: &'a AnthropicPlatformRoute,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<AnthropicRetryGrant, AnthropicError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.credentials
                .refresh_after_rejection()
                .await
                .map_err(|_| renewal_error())?;
            // Reviewed admission uses no credential material. Refuse a stale
            // target before reserving another inference request.
            self.model
                .resolved
                .ensure_current_admission(SubscriptionClock.now())
                .map_err(|_| renewal_error())?;
            let gate = self.credentials.request_gate().ok_or_else(renewal_error)?;
            let permit = gate
                .acquire_inference(self.input_bound)
                .map_err(|_| renewal_error())?;
            let ((access_token, _, _), identity) = self
                .credentials
                .identity_credentials()
                .map_err(|_| renewal_error())?;
            let account_binding = digest_binding(
                b"claude-oauth-provider-account",
                identity.account_uuid().as_str().as_bytes(),
            )
            .map_err(|_| renewal_error())?;
            if identity != self.identity || account_binding != self.expected_account_binding {
                return Err(renewal_error());
            }
            let checked_at = SubscriptionClock.now();
            self.model
                .resolved
                .ensure_current_admission(checked_at)
                .map_err(|_| renewal_error())?;
            Arc::clone(&self.model)
                .revalidate_runtime_authority(checked_at)
                .await
                .map_err(|_| renewal_error())?;
            let route = route.with_oauth_access_token(access_token)?;
            Ok(AnthropicRetryGrant {
                route,
                dispatch_authorization: Some(permit_authorization(permit)),
            })
        })
    }
}

fn renewal_error() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::Authentication,
        AnthropicErrorPhase::Prepare,
        "Claude OAuth credential renewal after HTTP 401 was refused",
    )
}

fn map_credential_error(error: ClaudeNativeAuthError) -> ModelError {
    let restored_unsent = matches!(error, ClaudeNativeAuthError::Transport(Some(cause))
        if cause.nothing_sent() && matches!(cause.phase,
            plakous_model::ModelRetryPhase::Resolution | plakous_model::ModelRetryPhase::Connect | plakous_model::ModelRetryPhase::Tls));
    let kind = match error {
        ClaudeNativeAuthError::CompatibilityInactive
        | ClaudeNativeAuthError::CompatibilityNotAccepted => ModelErrorKind::Unavailable,
        ClaudeNativeAuthError::Transport(_) if restored_unsent => ModelErrorKind::Transport,
        _ => ModelErrorKind::Authentication,
    };
    let message = match error {
        ClaudeNativeAuthError::Transport(Some(cause)) if restored_unsent => {
            format!("Claude sign-in refresh failed: {cause}; credential unchanged")
        }
        ClaudeNativeAuthError::Transport(Some(cause)) => {
            format!("Claude sign-in refresh failed: {cause}; credential recovery required")
        }
        ClaudeNativeAuthError::Transport(None) => {
            "Claude sign-in refresh failed: network exchange failed; credential recovery required"
                .to_owned()
        }
        _ => "Claude OAuth credential refresh failed".to_owned(),
    };
    ModelError::new(kind, message)
}

fn contextual_refresh_error(
    mut error: ModelError,
    descriptor: &ResolvedModelDescriptor,
) -> ModelError {
    let retryability = if error.kind() == ModelErrorKind::Transport {
        plakous_model::ModelRetryability::Eligible { retry_after: None }
    } else {
        plakous_model::ModelRetryability::Unknown
    };
    if let Ok(context) = plakous_model::ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        plakous_model::ModelErrorPhase::Authentication,
        retryability,
    ) {
        let _ = error.attach_context(context);
    }
    error
}

fn rejection(
    kind: ModelPreparationErrorKind,
    error: impl std::fmt::Display,
) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, error.to_string())
}

fn reference_client_wire_observation(
    descriptor: &ResolvedModelDescriptor,
    complete_wire_bytes: u64,
) -> Result<CountObservation, ModelPreparationError> {
    let measurement = descriptor
        .context_contract()
        .and_then(|contract| contract.measurements().first())
        .ok_or_else(|| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Claude OAuth context measurement is unavailable",
            )
        })?;
    CountObservation::new(
        measurement.certificate_id(),
        measurement.revision(),
        complete_wire_bytes,
    )
    .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))
}

fn digest_binding(label: &[u8], value: &[u8]) -> Result<Digest, ClaudeNativeAuthError> {
    Digest::sha256(DigestDomain::ModelBinding, 1, &[label, value])
        .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)
}

fn pin_expires_at(now: Timestamp) -> Result<Timestamp, ClaudeNativeAuthError> {
    let expires_at = Timestamp::new(CLAUDE_OAUTH_EXPIRES_AT_UNIX, 0)
        .map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)?;
    if expires_at <= now {
        return Err(ClaudeNativeAuthError::CompatibilityInactive);
    }
    Ok(expires_at)
}

fn oauth_resolved_target(
    route: AnthropicPlatformRoute,
    model_id: &str,
    context_authority_binding: Digest,
    now: Timestamp,
    expires_at: Timestamp,
) -> Result<ResolvedAnthropicMessages, AnthropicError> {
    let row = crate::oauth::claude_enrollment(model_id)
        .filter(|row| row.oauth)
        .ok_or_else(|| {
            configuration_identity(format!(
                "Claude reference context row is unavailable for {model_id}"
            ))
        })?;
    let context_window = row.context_window;
    let remote = ModelId::new(model_id).map_err(configuration_identity)?;
    let family = ModelFamilyId::new(row.family).map_err(configuration_identity)?;
    let max_out = claude_max_output_tokens(model_id);
    let efforts = claude_oauth_reasoning_effort_options(model_id);
    let thinking = if efforts.is_empty() {
        ThinkingSupport::Disabled
    } else {
        ThinkingSupport::Adaptive
    };
    let model = AnthropicModel::new(remote.clone(), family.clone(), max_out, thinking)?;
    let profile = ClaudeProfile::new(
        ModelAdapterProfileId::new(CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID)
            .map_err(configuration_identity)?,
    );
    let provenance = FactProvenance::new(FactSource::RouteEvidence, now, Some(expires_at));
    let native = |feature: &str| {
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::new(
                None,
                Some(format!("{CLAUDE_OAUTH_ADAPTER_REVISION}:{feature}")),
            ),
            provenance,
        )
    };
    let mut features = std::collections::BTreeMap::new();
    features.insert(Capability::Streaming, native("streaming"));
    features.insert(Capability::Tools, native("client-tools"));
    features.insert(Capability::ParallelTools, native("parallel-tools"));
    features.insert(Capability::ImageInput, native("image-input"));
    features.insert(
        Capability::Caching,
        native(&format!("prompt-caching-scope:{REFERENCE_REVISION}")),
    );
    let effort_note = efforts
        .iter()
        .map(|effort| format!("{effort:?}").to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(",");
    if !efforts.is_empty() {
        features.insert(
            Capability::Reasoning,
            native(&format!("adaptive-thinking:{effort_note}")),
        );
    }
    let capabilities = CapabilitySet::new(features, std::collections::BTreeMap::new());
    let protocol = ProtocolId::new("anthropic.messages").map_err(configuration_identity)?;
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol,
        route.id().clone(),
        remote.clone(),
        Some(remote),
        Some(family),
        profile.id().clone(),
        capabilities,
    )
    .with_context_contract(reference_client_context_contract(
        model_id,
        context_window,
        context_authority_binding,
        now,
        expires_at,
    )?);
    let dialect = AnthropicMessagesDialect::claude_oauth_for(model_id);
    ResolvedAnthropicMessages::new_reviewed_admission(
        dialect,
        route,
        model,
        profile,
        descriptor,
        crate::AnthropicResolutionTrust::ReviewedSubscriptionAdmission {
            revision: CLAUDE_OAUTH_SOURCE_REVISION,
            reviewed_at: Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0)
                .map_err(configuration_identity)?,
            expires_at,
        },
        Arc::new(SubscriptionClock),
    )
}

fn reference_client_context_contract(
    model_id: &str,
    reported_context_window: u64,
    authority_binding: Digest,
    observed_at: Timestamp,
    expires_at: Timestamp,
) -> Result<ContextContract, AnthropicError> {
    let maximum_projected_input = reported_context_window
        .checked_mul(REFERENCE_INPUT_PERCENT)
        .map(|scaled| scaled / 100)
        .filter(|value| *value != 0)
        .ok_or_else(|| configuration_identity("invalid Claude reference input envelope"))?;
    let catalog_receipt = reference_client_catalog_receipt(
        authority_binding,
        model_id,
        reported_context_window,
        maximum_projected_input,
    )?;
    let freshness =
        EvidenceFreshness::new(observed_at, Some(expires_at)).map_err(configuration_identity)?;
    let capacity = CapacityCertificate::reference_client_envelope(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "claude-authenticated-reference-input-envelope",
        REFERENCE_CAPACITY_REVISION,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(reported_context_window),
            maximum_projected_input,
            auto_compact_input: None,
        },
        authority_binding,
        freshness,
        catalog_receipt,
        REFERENCE_REVISION,
    )
    .map_err(configuration_identity)?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        REFERENCE_MEASUREMENT_ID,
        REFERENCE_MEASUREMENT_REVISION,
        "complete-lowered-messages-wire-utf8-byte-upper-bound",
        REFERENCE_MEASUREMENT_REVISION,
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt },
        Some(authority_binding),
        freshness,
    )
    .map_err(configuration_identity)?;
    ContextContract::new(
        ProtocolId::new("messages").map_err(configuration_identity)?,
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        "reference-client-envelope-no-output-authority-v1",
        capacity,
        vec![measurement],
    )
    .map_err(configuration_identity)
}

fn reference_client_catalog_receipt(
    authority_binding: Digest,
    model_id: &str,
    reported_context_window: u64,
    maximum_projected_input: u64,
) -> Result<Digest, AnthropicError> {
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-anthropic/claude-reference-client-context-row-v2",
            REFERENCE_ARTIFACT_ID.as_bytes(),
            REFERENCE_ARTIFACT_REVISION.as_bytes(),
            REFERENCE_ARTIFACT_SHA256.as_bytes(),
            REFERENCE_REVISION.as_bytes(),
            CLAUDE_OAUTH_SOURCE_REVISION.as_bytes(),
            &authority_binding.identity_bytes(),
            model_id.as_bytes(),
            &reported_context_window.to_be_bytes(),
            &REFERENCE_INPUT_PERCENT.to_be_bytes(),
            &maximum_projected_input.to_be_bytes(),
        ],
    )
    .map_err(configuration_identity)
}

fn configuration_identity(error: impl std::fmt::Display) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        error.to_string(),
    )
}

#[cfg(test)]
#[path = "oauth_model/auth_binding_tests.rs"]
mod auth_binding_tests;

#[cfg(test)]
#[path = "oauth_model/credential_wait_tests.rs"]
mod credential_wait_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]
mod tests;
