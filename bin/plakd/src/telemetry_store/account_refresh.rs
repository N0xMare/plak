//! Exact-account refresh history for the web host's operator console.

use super::{CredentialRefreshRow, PlakPaths, TelemetryStore, TelemetryStoreOpenError};

impl TelemetryStore {
    /// Exact-account settled history for the operator console. Unattributed
    /// older rows never become evidence for a default or named account.
    pub(crate) fn latest_account_refresh_outcome(
        paths: &PlakPaths,
        account: &plakous_hosted::account::AccountRef,
    ) -> Result<Option<CredentialRefreshRow>, TelemetryStoreOpenError> {
        let Some(provider) = account.vertical().capability().native_provider() else {
            return Ok(None);
        };
        Self::read_credential_refresh(paths, provider.as_str(), true, Some(account.as_str()))
    }
}
