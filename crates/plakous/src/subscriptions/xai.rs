//! The SuperGrok subscription manager behind native operations.

use super::{
    Arc, LoadedAcceptance, NATIVE_INFERENCE_OPERATION_TIMEOUT, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, PRIMARY_SLOT, SubscriptionRequestGate, Timestamp,
    credential_refresh_observer, map_xai_error, map_xai_offline_status, run_blocking,
};

pub(super) fn xai_native_http_config()
-> Result<plakous_xai::StandardXaiHttpConfig, NativeSubscriptionError> {
    plakous_xai::StandardXaiHttpConfig::new(
        NATIVE_INFERENCE_OPERATION_TIMEOUT,
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
        let notice = Self::notice(NativeProvider::Xai);
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
        let local_status = run_blocking(move || {
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
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
            let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            let mut manager = plakous_xai::XaiNativeAuthManager::open_selected_slot(
                path,
                slot,
                client,
                provider_acceptance,
            )
            .map_err(map_xai_error)?;
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
}
