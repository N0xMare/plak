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
    application_execution_failure, credential_state, credential_unavailable_failure,
    native_subscription_failure, oauth_status_failure_cell, product_state_roots_from,
};
use crate::home::paths as product_paths;
use crate::presentation::{
    CliModelSummary, CliStatus, CompatibilityState, CredentialState, PresentationBackend,
};
use plakous_hosted::account::{AccountRef, Vertical};
use plakous_hosted::agent as product_agent;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::openrouter as product_openrouter;
use plakous_hosted::subscription_usage as product_usage;

pub(crate) fn native_provider(account: &AccountRef) -> CliResult<NativeProvider> {
    account
        .vertical()
        .capability()
        .native_provider()
        .ok_or_else(|| {
            plakous_hosted::capabilities::AccountAdmissionError::AccountTargetMismatch {
                account: account.to_string(),
            }
            .into()
        })
}

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
    /// Present exactly when `completion` is a device code.
    pub device_code: Option<DeviceCodePrompt>,
    /// Opening the login retired a stored record this build could not use.
    pub replaced_unusable: bool,
}

/// The one-time code a device-code login shows, and how long it stays valid.
pub struct DeviceCodePrompt {
    pub user_code: zeroize::Zeroizing<String>,
    pub expires_in: std::time::Duration,
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

/// The one classification of a stored subscription credential.
///
/// Offline status, login, logout, refresh, usage and model runs all read a
/// record through this, so the offline and online paths cannot disagree about
/// what it is or what repairs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialVerdict {
    /// Nothing stored, or a revoked tombstone.
    SignedOut,
    /// Ready under an admissible compatibility profile.
    Usable(CompatibilityState),
    /// The access token must be refreshed before use.
    NeedsRefresh(CompatibilityState),
    /// A refresh outcome is ambiguous.
    Quarantined,
    /// A record this build cannot use; a new login replaces it.
    NeedsLogin(LoginReason),
    /// A newer build wrote the stored login; this build neither uses nor
    /// rewrites it unless the operator confirms replacing it.
    WrittenByNewerBuild,
    /// The compiled compatibility profile is no longer admissible.
    ProfileInactive,
    /// Native subscription storage is unavailable on this platform.
    Unsupported,
}

/// Why a stored record needs a new login.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginReason {
    /// Written by an earlier build in a retired payload format.
    RetiredFormat,
    /// Written under another compatibility profile (or an older build whose
    /// format reads as one).
    ProfileChanged,
}

/// The account command that observed a credential; its remedy never names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountCommand {
    Status,
    Login,
    Logout,
    LogoutLocal,
    Refresh,
    Usage,
    Run,
}

impl CredentialVerdict {
    pub(crate) fn from_status(status: CliStatus) -> Self {
        let inactive = status.compatibility == CompatibilityState::Inactive;
        match status.credential {
            CredentialState::SignedOut | CredentialState::Revoked => Self::SignedOut,
            CredentialState::Ready
            | CredentialState::RefreshRequired
            | CredentialState::Refreshing
                if inactive =>
            {
                Self::ProfileInactive
            }
            // A refresh another process owns settles to a new Ready generation
            // or to its own failure state; it is a stored, usable login now.
            CredentialState::Ready | CredentialState::Refreshing => {
                Self::Usable(status.compatibility)
            }
            CredentialState::RefreshRequired => Self::NeedsRefresh(status.compatibility),
            CredentialState::Quarantined => Self::Quarantined,
            CredentialState::LoginRequired => Self::NeedsLogin(LoginReason::RetiredFormat),
            CredentialState::ProfileChanged => Self::NeedsLogin(LoginReason::ProfileChanged),
            CredentialState::WrittenByNewerBuild => Self::WrittenByNewerBuild,
            CredentialState::Unsupported => Self::Unsupported,
        }
    }

    /// Classifies an offline read; a record refused as another profile is a
    /// verdict, not a failure.
    fn classify(read: Result<CliStatus, OfflineReadError>) -> Result<Self, OfflineReadError> {
        match read {
            Ok(status) => Ok(Self::from_status(status)),
            Err(OfflineReadError::Subscription(
                plakous::subscriptions::NativeSubscriptionError::CredentialProfileChanged,
            )) => Ok(Self::NeedsLogin(LoginReason::ProfileChanged)),
            Err(OfflineReadError::Subscription(
                plakous::subscriptions::NativeSubscriptionError::CredentialWrittenByNewerBuild,
            )) => Ok(Self::WrittenByNewerBuild),
            Err(error) => Err(error),
        }
    }

    pub(crate) const fn stored(self) -> StoredCredential {
        match self {
            Self::SignedOut => StoredCredential::Absent,
            Self::Usable(compatibility) | Self::NeedsRefresh(compatibility) => {
                StoredCredential::Usable(compatibility)
            }
            Self::Quarantined
            | Self::NeedsLogin(_)
            | Self::WrittenByNewerBuild
            | Self::ProfileInactive
            | Self::Unsupported => StoredCredential::Unusable,
        }
    }

    /// The repair for this record as observed by `after`, never naming
    /// `after` itself: a failed step always points somewhere else.
    pub(crate) fn remedy(self, provider: &AccountRef, after: AccountCommand) -> Option<String> {
        let name = provider.as_str();
        let login = format!("run plak login {name}");
        let refresh = format!("run plak refresh {name}");
        let retire = format!("run plak logout {name} --local");
        let primary = match self {
            Self::Usable(_) => return None,
            Self::Quarantined if provider.vertical() == Vertical::Grok => {
                format!("{refresh}; {login} if that fails")
            }
            Self::SignedOut | Self::NeedsLogin(_) | Self::Quarantined => login.clone(),
            Self::NeedsRefresh(_) => refresh.clone(),
            Self::WrittenByNewerBuild if after == AccountCommand::Login => {
                return Some("use the newer plak build that wrote it".to_owned());
            }
            Self::WrittenByNewerBuild => format!(
                "use the newer plak build that wrote it, or {login} to replace it (that build must then log in again)"
            ),
            Self::ProfileInactive => "use a newer plak build".to_owned(),
            Self::Unsupported => "use an API-key account (plak key set <account>)".to_owned(),
        };
        Some(match after {
            AccountCommand::Refresh if primary.starts_with(&refresh) => login,
            AccountCommand::Login if primary.contains(&login) => retire,
            AccountCommand::LogoutLocal if primary.contains("--local") => login,
            _ => primary,
        })
    }
}

/// What `plak logout` reached at the provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Revocation {
    /// The provider revoked the credential; the local state it left.
    Revoked(CredentialState),
    /// This build refused the stored record before any request, so it was
    /// not revoked remotely; local retirement is its repair.
    Unusable,
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
    /// Local selections, including the permanent defaults. No credential bytes.
    fn accounts(&mut self) -> CliResult<Vec<AccountRef>> {
        plakous_hosted::capabilities::ROUTES
            .iter()
            .map(|route| route.account.parse().map_err(CliFailure::from))
            .collect()
    }
    /// A readable sibling with the same authenticated upstream identity, if known.
    fn same_upstream_account(&mut self, _account: &AccountRef) -> Option<AccountRef> {
        None
    }
    fn remove_account(&mut self, account: &AccountRef) -> CliResult<()> {
        Err(super::support::account_unavailable(account))
    }
    fn prepare_login(&mut self, _account: &AccountRef) -> CliResult<()> {
        Ok(())
    }
    fn end_login_attempt(&mut self, _account: &AccountRef) {}
    fn notice(&self, provider: &AccountRef) -> CliResult<CliNotice>;

    fn offline_status(&mut self, provider: &AccountRef) -> CliResult<CliStatus>;

    /// Offline classification for `plak logout`. A record this build refuses
    /// (another profile or a retired payload format) is [`StoredCredential::Unusable`]
    /// rather than a failure, because local retirement is its repair.
    fn stored_credential(&mut self, provider: &AccountRef) -> CliResult<StoredCredential>;

    /// Offline status for the all-accounts table. A failure is that row's
    /// credential cell, not a reason to drop the other accounts.
    fn offline_status_row(&mut self, provider: &AccountRef) -> Result<CliStatus, String> {
        self.offline_status(provider)
            .map_err(|failure| failure.message().to_owned())
    }

    fn subscription_usage(&mut self, provider: &AccountRef) -> CliResult<SubscriptionUsageRead>;

    /// Whether a `Ready` credential's stored access token has expired, read
    /// locally without provider network I/O.
    fn access_token_expired(&mut self, provider: &AccountRef) -> CliResult<bool>;

    /// Reads the OpenRouter inference key's own allowance on demand.
    fn openrouter_key_usage(
        &mut self,
        account: &AccountRef,
    ) -> CliResult<product_usage::VerticalObservation>;

    fn acknowledge_exact(
        &mut self,
        provider: &AccountRef,
        operator_response: &str,
    ) -> CliResult<()>;

    type PendingLogin;

    fn begin_login(
        &mut self,
        provider: &AccountRef,
        mode: NativeLoginMode,
    ) -> CliResult<LoginChallenge<Self::PendingLogin>>;

    fn finish_login(
        &mut self,
        pending: Self::PendingLogin,
        response: LoginResponse<'_>,
    ) -> CliResult<LoginOutcome>;

    /// Best-effort product-only provenance for Harbor. Failure never changes
    /// the successfully persisted credential or its Ready outcome.
    fn record_login_host(&mut self, _provider: &AccountRef) -> bool {
        true
    }

    fn refresh(&mut self, provider: &AccountRef) -> CliResult<CredentialState>;

    /// Claims one refresh grant regardless of expiry; returns the new state
    /// and access-token expiry (Unix seconds).
    fn refresh_now(&mut self, provider: &AccountRef) -> CliResult<(CredentialState, u64)>;

    fn revoke(&mut self, provider: &AccountRef) -> CliResult<Revocation>;

    fn logout_local(&mut self, provider: &AccountRef) -> CliResult<CredentialState>;

    /// Retires a newer build's login state after the operator confirmed it.
    fn replace_newer_build(&mut self, provider: &AccountRef) -> CliResult<CredentialState>;

    fn models(&mut self, provider: &AccountRef) -> CliResult<Vec<CliModelSummary>>;

    fn openrouter_models(&mut self, account: &AccountRef) -> CliResult<Vec<String>>;
    /// One direct API route's catalog: live list, cache or bundled table.
    fn platform_catalog(
        &mut self,
        account: &AccountRef,
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
            unavailable_reason: model.unavailable_reason().map(str::to_owned),
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
    login_subscriptions: Option<(AccountRef, NativeSubscriptions)>,
    extra_telemetry_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
    exchange_observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
    /// The process's one job bound, shared by every state root it opens.
    jobs: plakous_hosted::agent::ProcessJobs,
    _credential_telemetry: Option<crate::telemetry_sink::CredentialTelemetryGuard>,
}

impl PlakousApplication {
    /// Supplies read-only home paths without opening a product store or telemetry.
    pub(crate) fn with_account_home(mut self, paths: Option<product_paths::PlakPaths>) -> Self {
        if self.product.is_none() {
            self.product = paths;
        }
        self
    }
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
            login_subscriptions: None,
            extra_telemetry_sink,
            exchange_observer,
            jobs: plakous_hosted::agent::ProcessJobs::default(),
            _credential_telemetry: credential_telemetry,
        })
    }

    /// This application, opening its state roots under `jobs`, the
    /// process's one job bound.
    #[must_use]
    pub fn with_process_jobs(mut self, jobs: &plakous_hosted::agent::ProcessJobs) -> Self {
        self.jobs = jobs.clone();
        self
    }

    /// An accounts-and-catalog handle that will never open product state
    /// roots: the deferred TUI preparation thread builds one of these per
    /// attach, and it needs credentials and the catalog only.
    pub fn for_accounts(state_home: PathBuf) -> CliResult<Self> {
        Self::new(state_home, None, None)
    }

    #[cfg(test)]
    pub(crate) fn for_test_accounts(state_home: PathBuf) -> CliResult<Self> {
        let mut application = Self::for_accounts(state_home)?;
        application.subscriptions = application.subscriptions.with_clock(test_clock());
        Ok(application)
    }

    /// A facade failure with its remedy, for `account` when it is known.
    pub(crate) fn subscription_failure(
        &self,
        error: plakous::subscriptions::NativeSubscriptionError,
        account: Option<NativeProvider>,
    ) -> CliFailure {
        native_subscription_failure(error, account, &self.state_home)
    }

    pub(crate) fn account_failure(
        &self,
        error: plakous::subscriptions::NativeSubscriptionError,
        account: &AccountRef,
    ) -> CliFailure {
        if error == plakous::subscriptions::NativeSubscriptionError::AccountBusy {
            return CliFailure::from(
                plakous_hosted::capabilities::AccountAdmissionError::AccountBusy {
                    account: account.to_string(),
                },
            );
        }
        if account.is_named() {
            use plakous::subscriptions::NativeSubscriptionError;
            use plakous_hosted::capabilities::AccountAdmissionError;
            if error == NativeSubscriptionError::AccountSignedIn {
                return CliFailure::from(AccountAdmissionError::AccountSignedIn {
                    account: account.to_string(),
                });
            }
            if error == NativeSubscriptionError::CredentialUnavailable {
                return super::support::account_unavailable(account);
            }
            return CliFailure::classified(CliFailureKind::Auth, format!("{account}: {error}"))
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
                    account: plakous_host_protocol::notice_remedy::AccountTarget::from_account(
                        &account.to_string(),
                    ),
                });
        }
        match native_provider(account) {
            Ok(provider) => self.subscription_failure(error, Some(provider)),
            Err(failure) => failure,
        }
    }

    pub(crate) fn subscriptions_for(&self, account: &AccountRef) -> CliResult<NativeSubscriptions> {
        let provider = native_provider(account)?;
        if let Some((selected, subscriptions)) = &self.login_subscriptions {
            if selected == account {
                return Ok(subscriptions.clone());
            }
        }
        if let Some(name) = account.name() {
            return self
                .subscriptions
                .for_named_account(provider, name.as_str())
                .map_err(|error| self.account_failure(error, account));
        }
        Ok(self.subscriptions.clone())
    }

    pub(super) fn refreshed_credential(
        &mut self,
        account: &AccountRef,
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
                if account.is_named() {
                    return Err(super::support::account_unavailable(account));
                }
                let provider = native_provider(account)?;
                let status = AccountOps::offline_status(self, account)
                    .ok()
                    .map(|status| status.credential);
                Err(credential_unavailable_failure(
                    provider,
                    status,
                    &self.state_home,
                ))
            }
            Err(error) => Err(self.account_failure(error, account)),
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
            self.product_paths()?,
            self.state_home.clone(),
            self.extra_telemetry_sink.clone(),
            self.session_records()?,
            &self.jobs,
        )?;
        let roots = roots.with_exchange_observer(self.exchange_observer.clone());
        Ok(roots)
    }
}

enum OfflineReadError {
    Execution,
    Selection(CliFailure),
    Subscription(plakous::subscriptions::NativeSubscriptionError),
}

fn stored_credential_from(
    read: Result<CliStatus, OfflineReadError>,
) -> Result<StoredCredential, OfflineReadError> {
    CredentialVerdict::classify(read).map(CredentialVerdict::stored)
}

/// A record this build refuses is never answered with a remedy naming
/// `plak logout`, the command that asked.
fn revocation_from(
    result: Result<
        plakous::subscriptions::NativeAuthStatus,
        plakous::subscriptions::NativeSubscriptionError,
    >,
) -> Result<Revocation, plakous::subscriptions::NativeSubscriptionError> {
    match result {
        Ok(status) => Ok(Revocation::Revoked(credential_state(status))),
        Err(
            plakous::subscriptions::NativeSubscriptionError::CredentialProfileChanged
            | plakous::subscriptions::NativeSubscriptionError::CredentialWrittenByNewerBuild,
        ) => Ok(Revocation::Unusable),
        Err(error) => Err(error),
    }
}

impl PlakousApplication {
    fn read_offline(&mut self, account: &AccountRef) -> Result<CliStatus, OfflineReadError> {
        let provider = native_provider(account).map_err(OfflineReadError::Selection)?;
        let subscriptions = self
            .subscriptions_for(account)
            .map_err(OfflineReadError::Selection)?;
        if account.is_named()
            && !self
                .executor
                .block_on(subscriptions.account_exists(provider))
                .map_err(|_| OfflineReadError::Execution)?
                .map_err(OfflineReadError::Subscription)?
        {
            return Err(OfflineReadError::Subscription(
                plakous::subscriptions::NativeSubscriptionError::CredentialUnavailable,
            ));
        }
        match self
            .executor
            .block_on(super::support::offline_subscription_status(
                &subscriptions,
                provider,
            )) {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(OfflineReadError::Subscription(error)),
            Err(_) => Err(OfflineReadError::Execution),
        }
    }
}

impl AccountOps for PlakousApplication {
    fn remove_account(&mut self, account: &AccountRef) -> CliResult<()> {
        if !account.is_named() {
            return Err(CliFailure::from(
                plakous_hosted::capabilities::AccountAdmissionError::DefaultAccountNotRemovable {
                    account: account.to_string(),
                },
            ));
        }
        if account.vertical().is_api() {
            crate::home::secrets::clear_api_key(self.product_paths()?, account.as_str())
                .map_err(CliFailure::new)?;
        } else {
            let subscriptions = self.subscriptions_for(account)?;
            self.executor
                .block_on(subscriptions.remove_signed_out_account(native_provider(account)?))
                .map_err(application_execution_failure)?
                .map_err(|error| self.account_failure(error, account))?;
        }
        Ok(())
    }
    fn accounts(&mut self) -> CliResult<Vec<AccountRef>> {
        let mut accounts = Vec::new();
        for route in plakous_hosted::capabilities::ROUTES {
            let vertical = route.account.parse::<AccountRef>()?.vertical();
            accounts.push(AccountRef::default_for(vertical));
            if !vertical.capability().compiled {
                continue;
            }
            let names = if let Some(provider) = vertical.capability().native_provider() {
                self.executor
                    .block_on(self.subscriptions.named_accounts(provider))
                    .map_err(application_execution_failure)?
                    .map_err(|error| self.subscription_failure(error, Some(provider)))?
            } else if let Some(paths) = self.product.as_ref() {
                crate::home::secrets::list_shelf_secrets(
                    paths,
                    crate::home::paths::SecretShelf::Api(vertical),
                )
                .map_err(CliFailure::new)?
            } else {
                Vec::new()
            };
            for name in names {
                accounts.push(format!("{}/{name}", vertical.as_str()).parse()?);
            }
        }
        Ok(accounts)
    }

    fn same_upstream_account(&mut self, account: &AccountRef) -> Option<AccountRef> {
        if !account.is_named() {
            return None;
        }
        let provider = native_provider(account).ok()?;
        let selected = self.subscriptions_for(account).ok()?;
        let fingerprint = self
            .executor
            .block_on(selected.upstream_account_fingerprint(provider))
            .ok()?
            .ok()??;
        let mut siblings = vec![AccountRef::default_for(account.vertical())];
        let names = self
            .executor
            .block_on(self.subscriptions.named_accounts(provider))
            .ok()?
            .ok()?;
        siblings.extend(names.into_iter().take(64).filter_map(|name| {
            format!("{}/{name}", account.vertical().as_str())
                .parse()
                .ok()
        }));
        siblings
            .into_iter()
            .filter(|sibling| sibling != account)
            .find(|sibling| {
                let Ok(subscriptions) = self.subscriptions_for(sibling) else {
                    return false;
                };
                self.executor
                    .block_on(subscriptions.upstream_account_fingerprint(provider))
                    .ok()
                    .and_then(Result::ok)
                    .flatten()
                    == Some(fingerprint)
            })
    }

    fn prepare_login(&mut self, account: &AccountRef) -> CliResult<()> {
        let subscriptions = self.subscriptions_for(account)?;
        let selected = self
            .executor
            .block_on(subscriptions.create_account_for_login(native_provider(account)?))
            .map_err(application_execution_failure)?
            .map_err(|error| self.account_failure(error, account))?;
        self.login_subscriptions = Some((account.clone(), selected));
        Ok(())
    }

    fn end_login_attempt(&mut self, account: &AccountRef) {
        if self
            .login_subscriptions
            .as_ref()
            .is_some_and(|(selected, _)| selected == account)
        {
            self.login_subscriptions = None;
        }
    }

    fn notice(&self, account: &AccountRef) -> CliResult<CliNotice> {
        let provider = native_provider(account)?;
        let notice = NativeSubscriptions::notice(provider)
            .map_err(|error| self.account_failure(error, account))?;
        Ok(CliNotice {
            profile_id: notice.profile_id(),
            notice_revision: notice.notice_revision(),
            reviewed_at: notice.reviewed_at(),
            expires_at: notice.expires_at(),
        })
    }

    fn offline_status(&mut self, account: &AccountRef) -> CliResult<CliStatus> {
        self.read_offline(account).map_err(|error| match error {
            OfflineReadError::Execution => application_execution_failure(()),
            OfflineReadError::Selection(error) => error,
            OfflineReadError::Subscription(error) => self.account_failure(error, account),
        })
    }

    fn stored_credential(&mut self, account: &AccountRef) -> CliResult<StoredCredential> {
        let read = self.read_offline(account);
        stored_credential_from(read).map_err(|error| match error {
            OfflineReadError::Execution => application_execution_failure(()),
            OfflineReadError::Selection(error) => error,
            OfflineReadError::Subscription(error) => self.account_failure(error, account),
        })
    }

    fn offline_status_row(&mut self, account: &AccountRef) -> Result<CliStatus, String> {
        let provider = native_provider(account).map_err(|error| error.message().to_owned())?;
        self.read_offline(account).map_err(|error| match error {
            OfflineReadError::Execution => "application execution failed".to_owned(),
            OfflineReadError::Selection(error) => error.message().to_owned(),
            OfflineReadError::Subscription(error) => {
                if account.is_named() {
                    self.account_failure(error, account).message().to_owned()
                } else {
                    oauth_status_failure_cell(error, provider, &self.state_home)
                }
            }
        })
    }

    fn subscription_usage(&mut self, account: &AccountRef) -> CliResult<SubscriptionUsageRead> {
        let subscriptions = self.subscriptions_for(account)?;
        self.executor
            .block_on(subscriptions.subscription_usage(native_provider(account)?))
            .map_err(application_execution_failure)
    }

    fn access_token_expired(&mut self, account: &AccountRef) -> CliResult<bool> {
        let subscriptions = self.subscriptions_for(account)?;
        self.executor
            .block_on(subscriptions.access_token_expired(native_provider(account)?))
            .map_err(application_execution_failure)?
            .map_err(|error| self.account_failure(error, account))
    }

    fn openrouter_key_usage(
        &mut self,
        account: &AccountRef,
    ) -> CliResult<product_usage::VerticalObservation> {
        let credentials =
            crate::openrouter_product::ProductOpenRouterCredentials::shared(self.product_paths()?);
        Ok(
            match product_openrouter::probe_openrouter_key_usage(credentials.as_ref(), account) {
                product_openrouter::OpenRouterKeyUsageProbe::NotConfigured => {
                    product_usage::VerticalObservation::NotConnected
                }
                product_openrouter::OpenRouterKeyUsageProbe::Ok(usage, label) => {
                    product_usage::VerticalObservation::OpenRouterKey(usage, label)
                }
                product_openrouter::OpenRouterKeyUsageProbe::Failed(reason) => {
                    product_usage::VerticalObservation::Unavailable(reason)
                }
                product_openrouter::OpenRouterKeyUsageProbe::Rejected { reason, source } => {
                    product_usage::VerticalObservation::RejectedKey { reason, source }
                }
            },
        )
    }

    fn acknowledge_exact(
        &mut self,
        account: &AccountRef,
        operator_response: &str,
    ) -> CliResult<()> {
        let subscriptions = self.subscriptions_for(account)?;
        self.executor
            .block_on(subscriptions.acknowledge_exact(native_provider(account)?, operator_response))
            .map_err(application_execution_failure)?
            .map_err(|error| self.account_failure(error, account))
    }

    /// The facade's pending login does not say whose it is; the account is
    /// kept beside it so a failed completion names the account to log in to.
    type PendingLogin = (AccountRef, plakous::subscriptions::PendingNativeLogin);

    fn begin_login(
        &mut self,
        account: &AccountRef,
        mode: NativeLoginMode,
    ) -> CliResult<LoginChallenge<Self::PendingLogin>> {
        let subscriptions = self.subscriptions_for(account)?;
        let pending = self
            .executor
            .block_on(subscriptions.begin_login(native_provider(account)?, mode))
            .map_err(application_execution_failure)?
            .map_err(|error| self.account_failure(error, account))?;
        let replaced_unusable = pending.replaced_unusable_record();
        let instructions = pending.instructions();
        let authorize_url = instructions.verification_uri().to_owned();
        let completion = instructions.completion();
        let device_code = instructions.device_code().map(|code| DeviceCodePrompt {
            user_code: zeroize::Zeroizing::new(code.user_code().to_owned()),
            expires_in: code.expires_in(),
        });
        Ok(LoginChallenge {
            pending: (account.clone(), pending),
            authorize_url,
            completion,
            device_code,
            replaced_unusable,
        })
    }

    fn finish_login(
        &mut self,
        pending: Self::PendingLogin,
        response: LoginResponse<'_>,
    ) -> CliResult<LoginOutcome> {
        let (account, pending) = pending;
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
            .map_err(|error| self.account_failure(error, &account))
    }

    fn record_login_host(&mut self, account: &AccountRef) -> bool {
        let Ok(provider) = native_provider(account) else {
            return false;
        };
        let root = self.state_home.join("plak/subscriptions");
        let directory = account.name().map_or_else(
            || root.join(provider.as_str()),
            |name| {
                root.join("accounts")
                    .join(account.vertical().as_str())
                    .join(name.as_str())
            },
        );
        self.executor
            .block_on(async move {
                tokio::task::spawn_blocking(move || super::login_host::record(&directory))
                    .await
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn refresh(&mut self, account: &AccountRef) -> CliResult<CredentialState> {
        let subscriptions = self.subscriptions_for(account)?;
        let result = self
            .executor
            .block_on(subscriptions.refresh(native_provider(account)?))
            .map_err(application_execution_failure)?;
        self.refreshed_credential(account, result)
    }

    fn refresh_now(&mut self, account: &AccountRef) -> CliResult<(CredentialState, u64)> {
        let subscriptions = self.subscriptions_for(account)?;
        let result = self
            .executor
            .block_on(subscriptions.refresh_now(native_provider(account)?))
            .map_err(application_execution_failure)?;
        let expires_at = result
            .as_ref()
            .ok()
            .map(|forced| forced.access_expires_at());
        let state = self.refreshed_credential(
            account,
            result.map(plakous::subscriptions::ForcedRefresh::status),
        )?;
        Ok((state, expires_at.unwrap_or(0)))
    }

    fn revoke(&mut self, account: &AccountRef) -> CliResult<Revocation> {
        let subscriptions = self.subscriptions_for(account)?;
        let result = self
            .executor
            .block_on(subscriptions.revoke(native_provider(account)?))
            .map_err(application_execution_failure)?;
        revocation_from(result).map_err(|error| self.account_failure(error, account))
    }

    fn logout_local(&mut self, account: &AccountRef) -> CliResult<CredentialState> {
        let subscriptions = self.subscriptions_for(account)?;
        self.executor
            .block_on(subscriptions.logout_local(native_provider(account)?))
            .map_err(application_execution_failure)?
            .map(credential_state)
            .map_err(|error| self.account_failure(error, account))
    }

    fn replace_newer_build(&mut self, account: &AccountRef) -> CliResult<CredentialState> {
        let subscriptions = self.subscriptions_for(account)?;
        self.executor
            .block_on(subscriptions.replace_newer_build_state(native_provider(account)?))
            .map_err(application_execution_failure)?
            .map(credential_state)
            .map_err(|error| self.account_failure(error, account))
    }

    fn openrouter_models(&mut self, account: &AccountRef) -> CliResult<Vec<String>> {
        let paths = self.product_paths()?;
        self.executor
            .block_on(crate::openrouter_product::list_models(paths, account))
            .map_err(application_execution_failure)?
    }

    fn platform_catalog(
        &mut self,
        account: &AccountRef,
    ) -> CliResult<plakous_hosted::platform::ApiModelCatalog> {
        let provider = account
            .vertical()
            .capability()
            .platform_provider()
            .ok_or_else(|| {
                plakous_hosted::capabilities::AccountAdmissionError::AccountTargetMismatch {
                    account: account.to_string(),
                }
            })?;
        let paths = self.product_paths()?.clone();
        let credentials = crate::platform_product::ProductPlatformCredentials::shared(&paths);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        self.executor
            .block_on(async move {
                plakous_hosted::platform::api_model_catalog(
                    provider,
                    account,
                    credentials.as_ref(),
                    Some(paths.home()),
                    plakous_hosted::platform::ApiModelLists::Live,
                    now,
                )
                .await
            })
            .map_err(application_execution_failure)?
    }

    fn models(&mut self, account: &AccountRef) -> CliResult<Vec<CliModelSummary>> {
        let provider = native_provider(account)?;
        let subscriptions = self.subscriptions_for(account)?;
        let catalog = self
            .executor
            .block_on(subscriptions.model_catalog(provider))
            .map_err(application_execution_failure)?
            .map_err(|error| self.account_failure(error, account))?;
        if catalog.provider() != provider {
            return Err(CliFailure::new("native model catalog provider mismatch"));
        }
        catalog_summaries(provider, &catalog)
    }
}

impl PresentationBackend for PlakousApplication {
    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        AccountOps::offline_status(self, &super::support::cli_provider_name(provider).parse()?)
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        AccountOps::models(self, &super::support::cli_provider_name(provider).parse()?)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "compiled review-window fixture")]
pub(crate) fn test_clock() -> Arc<dyn plakous::model::Clock> {
    let reviewed_at = [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ]
    .into_iter()
    .filter_map(|provider| {
        NativeSubscriptions::notice(provider)
            .ok()
            .map(plakous::subscriptions::CompatibilityNotice::reviewed_at)
    })
    .max()
    .expect("compiled notices");
    Arc::new(plakous::model::FixedClock::new(
        plakous::model::Timestamp::new(reviewed_at + 60, 0).expect("review time"),
    ))
}

#[cfg(test)]
mod tests;
