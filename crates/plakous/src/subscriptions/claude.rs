//! The Claude OAuth subscription manager behind native operations.

use super::{
    Arc, LoadedAcceptance, NativeModelSummary, NativeModelVisibility, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, PRIMARY_SLOT, SubscriptionRequestClass,
    SubscriptionRequestGate, Timestamp, credential_refresh_observer, map_claude_error,
    map_claude_offline_status, run_blocking,
};

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
        let notice = Self::notice(NativeProvider::Claude);
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
