use super::common_error::map_credential_status;
use super::login::{PendingCompatibilityConsent, complete_pending_with_consent};
use super::vertical::{NativeFuture, NativeVertical, NativeVerticalEntry};
use super::{
    Arc, CompatibilityNotice, CompiledProviderConfiguration, LoadedAcceptance, ModelId,
    NativeAuthStatus, NativeLoginCompletion, NativeLoginInstructions, NativeLoginMode,
    NativeModelResolutionError, NativeModelSelector, NativeModelSummary, NativeModelVisibility,
    NativeProvider, NativeSubscriptionError, NativeSubscriptions, NativeToolMode,
    PendingNativeLogin, ResolvedModelDescriptor, ResolvedNativeModel, RuntimeModelAuthority,
    SelectedProviderConfiguration, SessionLineage, SubscriptionRequestGate,
    SubscriptionResolutionEvidence, SubscriptionUsageRead, Timestamp, credential_refresh_observer,
    detached, run_blocking, unix_now,
};

pub(super) fn xai_native_http_config()
-> Result<plakous_xai::StandardXaiHttpConfig, NativeSubscriptionError> {
    plakous_xai::StandardXaiHttpConfig::new(
        super::responses_budget::NATIVE_INFERENCE_OPERATION_TIMEOUT,
        plakous_xai::StandardXaiHttpConfig::default().event_channel_capacity(),
        plakous_xai::StandardXaiHttpConfig::default().responses_limits(),
    )
    .map_err(|_| NativeSubscriptionError::Internal)
}

pub(super) struct XaiOnline {
    pub(super) manager: Arc<plakous_xai::XaiNativeAuthManager>,
    pub(super) acceptance: LoadedAcceptance,
}

pub(super) fn xai_inference_client(
    observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
) -> Result<plakous_http::Client, NativeSubscriptionError> {
    plakous_http::Client::new_http1(plakous_http::Limits::for_streaming_inference())
        .map(|client| client.pooled().with_observer(observer))
        .map_err(|_| NativeSubscriptionError::Internal)
}

impl NativeSubscriptions {
    pub(super) async fn xai_online(&self) -> Result<XaiOnline, NativeSubscriptionError> {
        self.xai_online_with_gate(None, true).await
    }

    pub(super) async fn xai_online_for_login(&self) -> Result<XaiOnline, NativeSubscriptionError> {
        self.xai_online_with_gate(None, false).await
    }

    pub(super) async fn xai_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<XaiOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Xai)?;
        let now = self.state.now()?;
        run_blocking(move || paths.prepare()).await??;
        // Checked before anything else is read or claimed, so no online path
        // writes over a newer build's state.
        if self
            .written_by_newer_build_unlocked(NativeProvider::Xai)
            .await?
        {
            return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
        }
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Xai);
        let status_path = path.clone();
        let slot = self.state.slot_id()?;
        let status_slot = slot.clone();
        let local_status = run_blocking(move || {
            let slot = status_slot;
            let admin = plakous_xai::XaiOfflineSelectedSlot::open_explicit(status_path, slot)
                .map_err(map_xai_error)?;
            admin
                .status()
                .map(map_xai_offline_status)
                .map_err(map_xai_error)
        })
        .await??;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Xai,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        let catalog_observer = self.exchange_observer.clone();
        let catalog_retries = self.catalog_retries;
        let source_scope = self.state.source_scope();
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let secondary = notice
                .secondary_source_revision
                .ok_or(NativeSubscriptionError::Internal)?;
            let provider_acceptance = plakous_xai::XaiCompatibilityAcceptance::accept_exact(
                notice.profile_id,
                notice.source_revision,
                secondary,
                notice.notice_revision,
                notice.kill_switch_revision,
                timestamp,
            )
            .map_err(map_xai_error)?;
            // Auth and catalog keep the default phase bounds. Inference uses
            // its own H1 client with the model's longer response-head budget.
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;

            let mut manager = plakous_xai::XaiNativeAuthManager::open_selected_slot(
                path,
                slot,
                client,
                provider_acceptance,
            )
            .map_err(map_xai_error)?;
            if let Some(scope) = source_scope {
                manager = manager
                    .with_credential_source_scope(scope)
                    .map_err(map_xai_error)?;
            }
            manager = manager
                .with_catalog_observer(catalog_observer)
                .map_err(map_xai_error)?;
            if !catalog_retries {
                manager = manager.without_catalog_retries().map_err(map_xai_error)?;
            }
            if let Some(gate) = request_gate {
                manager = manager
                    .with_subscription_request_gate(gate)
                    .map_err(map_xai_error)?;
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Xai) {
                manager = manager
                    .with_credential_refresh_observer(observer)
                    .map_err(map_xai_error)?;
            }
            Ok(XaiOnline {
                manager: Arc::new(manager),
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }

    /// The selectable models of a fresh SuperGrok catalog.
    pub(super) async fn xai_catalog_models(
        &self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError> {
        {
            let online = self.xai_online_with_gate(None, true).await?;
            online
                .manager
                .refresh_if_needed()
                .await
                .map_err(map_xai_error)?;
        }
        let online = self.xai_online_with_gate(Some(gate), true).await?;
        let receipt = online
            .manager
            .refresh_catalog()
            .await
            .map_err(map_xai_error)?;
        Ok(receipt
            .entries()
            .iter()
            .filter(|entry| facade_xai_model_is_selectable(entry.model().as_str()))
            .map(|entry| NativeModelSummary {
                unavailable_reason: None,
                exact_id: entry.model().as_str().to_owned(),
                visibility: NativeModelVisibility::List,
                native_tool_mode: None,
                streaming: true,
                tools: true,
                reasoning_efforts: plakous_xai::subscription_model_reasoning_effort_options(
                    entry.model().as_str(),
                )
                .to_vec(),
                default_reasoning_effort: None,
                service_tiers: Vec::new(),
                default_service_tier: None,
                reported_context_length: Some(u64::from(
                    entry.diagnostics().reported_context_length().get(),
                )),
            })
            .collect())
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_xai_offline_status(
    status: plakous_xai::XaiOfflineCredentialStatus,
) -> NativeAuthStatus {
    match status {
        plakous_xai::XaiOfflineCredentialStatus::SignedOut => NativeAuthStatus::SignedOut,
        plakous_xai::XaiOfflineCredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_xai::XaiOfflineCredentialStatus::RefreshRequired => {
            NativeAuthStatus::RefreshRequired
        }
        plakous_xai::XaiOfflineCredentialStatus::Quarantined => NativeAuthStatus::Quarantined,
        plakous_xai::XaiOfflineCredentialStatus::Refreshing => NativeAuthStatus::Refreshing,
        plakous_xai::XaiOfflineCredentialStatus::Revoked => NativeAuthStatus::Revoked,
        plakous_xai::XaiOfflineCredentialStatus::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}
#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_xai_error(error: plakous_xai::XaiNativeAuthError) -> NativeSubscriptionError {
    let mapped = match &error {
        plakous_xai::XaiNativeAuthError::CompatibilityDrift(reason) => {
            NativeSubscriptionError::CompatibilityDrift {
                provider: NativeProvider::Xai,
                reason: reason.clone(),
            }
        }
        plakous_xai::XaiNativeAuthError::CompatibilityNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_xai::XaiNativeAuthError::CompatibilityInactive => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_xai::XaiNativeAuthError::Store(plakous_auth::PrivateStoreError::ForeignProfile)
        | plakous_xai::XaiNativeAuthError::StoredCredentialFormatRetired => {
            NativeSubscriptionError::CredentialProfileChanged
        }
        plakous_xai::XaiNativeAuthError::Store(
            plakous_auth::PrivateStoreError::WrittenByNewerBuild,
        )
        | plakous_xai::XaiNativeAuthError::StoredCredentialWrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        plakous_xai::XaiNativeAuthError::Store(_) => NativeSubscriptionError::UnsafeStorage,
        plakous_xai::XaiNativeAuthError::NotLoggedIn
        | plakous_xai::XaiNativeAuthError::NotReady
        | plakous_xai::XaiNativeAuthError::RefreshQuarantined => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_xai::XaiNativeAuthError::CatalogUnavailable
        | plakous_xai::XaiNativeAuthError::RequestedModelNotAdmitted
        | plakous_xai::XaiNativeAuthError::Model(_)
        | plakous_xai::XaiNativeAuthError::LanguageModel(_) => {
            NativeSubscriptionError::ModelUnavailable
        }
        plakous_xai::XaiNativeAuthError::Request
        | plakous_xai::XaiNativeAuthError::HttpRequest(_) => {
            NativeSubscriptionError::Transport(None)
        }
        plakous_xai::XaiNativeAuthError::Lookup(error) => error.exhaustion().map_or(
            NativeSubscriptionError::Internal,
            |(attempts, cause, retry_after)| NativeSubscriptionError::ModelLookup {
                attempts,
                cause,
                retry_after,
            },
        ),
        plakous_xai::XaiNativeAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(*cause)
        }
        plakous_xai::XaiNativeAuthError::LoginTokenExchange => {
            NativeSubscriptionError::LoginTokenExchange
        }
        plakous_xai::XaiNativeAuthError::LoginSigningKeys => {
            NativeSubscriptionError::LoginSigningKeys
        }
        plakous_xai::XaiNativeAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_xai::XaiNativeAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_xai::XaiNativeAuthError::InvalidCallback => {
            NativeSubscriptionError::InvalidCallback
        }
        plakous_xai::XaiNativeAuthError::LoginExpired => NativeSubscriptionError::InvalidCallback,
        plakous_xai::XaiNativeAuthError::RandomnessUnavailable
        | plakous_xai::XaiNativeAuthError::LoopbackUnavailable
        | plakous_xai::XaiNativeAuthError::AuthorizationDenied
        | plakous_xai::XaiNativeAuthError::InvalidTokenResponse(_)
        | plakous_xai::XaiNativeAuthError::InvalidIdToken(_)
        | plakous_xai::XaiNativeAuthError::InvalidJwks
        | plakous_xai::XaiNativeAuthError::UnknownSigningKey
        | plakous_xai::XaiNativeAuthError::AccountChanged
        | plakous_xai::XaiNativeAuthError::SelectionChanged
        | plakous_xai::XaiNativeAuthError::InvalidStoredCredential
        | plakous_xai::XaiNativeAuthError::HttpStatus(_) => NativeSubscriptionError::Authentication,
        plakous_xai::XaiNativeAuthError::Binding(_)
        | plakous_xai::XaiNativeAuthError::CredentialValue(_)
        | plakous_xai::XaiNativeAuthError::Route(_)
        | plakous_xai::XaiNativeAuthError::Identity(_) => NativeSubscriptionError::Internal,
        _ => NativeSubscriptionError::Internal,
    };
    drop(error);
    mapped
}
pub(super) fn facade_xai_model_is_selectable(model_id: &str) -> bool {
    plakous_xai::is_subscription_coding_model_id(model_id)
}
impl NativeSubscriptions {
    pub(super) fn attach_compile_authority_from_xai(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_xai::XaiModelResolutionEvidence,
    ) -> Result<RuntimeModelAuthority, NativeModelResolutionError> {
        RuntimeModelAuthority::new(
            descriptor,
            evidence.account_fingerprint(),
            evidence.credential_authority_binding(),
            evidence.selected_credential_generation(),
            evidence.catalog_observed_at(),
            Some(evidence.catalog_expires_at()),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
            )
        })
        .map(|authority| {
            authority.with_admission_witness(evidence.runtime_admission_witness().clone())
        })
    }
}
fn responses_configuration_error(
    error: plakous_responses::ResponsesProviderConfigurationError,
) -> NativeSubscriptionError {
    match error {
        plakous_responses::ResponsesProviderConfigurationError::PolicyMismatch(field) => {
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch(field)
        }
        plakous_responses::ResponsesProviderConfigurationError::Identity => {
            NativeSubscriptionError::ProviderConfigurationIdentityRejected
        }
        plakous_responses::ResponsesProviderConfigurationError::ProviderConfiguration(error) => {
            NativeSubscriptionError::ProviderConfigurationCompileRejected(error)
        }
        _ => NativeSubscriptionError::ProviderConfigurationRejected,
    }
}
struct PendingLogin(plakous_xai::XaiRemoteBrowserLogin);
impl super::login::NativePendingLogin for PendingLogin {
    fn validate_remote_input(&self, input: &str) -> Result<(), NativeSubscriptionError> {
        self.0
            .validate_authorization_input(input)
            .map_err(map_xai_error)
    }
    fn instructions(&self) -> NativeLoginInstructions<'_> {
        NativeLoginInstructions {
            verification_uri: self.0.authorization_url(),
            completion: NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
            device_code: None,
        }
    }
    fn complete(
        self: Box<Self>,
        _consent: PendingCompatibilityConsent,
    ) -> NativeFuture<'static, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async { Err(NativeSubscriptionError::Internal) })
    }
    fn complete_remote(
        self: Box<Self>,
        consent: PendingCompatibilityConsent,
        authorization_input: &str,
    ) -> NativeFuture<'_, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            complete_pending_with_consent(&consent, unix_now, || async move {
                self.0
                    .complete(authorization_input)
                    .await
                    .map(|status| map_credential_status(status.status()))
                    .map_err(map_xai_error)
            })
            .await
        })
    }
}
impl NativeVertical for Vertical {
    fn enrolled_models(&self) -> Vec<&'static str> {
        plakous_xai::subscription_coding_model_ids().collect()
    }
    fn enrolled_tool_mode(&self, _model: &str) -> Option<NativeToolMode> {
        None
    }
    fn enrolls(&self, model: &str) -> bool {
        plakous_xai::is_subscription_coding_model_id(model)
    }
    fn route_ids(
        &self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        (
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.protocol_id(),
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.route().route_id(),
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_RESPONSES_ADAPTER_REVISION,
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_GROK_BUILD_SOURCE_REVISION,
        )
    }
    fn hosted_retrieval(&self) -> plakous_model::HostedRetrievalTable {
        plakous_xai::SUPERGROK_HOSTED_RETRIEVAL
    }
    fn maximum_output_tokens(&self, model: &str) -> Option<u32> {
        plakous_xai::grok_max_output_tokens(model)
    }
    fn notice(&self) -> CompatibilityNotice {
        let provider = NativeProvider::Xai;
        {
            let profile = plakous_xai::XaiSubscriptionIntegrationProfile;
            CompatibilityNotice {
                provider,
                profile_id: profile.id(),
                source_revision: profile.source_revision(),
                secondary_source_revision: Some(profile.official_model_source_revision()),
                notice_revision: profile.notice_revision(),
                provenance: "pinned_compatibility",
                reviewed_at: profile.reviewed_at(),
                expires_at: profile.expires_at(),
                kill_switch_revision: profile.kill_switch_revision(),
                killed: profile.is_killed(),
            }
        }
    }
    fn upstream_account_fingerprint<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<Option<plakous_model::Digest>, NativeSubscriptionError>> {
        let state = subscriptions.state.clone();
        Box::pin(async move {
            run_blocking(move || {
                plakous_xai::XaiOfflineSelectedSlot::open_explicit(
                    state.provider_record(NativeProvider::Xai),
                    state.slot_id()?,
                )
                .map_err(map_xai_error)?
                .upstream_account_fingerprint()
                .map_err(map_xai_error)
            })
            .await?
        })
    }

    fn offline_status<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Xai;
            Ok({
                let path = subscriptions.state.provider_record(provider);
                let slot = subscriptions.state.slot_id()?;
                run_blocking(move || {
                    let admin = plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?;
                    admin
                        .status()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await??
            })
        })
    }
    fn access_expires_at<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<u64, NativeSubscriptionError>> {
        Box::pin(async move {
            subscriptions
                .xai_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_xai_error)
        })
    }
    fn refresh<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        force: bool,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            {
                let online = subscriptions.xai_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_now().await })
                        .await?
                        .map_err(map_xai_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_xai_error)?;
                }
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_xai_error)
            }
        })
    }
    fn revoke<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            {
                subscriptions
                    .xai_online()
                    .await?
                    .manager
                    .revoke()
                    .await
                    .map_err(map_xai_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
        })
    }
    fn logout<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Xai;
            {
                let path = subscriptions.state.provider_record(provider);
                let slot = subscriptions.state.slot_id()?;
                run_blocking(move || {
                    plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?
                        .logout_local()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await?
            }
        })
    }
    fn catalog<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> NativeFuture<'a, Result<Vec<NativeModelSummary>, NativeSubscriptionError>> {
        Box::pin(async move { subscriptions.xai_catalog_models(gate).await })
    }
    fn resolve<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        model_selector: &'a NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        attach: bool,
        _lineage: SessionLineage,
    ) -> NativeFuture<
        'a,
        Result<(ResolvedNativeModel, Option<RuntimeModelAuthority>), NativeModelResolutionError>,
    > {
        Box::pin(async move {
            {
                let NativeModelSelector::Exact(selected_model) = model_selector else {
                    return Err(NativeModelResolutionError::before_selection(
                        NativeSubscriptionError::ModelUnavailable,
                    ));
                };
                let selected_model = selected_model.clone();
                let online = subscriptions
                    .xai_online_with_gate(Some(gate), true)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let config = xai_native_http_config().map_err(|error| {
                    NativeModelResolutionError::after_selection(selected_model.clone(), error)
                })?;
                let inference_client = xai_inference_client(
                    subscriptions.exchange_observer.clone(),
                )
                .map_err(|error| {
                    NativeModelResolutionError::after_selection(selected_model.clone(), error)
                })?;
                let (model, resolution, retrieval, images) = online
                    .manager
                    .resolve_model_with_retrieval(selected_model.as_str(), inference_client, config)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_xai_error(error),
                        )
                    })?;
                let descriptor = resolution
                    .validated_descriptor()
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::ResolutionEvidenceRejected,
                        )
                    })?
                    .clone();
                let compile_authority = if attach {
                    Some(NativeSubscriptions::attach_compile_authority_from_xai(
                        &selected_model,
                        &descriptor,
                        &resolution,
                    )?)
                } else {
                    None
                };
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: resolution.account_fingerprint().to_string(),
                    credential_authority_binding: resolution
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: resolution.selected_credential_generation(),
                    credential_selection_epoch: resolution.credential_selection_epoch().to_owned(),
                    route_fingerprint: resolution.route_fingerprint().to_owned(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: Some(resolution.runtime_admission_witness().clone()),
                    profile_revision: resolution.profile_revision().to_owned(),
                    exact: Some(Arc::new(resolution)),
                    exact_provider: Some(NativeProvider::Xai),
                };
                Ok((
                    ResolvedNativeModel {
                        service_tiers: Vec::new(),
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                        retrieval: Some(Arc::new(
                            crate::responses_retrieval::XaiRetrievalPort::new(retrieval),
                        )),
                        image_gen: Some(Arc::new(crate::responses_images::XaiImagePort::new(
                            images,
                        ))),
                    },
                    compile_authority,
                ))
            }
        })
    }
    fn begin_login<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        mode: NativeLoginMode,
        replaced_unusable: bool,
    ) -> NativeFuture<'a, Result<PendingNativeLogin, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Xai;
            {
                let online = subscriptions.xai_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    effects: crate::subscriptions::NativeLoginEffects(Arc::default()),
                    state: subscriptions.state.clone(),
                    notice: NativeSubscriptions::notice(provider)?,
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                // xAI's hosted page always shows a code to paste into Grok Build,
                // local or remote. Do not bind or wait on loopback.
                let inner = match mode {
                    NativeLoginMode::Browser | NativeLoginMode::Remote => Box::new(PendingLogin(
                        online
                            .manager
                            .begin_remote_browser_login()
                            .map_err(map_xai_error)?,
                    )),
                };
                Ok(PendingNativeLogin {
                    inner,
                    consent,
                    replaced_unusable,
                })
            }
        })
    }
    fn configure(
        &self,
        selected: &SelectedProviderConfiguration,
        resolved: &ResolvedNativeModel,
        authority: &RuntimeModelAuthority,
    ) -> Result<CompiledProviderConfiguration, NativeModelResolutionError> {
        let requested_model = selected.route().requested_model().clone();
        let evidence = resolved
            .evidence
            .exact
            .as_ref()
            .and_then(|value| {
                value
                    .as_ref()
                    .as_any()
                    .downcast_ref::<plakous_xai::XaiModelResolutionEvidence>()
            })
            .ok_or_else(|| {
                NativeModelResolutionError::after_selection(
                    requested_model.clone(),
                    NativeSubscriptionError::Internal,
                )
            })?;
        plakous_xai::compile_supergrok_subscription_configuration(selected, authority, evidence)
            .map_err(|error| {
                NativeModelResolutionError::after_selection(
                    requested_model,
                    responses_configuration_error(error),
                )
            })
    }
    fn tool_mode(&self, _resolved: &ResolvedNativeModel) -> Option<NativeToolMode> {
        None
    }
    fn usage_reader<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<
        'a,
        Result<NativeFuture<'static, SubscriptionUsageRead>, NativeSubscriptionError>,
    > {
        Box::pin(async move {
            let manager = subscriptions.xai_online().await?.manager;
            let read: NativeFuture<'static, SubscriptionUsageRead> =
                Box::pin(async move { manager.subscription_usage().await });
            Ok(read)
        })
    }
}

struct Vertical;
pub(super) static ENTRY: NativeVerticalEntry = NativeVerticalEntry {
    provider: NativeProvider::Xai,
    vertical: &Vertical,
};
