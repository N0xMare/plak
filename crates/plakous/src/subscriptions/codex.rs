//! The Codex subscription manager behind native operations.

use super::{
    Arc, LoadedAcceptance, NATIVE_INFERENCE_OPERATION_TIMEOUT, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, SubscriptionRequestGate, Timestamp,
    credential_refresh_observer, map_codex_error, map_codex_status, run_blocking,
};

pub(super) fn codex_native_http_config()
-> Result<plakous_openai::StandardOpenAiHttpConfig, NativeSubscriptionError> {
    plakous_openai::StandardOpenAiHttpConfig::new(
        NATIVE_INFERENCE_OPERATION_TIMEOUT,
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
        let notice = Self::notice(NativeProvider::Codex);
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
        let local_status = plakous_openai::CodexSubscriptionLocalAdmin::open(path.clone())
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
            #[cfg(any(test, feature = "test-clock"))]
            let manager = plakous_openai::CodexSubscriptionCredentialManager::new_with_clock(
                path,
                client.clone(),
                provider_acceptance,
                clock,
            )
            .map_err(map_codex_error)?;
            #[cfg(not(any(test, feature = "test-clock")))]
            let manager = plakous_openai::CodexSubscriptionCredentialManager::new(
                path,
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
                manager = manager.with_ephemeral_loopback_for_tests();
            }
            Ok(CodexOnline {
                manager: Arc::new(manager),
                client,
                acceptance: loaded_acceptance,
            })
        })
        .await?
    }
}
