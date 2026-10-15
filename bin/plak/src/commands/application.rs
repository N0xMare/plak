//! The product application port and its one `plakous` facade implementation.
//!
//! [`AccountOps`] is the writer-free account port the command workflow drives.
//! [`PlakousApplication`] is the only owner of the facade's async executor and
//! native-subscription service; tests replace the port and all three process
//! streams without touching storage or a network.

use std::path::PathBuf;
use std::sync::Arc;

use plakous::application::ApplicationExecutor;
use plakous::model::SubscriptionUsageRead;
use plakous::subscriptions::{
    NativeLoginCompletion, NativeLoginMode, NativeModelCatalog, NativeProvider,
    NativeSubscriptions, SubscriptionPaths,
};

use super::support::{
    application_execution_failure, compatibility_state, credential_state,
    credential_unavailable_failure, native_subscription_failure, oauth_status_failure_cell,
    product_state_roots_from,
};
use crate::home::paths as product_paths;
use crate::presentation::{
    CliModelSummary, CliStatus, CompatibilityState, CredentialState, PresentationBackend,
};
use plakous_hosted::agent as product_agent;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::openrouter as product_openrouter;
use plakous_hosted::subscription_usage as product_usage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliNotice {
    pub profile_id: &'static str,
    pub notice_revision: &'static str,
    pub reviewed_at: u64,
    pub expires_at: u64,
}

/// A displayed challenge and the exact opaque authorization it came from.
/// The pending token is consumed intact; presentation fields never reconstruct it.
pub struct LoginChallenge<P> {
    pub pending: P,
    pub authorize_url: String,
    pub completion: NativeLoginCompletion,
}

pub enum LoginResponse<'a> {
    Pasted(&'a str),
    Automatic { timeout: std::time::Duration },
}

/// What `plak logout` finds in the local store, read offline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoredCredential {
    /// Nothing stored, or already retired by an earlier logout.
    Absent,
    /// A credential the provider can still revoke, with the acceptance state
    /// its remote revoke needs.
    Usable(CompatibilityState),
    /// A stored credential this build cannot use, so it cannot revoke it
    /// remotely either: quarantined, login-required, from another profile, or
    /// under an inactive compiled compatibility profile.
    Unusable,
}

impl StoredCredential {
    pub(crate) fn from_status(status: CliStatus) -> Self {
        match status.credential {
            CredentialState::SignedOut | CredentialState::Revoked => Self::Absent,
            CredentialState::Ready | CredentialState::RefreshRequired
                if status.compatibility != CompatibilityState::Inactive =>
            {
                Self::Usable(status.compatibility)
            }
            CredentialState::Ready
            | CredentialState::RefreshRequired
            | CredentialState::Quarantined
            | CredentialState::LoginRequired
            | CredentialState::Unsupported => Self::Unusable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginOutcome {
    Completed(CredentialState),
    Cancelled,
    TimedOut,
}

/// Synchronous account operations. Commands own terminal input and output;
/// the facade owns login admission, the opaque pending authority, and completion.
pub trait AccountOps {
    fn notice(&self, provider: NativeProvider) -> CliNotice;

    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus>;

    /// Offline classification for `plak logout`. A record this build refuses
    /// (another profile or a retired payload format) is [`StoredCredential::Unusable`]
    /// rather than a failure, because local retirement is its repair.
    fn stored_credential(&mut self, provider: NativeProvider) -> CliResult<StoredCredential>;

    /// Offline status for the all-accounts table. A failure is that row's
    /// credential cell, not a reason to drop the other accounts.
    fn offline_status_row(&mut self, provider: NativeProvider) -> Result<CliStatus, String> {
        self.offline_status(provider)
            .map_err(|failure| failure.message().to_owned())
    }

    fn subscription_usage(&mut self, provider: NativeProvider) -> CliResult<SubscriptionUsageRead>;

    /// Reads the OpenRouter inference key's own allowance on demand.
    fn openrouter_key_usage(&mut self) -> CliResult<product_usage::VerticalObservation>;

    fn acknowledge_exact(
        &mut self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> CliResult<()>;

    type PendingLogin;

    fn begin_login(
        &mut self,
        provider: NativeProvider,
        mode: NativeLoginMode,
    ) -> CliResult<LoginChallenge<Self::PendingLogin>>;

    fn finish_login(
        &mut self,
        pending: Self::PendingLogin,
        response: LoginResponse<'_>,
    ) -> CliResult<LoginOutcome>;

    fn refresh(&mut self, provider: NativeProvider) -> CliResult<CredentialState>;

    fn revoke(&mut self, provider: NativeProvider) -> CliResult<CredentialState>;

    fn logout_local(&mut self, provider: NativeProvider) -> CliResult<CredentialState>;

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>>;

    fn openrouter_models(&mut self) -> CliResult<Vec<String>>;
    /// One direct API route's catalog: live list, cache or bundled table.
    fn platform_catalog(
        &mut self,
        provider: plakous_hosted::binding::PlatformProvider,
    ) -> CliResult<plakous_hosted::platform::ApiModelCatalog>;
}

pub fn catalog_summaries(
    provider: NativeProvider,
    catalog: &NativeModelCatalog,
) -> CliResult<Vec<CliModelSummary>> {
    if catalog.provider() != provider {
        return Err(CliFailure::new("native model catalog provider mismatch"));
    }
    Ok(catalog
        .models()
        .iter()
        .map(|model| CliModelSummary {
            exact_id: model.exact_id().to_owned(),
            visibility: model.visibility(),
            native_tool_mode: model.native_tool_mode(),
            streaming: model.supports_streaming(),
            tools: model.supports_tools(),
            reasoning_efforts: model.reasoning_efforts().to_vec(),
            service_tiers: model
                .service_tiers()
                .iter()
                .map(|tier| {
                    (
                        tier.id().to_owned(),
                        tier.name().to_owned(),
                        tier.description().to_owned(),
                    )
                })
                .collect(),
            reported_context_length: model.reported_context_length(),
        })
        .collect())
}

/// The account and catalog service, constructed once per process.
///
/// It owns the async executor, the native-subscription state and the
/// credential-telemetry guard, and it remembers the two homes the composition
/// root resolved — so no method here reads the environment again, and every
/// product state root this service opens carries the build's extra telemetry
/// sink.
pub struct PlakousApplication {
    pub(crate) executor: ApplicationExecutor,
    pub(crate) subscriptions: NativeSubscriptions,
    product: Option<product_paths::PlakPaths>,
    session_records: Option<Arc<dyn crate::store::session_records::SessionRecordStore>>,
    state_home: PathBuf,
    extra_telemetry_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
    exchange_observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
    _credential_telemetry: Option<crate::telemetry_sink::CredentialTelemetryGuard>,
}

impl PlakousApplication {
    /// `state_home` is the XDG state home and `extra_telemetry_sink` the
    /// binary's optional OTLP bridge; both come from the composition root.
    pub fn new(
        state_home: PathBuf,
        extra_telemetry_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
        product: Option<(
            product_paths::PlakPaths,
            Arc<dyn crate::store::session_records::SessionRecordStore>,
        )>,
    ) -> CliResult<Self> {
        let paths = SubscriptionPaths::from_xdg_state_home(state_home.clone())
            .map_err(|error| native_subscription_failure(error, None, &state_home))?;
        let executor = ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor construction failed"))?;
        let (product, session_records) =
            product.map_or((None, None), |(paths, store)| (Some(paths), Some(store)));
        let credential_telemetry = product
            .as_ref()
            .and_then(crate::telemetry_sink::install_product_credential_telemetry);
        let exchange_observer = if let Some(product) = &product {
            crate::commands::exchange_capture::from_environment(product.home())
                .map_err(|error| CliFailure::classified(CliFailureKind::Usage, error))?
        } else {
            None
        };
        let subscriptions = NativeSubscriptions::new(paths);
        let subscriptions = subscriptions.with_exchange_observer(exchange_observer.clone());
        Ok(Self {
            executor,
            subscriptions,
            product,
            session_records,
            state_home,
            extra_telemetry_sink,
            exchange_observer,
            _credential_telemetry: credential_telemetry,
        })
    }

    /// An accounts-and-catalog handle that will never open product state
    /// roots: the deferred TUI preparation thread builds one of these per
    /// attach, and it needs credentials and the catalog only.
    pub fn for_accounts(state_home: PathBuf) -> CliResult<Self> {
        Self::new(state_home, None, None)
    }

    /// A facade failure with its remedy, for `account` when it is known.
    pub(crate) fn subscription_failure(
        &self,
        error: plakous::subscriptions::NativeSubscriptionError,
        account: Option<NativeProvider>,
    ) -> CliFailure {
        native_subscription_failure(error, account, &self.state_home)
    }

    pub(super) fn refreshed_credential(
        &mut self,
        provider: NativeProvider,
        result: Result<
            plakous::subscriptions::NativeAuthStatus,
            plakous::subscriptions::NativeSubscriptionError,
        >,
    ) -> CliResult<CredentialState> {
        match result {
            Ok(status) => Ok(credential_state(status)),
            // The facade does not say why no credential is ready; the offline
            // status does, and names the repair for that state.
            Err(plakous::subscriptions::NativeSubscriptionError::CredentialUnavailable) => {
                let status = AccountOps::offline_status(self, provider)
                    .ok()
                    .map(|status| status.credential);
                Err(credential_unavailable_failure(
                    provider,
                    status,
                    &self.state_home,
                ))
            }
            Err(error) => Err(self.subscription_failure(error, Some(provider))),
        }
    }

    /// The product home this service resolved at construction.
    pub(crate) fn product_paths(&self) -> CliResult<&product_paths::PlakPaths> {
        self.product
            .as_ref()
            .ok_or_else(|| CliFailure::new("product home is unavailable"))
    }

    pub(crate) fn session_records(
        &self,
    ) -> CliResult<Arc<dyn crate::store::session_records::SessionRecordStore>> {
        self.session_records
            .clone()
            .ok_or_else(|| CliFailure::new("product home is unavailable"))
    }

    /// Product state roots under this service's homes, with the extra
    /// telemetry sink attached.
    pub(crate) fn state_roots(&self) -> CliResult<product_agent::ProductStateRoots> {
        let roots = product_state_roots_from(
            self.product_paths()?.home().to_path_buf(),
            self.state_home.clone(),
            self.extra_telemetry_sink.clone(),
            self.session_records()?,
        )?;
        let roots = roots.with_exchange_observer(self.exchange_observer.clone());
        Ok(roots)
    }
}

enum OfflineReadError {
    Execution,
    Subscription(plakous::subscriptions::NativeSubscriptionError),
}

fn stored_credential_from(
    read: Result<CliStatus, OfflineReadError>,
) -> Result<StoredCredential, OfflineReadError> {
    match read {
        Ok(status) => Ok(StoredCredential::from_status(status)),
        Err(OfflineReadError::Subscription(
            plakous::subscriptions::NativeSubscriptionError::CredentialProfileChanged,
        )) => Ok(StoredCredential::Unusable),
        Err(error) => Err(error),
    }
}

impl PlakousApplication {
    fn read_offline(&mut self, provider: NativeProvider) -> Result<CliStatus, OfflineReadError> {
        match self.executor.block_on(self.subscriptions.status(provider)) {
            Ok(Ok(status)) => Ok(CliStatus {
                compatibility: compatibility_state(status.compatibility()),
                credential: credential_state(status.credential()),
            }),
            Ok(Err(error)) => Err(OfflineReadError::Subscription(error)),
            Err(_) => Err(OfflineReadError::Execution),
        }
    }
}

impl AccountOps for PlakousApplication {
    fn notice(&self, provider: NativeProvider) -> CliNotice {
        let notice = NativeSubscriptions::notice(provider);
        CliNotice {
            profile_id: notice.profile_id(),
            notice_revision: notice.notice_revision(),
            reviewed_at: notice.reviewed_at(),
            expires_at: notice.expires_at(),
        }
    }

    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        self.read_offline(provider).map_err(|error| match error {
            OfflineReadError::Execution => application_execution_failure(()),
            OfflineReadError::Subscription(error) => {
                self.subscription_failure(error, Some(provider))
            }
        })
    }

    fn stored_credential(&mut self, provider: NativeProvider) -> CliResult<StoredCredential> {
        let read = self.read_offline(provider);
        stored_credential_from(read).map_err(|error| match error {
            OfflineReadError::Execution => application_execution_failure(()),
            OfflineReadError::Subscription(error) => {
                self.subscription_failure(error, Some(provider))
            }
        })
    }

    fn offline_status_row(&mut self, provider: NativeProvider) -> Result<CliStatus, String> {
        self.read_offline(provider).map_err(|error| match error {
            OfflineReadError::Execution => "application execution failed".to_owned(),
            OfflineReadError::Subscription(error) => {
                oauth_status_failure_cell(error, provider, &self.state_home)
            }
        })
    }

    fn subscription_usage(&mut self, provider: NativeProvider) -> CliResult<SubscriptionUsageRead> {
        self.executor
            .block_on(self.subscriptions.subscription_usage(provider))
            .map_err(application_execution_failure)
    }

    fn openrouter_key_usage(&mut self) -> CliResult<product_usage::VerticalObservation> {
        let credentials =
            crate::openrouter_product::ProductOpenRouterCredentials::shared(self.product_paths()?);
        Ok(
            match product_openrouter::probe_openrouter_key_usage(credentials.as_ref()) {
                product_openrouter::OpenRouterKeyUsageProbe::NotConfigured => {
                    product_usage::VerticalObservation::NotConnected
                }
                product_openrouter::OpenRouterKeyUsageProbe::Ok(usage, label) => {
                    product_usage::VerticalObservation::OpenRouterKey(usage, label)
                }
                product_openrouter::OpenRouterKeyUsageProbe::Failed(reason) => {
                    product_usage::VerticalObservation::Unavailable(reason)
                }
            },
        )
    }

    fn acknowledge_exact(
        &mut self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> CliResult<()> {
        self.executor
            .block_on(
                self.subscriptions
                    .acknowledge_exact(provider, operator_response),
            )
            .map_err(application_execution_failure)?
            .map_err(|error| self.subscription_failure(error, Some(provider)))
    }

    /// The facade's pending login does not say whose it is; the account is
    /// kept beside it so a failed completion names the account to log in to.
    type PendingLogin = (NativeProvider, plakous::subscriptions::PendingNativeLogin);

    fn begin_login(
        &mut self,
        provider: NativeProvider,
        mode: NativeLoginMode,
    ) -> CliResult<LoginChallenge<Self::PendingLogin>> {
        let pending = self
            .executor
            .block_on(self.subscriptions.begin_login(provider, mode))
            .map_err(application_execution_failure)?
            .map_err(|error| self.subscription_failure(error, Some(provider)))?;
        let instructions = pending.instructions();
        let authorize_url = instructions.verification_uri().to_owned();
        let completion = instructions.completion();
        Ok(LoginChallenge {
            pending: (provider, pending),
            authorize_url,
            completion,
        })
    }

    fn finish_login(
        &mut self,
        pending: Self::PendingLogin,
        response: LoginResponse<'_>,
    ) -> CliResult<LoginOutcome> {
        let (provider, pending) = pending;
        let completed = match response {
            LoginResponse::Pasted(input) => self
                .executor
                .block_on(pending.complete_remote(input))
                .map_err(application_execution_failure)?,
            LoginResponse::Automatic { timeout } => {
                let wait = self.executor.block_on(async {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => None,
                        result = tokio::time::timeout(timeout, pending.complete()) => Some(result),
                    }
                }).map_err(application_execution_failure)?;
                match wait {
                    None => return Ok(LoginOutcome::Cancelled),
                    Some(Err(_)) => return Ok(LoginOutcome::TimedOut),
                    Some(Ok(result)) => result,
                }
            }
        };
        completed
            .map(credential_state)
            .map(LoginOutcome::Completed)
            .map_err(|error| self.subscription_failure(error, Some(provider)))
    }

    fn refresh(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        let result = self
            .executor
            .block_on(self.subscriptions.refresh(provider))
            .map_err(application_execution_failure)?;
        self.refreshed_credential(provider, result)
    }

    fn revoke(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        self.executor
            .block_on(self.subscriptions.revoke(provider))
            .map_err(application_execution_failure)?
            .map(credential_state)
            .map_err(|error| self.subscription_failure(error, Some(provider)))
    }

    fn logout_local(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        self.executor
            .block_on(self.subscriptions.logout_local(provider))
            .map_err(application_execution_failure)?
            .map(credential_state)
            .map_err(|error| self.subscription_failure(error, Some(provider)))
    }

    fn openrouter_models(&mut self) -> CliResult<Vec<String>> {
        let paths = self.product_paths()?;
        self.executor
            .block_on(crate::openrouter_product::list_models(paths))
            .map_err(application_execution_failure)?
    }

    fn platform_catalog(
        &mut self,
        provider: plakous_hosted::binding::PlatformProvider,
    ) -> CliResult<plakous_hosted::platform::ApiModelCatalog> {
        let paths = self.product_paths()?.clone();
        let credentials = crate::platform_product::ProductPlatformCredentials::shared(&paths)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        self.executor
            .block_on(async move {
                plakous_hosted::platform::api_model_catalog(
                    provider,
                    credentials.as_ref(),
                    Some(paths.home()),
                    plakous_hosted::platform::ApiModelLists::Live,
                    now,
                )
                .await
            })
            .map_err(application_execution_failure)
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        let catalog = self
            .executor
            .block_on(self.subscriptions.model_catalog(provider))
            .map_err(application_execution_failure)?
            .map_err(|error| self.subscription_failure(error, Some(provider)))?;
        if catalog.provider() != provider {
            return Err(CliFailure::new("native model catalog provider mismatch"));
        }
        catalog_summaries(provider, &catalog)
    }
}

impl PresentationBackend for PlakousApplication {
    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        AccountOps::offline_status(self, provider)
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        AccountOps::models(self, provider)
    }
}

#[cfg(test)]
mod tests {
    use plakous::subscriptions::NativeSubscriptionError;

    use super::{OfflineReadError, StoredCredential, stored_credential_from};

    #[test]
    fn logout_reads_a_foreign_or_retired_record_as_unusable_not_a_failure() {
        assert!(matches!(
            stored_credential_from(Err(OfflineReadError::Subscription(
                NativeSubscriptionError::CredentialProfileChanged
            ))),
            Ok(StoredCredential::Unusable)
        ));
        assert!(matches!(
            stored_credential_from(Err(OfflineReadError::Subscription(
                NativeSubscriptionError::UnsafeStorage
            ))),
            Err(OfflineReadError::Subscription(
                NativeSubscriptionError::UnsafeStorage
            ))
        ));
    }
}
