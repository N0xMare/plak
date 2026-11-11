//! Non-interactive account workflows for the authenticated host operator.

use super::{
    AccountOps, AccountRef, CliFailure, CliResult, CompatibilityState, CredentialState,
    LoginChallenge, LoginOutcome, NativeLoginMode, PlakousApplication, Revocation,
    StoredCredential, application_execution_failure, credential_state, login_challenge,
    native_provider,
};

pub(crate) enum HostLoginStartError {
    DeviceUnavailable,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub(crate) enum HostAccountError {
    Refused(CliFailure),
    OutcomeUnknown(CliFailure),
}

impl HostAccountError {
    fn effectful_call(error: CliFailure) -> Self {
        use std::error::Error as _;
        if error.source().is_some_and(|source| {
            source.is::<plakous_hosted::capabilities::AccountAdmissionError>()
        }) {
            Self::Refused(error)
        } else {
            Self::OutcomeUnknown(error)
        }
    }
}

/// Exact non-interactive preparation after host consent, before provider I/O.
pub(crate) fn prepare_host_login(
    app: &mut impl AccountOps,
    account: &AccountRef,
    notice_revision: &str,
    replace_newer: bool,
) -> CliResult<()> {
    let notice = app.notice(account)?;
    if notice.notice_revision != notice_revision {
        return Err(CliFailure::new(
            "exact integration notice consent is required",
        ));
    }
    app.prepare_login(account)?;
    let status = app.offline_status(account)?;
    if matches!(
        status.compatibility,
        CompatibilityState::Inactive | CompatibilityState::Unsupported
    ) {
        return Err(CliFailure::new("integration is inactive or unavailable"));
    }
    if status.credential == CredentialState::WrittenByNewerBuild {
        if !replace_newer {
            return Err(CliFailure::new(
                "explicit newer-build replacement consent is required",
            ));
        }
        app.replace_newer_build(account)?;
    }
    app.acknowledge_exact(account, notice_revision)
}

pub(crate) fn logout_host_account(
    app: &mut impl AccountOps,
    account: &AccountRef,
    local_only: bool,
) -> Result<Option<bool>, HostAccountError> {
    fn retire(app: &mut impl AccountOps, account: &AccountRef) -> Result<(), HostAccountError> {
        let status = app
            .logout_local(account)
            .map_err(HostAccountError::effectful_call)?;
        if !matches!(
            status,
            CredentialState::Revoked | CredentialState::SignedOut
        ) {
            return Err(HostAccountError::OutcomeUnknown(CliFailure::new(
                "local credential retirement did not settle",
            )));
        }
        Ok(())
    }
    let stored = app
        .stored_credential(account)
        .map_err(HostAccountError::Refused)?;
    if local_only
        || matches!(
            stored,
            StoredCredential::Absent | StoredCredential::Unusable
        )
    {
        retire(app, account)?;
        return Ok((!matches!(stored, StoredCredential::Absent)).then_some(true));
    }
    if !matches!(
        stored,
        StoredCredential::Usable(CompatibilityState::Current)
    ) {
        return Err(HostAccountError::Refused(CliFailure::new(
            "exact integration notice consent is required",
        )));
    }
    match app
        .revoke(account)
        .map_err(HostAccountError::effectful_call)?
    {
        Revocation::Revoked(CredentialState::Revoked | CredentialState::SignedOut) => {
            Ok(Some(false))
        }
        Revocation::Revoked(_) => Err(HostAccountError::OutcomeUnknown(CliFailure::new(
            "remote credential revocation did not settle",
        ))),
        Revocation::Unusable => {
            retire(app, account)?;
            Ok(Some(true))
        }
    }
}

impl PlakousApplication {
    pub(crate) fn begin_host_login(
        &mut self,
        account: &AccountRef,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<LoginChallenge<<Self as AccountOps>::PendingLogin>, HostLoginStartError> {
        let subscriptions = self
            .subscriptions_for(account)
            .map_err(|_| HostLoginStartError::Failed)?;
        let provider = native_provider(account).map_err(|_| HostLoginStartError::Failed)?;
        let pending = self.executor.block_on(async move {
            if *cancelled.borrow() { return None; }
            tokio::select! { biased; _ = cancelled.changed() => None, result = subscriptions.begin_login(provider, NativeLoginMode::Remote) => Some(result) }
        }).map_err(|_| HostLoginStartError::Failed)?
            .ok_or(HostLoginStartError::Cancelled)?
            .map_err(|error| {
                if error == plakous::subscriptions::NativeSubscriptionError::DeviceLoginUnavailable
                {
                    HostLoginStartError::DeviceUnavailable
                } else {
                    HostLoginStartError::Failed
                }
            })?;
        Ok(login_challenge(account, pending))
    }
    /// Finishes one host-owned attempt without installing process signal
    /// handlers. Dropping completion retains the facade's recovery semantics.
    pub(crate) fn finish_host_login(
        &mut self,
        pending: <Self as AccountOps>::PendingLogin,
        pasted: Option<&str>,
        timeout: std::time::Duration,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<LoginOutcome, HostAccountError> {
        let (account, pending) = pending;
        let effects = pending.effects();
        let result = self
            .executor
            .block_on(async move {
                if *cancelled.borrow() {
                    return Ok(LoginOutcome::Cancelled);
                }
                let completion = async move {
                    match pasted {
                        Some(input) => pending.complete_remote(input).await,
                        None => pending.complete().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => Ok(LoginOutcome::Cancelled),
                    result = tokio::time::timeout(timeout, completion) => match result {
                        Err(_) => Ok(LoginOutcome::TimedOut),
                        Ok(result) => result.map(credential_state).map(LoginOutcome::Completed),
                    },
                }
            })
            .map_err(application_execution_failure)
            .map_err(HostAccountError::Refused)?;
        result.map_err(|error| {
            let error = self.account_failure(error, &account);
            if effects.may_have_started() {
                HostAccountError::OutcomeUnknown(error)
            } else {
                HostAccountError::Refused(error)
            }
        })
    }
}
