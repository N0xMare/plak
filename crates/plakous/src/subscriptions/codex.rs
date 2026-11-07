use super::login::{PendingCompatibilityConsent, complete_pending_with_consent};
use super::vertical::{NativeFuture, NativeVertical, NativeVerticalEntry};
use super::{
    Arc, CompatibilityNotice, CompiledProviderConfiguration, LoadedAcceptance, ModelId,
    NativeAuthStatus, NativeDeviceCode, NativeLoginCompletion, NativeLoginInstructions,
    NativeLoginMode, NativeModelResolutionError, NativeModelSelector, NativeModelSummary,
    NativeModelVisibility, NativeProvider, NativeServiceTierSummary, NativeSubscriptionError,
    NativeSubscriptions, NativeToolMode, PendingNativeLogin, ResolvedModelDescriptor,
    ResolvedNativeModel, RuntimeModelAuthority, SelectedProviderConfiguration, SessionLineage,
    SubscriptionRequestGate, SubscriptionResolutionEvidence, SubscriptionUsageRead, Timestamp,
    credential_refresh_observer, detached, guidance, run_blocking, unix_now,
};

pub(super) fn codex_native_http_config()
-> Result<plakous_openai::StandardOpenAiHttpConfig, NativeSubscriptionError> {
    plakous_openai::StandardOpenAiHttpConfig::new(
        super::responses_budget::NATIVE_INFERENCE_OPERATION_TIMEOUT,
        plakous_openai::StandardOpenAiHttpConfig::default().event_channel_capacity(),
        plakous_openai::StandardOpenAiHttpConfig::default().responses_limits(),
    )
    .map_err(|_| NativeSubscriptionError::Internal)
}

pub(super) struct CodexOnline {
    pub(super) manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
    pub(super) client: plakous_http::Client,
    pub(super) acceptance: LoadedAcceptance,
}

impl NativeSubscriptions {
    pub(super) async fn codex_online(&self) -> Result<CodexOnline, NativeSubscriptionError> {
        self.codex_online_with_gate(None, true).await
    }

    /// Open Codex online surface for an explicit login (skips recovery quarantine).
    pub(super) async fn codex_online_for_login(
        &self,
    ) -> Result<CodexOnline, NativeSubscriptionError> {
        self.codex_online_with_gate(None, false).await
    }

    pub(super) async fn codex_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<CodexOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Codex)?;
        let now = self.state.now()?;
        run_blocking(move || paths.prepare()).await??;
        // Checked before anything else is read or claimed, so no online path
        // writes over a newer build's state.
        if self
            .written_by_newer_build_unlocked(NativeProvider::Codex)
            .await?
        {
            return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
        }
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Codex);
        let slot = self.state.slot_id()?;
        let local_status =
            plakous_openai::CodexSubscriptionLocalAdmin::open(path.clone(), self.state.slot_id()?)
                .await
                .map_err(map_codex_error)?
                .status()
                .await
                .map(map_codex_status)
                .map_err(map_codex_error)?;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Codex,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        #[cfg(any(test, feature = "test-clock"))]
        let clock = self.state.clock();
        #[cfg(test)]
        let fixture_transport = self.codex_test_transport.clone();
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let provider_acceptance = plakous_openai::CodexCompatibilityAcceptance::accept_exact(
                notice.profile_id,
                notice.source_revision,
                notice.notice_revision,
                timestamp,
            )
            .map_err(map_codex_error)?;
            // The strict transport opens one connection per exchange, so H2
            // provides no multiplexing benefit here. Pin H1 before dispatch:
            // an OAuth POST cannot be retried safely after an H2 stream reset.
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;
            #[cfg(test)]
            let client = fixture_transport
                .as_ref()
                .map_or(client, |(client, _)| client.clone());
            #[cfg(any(test, feature = "test-clock"))]
            let manager = plakous_openai::CodexSubscriptionCredentialManager::new_with_clock(
                path,
                slot,
                client.clone(),
                provider_acceptance,
                clock,
            )
            .map_err(map_codex_error)?;
            #[cfg(not(any(test, feature = "test-clock")))]
            let manager = plakous_openai::CodexSubscriptionCredentialManager::new(
                path,
                slot,
                client.clone(),
                provider_acceptance,
            )
            .map_err(map_codex_error)?;
            let mut manager = manager;
            if let Some(gate) = request_gate {
                manager = manager.with_subscription_request_gate(gate);
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Codex) {
                manager = manager.with_credential_refresh_observer(observer);
            }
            // Unit tests never bind the pinned Codex callback ports.
            #[cfg(test)]
            {
                manager = manager
                    .with_fixture_endpoints_for_tests(fixture_transport.map(|(_, origin)| origin));
            }
            Ok(CodexOnline {
                manager: Arc::new(manager),
                client,
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }

    /// The account-admitted text models of a fresh Codex catalog.
    pub(super) async fn codex_catalog_models(
        &self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError> {
        let online = self.codex_online_with_gate(Some(gate), true).await?;
        let resolver = plakous_openai::CodexModelResolver::new(
            Arc::clone(&online.manager),
            online.client.with_observer(self.exchange_observer.clone()),
            codex_native_http_config()?,
        )
        .map_err(map_codex_error)?;
        let resolver = if self.catalog_retries {
            resolver
        } else {
            resolver.without_catalog_retries()
        };
        let catalog = resolver.refresh_catalog().await.map_err(map_codex_error)?;
        let mut models: Vec<_> = catalog
            .entries()
            .iter()
            .filter(|entry| entry.supports_text())
            .map(|entry| NativeModelSummary {
                unavailable_reason: None,
                exact_id: entry.model_id().to_owned(),
                visibility: entry
                    .visibility()
                    .map_or(NativeModelVisibility::None, facade_codex_visibility),
                native_tool_mode: facade_native_tool_mode(entry.tool_mode()),
                streaming: true,
                tools: entry.supports_tools(),
                reasoning_efforts: entry.reasoning_effort_options().to_vec(),
                default_reasoning_effort: entry.default_reasoning_effort(),
                service_tiers: entry
                    .service_tiers()
                    .iter()
                    .map(|tier| NativeServiceTierSummary {
                        id: tier.id().to_owned(),
                        name: tier.name().to_owned(),
                        description: tier.description().to_owned(),
                    })
                    .collect(),
                default_service_tier: entry.default_service_tier().map(str::to_owned),
                reported_context_length: entry
                    .capacity_facts()
                    .context_window()
                    .map(|window| u64::from(window.get())),
            })
            .collect();
        models.extend(catalog.drifted_models().iter().map(|(model, reason)| {
            NativeModelSummary::unavailable(
                model.clone(),
                catalog
                    .drifted_visibility(model)
                    .map_or(NativeModelVisibility::None, facade_codex_visibility),
                reason.message("Codex"),
            )
        }));
        Ok(models)
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_codex_status(status: plakous_openai::CodexCredentialStatus) -> NativeAuthStatus {
    match status {
        plakous_openai::CodexCredentialStatus::SignedOut => NativeAuthStatus::SignedOut,
        plakous_openai::CodexCredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_openai::CodexCredentialStatus::RefreshRequired => NativeAuthStatus::RefreshRequired,
        plakous_openai::CodexCredentialStatus::Quarantined => NativeAuthStatus::Quarantined,
        plakous_openai::CodexCredentialStatus::Refreshing => NativeAuthStatus::Refreshing,
        plakous_openai::CodexCredentialStatus::Revoked => NativeAuthStatus::Revoked,
        plakous_openai::CodexCredentialStatus::LoginRequired => NativeAuthStatus::LoginRequired,
        plakous_openai::CodexCredentialStatus::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}
#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_codex_error(
    error: plakous_openai::NativeCodexAuthError,
) -> NativeSubscriptionError {
    match error {
        plakous_openai::NativeCodexAuthError::CompatibilityDrift(reason) => {
            NativeSubscriptionError::CompatibilityDrift {
                provider: NativeProvider::Codex,
                reason,
            }
        }
        plakous_openai::NativeCodexAuthError::ProfileNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_openai::NativeCodexAuthError::ProfileNotCurrent => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_openai::NativeCodexAuthError::Store => NativeSubscriptionError::UnsafeStorage,
        plakous_openai::NativeCodexAuthError::WrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        plakous_openai::NativeCodexAuthError::ProfileChanged => {
            NativeSubscriptionError::CredentialProfileChanged
        }
        // A login stored by an earlier build (no selection epoch) needs a new login.
        plakous_openai::NativeCodexAuthError::Unavailable
        | plakous_openai::NativeCodexAuthError::LoginRequired => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_openai::NativeCodexAuthError::Catalog => NativeSubscriptionError::ModelUnavailable,
        plakous_openai::NativeCodexAuthError::CatalogCredential => {
            NativeSubscriptionError::CatalogCredentialRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogAuthorizationHeader => {
            NativeSubscriptionError::CatalogAuthorizationHeaderRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogAccountHeader => {
            NativeSubscriptionError::CatalogAccountHeaderRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogRefresh => {
            NativeSubscriptionError::CatalogRefreshRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogMediaType => {
            NativeSubscriptionError::CatalogMediaTypeRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogDocument => {
            NativeSubscriptionError::CatalogDocumentRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogIdentifier => {
            NativeSubscriptionError::CatalogIdentifierRejected
        }
        plakous_openai::NativeCodexAuthError::LoginTokenTransport => {
            NativeSubscriptionError::LoginTokenTransport
        }
        plakous_openai::NativeCodexAuthError::LoginTokenRejected => {
            NativeSubscriptionError::LoginTokenRejected
        }
        plakous_openai::NativeCodexAuthError::LoginTokenResponseInvalid => {
            NativeSubscriptionError::LoginTokenResponseInvalid
        }
        plakous_openai::NativeCodexAuthError::LoginSigningKeys => {
            NativeSubscriptionError::LoginSigningKeys
        }
        plakous_openai::NativeCodexAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_openai::NativeCodexAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_openai::NativeCodexAuthError::Lookup(error) => error.exhaustion().map_or(
            NativeSubscriptionError::Internal,
            |(attempts, cause, retry_after)| NativeSubscriptionError::ModelLookup {
                attempts,
                cause,
                retry_after,
            },
        ),
        plakous_openai::NativeCodexAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(cause)
        }
        plakous_openai::NativeCodexAuthError::Coordination => NativeSubscriptionError::Internal,
        plakous_openai::NativeCodexAuthError::Callback => NativeSubscriptionError::InvalidCallback,
        plakous_openai::NativeCodexAuthError::DeviceLoginUnavailable => {
            NativeSubscriptionError::DeviceLoginUnavailable
        }
        plakous_openai::NativeCodexAuthError::DeviceCodeExpired => {
            NativeSubscriptionError::DeviceCodeExpired
        }
        plakous_openai::NativeCodexAuthError::Binding => {
            NativeSubscriptionError::AuthenticationBinding
        }
        plakous_openai::NativeCodexAuthError::Provider => {
            NativeSubscriptionError::AuthenticationProviderRejected
        }
        plakous_openai::NativeCodexAuthError::Validation => {
            NativeSubscriptionError::AuthenticationValidation
        }
        plakous_openai::NativeCodexAuthError::AccountChanged
        | plakous_openai::NativeCodexAuthError::SelectionChanged => {
            NativeSubscriptionError::AuthenticationAccountChanged
        }
        _ => NativeSubscriptionError::Internal,
    }
}
pub(super) fn facade_codex_visibility(
    visibility: plakous_openai::CodexCatalogVisibility,
) -> NativeModelVisibility {
    match visibility {
        plakous_openai::CodexCatalogVisibility::List => NativeModelVisibility::List,
        plakous_openai::CodexCatalogVisibility::Hide => NativeModelVisibility::Hide,
        plakous_openai::CodexCatalogVisibility::None => NativeModelVisibility::None,
    }
}
pub(super) fn codex_native_tool_mode(
    evidence: &plakous_openai::CodexModelResolutionEvidence,
) -> Option<NativeToolMode> {
    facade_native_tool_mode(evidence.tool_mode())
}
pub(super) fn facade_native_tool_mode(
    mode: Option<plakous_openai::CodexCatalogToolMode>,
) -> Option<NativeToolMode> {
    mode.map(|mode| match mode {
        plakous_openai::CodexCatalogToolMode::Direct => NativeToolMode::Direct,
        plakous_openai::CodexCatalogToolMode::CodeMode => NativeToolMode::CodeMode,
        plakous_openai::CodexCatalogToolMode::CodeModeOnly => NativeToolMode::CodeModeOnly,
    })
}
pub(super) fn first_codex_compatible_model<'a>(
    entries: impl IntoIterator<Item = (&'a str, bool, bool, bool)>,
) -> Option<&'a str> {
    entries
        .into_iter()
        .find_map(|(model, text, tools, reasoning)| (text && tools && reasoning).then_some(model))
}
impl NativeSubscriptions {
    pub(super) fn attach_compile_authority_from_codex(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_openai::CodexModelResolutionEvidence,
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
    }
}
fn responses_configuration_error(
    error: plakous_openai::ResponsesProviderConfigurationError,
) -> NativeSubscriptionError {
    match error {
        plakous_openai::ResponsesProviderConfigurationError::PolicyMismatch(field) => {
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch(field)
        }
        plakous_openai::ResponsesProviderConfigurationError::Identity => {
            NativeSubscriptionError::ProviderConfigurationIdentityRejected
        }
        plakous_openai::ResponsesProviderConfigurationError::ProviderConfiguration(error) => {
            NativeSubscriptionError::ProviderConfigurationCompileRejected(error)
        }
        _ => NativeSubscriptionError::ProviderConfigurationRejected,
    }
}
enum PendingNativeLoginKind {
    CodexLocal {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexBrowserLogin,
    },
    CodexDevice {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexDeviceLogin,
    },
}
impl super::login::NativePendingLogin for PendingNativeLoginKind {
    fn instructions(&self) -> NativeLoginInstructions<'_> {
        match self {
            PendingNativeLoginKind::CodexLocal { login, .. } => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::Automatic,
                device_code: None,
            },
            PendingNativeLoginKind::CodexDevice { login, .. } => NativeLoginInstructions {
                verification_uri: login.verification_url(),
                completion: NativeLoginCompletion::DeviceCode,
                device_code: Some(NativeDeviceCode {
                    user_code: login.user_code(),
                    deadline: login.deadline(),
                }),
            },
        }
    }
    fn complete(
        self: Box<Self>,
        consent: PendingCompatibilityConsent,
    ) -> NativeFuture<'static, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let inner = *self;
            match inner {
                PendingNativeLoginKind::CodexLocal { manager, login } => {
                    complete_pending_with_consent(&consent, unix_now, || async move {
                        manager
                            .complete_browser_login(login)
                            .await
                            .map(map_codex_status)
                            .map_err(map_codex_error)
                    })
                    .await
                }
                PendingNativeLoginKind::CodexDevice { manager, login } => {
                    let poller = Arc::clone(&manager);
                    Box::pin(complete_approved_with_consent(
                        &consent,
                        unix_now,
                        async move {
                            poller
                                .poll_device_authorization(login)
                                .await
                                .map_err(map_codex_error)
                        },
                        |authorization| async move {
                            manager
                                .complete_device_authorization(authorization)
                                .await
                                .map(map_codex_status)
                                .map_err(map_codex_error)
                        },
                    ))
                    .await
                }
            }
        })
    }
    fn complete_remote(
        self: Box<Self>,
        consent: PendingCompatibilityConsent,
        _authorization_input: &str,
    ) -> NativeFuture<'_, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            complete_pending_with_consent(&consent, unix_now, || async move {
                match *self {
                    Self::CodexLocal { .. } | Self::CodexDevice { .. } => {
                        Err(NativeSubscriptionError::Internal)
                    }
                }
            })
            .await
        })
    }
}
impl NativeVertical for Vertical {
    fn enrolled_models(&self) -> Vec<&'static str> {
        plakous_openai::RELEASE_ENROLLED_CODEX_MODELS
            .iter()
            .map(|(model, _)| *model)
            .collect()
    }
    fn enrolled_tool_mode(&self, model: &str) -> Option<NativeToolMode> {
        facade_native_tool_mode(plakous_openai::release_enrolled_codex_tool_mode(model))
    }
    fn enrolls(&self, model: &str) -> bool {
        plakous_openai::release_enrolled_codex_model(model)
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
            plakous_openai::OPENAI_CODEX_RESPONSES_PROTOCOL_ID,
            plakous_openai::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_RESPONSES_ADAPTER_REVISION,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_CODEX_SOURCE_REVISION,
        )
    }
    fn hosted_retrieval(&self) -> plakous_model::HostedRetrievalTable {
        plakous_openai::CODEX_HOSTED_RETRIEVAL
    }
    fn maximum_output_tokens(&self, _model: &str) -> Option<u32> {
        None
    }
    fn notice(&self) -> CompatibilityNotice {
        let provider = NativeProvider::Codex;
        {
            let profile = plakous_openai::CodexSubscriptionIntegrationProfile;
            CompatibilityNotice {
                provider,
                profile_id: profile.id(),
                source_revision: profile.source_revision(),
                secondary_source_revision: None,
                notice_revision: profile.notice_revision(),
                provenance: "pinned_compatibility",
                reviewed_at: profile.reviewed_at(),
                expires_at: profile.expires_at(),
                kill_switch_revision: profile.kill_switch_revision(),
                killed: profile.is_killed(),
            }
        }
    }
    fn offline_status<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Codex;
            plakous_openai::CodexSubscriptionLocalAdmin::open(
                subscriptions.state.provider_record(provider),
                subscriptions.state.slot_id()?,
            )
            .await
            .map_err(map_codex_error)?
            .status()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error)
        })
    }
    fn access_expires_at<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<u64, NativeSubscriptionError>> {
        Box::pin(async move {
            subscriptions
                .codex_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_codex_error)
        })
    }
    fn refresh<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        force: bool,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            {
                let online = subscriptions.codex_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_after_rejection().await })
                        .await?
                        .map_err(map_codex_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_codex_error)?;
                }
                online
                    .manager
                    .status()
                    .map(map_codex_status)
                    .map_err(map_codex_error)
            }
        })
    }
    fn revoke<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            subscriptions
                .codex_online()
                .await?
                .manager
                .revoke()
                .await
                .map(map_codex_status)
                .map_err(map_codex_error)
        })
    }
    fn logout<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Codex;
            plakous_openai::CodexSubscriptionLocalAdmin::open(
                subscriptions.state.provider_record(provider),
                subscriptions.state.slot_id()?,
            )
            .await
            .map_err(map_codex_error)?
            .logout_local()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error)
        })
    }
    fn catalog<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> NativeFuture<'a, Result<Vec<NativeModelSummary>, NativeSubscriptionError>> {
        Box::pin(async move { subscriptions.codex_catalog_models(gate).await })
    }
    #[allow(
        clippy::too_many_lines,
        reason = "exact selection and pre/post-selection evidence stay together within the provider vertical"
    )]
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
                let exact_selection = model_selector.exact_model().cloned();
                let online = subscriptions
                    .codex_online_with_gate(Some(gate), true)
                    .await
                    .map_err(|error| match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    })?;
                let resolver = plakous_openai::CodexModelResolver::new(
                    Arc::clone(&online.manager),
                    online
                        .client
                        .with_observer(subscriptions.exchange_observer.clone()),
                    codex_native_http_config().map_err(|error| match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    })?,
                )
                .map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::ProviderClientConstructionRejected
                        }
                        other => map_codex_error(other),
                    };
                    match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    }
                })?
                .with_inference_observer(subscriptions.exchange_observer.clone());
                let resolver = if subscriptions.catalog_retries {
                    resolver
                } else {
                    resolver.without_catalog_retries()
                };
                let catalog = resolver.refresh_catalog().await.map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::CatalogValidationRejected
                        }
                        other => map_codex_error(other),
                    };
                    match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    }
                })?;
                let selected_model = match model_selector {
                    NativeModelSelector::Exact(model) => model.clone(),
                    NativeModelSelector::CodexFirstCompatible => {
                        let selected =
                            first_codex_compatible_model(catalog.entries().iter().map(|entry| {
                                (
                                    entry.model_id(),
                                    entry.supports_text(),
                                    entry.supports_tools(),
                                    entry.supports_reasoning(),
                                )
                            }))
                            .ok_or_else(|| {
                                NativeModelResolutionError::before_selection(
                                    NativeSubscriptionError::ModelUnavailable,
                                )
                            })?;
                        ModelId::new(selected.to_owned()).map_err(|_| {
                            NativeModelResolutionError::before_selection(
                                NativeSubscriptionError::ModelUnavailable,
                            )
                        })?
                    }
                };
                let (model, resolution, retrieval, images) = resolver
                    .resolve_with_retrieval(selected_model.as_str())
                    .map_err(|error| {
                        let error = match error {
                            plakous_openai::NativeCodexAuthError::Validation => {
                                NativeSubscriptionError::ResolutionEvidenceRejected
                            }
                            other => map_codex_error(other),
                        };
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let model = guidance::with_content_filter_guidance(
                    model,
                    resolution.content_filter_guidance(),
                );
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
                    Some(NativeSubscriptions::attach_compile_authority_from_codex(
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
                    credential_selection_epoch: resolution.credential_selection_epoch().to_string(),
                    route_fingerprint: resolution.route_fingerprint().to_string(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: None,
                    profile_revision: resolution.profile_revision().to_owned(),
                    exact: Some(Arc::new(resolution)),
                    exact_provider: Some(NativeProvider::Codex),
                };
                Ok((
                    ResolvedNativeModel {
                        service_tiers: catalog
                            .entries()
                            .iter()
                            .find(|entry| entry.model_id() == selected_model.as_str())
                            .map(|entry| {
                                entry
                                    .service_tiers()
                                    .iter()
                                    .map(|tier| tier.id().to_owned())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                        retrieval: Some(Arc::new(
                            crate::responses_retrieval::CodexRetrievalPort::new(retrieval),
                        )),
                        image_gen: Some(Arc::new(crate::responses_images::CodexImagePort::new(
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
            let provider = NativeProvider::Codex;
            {
                let online = subscriptions.codex_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: subscriptions.state.clone(),
                    notice: NativeSubscriptions::notice(provider)?,
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                let manager = online.manager;
                let inner = match mode {
                    NativeLoginMode::Browser => PendingNativeLoginKind::CodexLocal {
                        login: manager
                            .begin_browser_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                    NativeLoginMode::Remote => PendingNativeLoginKind::CodexDevice {
                        login: manager
                            .begin_device_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                };
                Ok(PendingNativeLogin {
                    inner: Box::new(inner),
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
                    .downcast_ref::<plakous_openai::CodexModelResolutionEvidence>()
            })
            .ok_or_else(|| {
                NativeModelResolutionError::after_selection(
                    requested_model.clone(),
                    NativeSubscriptionError::Internal,
                )
            })?;
        plakous_openai::compile_codex_subscription_configuration(selected, authority, evidence)
            .map_err(|error| {
                NativeModelResolutionError::after_selection(
                    requested_model,
                    responses_configuration_error(error),
                )
            })
    }
    fn tool_mode(&self, resolved: &ResolvedNativeModel) -> Option<NativeToolMode> {
        resolved
            .evidence
            .exact
            .as_ref()
            .and_then(|value| {
                value
                    .as_ref()
                    .as_any()
                    .downcast_ref::<plakous_openai::CodexModelResolutionEvidence>()
            })
            .and_then(codex_native_tool_mode)
    }
    fn usage_reader<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<
        'a,
        Result<NativeFuture<'static, SubscriptionUsageRead>, NativeSubscriptionError>,
    > {
        Box::pin(async move {
            let manager = subscriptions.codex_online().await?.manager;
            let read: NativeFuture<'static, SubscriptionUsageRead> =
                Box::pin(async move { manager.subscription_usage().await });
            Ok(read)
        })
    }
}

struct Vertical;
pub(super) static ENTRY: NativeVerticalEntry = NativeVerticalEntry {
    provider: NativeProvider::Codex,
    vertical: &Vertical,
};

/// Waits for an out-of-band approval before any local state changes, then
/// completes it under the same consent, lock and recovery rules as every login.
pub(super) async fn complete_approved_with_consent<A, T, F, Fut, C>(
    consent: &PendingCompatibilityConsent,
    now: C,
    approval: A,
    operation: F,
) -> Result<NativeAuthStatus, NativeSubscriptionError>
where
    A: Future<Output = Result<T, NativeSubscriptionError>>,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = Result<NativeAuthStatus, NativeSubscriptionError>>,
    C: FnMut() -> Result<u64, NativeSubscriptionError>,
{
    let approved = approval.await?;
    complete_pending_with_consent(consent, now, || operation(approved)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::ProviderConfigurationError;
    use plakous_openai::ResponsesProviderConfigurationError as Responses;
    #[test]
    fn policy_compilation_errors_map_to_their_resolution_failures() {
        assert_eq!(
            responses_configuration_error(Responses::PolicyMismatch("cache")),
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch("cache")
        );
        assert_eq!(
            responses_configuration_error(Responses::Identity),
            NativeSubscriptionError::ProviderConfigurationIdentityRejected
        );
        assert_eq!(
            responses_configuration_error(Responses::ProviderConfiguration(
                ProviderConfigurationError::ForeignTarget
            )),
            NativeSubscriptionError::ProviderConfigurationCompileRejected(
                ProviderConfigurationError::ForeignTarget
            )
        );
    }
}
