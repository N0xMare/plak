//! Production Claude OAuth LanguageModel: identity-first Messages + HTTP factory.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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
    RuntimeModelAuthorityOperation, SessionLineage, Support, SystemClock, Timestamp,
    ValidatedRequestPlan,
};

use crate::http::AnthropicDispatchAuthorization;
use crate::oauth::{
    CLAUDE_OAUTH_ADAPTER_REVISION, CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
    CLAUDE_OAUTH_EXPIRES_AT_UNIX, CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_SOURCE_REVISION,
    claude_max_output_tokens, claude_oauth_reasoning_effort_options,
};
use crate::{
    AnthropicAccountIdentity, AnthropicApiKey, AnthropicError, AnthropicErrorKind,
    AnthropicErrorPhase, AnthropicHttpDeadline, AnthropicLocalHttpServiceFactory,
    AnthropicMessagesDialect, AnthropicModel, AnthropicPlatformRoute, AnthropicRetryGrant,
    ClaudeCredentialRenewal, ClaudeNativeAuthError, ClaudeNativeAuthManager, ClaudeProfile,
    MessagesRequestTranslator, ResolvedAnthropicMessages, ThinkingSupport, ToolOwnership,
    start_anthropic_messages_model_call,
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
    #[cfg(test)]
    test_dial_origin: Option<Origin>,
}

impl ClaudeOAuthLanguageModel {
    fn new(
        resolved: ResolvedAnthropicMessages,
        client: Client,
        authority: RuntimeModelAuthority,
        credentials: ClaudeNativeAuthManager,
        identity: crate::ClaudeOAuthRequestIdentity,
    ) -> Self {
        Self {
            identity,
            resolved,
            client,
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
        let authority = self.authority.clone();
        let descriptor = self.resolved.descriptor().clone();
        let credentials = self.credentials.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            let Ok(((_, generation, account_binding), current_identity)) =
                credentials.identity_credentials()
            else {
                return Err(RuntimeModelAuthorityError::Stale);
            };
            if current_identity != identity {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            let account = digest_binding(
                b"claude-oauth-grant-lineage-account",
                account_binding.as_str().as_bytes(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            let credential_authority = digest_binding(
                b"claude-oauth-grant-lineage-credential",
                account_binding.as_str().as_bytes(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            if account != authority.account_binding()
                || credential_authority != authority.credential_authority_binding()
            {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            if generation == authority.credential_generation() {
                authority.validate(&descriptor, checked_at)?;
                return Ok(authority);
            }
            let refreshed = RuntimeModelAuthority::new(
                &descriptor,
                account,
                credential_authority,
                generation,
                checked_at,
                authority.valid_until(),
            )
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
            refreshed.validate(&descriptor, checked_at)?;
            Ok(refreshed)
        })
    }

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
        let clock = Arc::new(SystemClock);
        let ids = Arc::new(plakous_model::SystemIdGenerator);
        let input_bound = wire_size.max(1);
        let expected_account_binding = self.authority.account_binding();
        let identity = self.identity.clone();
        let turn_rooted = matches!(
            plan.request().metadata().root(),
            ModelOperationRoot::Turn(_)
        );
        let retry_model = Arc::clone(&self);
        #[cfg(test)]
        let test_dial_origin = self.test_dial_origin.clone();
        Ok(PreparedModelCall::new(
            record,
            move |call_context: ModelCallContext| {
                Box::pin(async move {
                    let permit = refresh_then_reserve_inference(&credentials, input_bound)
                        .await
                        .map_err(|error| contextual_refresh_error(error, &descriptor))?;
                    let ((access_token, _, grant_binding), current_identity) =
                        credentials.identity_credentials().map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Authentication,
                                "Claude OAuth dispatch credentials were unavailable",
                            )
                        })?;
                    let account_binding = digest_binding(
                        b"claude-oauth-grant-lineage-account",
                        grant_binding.as_str().as_bytes(),
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
                                let checked_at = SystemClock.now();
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
                                let checked_at = SystemClock.now();
                                retry_model
                                    .resolved
                                    .ensure_current_admission(checked_at)
                                    .map_err(|_| oauth_retry_error())?;
                                Arc::clone(&retry_model)
                                    .revalidate_runtime_authority(checked_at)
                                    .await
                                    .map_err(|_| oauth_retry_error())?;
                                let ((access_token, _, grant_binding), current_identity) =
                                    credentials
                                        .identity_credentials()
                                        .map_err(|_| oauth_retry_error())?;
                                let account_binding = digest_binding(
                                    b"claude-oauth-grant-lineage-account",
                                    grant_binding.as_str().as_bytes(),
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
                    start_anthropic_messages_model_call(factory, descriptor, call_context).await
                })
            },
        ))
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
        if !crate::oauth::claude_oauth_enrolled_model(requested_model) {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        self.refresh_if_needed().await?;
        self.ensure_account_identity().await?;
        let ((access_token, generation, grant_binding), identity) = self.identity_credentials()?;
        let key = AnthropicApiKey::new(access_token)
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let account = AnthropicAccountIdentity::new(grant_binding.as_str())
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let route = AnthropicPlatformRoute::claude_oauth(key, account)
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let now = timestamp_now().map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)?;
        let expires_at = pin_expires_at(now)?;
        let account_binding = digest_binding(
            b"claude-oauth-grant-lineage-account",
            grant_binding.as_str().as_bytes(),
        )?;
        let credential_authority_binding = digest_binding(
            b"claude-oauth-grant-lineage-credential",
            grant_binding.as_str().as_bytes(),
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
                .ensure_current_admission(SystemClock.now())
                .map_err(|_| renewal_error())?;
            let gate = self.credentials.request_gate().ok_or_else(renewal_error)?;
            let permit = gate
                .acquire_inference(self.input_bound)
                .map_err(|_| renewal_error())?;
            let ((access_token, _, grant_binding), identity) = self
                .credentials
                .identity_credentials()
                .map_err(|_| renewal_error())?;
            let account_binding = digest_binding(
                b"claude-oauth-grant-lineage-account",
                grant_binding.as_str().as_bytes(),
            )
            .map_err(|_| renewal_error())?;
            if identity != self.identity || account_binding != self.expected_account_binding {
                return Err(renewal_error());
            }
            let checked_at = SystemClock.now();
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

fn timestamp_now() -> Result<Timestamp, ClaudeNativeAuthError> {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)?;
    Timestamp::new(duration.as_secs(), duration.subsec_nanos())
        .map_err(|_| ClaudeNativeAuthError::CompatibilityInactive)
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
        CLAUDE_OAUTH_SOURCE_REVISION,
        Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).map_err(configuration_identity)?,
        expires_at,
        Arc::new(SystemClock),
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
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::StreamExt as _;
    use plakous_auth::subscription_limit::{
        InMemorySubscriptionRequestGate, SubscriptionRequestClass, SubscriptionRequestGate,
        SubscriptionRequestLimits, SystemSubscriptionGateClock,
    };
    use plakous_auth::{CredentialSlotId, CredentialStatus};
    use plakous_http::Limits;
    use plakous_model::{
        Actor, AttemptId, ContentBlock, ConversationItem, ConversationView,
        EffectiveProviderConfigurationId, EntryId, ModelRequest, Provenance, RequestMetadata,
        RunId, SelectedProviderConfigurationId, TextBlock, TurnId, ValidationMode,
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_rustls::{TlsAcceptor, server::TlsStream};
    use uuid::Uuid;

    use super::*;
    use crate::oauth::{
        CLAUDE_OAUTH_INFERENCE_SCOPE, CLAUDE_OAUTH_KILL_SWITCH_REVISION,
        CLAUDE_OAUTH_NOTICE_REVISION, CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_SOURCE_REVISION,
    };
    use crate::{AnthropicMessagesTargetBinding, ClaudeCompatibilityAcceptance};

    const TEST_GRANT_BINDING: &str = "grant-fixture-authority";

    #[test]
    fn claude_oauth_default_dispatch_budget_is_the_native_route_budget() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 300);
        let deadline = AnthropicHttpDeadline::new(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .expect("native dispatch budget");
        assert_eq!(deadline.duration(), Duration::from_secs(300));
    }

    fn test_context_authority() -> Digest {
        RuntimeModelAuthority::authority_class_for(
            digest_binding(
                b"claude-oauth-grant-lineage-account",
                TEST_GRANT_BINDING.as_bytes(),
            )
            .expect("account binding"),
            digest_binding(
                b"claude-oauth-grant-lineage-credential",
                TEST_GRANT_BINDING.as_bytes(),
            )
            .expect("credential binding"),
        )
        .expect("context authority")
    }

    fn private_store(temp: &tempfile::TempDir) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = temp.path().join("credentials");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory.join("claude.json")
    }

    fn manager(path: PathBuf) -> ClaudeNativeAuthManager {
        manager_with_client(path, Client::new_http1(Limits::default()).unwrap())
    }

    fn manager_with_client(path: PathBuf, client: Client) -> ClaudeNativeAuthManager {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
        let acceptance = ClaudeCompatibilityAcceptance::accept_exact(
            CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            CLAUDE_OAUTH_SOURCE_REVISION,
            CLAUDE_OAUTH_NOTICE_REVISION,
            CLAUDE_OAUTH_KILL_SWITCH_REVISION,
            now,
        )
        .unwrap();
        let slot = CredentialSlotId::new("primary").unwrap();
        ClaudeNativeAuthManager::open_selected_slot(
            path,
            slot,
            client,
            acceptance,
            crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
        )
        .unwrap()
    }

    async fn read_local_request(stream: &mut TlsStream<TcpStream>) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        let head_end = loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request head ended early");
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let head = String::from_utf8_lossy(&request[..head_end]);
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while request.len() < head_end + content_length {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request body ended early");
            request.extend_from_slice(&buffer[..count]);
        }
        request
    }

    async fn local_peer(responses: Vec<Vec<u8>>) -> (Client, Origin, JoinHandle<Vec<Vec<u8>>>) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .unwrap();
        let certificate = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .unwrap();
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = Client::with_tls_config_and_identity(
            Limits::default(),
            client_config,
            plakous_http::TlsConfigIdentity::from_parts(
                1,
                &[b"claude-oauth-model-local-v1", certificate.as_ref()],
            )
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let peer = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                requests.push(read_local_request(&mut stream).await);
                if response.is_empty() {
                    // Read the request, then drop TLS without `close_notify`.
                    drop(stream);
                    continue;
                }
                stream.write_all(&response).await.unwrap();
                let _ = stream.shutdown().await;
            }
            requests
        });
        (client, origin, peer)
    }

    fn inference_gate(inference: u32) -> Arc<InMemorySubscriptionRequestGate> {
        request_gate(0, inference)
    }

    fn request_gate(refresh: u32, inference: u32) -> Arc<InMemorySubscriptionRequestGate> {
        let limits = SubscriptionRequestLimits::new(
            1,
            refresh,
            inference,
            1_000_000,
            Some(Duration::from_secs(300)),
            0,
        )
        .unwrap();
        Arc::new(InMemorySubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ))
    }

    fn local_response(status: u16, content_type: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Local\r\ncontent-type: {content_type}\r\nrequest-id: req_oauth_local\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ).into_bytes()
    }

    const LOCAL_SUCCESS_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_local_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"local\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    async fn start_local_model(
        manager: ClaudeNativeAuthManager,
        client: Client,
        origin: Origin,
        workflow: bool,
    ) -> Result<Vec<plakous_model::ModelEvent>, Box<dyn std::error::Error>> {
        let (_, authority, descriptor) = manager
            .resolve_model("claude-sonnet-5", client.clone(), SessionLineage::Root)
            .await?;
        let now = authority.observed_at();
        let (token, _, binding) = manager.dispatch_credentials()?;
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new(token)?,
            AnthropicAccountIdentity::new(binding.as_str())?,
        )?;
        let resolved = oauth_resolved_target(
            route,
            "claude-sonnet-5",
            authority.authority_class_binding()?,
            now,
            pin_expires_at(now)?,
        )?
        .with_session_lineage(SessionLineage::Root);
        assert_eq!(
            resolved.route().origin(),
            crate::oauth::CLAUDE_OAUTH_MESSAGES_ORIGIN
        );
        let mut model = ClaudeOAuthLanguageModel::new(
            resolved,
            client,
            authority.clone(),
            manager.clone(),
            manager.request_identity()?,
        );
        model.test_dial_origin = Some(origin);
        let request = if workflow {
            workflow_request(now)
        } else {
            request(now)
        };
        let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
        let prepared = Arc::new(model).prepare_call(
            plan,
            ModelPreparationContext::new(authority, now)
                .with_cache_affinity(test_affinity(99, 0))
                .with_turn_position(plakous_model::TurnPosition::new(7, 9)),
        )?;
        let mut stream = prepared
            .start(ModelCallContext::new(
                AttemptId::try_from(Uuid::from_u128(4))?,
                plakous_model::CancellationToken::new(),
                None,
            ))
            .await?;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event?);
        }
        Ok(events)
    }

    fn configuration_ids() -> (
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    ) {
        let selected = Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"claude-oauth-context-test-selected"],
        )
        .expect("selected identity");
        let effective = Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"claude-oauth-context-test-effective"],
        )
        .expect("effective identity");
        (
            serde_json::from_value(serde_json::to_value(selected).expect("selected JSON"))
                .expect("selected configuration ID"),
            serde_json::from_value(serde_json::to_value(effective).expect("effective JSON"))
                .expect("effective configuration ID"),
        )
    }

    fn test_affinity(session: u128, epoch: u64) -> plakous_model::ModelCacheAffinity {
        plakous_model::ModelCacheAffinity::from_runtime_entropy(
            plakous_model::SessionId::try_from(Uuid::from_u128(session)).unwrap(),
            plakous_model::BranchId::try_from(Uuid::from_u128(100)).unwrap(),
            epoch,
            [1; 32],
        )
        .unwrap()
    }

    fn request(created_at: Timestamp) -> ModelRequest {
        request_with_metadata(RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(2)).expect("run ID"),
            TurnId::try_from(Uuid::from_u128(3)).expect("turn ID"),
            AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
            created_at,
        ))
    }

    fn workflow_request(created_at: Timestamp) -> ModelRequest {
        request_with_metadata(RequestMetadata::for_session_workflow(
            RunId::try_from(Uuid::from_u128(2)).expect("run ID"),
            plakous_model::SessionWorkflowRootId::new([5; 32]).expect("workflow ID"),
            AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
            created_at,
        ))
    }

    fn request_with_metadata(metadata: RequestMetadata) -> ModelRequest {
        request_with_text(metadata, "count the complete lowered body")
    }

    fn request_with_text(metadata: RequestMetadata, text: &str) -> ModelRequest {
        let (selected, effective) = configuration_ids();
        ModelRequest::builder(
            selected,
            effective,
            ConversationView::new(vec![
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(1)).expect("entry ID"),
                    Actor::User,
                    text.as_bytes()
                        .chunks(250_000)
                        .map(|chunk| {
                            ContentBlock::Text(
                                TextBlock::new(std::str::from_utf8(chunk).unwrap(), None)
                                    .expect("text"),
                            )
                        })
                        .collect(),
                    Provenance::UserInput,
                )
                .expect("conversation item"),
            ]),
            metadata,
        )
        .build()
        .expect("request")
    }

    #[test]
    fn refreshed_oauth_target_replays_persisted_thinking_without_reusing_stale_plans() {
        use plakous_model::{
            CapabilityRequirement, FixedClock, HistorySegment, HistorySegmentKind, ReasoningEffort,
            ReasoningPolicy, SystemIdGenerator,
        };

        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
        let later = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX + 60, 0).unwrap();
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-fixture").unwrap(),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).unwrap(),
        )
        .unwrap();
        let resolve = |time| {
            oauth_resolved_target(
                route.clone(),
                "claude-opus-5",
                test_context_authority(),
                time,
                pin_expires_at(time).unwrap(),
            )
            .unwrap()
        };
        let original = resolve(now);
        let refreshed = resolve(later);
        assert_ne!(
            original.descriptor().binding_digest().unwrap(),
            refreshed.descriptor().binding_digest().unwrap()
        );
        assert_eq!(
            original.descriptor().semantic_binding_digest().unwrap(),
            refreshed.descriptor().semantic_binding_digest().unwrap()
        );
        assert_ne!(
            AnthropicMessagesTargetBinding::from_resolved(&original).unwrap(),
            AnthropicMessagesTargetBinding::from_resolved(&refreshed).unwrap()
        );
        let build = |items| {
            let (selected, effective) = configuration_ids();
            ModelRequest::builder(
                selected,
                effective,
                ConversationView::new(items),
                request(now).metadata(),
            )
            .reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Medium),
                requirement: CapabilityRequirement::Required,
            })
            .build()
            .unwrap()
        };
        let user = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(101)).unwrap(),
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new("continue fixture", None).unwrap(),
            )],
            Provenance::UserInput,
        )
        .unwrap();
        let first_plan = original
            .descriptor()
            .validate_request(build(vec![user.clone()]), ValidationMode::Strict)
            .unwrap();
        let prepared = MessagesRequestTranslator::new(original.clone())
            .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
            ))
            .with_session_id(Some(test_affinity(99, 0).session()))
            .translate(&first_plan, ToolOwnership::default())
            .unwrap();
        assert!(
            MessagesRequestTranslator::new(refreshed.clone())
                .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                    crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                    crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap()
                ))
                .with_session_id(Some(test_affinity(99, 0).session()))
                .translate(&first_plan, ToolOwnership::default())
                .is_err()
        );

        for (kind, thinking) in [
            ("thinking", ""),
            ("thinking", "visible fixture"),
            ("redacted_thinking", ""),
        ] {
            let mut decoder = crate::AnthropicMessagesStreamDecoder::new(
                prepared.clone(),
                Arc::new(FixedClock::new(now)),
                Arc::new(SystemIdGenerator),
            )
            .unwrap();
            let mut events = vec![serde_json::json!({
                "type":"message_start", "message": {"id":"msg_fixture", "type":"message", "role":"assistant", "model":"claude-opus-5", "content":[], "stop_reason":null, "stop_sequence":null, "usage":{"input_tokens":1,"output_tokens":0}}
            })];
            let block = if kind == "thinking" {
                serde_json::json!({"type":"thinking", "thinking":""})
            } else {
                serde_json::json!({"type":"redacted_thinking", "data":"opaque-redacted-fixture"})
            };
            events.push(
                serde_json::json!({"type":"content_block_start", "index":0, "content_block":block}),
            );
            if kind == "thinking" {
                if !thinking.is_empty() {
                    events.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":thinking}}));
                }
                events.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature-fixture"}}));
            }
            events.extend([
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"done"}}),
                serde_json::json!({"type":"content_block_stop","index":1}),
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
                serde_json::json!({"type":"message_stop"}),
            ]);
            for event in events {
                let wire = format!(
                    "event: {}\ndata: {}\n\n",
                    event["type"].as_str().unwrap(),
                    event
                );
                decoder.push(wire.as_bytes()).unwrap();
            }
            decoder.finish_eof().unwrap();
            let (response, _, _) = decoder.into_terminal_projection().unwrap().into_parts();
            let segment = HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                response.output().to_vec(),
            )
            .unwrap();
            let persisted = serde_json::to_vec(&segment).unwrap();
            let reopened: HistorySegment = serde_json::from_slice(&persisted).unwrap();
            assert_eq!(reopened.canonical_bytes(), segment.canonical_bytes());
            let mut items = vec![user.clone()];
            items.extend(reopened.items().iter().cloned());
            items.push(
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(102)).unwrap(),
                    Actor::User,
                    vec![ContentBlock::Text(
                        TextBlock::new("next fixture", None).unwrap(),
                    )],
                    Provenance::UserInput,
                )
                .unwrap(),
            );
            let mut requests = Vec::new();
            for target in [original.clone(), refreshed.clone()] {
                let plan = target
                    .descriptor()
                    .validate_request(build(items.clone()), ValidationMode::Strict)
                    .unwrap();
                let prepared = MessagesRequestTranslator::new(target)
                    .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                        crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc")
                            .unwrap(),
                        crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
                    ))
                    .with_session_id(Some(test_affinity(99, 0).session()))
                    .translate(&plan, ToolOwnership::default())
                    .unwrap();
                assert_eq!(prepared.replay_diagnostics(), [] as [String; 0]);
                requests
                    .push(serde_json::from_slice::<serde_json::Value>(prepared.body()).unwrap());
            }
            assert_eq!(requests[0]["messages"], requests[1]["messages"]);
            let replayed = &requests[1]["messages"][1]["content"];
            assert_eq!(replayed[0]["type"], kind);
            if kind == "thinking" {
                assert_eq!(replayed[0]["thinking"], thinking);
                assert_eq!(replayed[0]["signature"], "opaque-signature-fixture");
            } else {
                assert_eq!(replayed[0]["data"], "opaque-redacted-fixture");
            }
            assert_eq!(replayed[1]["type"], "text");
        }
    }

    /// A resumed session must never hand Anthropic the model's own reasoning as
    /// assistant text.
    ///
    /// Live Claude Opus 5 returned `stop_reason=refusal`,
    /// `category=reasoning_extraction` on the resume position only, four times
    /// across two builds, with a benign prompt. A reasoning block that reaches
    /// lowering without its replay envelope is the shape that does it: the
    /// Messages contract replays a thinking block exactly with its signature
    /// and a redacted block opaque, and never as text.
    #[test]
    fn a_resumed_reasoning_block_without_its_signature_is_never_replayed_as_text() {
        use plakous_model::{CapabilityRequirement, ReasoningEffort, ReasoningPolicy};

        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-fixture").unwrap(),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).unwrap(),
        )
        .unwrap();
        let original = oauth_resolved_target(
            route,
            "claude-opus-5",
            test_context_authority(),
            now,
            pin_expires_at(now).unwrap(),
        )
        .unwrap();
        let build = |items| {
            let (selected, effective) = configuration_ids();
            ModelRequest::builder(
                selected,
                effective,
                ConversationView::new(items),
                request(now).metadata(),
            )
            .reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Medium),
                requirement: CapabilityRequirement::Required,
            })
            .build()
            .unwrap()
        };
        let user = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(201)).unwrap(),
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new("copy the seed", None).unwrap(),
            )],
            Provenance::UserInput,
        )
        .unwrap();
        // Exactly what a restored turn looks like when the signature did not
        // survive persistence: the thinking text is there, the envelope is not.
        let assistant = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(202)).unwrap(),
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(
                    plakous_model::ReasoningBlock::new(
                        plakous_model::ReasoningVisibility::Visible,
                        Some("the user wants the seed copied, so I will read it".to_owned()),
                        None,
                    )
                    .unwrap(),
                ),
                ContentBlock::Text(TextBlock::new("done", None).unwrap()),
            ],
            Provenance::ModelOutput,
        )
        .unwrap();
        let followup = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(203)).unwrap(),
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new("now do it again", None).unwrap(),
            )],
            Provenance::UserInput,
        )
        .unwrap();
        let plan = original
            .descriptor()
            .validate_request(
                build(vec![user, assistant, followup]),
                ValidationMode::Strict,
            )
            .unwrap();
        // The exact translator dispatch builds, so the replay discipline cannot
        // be dropped from one caller and kept in the other.
        let prepared = oauth_request_translator(
            original,
            crate::ClaudeOAuthRequestIdentity::new(
                crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
            ),
            Some(test_affinity(99, 0).session()),
        )
        .translate(&plan, ToolOwnership::default())
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(prepared.body()).unwrap();
        let replayed = &body["messages"][1]["content"];
        let blocks = replayed.as_array().expect("assistant content is an array");
        // The reasoning is gone, not converted. Only the assistant's visible
        // text survives.
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "done");
        let wire = String::from_utf8(prepared.body().to_vec()).unwrap();
        assert!(
            !wire.contains("the user wants the seed copied"),
            "the model's reasoning reached the wire as text"
        );
    }

    #[test]
    fn enrolled_oauth_descriptor_uses_route_evidence_and_pin_expiry() {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
        let expires_at = pin_expires_at(now).expect("pin expiry");
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-fixture").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("route");
        let resolved = oauth_resolved_target(
            route,
            "claude-sonnet-5",
            test_context_authority(),
            now,
            expires_at,
        )
        .expect("enrolled sonnet-5 descriptor");
        let descriptor = resolved.descriptor();
        assert_eq!(descriptor.requested_model().as_str(), "claude-sonnet-5");
        assert_eq!(
            descriptor.profile().as_str(),
            CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
        );
        for capability in [
            Capability::Streaming,
            Capability::Tools,
            Capability::ParallelTools,
            Capability::Reasoning,
            Capability::Caching,
        ] {
            let fact = descriptor
                .capabilities()
                .get(capability)
                .unwrap_or_else(|| panic!("missing {capability:?}"));
            assert_eq!(fact.support(), Support::Native);
            assert_eq!(fact.provenance().source(), FactSource::RouteEvidence);
            assert_eq!(fact.provenance().observed_at(), now);
            assert_eq!(fact.provenance().expires_at(), Some(expires_at));
        }
        let contract = descriptor.context_contract().expect("context contract");
        assert_eq!(contract.projection_revision(), "messages.projection.v3");
        assert!(matches!(
            contract.capacity().evidence_class(),
            plakous_model::ContextEvidenceClass::ReferenceClientEnvelope {
                catalog_receipt,
                reference_revision,
                ..
            } if reference_revision == REFERENCE_REVISION
                && *catalog_receipt == reference_client_catalog_receipt(
                    test_context_authority(),
                    "claude-sonnet-5",
                    1_000_000,
                    1_000_000,
                ).expect("catalog receipt")
        ));
        assert_eq!(
            contract.capacity().non_guarantee_disposition(),
            Some(plakous_model::ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
        );
        assert_eq!(
            contract.capacity().rule(),
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1_000_000),
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            }
        );
        assert_eq!(
            contract.capacity().authority_binding(),
            Some(test_context_authority())
        );
        let measurement = contract.measurements().first().expect("measurement");
        assert_eq!(
            measurement.authority_binding(),
            Some(test_context_authority())
        );
        assert!(matches!(
            measurement.kind(),
            CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt }
                if catalog_receipt == reference_client_catalog_receipt(
                    test_context_authority(),
                    "claude-sonnet-5",
                    1_000_000,
                    1_000_000,
                ).expect("catalog receipt")
        ));
        assert_eq!(resolved.model().thinking(), ThinkingSupport::Adaptive);
        assert_eq!(
            resolved.dialect().beta_header().as_deref(),
            Some(crate::claude_oauth_beta_header("claude-sonnet-5"))
        );
        assert!(now < expires_at);
        assert_eq!(
            contract.capacity().freshness().expires_at(),
            Some(expires_at)
        );
    }

    #[test]
    fn reference_receipt_changes_with_authority_model_and_context_row() {
        let authority = test_context_authority();
        let baseline =
            reference_client_catalog_receipt(authority, "claude-sonnet-5", 1_000_000, 1_000_000)
                .expect("baseline receipt");
        let other_authority = RuntimeModelAuthority::authority_class_for(
            digest_binding(b"claude-oauth-account", b"other").expect("account"),
            digest_binding(b"claude-oauth-credential", b"other").expect("credential"),
        )
        .expect("other authority");
        assert_ne!(
            baseline,
            reference_client_catalog_receipt(
                other_authority,
                "claude-sonnet-5",
                1_000_000,
                1_000_000,
            )
            .expect("authority receipt")
        );
        assert_ne!(
            baseline,
            reference_client_catalog_receipt(authority, "claude-opus-5", 1_000_000, 1_000_000,)
                .expect("model receipt")
        );
        assert_ne!(
            baseline,
            reference_client_catalog_receipt(authority, "claude-sonnet-5", 200_000, 200_000,)
                .expect("context receipt")
        );
    }

    #[test]
    fn oauth_complete_wire_boundary_preserves_typed_context_denial() {
        let temp = tempfile::tempdir().unwrap();
        // Preparation needs identity and authority, but no stored grant or dispatch.
        let credentials = manager(private_store(&temp));
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
        let expiry = pin_expires_at(now).unwrap();
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("offline-unused").unwrap(),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).unwrap(),
        )
        .unwrap();
        let resolved = oauth_resolved_target(
            route,
            "claude-sonnet-5",
            test_context_authority(),
            now,
            expiry,
        )
        .unwrap();
        let descriptor = resolved.descriptor().clone();
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            digest_binding(
                b"claude-oauth-grant-lineage-account",
                TEST_GRANT_BINDING.as_bytes(),
            )
            .unwrap(),
            digest_binding(
                b"claude-oauth-grant-lineage-credential",
                TEST_GRANT_BINDING.as_bytes(),
            )
            .unwrap(),
            1,
            now,
            Some(expiry),
        )
        .unwrap();
        let identity = crate::ClaudeOAuthRequestIdentity::new(
            crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
            crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
        );
        let model = Arc::new(ClaudeOAuthLanguageModel::new(
            resolved,
            Client::new_http1(Limits::default()).unwrap(),
            authority.clone(),
            credentials,
            identity,
        ));
        let context =
            ModelPreparationContext::new(authority, now).with_cache_affinity(test_affinity(99, 0));
        let plan_for = |text: &str| {
            descriptor
                .validate_request(
                    request_with_text(request(now).metadata(), text),
                    ValidationMode::Strict,
                )
                .unwrap()
        };
        // Keep four text blocks at every boundary, below the per-string bound.
        let baseline_text = "x".repeat(750_001);
        let baseline = model
            .clone()
            .prepare_call(plan_for(&baseline_text), context.clone())
            .unwrap();
        let overhead = baseline.record().projection().wire_size() - 750_001;
        let limit = 1_000_000;
        assert_eq!(
            descriptor.context_contract().unwrap().capacity().rule(),
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(limit),
                maximum_projected_input: limit,
                auto_compact_input: None,
            }
        );
        for wire_size in [limit - 1, limit, limit + 1] {
            let text = "x".repeat(usize::try_from(wire_size - overhead).unwrap());
            let plan = plan_for(&text);
            let input_identity = plan.request().model_input_identity();
            let lowered = oauth_request_translator(
                model.resolved.clone(),
                model.identity.clone(),
                Some(test_affinity(99, 0).session()),
            )
            .translate(&plan, ToolOwnership::default())
            .unwrap();
            assert_eq!(u64::try_from(lowered.body().len()).unwrap(), wire_size);
            let body: serde_json::Value = serde_json::from_slice(lowered.body()).unwrap();
            assert_eq!(
                body["max_tokens"],
                crate::claude_oauth_request_max_tokens("claude-sonnet-5").expect("request default")
            );
            let result = model.clone().prepare_call(plan.clone(), context.clone());
            assert_eq!(result.unwrap().record().projection().wire_size(), wire_size);
            if wire_size <= limit {
                continue;
            }
            let error = model
                .clone()
                .prepare_call(plan, context.clone().with_required_input_bound())
                .expect_err("the paid fold requires a conservative input bound");
            assert!(matches!(
                error,
                ModelPreparationError::ContextAdmissionDenied { .. }
            ));
            let status = error.context_status().unwrap();
            assert_eq!(status.model_input_identity(), input_identity);
            assert_eq!(
                status.measurement().unwrap().bound().upper_bound(),
                wire_size
            );
            let status_identity = status.identity();
            let projected = ModelError::from(error);
            assert_eq!(projected.kind(), ModelErrorKind::ContextLimit);
            let failure = projected.context_capacity().unwrap();
            assert_eq!(failure.status_identity(), status_identity);
            assert_eq!(failure.model_input_identity(), input_identity);
            assert!(!failure.is_compactable());
        }
    }

    #[test]
    fn unknown_reference_context_row_fails_closed() {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
        let expires_at = pin_expires_at(now).expect("pin expiry");
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-unknown-access-token-fixture").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("route");
        assert!(
            oauth_resolved_target(
                route,
                "claude-future-neighbor",
                test_context_authority(),
                now,
                expires_at,
            )
            .is_err()
        );
        assert!(crate::oauth::claude_enrollment("claude-future-neighbor").is_none());
    }

    #[tokio::test]
    async fn prepared_call_binds_complete_wire_byte_observation_to_runtime_authority() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manager = manager(private_store(&temp));
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-lineage-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .expect("credential");
        let client = Client::new_http1(Limits::default()).expect("client");
        let (model, authority, descriptor) = manager
            .resolve_model("claude-sonnet-5", client, SessionLineage::Root)
            .await
            .expect("resolved model");
        let prepared_at = authority.observed_at();
        let plan = descriptor
            .validate_request(request(prepared_at), ValidationMode::Strict)
            .expect("validated plan");
        let prepared = model
            .prepare_call(
                plan,
                ModelPreparationContext::new(authority.clone(), prepared_at)
                    .with_cache_affinity(test_affinity(99, 0)),
            )
            .expect("prepared call");
        let projection = prepared.record().projection();
        assert_eq!(projection.projection_revision(), "messages.projection.v3");
        assert_eq!(
            projection.authority_class(),
            authority
                .authority_class_binding()
                .expect("authority class")
        );
        let contract = descriptor.context_contract().expect("context contract");
        assert_eq!(
            contract.capacity().authority_binding(),
            Some(projection.authority_class())
        );
        let certificate = contract.measurements().first().expect("measurement");
        assert_eq!(
            certificate.authority_binding(),
            Some(projection.authority_class())
        );
        let observation = projection
            .token_projection()
            .observation()
            .expect("count observation");
        assert_eq!(observation.certificate_id(), certificate.certificate_id());
        assert_eq!(observation.certificate_revision(), certificate.revision());
        assert_eq!(observation.tokens(), projection.wire_size());
        assert!(projection.wire_size() > 0);
    }

    async fn refreshed_session_models(
        manager: ClaudeNativeAuthManager,
    ) -> (Arc<ClaudeOAuthLanguageModel>, Arc<ClaudeOAuthLanguageModel>) {
        let (_, authority, descriptor) = manager
            .resolve_model(
                "claude-sonnet-5",
                Client::new_http1(Limits::default()).unwrap(),
                SessionLineage::Root,
            )
            .await
            .unwrap();
        let now = authority.observed_at();
        let (token, _, binding) = manager.dispatch_credentials().unwrap();
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new(token).unwrap(),
            AnthropicAccountIdentity::new(binding.as_str()).unwrap(),
        )
        .unwrap();
        let resolved = oauth_resolved_target(
            route,
            "claude-sonnet-5",
            authority.authority_class_binding().unwrap(),
            now,
            pin_expires_at(now).unwrap(),
        )
        .unwrap();
        let model = Arc::new(ClaudeOAuthLanguageModel::new(
            resolved,
            Client::new_http1(Limits::default()).unwrap(),
            authority.clone(),
            manager.clone(),
            manager.request_identity().unwrap(),
        ));
        let later = Timestamp::new(now.seconds() + 1, 0).unwrap();
        let refreshed = oauth_resolved_target(
            model.resolved.route().clone(),
            "claude-sonnet-5",
            authority.authority_class_binding().unwrap(),
            later,
            pin_expires_at(later).unwrap(),
        )
        .unwrap();
        assert_ne!(
            descriptor.binding_digest().unwrap(),
            refreshed.descriptor().binding_digest().unwrap()
        );
        let refreshed_authority = RuntimeModelAuthority::new(
            refreshed.descriptor(),
            authority.account_binding(),
            authority.credential_authority_binding(),
            authority.credential_generation(),
            later,
            authority.valid_until(),
        )
        .unwrap();
        let refreshed_model = Arc::new(ClaudeOAuthLanguageModel::new(
            refreshed,
            model.client.clone(),
            refreshed_authority,
            manager.clone(),
            manager.request_identity().unwrap(),
        ));
        (model, refreshed_model)
    }

    #[tokio::test]
    async fn revalidation_rejects_new_grant_even_for_same_provider_uuid() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        manager
            .persist_test_grant(
                "first-access",
                "first-refresh",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let (model, _) = refreshed_session_models(manager.clone()).await;
        manager
            .persist_test_grant(
                "second-access",
                "second-refresh",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let now = model.authority.observed_at();
        assert!(matches!(
            model.revalidate_runtime_authority(now).await,
            Err(RuntimeModelAuthorityError::Stale)
        ));
    }

    #[tokio::test]
    async fn preparation_uses_persisted_session_across_refresh_and_cache_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let (model, refreshed_model) = refreshed_session_models(manager).await;
        let now = model.authority.observed_at();
        let source = plakous_agent::internal::RuntimeContextState::genesis(
            test_affinity(99, 0).session(),
            test_affinity(99, 0).branch(),
            [1; 32],
        )
        .unwrap();
        let restored_state =
            plakous_agent::internal::RuntimeContextState::decode(&source.encode().unwrap())
                .unwrap()
                .retained_for_exact_resume();
        let fork = source
            .ordinary_fork(
                plakous_model::CheckpointId::try_from(Uuid::from_u128(102)).unwrap(),
                test_affinity(101, 0).session(),
                plakous_model::BranchId::try_from(Uuid::from_u128(103)).unwrap(),
                [2; 32],
            )
            .unwrap();
        let original_affinity = source.affinity().clone();
        let restored = restored_state.affinity().clone();
        let rotated = test_affinity(99, 1);
        assert_ne!(original_affinity.identity(), rotated.identity());
        let forked = fork.affinity().clone();
        let mut digests = Vec::new();
        for (candidate, affinity) in [
            (model.clone(), original_affinity),
            (refreshed_model.clone(), restored),
            (refreshed_model.clone(), rotated),
            (refreshed_model.clone(), forked),
        ] {
            let plan = candidate
                .resolved
                .descriptor()
                .validate_request(request(now), ValidationMode::Strict)
                .unwrap();
            let lowered = MessagesRequestTranslator::new(candidate.resolved.clone())
                .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                    crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                    crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
                ))
                .with_session_id(Some(affinity.session()))
                .translate(&plan, ToolOwnership::default())
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(lowered.body()).unwrap();
            assert_eq!(
                body["max_tokens"],
                crate::claude_oauth_request_max_tokens("claude-sonnet-5").expect("request default")
            );
            let metadata: serde_json::Value =
                serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
            assert_eq!(metadata["session_id"], affinity.session().to_string());
            let expected =
                Digest::sha256(DigestDomain::Blob, 1, &[WIRE_DOMAIN, lowered.body()]).unwrap();
            let context = ModelPreparationContext::new(
                candidate.authority.clone(),
                candidate.authority.observed_at(),
            )
            .with_cache_affinity(affinity);
            let prepared = candidate.prepare_call(plan, context).unwrap();
            assert_eq!(prepared.record().projection().wire_digest(), expected);
            digests.push(expected);
        }
        assert_eq!(digests[0], digests[1]);
        assert_eq!(digests[0], digests[2]);
        assert_ne!(digests[0], digests[3]);
    }

    #[tokio::test]
    async fn oauth_preparation_requires_explicit_logical_session() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let (model, _) = refreshed_session_models(manager).await;
        let now = model.authority.observed_at();
        let plan = model
            .resolved
            .descriptor()
            .validate_request(request(now), ValidationMode::Strict)
            .unwrap();
        for _ in 0..2 {
            let error = model
                .clone()
                .prepare_call(
                    plan.clone(),
                    ModelPreparationContext::new(model.authority.clone(), now),
                )
                .expect_err("missing session must reject preparation");
            assert!(matches!(
                &error,
                ModelPreparationError::Rejected {
                    kind: ModelPreparationErrorKind::Projection,
                    ..
                }
            ));
            assert!(
                error
                    .to_string()
                    .contains("runtime logical session identity")
            );
        }
    }

    #[test]
    fn haiku_5_5_is_enrolled_on_oauth_with_the_shared_window() {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
        let expires_at = pin_expires_at(now).expect("pin expiry");
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-haiku-access-token-fixture").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("route");
        let resolved = oauth_resolved_target(
            route,
            "claude-haiku-5-5",
            test_context_authority(),
            now,
            expires_at,
        )
        .expect("enrolled haiku 5.5");
        assert_eq!(resolved.model().remote_id().as_str(), "claude-haiku-5-5");
        assert_eq!(resolved.model().family().as_str(), "claude-5-haiku");
        assert_eq!(resolved.model().maximum_output_tokens(), 128_000);
        assert_eq!(resolved.model().thinking(), ThinkingSupport::Adaptive);
        assert!(resolved.dialect().is_claude_oauth());
        assert_eq!(
            resolved
                .descriptor()
                .context_contract()
                .expect("context")
                .capacity()
                .rule(),
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1_000_000),
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            }
        );
        assert_eq!(
            resolved.dialect().beta_header().as_deref(),
            Some(crate::claude_oauth_beta_header("claude-haiku-5-5"))
        );
    }

    #[test]
    fn haiku_4_5_is_not_enrolled() {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
        let expires_at = pin_expires_at(now).expect("pin expiry");
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-haiku-access-token-fixture").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("route");
        assert!(
            oauth_resolved_target(
                route,
                "claude-haiku-4-5",
                test_context_authority(),
                now,
                expires_at,
            )
            .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_then_reserve_charges_inference_only_after_success() {
        let temp = tempfile::tempdir().unwrap();
        let gate = inference_gate(1);
        let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
        let status = manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        assert_eq!(status.status(), CredentialStatus::Ready);
        let permit = refresh_then_reserve_inference(&manager, 32)
            .await
            .expect("fresh Claude credential must reserve inference");
        drop(permit);
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 0);
        assert_eq!(
            gate.snapshot()
                .unwrap()
                .class(SubscriptionRequestClass::AuthRefresh)
                .reserved(),
            0,
            "fresh access must not charge AuthRefresh"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_second_physical_dispatch_requires_a_second_inference_permit() {
        let temp = tempfile::tempdir().unwrap();
        let gate = inference_gate(2);
        let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        for _ in 0..2 {
            let mut permit = refresh_then_reserve_inference(&manager, 32)
                .await
                .expect("fresh permit for each physical dispatch");
            permit
                .authorize_dispatch()
                .expect("one permit authorizes once");
            drop(permit);
        }
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 2);
        assert_eq!(inference.dispatched(), 2);
        assert!(refresh_then_reserve_inference(&manager, 32).await.is_err());
    }

    #[tokio::test]
    async fn production_oauth_turn_retries_local_429_with_fresh_inference_permit() {
        let temp = tempfile::tempdir().unwrap();
        let refusal = local_response(
            429,
            "application/json",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}"#,
        );
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, peer) = local_peer(vec![refusal, success]).await;
        let gate = inference_gate(2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let events = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, false),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
        );
        let requests = timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|wire| wire.starts_with(b"POST /v1/messages?beta=true "))
        );
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 2);
        assert_eq!(inference.dispatched(), 2);
        assert_eq!(
            gate.snapshot()
                .unwrap()
                .class(SubscriptionRequestClass::AuthRefresh)
                .reserved(),
            0
        );
    }

    #[tokio::test]
    async fn production_oauth_turn_refreshes_after_local_401_with_new_permit() {
        let temp = tempfile::tempdir().unwrap();
        let rejected = local_response(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"fixture"}}"#,
        );
        let token = local_response(
            200,
            "application/json",
            r#"{"access_token":"offline-access-two","refresh_token":"offline-refresh-two","expires_in":3600,"scope":"user:inference","account":{"uuid":"12345678-1234-4234-8234-123456789abc"}}"#,
        );
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, peer) = local_peer(vec![rejected, token, success]).await;
        let gate = request_gate(1, 2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_test_token_origin(&origin)
            .unwrap()
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let events = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, false),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
        );
        let requests = timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with(b"POST /v1/messages?beta=true "));
        assert!(requests[1].starts_with(b"POST /v1/oauth/token "));
        assert!(requests[2].starts_with(b"POST /v1/messages?beta=true "));
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 2);
        assert_eq!(inference.dispatched(), 2);
        let refresh = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::AuthRefresh);
        assert_eq!(refresh.reserved(), 1);
        assert_eq!(refresh.dispatched(), 1);
    }

    #[tokio::test]
    async fn production_oauth_session_workflow_connect_retry_requires_fresh_authorization() {
        for inference_limit in [1, 2] {
            let temp = tempfile::tempdir().unwrap();
            let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
            let (client, origin, mut peer) = local_peer(vec![success]).await;
            let client = client.with_faults(Some(
                plakous_http::FaultPlan::parse("fault=connect,nth=1").unwrap(),
            ));
            let gate = inference_gate(inference_limit);
            let manager = manager_with_client(private_store(&temp), client.clone())
                .with_subscription_request_gate(gate.clone());
            manager
                .persist_test_grant(
                    "offline-access-one",
                    "offline-refresh-one",
                    3600,
                    CLAUDE_OAUTH_INFERENCE_SCOPE,
                )
                .unwrap();
            let result = timeout(
                Duration::from_secs(5),
                start_local_model(manager, client, origin, true),
            )
            .await
            .unwrap();
            let inference = gate
                .snapshot()
                .unwrap()
                .class(SubscriptionRequestClass::Inference);
            assert_eq!(inference.reserved(), 1);
            assert_eq!(inference.dispatched(), 1);
            let events = result.expect("unsent workflow refunds then takes a fresh permit");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
            );
            let requests = timeout(Duration::from_secs(5), &mut peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(requests.len(), 1, "unsent retry adds no paid dispatch");
            let wire = std::str::from_utf8(&requests[0]).unwrap();
            assert!(wire.contains("cc_prompt_index=7; cc_turn_index=9;"));
        }
    }

    #[tokio::test]
    async fn production_oauth_incomplete_requests_never_refund() {
        for fault in ["record-rejected", "write-reset"] {
            let temp = tempfile::tempdir().unwrap();
            let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
            let (client, origin, mut peer) = local_peer(vec![success]).await;
            let client = client.with_faults(Some(
                plakous_http::FaultPlan::parse(&format!("fault={fault},nth=1")).unwrap(),
            ));
            let gate = inference_gate(2);
            let manager = manager_with_client(private_store(&temp), client.clone())
                .with_subscription_request_gate(gate.clone());
            manager
                .persist_test_grant(
                    "offline-access-one",
                    "offline-refresh-one",
                    3600,
                    CLAUDE_OAUTH_INFERENCE_SCOPE,
                )
                .unwrap();
            let result = timeout(
                Duration::from_secs(5),
                start_local_model(manager, client, origin, true),
            )
            .await
            .unwrap();
            let inference = gate
                .snapshot()
                .unwrap()
                .class(SubscriptionRequestClass::Inference);
            assert_eq!(inference.reserved(), 2);
            assert_eq!(inference.dispatched(), 2);
            assert!(inference.input_token_upper_bound() > 0);
            let events = result.expect("incomplete workflow needs another consumed permit");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
            );
            let requests = timeout(Duration::from_secs(5), &mut peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(requests.len(), 1, "unsent retry adds no paid dispatch");
            let wire = std::str::from_utf8(&requests[0]).unwrap();
            assert!(wire.contains("cc_prompt_index=7; cc_turn_index=9;"));
        }
    }

    #[tokio::test]
    async fn production_oauth_session_workflow_401_never_refreshes_or_redispatches() {
        let temp = tempfile::tempdir().unwrap();
        let rejected = local_response(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"fixture"}}"#,
        );
        let (client, origin, peer) = local_peer(vec![rejected]).await;
        let gate = request_gate(1, 2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_test_token_origin(&origin)
            .unwrap()
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, true),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "workflow must surface the first 401");
        let requests = timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(b"POST /v1/messages?beta=true "));
        let snapshot = gate.snapshot().unwrap();
        let inference = snapshot.class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 1);
        let refresh = snapshot.class(SubscriptionRequestClass::AuthRefresh);
        assert_eq!(refresh.reserved(), 0);
        assert_eq!(refresh.dispatched(), 0);
    }

    /// The peer reads the turn's request and drops TLS without a byte or a
    /// `close_notify`: the cost-only turn replays it with a fresh permit.
    #[tokio::test]
    async fn production_oauth_turn_replays_a_dropped_exchange_after_send() {
        let temp = tempfile::tempdir().unwrap();
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, peer) = local_peer(vec![Vec::new(), success]).await;
        let gate = inference_gate(2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let events = timeout(
            Duration::from_secs(10),
            start_local_model(manager, client, origin, false),
        )
        .await
        .unwrap()
        .unwrap();
        let retries = events
            .iter()
            .filter_map(|event| match event {
                plakous_model::ModelEvent::Retry(retry) => Some(retry.cause().kind()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retries, ["transport_after_send"]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
        );
        let requests = timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.dispatched(), 2);
    }

    /// A session workflow (compaction) is single-dispatch: the same dropped
    /// exchange ends it, and the queued success is never requested.
    #[tokio::test]
    async fn production_oauth_session_workflow_drop_is_never_replayed() {
        let temp = tempfile::tempdir().unwrap();
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, peer) = local_peer(vec![Vec::new(), success]).await;
        let gate = inference_gate(2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(10),
            start_local_model(manager, client, origin, true),
        )
        .await
        .unwrap();
        assert!(
            result.is_err(),
            "the workflow surfaces the dropped exchange"
        );
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 1);
        peer.abort();
    }

    #[tokio::test]
    async fn production_oauth_retry_stops_when_second_inference_permit_is_denied() {
        let temp = tempfile::tempdir().unwrap();
        let refusal = local_response(
            429,
            "application/json",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}"#,
        );
        let (client, origin, peer) = local_peer(vec![refusal]).await;
        let gate = inference_gate(1);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, false),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "a denied second permit must end the turn");
        assert_eq!(
            timeout(Duration::from_secs(5), peer)
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 1);
        assert_eq!(inference.denied(), 1);
    }

    #[test]
    fn cancelled_retry_drops_reserved_permit_without_dispatch_authorization() {
        let gate = inference_gate(1);
        let permit = gate.acquire_inference(32).expect("reserve retry permit");
        let authorize = permit_authorization(permit);
        assert_eq!(
            gate.snapshot()
                .unwrap()
                .class(SubscriptionRequestClass::Inference)
                .dispatched(),
            0,
            "refresh completion alone must not count a physical send"
        );
        drop(authorize);
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.dispatched(), 0);
        assert_eq!(inference.abandoned(), 1);
    }

    #[test]
    fn one_retry_dispatch_authorization_consumes_one_permit() {
        let gate = inference_gate(1);
        let permit = gate.acquire_inference(32).expect("reserve retry permit");
        let authorize = permit_authorization(permit);
        (authorize.authorize)().expect("first physical send");
        assert!((authorize.authorize)().is_err(), "permit cannot be reused");
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.dispatched(), 1);
        assert_eq!(inference.abandoned(), 0);
    }

    #[test]
    fn refresh_transport_preserves_typed_cause_and_failure_prose() {
        let error = map_credential_error(ClaudeNativeAuthError::Transport(Some(
            plakous_model::ModelNetworkFailure {
                phase: plakous_model::ModelRetryPhase::Resolution,
                sent: plakous_model::ModelNetworkTransmission::NotSent,
                timed_out: true,
            },
        )));
        assert_eq!(error.kind(), ModelErrorKind::Transport);
        assert_eq!(
            error.message(),
            "Claude sign-in refresh failed: DNS lookup timed out before anything was sent; credential unchanged"
        );
    }

    #[test]
    fn refresh_without_unsent_proof_never_claims_the_credential_is_unchanged() {
        for cause in [
            None,
            Some(plakous_model::ModelNetworkFailure {
                phase: plakous_model::ModelRetryPhase::ResponseBody,
                sent: plakous_model::ModelNetworkTransmission::MayHaveSent,
                timed_out: false,
            }),
        ] {
            let error = map_credential_error(ClaudeNativeAuthError::Transport(cause));
            assert_eq!(error.kind(), ModelErrorKind::Authentication);
            assert!(!error.message().contains("credential unchanged"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_refresh_does_not_consume_inference_class() {
        let temp = tempfile::tempdir().unwrap();
        let gate = inference_gate(1);
        let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        manager.logout_local().unwrap();
        let error = refresh_then_reserve_inference(&manager, 32)
            .await
            .err()
            .expect("logged-out Claude refresh must fail");
        assert_eq!(error.kind(), ModelErrorKind::Authentication);
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 0);
        assert_eq!(inference.dispatched(), 0);
        assert_eq!(inference.denied(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_inference_gate_fails_after_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(private_store(&temp));
        manager
            .persist_test_grant(
                "access-token-value",
                "refresh-token-value",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let error = refresh_then_reserve_inference(&manager, 32)
            .await
            .err()
            .expect("Claude inference must fail closed without a gate");
        assert_eq!(error.kind(), ModelErrorKind::ResourceLimit);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generation_change_after_refresh_still_reserves_inference() {
        let temp = tempfile::tempdir().unwrap();
        let gate = inference_gate(1);
        let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
        let first = manager
            .persist_test_grant(
                "access-token-one",
                "refresh-token-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        manager
            .persist_test_grant(
                "access-token-two",
                "refresh-token-two",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let permit = refresh_then_reserve_inference(&manager, 32)
            .await
            .expect("rotated Claude generation must still reserve inference");
        drop(permit);
        let (token, generation, _) = manager.dispatch_credentials().unwrap();
        assert_eq!(token, "access-token-two");
        assert_ne!(generation, first.generation().get());
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.denied(), 0);
    }

    #[test]
    fn oauth_access_token_rebind_keeps_prepared_target_binding() {
        let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
        let expires_at = pin_expires_at(now).expect("pin expiry");
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-one").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("route");
        let resolved = oauth_resolved_target(
            route,
            "claude-sonnet-5",
            test_context_authority(),
            now,
            expires_at,
        )
        .expect("enrolled sonnet-5 descriptor");
        let binding =
            AnthropicMessagesTargetBinding::from_resolved(&resolved).expect("prepared binding");
        let rotated = resolved
            .route()
            .with_oauth_access_token("oauth-access-token-two")
            .expect("OAuth rebind");
        binding
            .validate_route(&rotated)
            .expect("rebind must retain prepared credential authority");
        let independently_wrapped = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-two").expect("key"),
            AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
        )
        .expect("independent route");
        assert!(
            binding.validate_route(&independently_wrapped).is_err(),
            "independently wrapping the rotated token must not reuse prepared authority"
        );
    }
}
