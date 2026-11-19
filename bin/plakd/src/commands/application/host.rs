//! Non-interactive account workflows for the authenticated host operator.

use super::{
    AccountOps, AccountRef, CliFailure, CliResult, CompatibilityState, CredentialState, Revocation,
    StoredCredential,
};

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
