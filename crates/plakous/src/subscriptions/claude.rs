use super::common_error::map_credential_status;
use super::login::{PendingCompatibilityConsent, complete_pending_with_consent};
use super::vertical::{NativeFuture, NativeVertical, NativeVerticalEntry};
use super::{
    Arc, CompatibilityNotice, CompiledProviderConfiguration, LoadedAcceptance, NativeAuthStatus,
    NativeLoginCompletion, NativeLoginInstructions, NativeLoginMode, NativeModelResolutionError,
    NativeModelSelector, NativeModelSummary, NativeModelVisibility, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, NativeToolMode, PRIMARY_SLOT, PendingNativeLogin,
    ResolvedNativeModel, RuntimeModelAuthority, SelectedProviderConfiguration, SessionLineage,
    SubscriptionRequestClass, SubscriptionRequestGate, SubscriptionResolutionEvidence,
    SubscriptionUsageRead, Timestamp, credential_refresh_observer, detached, run_blocking,
    unix_now,
};
use plakous_model::ResolvedProviderAuthorityBinding;

pub(super) struct ClaudeOnline {
    pub(super) manager: Arc<plakous_anthropic::ClaudeNativeAuthManager>,
    pub(super) acceptance: LoadedAcceptance,
}

impl NativeSubscriptions {
    pub(super) async fn claude_online(&self) -> Result<ClaudeOnline, NativeSubscriptionError> {
        self.claude_online_with_gate(None, true).await
    }

    pub(super) async fn claude_online_for_login(
        &self,
    ) -> Result<ClaudeOnline, NativeSubscriptionError> {
        self.claude_online_with_gate(None, false).await
    }

    pub(super) async fn claude_online_with_gate(
        &self,
        request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
        enforce_login_recovery: bool,
    ) -> Result<ClaudeOnline, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(NativeProvider::Claude)?;
        let now = self.state.now()?;
        run_blocking(move || paths.prepare()).await??;
        // Checked before anything else is read or claimed, so no online path
        // writes over a newer build's state.
        if self
            .written_by_newer_build_unlocked(NativeProvider::Claude)
            .await?
        {
            return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
        }
        let loaded_acceptance = {
            let paths = self.state.clone();
            run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
        };
        let accepted_at = loaded_acceptance.accepted_at;
        let path = self.state.provider_record(NativeProvider::Claude);
        let status_path = path.clone();
        let local_status = run_blocking(move || {
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let admin =
                plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(status_path, slot)
                    .map_err(map_claude_error)?;
            admin
                .status()
                .map(map_claude_offline_status)
                .map_err(map_claude_error)
        })
        .await??;
        if enforce_login_recovery {
            let paths = self.state.clone();
            let consent_generation = loaded_acceptance.generation.clone();
            if run_blocking(move || {
                paths.login_recovery_required_for_status(
                    NativeProvider::Claude,
                    Some(&consent_generation),
                    local_status,
                )
            })
            .await??
            {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
        }
        let identity_paths = self.state.clone();
        run_blocking(move || {
            let timestamp =
                Timestamp::new(accepted_at, 0).map_err(|_| NativeSubscriptionError::Internal)?;
            let provider_acceptance =
                plakous_anthropic::ClaudeCompatibilityAcceptance::accept_exact(
                    notice.profile_id,
                    notice.source_revision,
                    notice.notice_revision,
                    notice.kill_switch_revision,
                    timestamp,
                )
                .map_err(map_claude_error)?;
            let client = plakous_http::Client::new_http1(plakous_http::Limits::default())
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let mut manager = plakous_anthropic::ClaudeNativeAuthManager::open_selected_slot(
                path,
                slot,
                client,
                provider_acceptance,
                identity_paths.claude_client_device_id()?,
            )
            .map_err(map_claude_error)?;
            if let Some(gate) = request_gate {
                manager = manager.with_subscription_request_gate(gate);
            }
            if let Some(observer) = credential_refresh_observer(NativeProvider::Claude) {
                manager = manager.with_credential_refresh_observer(observer);
            }
            Ok(ClaudeOnline {
                manager: Arc::new(manager),
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }

    /// The enrolled Claude OAuth models, behind one catalog permit.
    pub(super) async fn claude_catalog_models(
        &self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError> {
        let online = self
            .claude_online_with_gate(Some(gate.clone()), true)
            .await?;
        let mut permit = gate
            .acquire(SubscriptionRequestClass::Catalog)
            .map_err(|_| NativeSubscriptionError::Internal)?;
        permit
            .authorize_dispatch()
            .map_err(|_| NativeSubscriptionError::Internal)?;
        Ok(online
            .manager
            .enrolled_catalog()
            .map_err(map_claude_error)?
            .into_iter()
            .map(|entry| NativeModelSummary {
                exact_id: entry.exact_id().to_owned(),
                visibility: NativeModelVisibility::List,
                native_tool_mode: None,
                streaming: true,
                tools: true,
                reasoning_efforts: plakous_anthropic::claude_oauth_reasoning_effort_options(
                    entry.exact_id(),
                )
                .to_vec(),
                default_reasoning_effort: None,
                service_tiers: Vec::new(),
                default_service_tier: None,
                // Claude publishes no OAuth catalog window; this is
                // the enrolled row's window, the one the session runs at.
                reported_context_length: Some(entry.input_tokens()),
            })
            .collect())
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_claude_offline_status(
    status: plakous_auth::OfflineCredentialLifecycle,
) -> NativeAuthStatus {
    match status {
        plakous_auth::OfflineCredentialLifecycle::SignedOut => NativeAuthStatus::SignedOut,
        plakous_auth::OfflineCredentialLifecycle::Ready => NativeAuthStatus::Ready,
        plakous_auth::OfflineCredentialLifecycle::RefreshRequired => {
            NativeAuthStatus::RefreshRequired
        }
        plakous_auth::OfflineCredentialLifecycle::Quarantined => NativeAuthStatus::Quarantined,
        plakous_auth::OfflineCredentialLifecycle::Refreshing => NativeAuthStatus::Refreshing,
        plakous_auth::OfflineCredentialLifecycle::Revoked => NativeAuthStatus::Revoked,
        plakous_auth::OfflineCredentialLifecycle::LoginRequired => NativeAuthStatus::LoginRequired,
        plakous_auth::OfflineCredentialLifecycle::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}
#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_claude_error(
    error: plakous_anthropic::ClaudeNativeAuthError,
) -> NativeSubscriptionError {
    let mapped = match &error {
        plakous_anthropic::ClaudeNativeAuthError::CompatibilityNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_anthropic::ClaudeNativeAuthError::CompatibilityInactive => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(
            plakous_auth::PrivateStoreError::ForeignProfile,
        )
        | plakous_anthropic::ClaudeNativeAuthError::StoredCredentialFormatRetired => {
            NativeSubscriptionError::CredentialProfileChanged
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(
            plakous_auth::PrivateStoreError::WrittenByNewerBuild,
        )
        | plakous_anthropic::ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(_) => {
            NativeSubscriptionError::UnsafeStorage
        }
        plakous_anthropic::ClaudeNativeAuthError::NotLoggedIn
        | plakous_anthropic::ClaudeNativeAuthError::NotReady
        | plakous_anthropic::ClaudeNativeAuthError::RefreshQuarantined => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_anthropic::ClaudeNativeAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(*cause)
        }
        plakous_anthropic::ClaudeNativeAuthError::HttpRequest(_) => {
            NativeSubscriptionError::Transport(None)
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginTokenExchange => {
            NativeSubscriptionError::LoginTokenExchange
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_anthropic::ClaudeNativeAuthError::InvalidCallback
        | plakous_anthropic::ClaudeNativeAuthError::LoginExpired => {
            NativeSubscriptionError::InvalidCallback
        }
        plakous_anthropic::ClaudeNativeAuthError::AccountIdentityUnavailable
        | plakous_anthropic::ClaudeNativeAuthError::InvalidTokenResponse
        | plakous_anthropic::ClaudeNativeAuthError::InvalidStoredCredential
        | plakous_anthropic::ClaudeNativeAuthError::Binding(_)
        | plakous_anthropic::ClaudeNativeAuthError::CredentialValue(_) => {
            NativeSubscriptionError::Authentication
        }
        _ => NativeSubscriptionError::Internal,
    };
    drop(error);
    mapped
}
fn messages_configuration_error(
    error: plakous_anthropic::MessagesProviderConfigurationError,
) -> NativeSubscriptionError {
    match error {
        plakous_anthropic::MessagesProviderConfigurationError::Identity => {
            NativeSubscriptionError::ProviderConfigurationIdentityRejected
        }
        plakous_anthropic::MessagesProviderConfigurationError::PolicyMismatch
        | plakous_anthropic::MessagesProviderConfigurationError::Provider(_) => {
            NativeSubscriptionError::ProviderConfigurationRejected
        }
    }
}
struct PendingLogin(plakous_anthropic::ClaudeOAuthLogin);
impl super::login::NativePendingLogin for PendingLogin {
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
                    .map_err(map_claude_error)
            })
            .await
        })
    }
}
impl NativeVertical for Vertical {
    fn enrolled_models(&self) -> Vec<&'static str> {
        plakous_anthropic::claude_oauth_enrolled_models().collect()
    }
    fn enrolled_tool_mode(&self, _model: &str) -> Option<NativeToolMode> {
        None
    }
    fn enrolls(&self, model: &str) -> bool {
        plakous_anthropic::claude_oauth_enrolled_model(model)
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
            plakous_anthropic::CLAUDE_OAUTH_PROTOCOL_ID,
            plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_SOURCE_REVISION,
        )
    }
    fn hosted_retrieval(&self) -> plakous_model::HostedRetrievalTable {
        plakous_anthropic::CLAUDE_OAUTH_HOSTED_RETRIEVAL
    }
    fn maximum_output_tokens(&self, model: &str) -> Option<u32> {
        Some(plakous_anthropic::claude_max_output_tokens(model))
    }
    fn notice(&self) -> CompatibilityNotice {
        let provider = NativeProvider::Claude;
        {
            let profile = plakous_anthropic::ClaudeSubscriptionIntegrationProfile;
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
            let provider = NativeProvider::Claude;
            Ok({
                let path = subscriptions.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let admin =
                        plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                            .map_err(map_claude_error)?;
                    admin
                        .status()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
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
                .claude_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_claude_error)
        })
    }
    fn refresh<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        force: bool,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            {
                let online = subscriptions.claude_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_after_rejection().await })
                        .await?
                        .map_err(map_claude_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_claude_error)?;
                }
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_claude_error)
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
                    .claude_online()
                    .await?
                    .manager
                    .revoke()
                    .map_err(map_claude_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
        })
    }
    fn logout<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Claude;
            {
                let path = subscriptions.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_claude_error)?
                        .logout_local()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
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
        Box::pin(async move { subscriptions.claude_catalog_models(gate).await })
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
        lineage: SessionLineage,
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
                    .claude_online_with_gate(Some(Arc::clone(&gate)), true)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let client = plakous_http::Client::new_http1(
                    plakous_http::Limits::for_streaming_inference(),
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let client = client
                    .pooled()
                    .with_observer(subscriptions.exchange_observer.clone());
                if !attach {
                    online.manager.refresh_if_needed().await.map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                }
                let mut permit = gate
                    .acquire(SubscriptionRequestClass::Catalog)
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::Internal,
                        )
                    })?;
                permit.authorize_dispatch().map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let session_id = plakous_model::SessionId::generate(
                    &plakous_model::SystemClock,
                    &plakous_model::SystemIdGenerator,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let (model, authority, descriptor, retrieval) = online
                    .manager
                    .resolve_model_with_retrieval(
                        selected_model.as_str(),
                        client,
                        lineage,
                        session_id,
                    )
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                drop(permit);
                let catalog_expires_at = authority.valid_until().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    )
                })?;
                if authority.observed_at() >= catalog_expires_at {
                    return Err(NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    ));
                }
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: authority.account_binding().to_string(),
                    credential_authority_binding: authority
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: authority.credential_generation(),
                    credential_selection_epoch: "primary".to_owned(),
                    route_fingerprint: plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID.to_owned(),
                    descriptor_digest: descriptor.binding_digest().map_or_else(
                        |_| "claude-oauth-descriptor".to_owned(),
                        |digest| digest.to_string(),
                    ),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION.to_owned(),
                    catalog_observed_at: authority.observed_at(),
                    catalog_expires_at,
                    runtime_admission_witness: None,
                    profile_revision: plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
                        .to_owned(),
                    exact: None,
                    exact_provider: None,
                };
                let resolved = ResolvedNativeModel {
                    service_tiers: Vec::new(),
                    selected_model,
                    descriptor,
                    model,
                    evidence,
                    retrieval: Some(Arc::new(
                        crate::anthropic_retrieval::AnthropicRetrievalPort::new(retrieval),
                    )),
                    image_gen: None,
                };
                let compile_authority = attach.then_some(authority);
                Ok((resolved, compile_authority))
            }
        })
    }
    fn begin_login<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        _mode: NativeLoginMode,
        replaced_unusable: bool,
    ) -> NativeFuture<'a, Result<PendingNativeLogin, NativeSubscriptionError>> {
        Box::pin(async move {
            let provider = NativeProvider::Claude;
            {
                let online = subscriptions.claude_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: subscriptions.state.clone(),
                    notice: NativeSubscriptions::notice(provider)?,
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                Ok(PendingNativeLogin {
                    inner: Box::new(PendingLogin(
                        online.manager.begin_login().map_err(map_claude_error)?,
                    )),
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
        let beta = plakous_anthropic::claude_oauth_beta_header(requested_model.as_str());
        let evidence = plakous_model::ProviderSemanticEvidence::new(
            1,
            b"plakous-anthropic/claude-oauth-semantic-evidence-v1".to_vec(),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderConfigurationIdentityRejected,
            )
        })?;
        let binding =
            ResolvedProviderAuthorityBinding::new(&resolved.descriptor, authority, evidence)
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        requested_model.clone(),
                        NativeSubscriptionError::ProviderConfigurationIdentityRejected,
                    )
                })?;
        let configuration = plakous_anthropic::compile_messages_oauth_configuration(
            selected,
            resolved.descriptor.clone(),
            binding,
            plakous_anthropic::ANTHROPIC_API_VERSION,
            beta,
        )
        .map_err(|error| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                messages_configuration_error(error),
            )
        })?;

        Ok(configuration)
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
            let manager = subscriptions.claude_online().await?.manager;
            let read: NativeFuture<'static, SubscriptionUsageRead> =
                Box::pin(async move { manager.subscription_usage().await });
            Ok(read)
        })
    }
}

struct Vertical;
pub(super) static ENTRY: NativeVerticalEntry = NativeVerticalEntry {
    provider: NativeProvider::Claude,
    vertical: &Vertical,
};

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_anthropic::MessagesProviderConfigurationError as Messages;
    use plakous_model::ProviderConfigurationError;
    #[test]
    fn policy_compilation_errors_map_to_their_resolution_failures() {
        assert_eq!(
            messages_configuration_error(Messages::Identity),
            NativeSubscriptionError::ProviderConfigurationIdentityRejected
        );
        for error in [
            Messages::PolicyMismatch,
            Messages::Provider(ProviderConfigurationError::ForeignTarget),
        ] {
            assert_eq!(
                messages_configuration_error(error),
                NativeSubscriptionError::ProviderConfigurationRejected
            );
        }
    }
}
