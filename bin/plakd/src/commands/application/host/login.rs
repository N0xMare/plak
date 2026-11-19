use super::HostAccountError;
use crate::commands::application::{
    AccountOps, AccountRef, LoginChallenge, LoginOutcome, NativeLoginMode, PlakousApplication,
    application_execution_failure, credential_state, login_challenge, native_provider,
};

pub(crate) enum HostLoginStartError {
    DeviceUnavailable,
    Cancelled,
    Failed,
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
