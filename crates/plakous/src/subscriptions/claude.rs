//! The Claude OAuth subscription manager behind native operations.

use super::{
    Arc, LoadedAcceptance, NativeProvider, NativeSubscriptionError, NativeSubscriptions,
    PRIMARY_SLOT, SubscriptionRequestGate, Timestamp, credential_refresh_observer,
    map_claude_error, map_claude_offline_status, run_blocking,
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
}
