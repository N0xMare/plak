//! The immutable route authority bound to one catalog receipt.

use super::{
    ACCESS_REFRESH_LEAD_SECONDS, Arc, CatalogReceiptCore, CatalogRenewal, Future,
    GrokCodingProfile, ManagerCore, RuntimeModelAuthorityError, SubscriptionRequestPermit,
    Timestamp, XaiNativeAuthError, access_needs_refresh, fmt, system_unix_seconds,
};

/// Binds one exact catalog receipt to a new immutable route authority.
pub(super) fn native_route_authority(
    core: &Arc<ManagerCore>,
    receipt: &CatalogReceiptCore,
) -> Result<crate::XaiRenewedAuthority, XaiNativeAuthError> {
    let admission = crate::NativeXaiCatalogAdmission::new(
        receipt.public.entries.clone(),
        GrokCodingProfile::pinned_compatibility().profile_id()?,
        receipt.public.observed_at,
        receipt.public.expires_at,
        receipt.fence.generation().get(),
        receipt.selection_epoch.clone(),
    );
    Ok(crate::XaiRenewedAuthority {
        authority: Arc::new(NativeRouteAuthority {
            core: Arc::clone(core),
            expected_receipt: receipt.clone(),
        }),
        admission,
    })
}

struct NativeRouteAuthority {
    pub(super) core: Arc<ManagerCore>,
    pub(super) expected_receipt: CatalogReceiptCore,
}

impl NativeRouteAuthority {
    async fn route_refresh(&self, force: bool) -> Result<(), crate::XaiAuthError> {
        self.core
            .refresh_with_force(
                Some(&self.expected_receipt.public.account),
                Some(&self.expected_receipt.selection_epoch),
                Some(&self.expected_receipt.authority_id),
                force,
            )
            .await
            .map_err(|error| match error {
                XaiNativeAuthError::SelectionChanged | XaiNativeAuthError::AccountChanged => {
                    crate::XaiAuthError::BindingMismatch
                }
                XaiNativeAuthError::NotLoggedIn | XaiNativeAuthError::NotReady => {
                    crate::XaiAuthError::MissingCredential
                }
                _ => crate::XaiAuthError::CredentialSource,
            })
    }

    /// The claimed refresh and the authenticated catalog resolution
    /// `resolve_model_with_evidence` performs, both bound to this exact
    /// selection, then its resolution lease. The new receipt must keep the
    /// account, selection epoch and credential authority; only the
    /// generation may advance.
    async fn renew_route(
        &self,
        floor: &crate::XaiRenewalFloor,
    ) -> Result<crate::XaiRenewedAuthority, crate::XaiAuthError> {
        let transient = |_| crate::XaiAuthError::CredentialSource;
        let expected = &self.expected_receipt;
        let receipt = self
            .core
            .refresh_catalog_with(Some(&CatalogRenewal {
                account: &expected.public.account,
                selection_epoch: &expected.selection_epoch,
                authority_id: &expected.authority_id,
                floor,
            }))
            .await
            .map_err(|error| match error {
                XaiNativeAuthError::SelectionChanged | XaiNativeAuthError::AccountChanged => {
                    crate::XaiAuthError::BindingMismatch
                }
                _ => crate::XaiAuthError::CredentialSource,
            })?;
        if receipt.public.account != expected.public.account
            || receipt.selection_epoch != expected.selection_epoch
            || receipt.authority_id != expected.authority_id
            || receipt.fence.generation() < expected.fence.generation()
        {
            crate::route::log_authority_rejection("native.renew.binding");
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let resolution_lease = self
            .core
            .acquire_resolution_lease(&receipt)
            .map_err(transient)?;
        let catalog = self
            .core
            .catalog
            .lock()
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if catalog.as_ref() != Some(&receipt) {
            return Err(crate::XaiAuthError::CredentialSource);
        }
        let renewed = native_route_authority(&self.core, &receipt).map_err(transient)?;
        self.core
            .ensure_resolution_receipt_current(&receipt)
            .map_err(transient)?;
        drop(catalog);
        drop(resolution_lease);
        Ok(renewed)
    }
}

impl crate::XaiSubscriptionCredentialAuthority for NativeRouteAuthority {
    fn account_binding(&self) -> &str {
        self.expected_receipt.public.account.as_str()
    }

    fn credential_authority(&self) -> &str {
        &self.expected_receipt.authority_id
    }

    fn credential_generation(&self) -> u64 {
        self.expected_receipt.fence.generation().get()
    }

    fn selection_epoch(&self) -> &str {
        &self.expected_receipt.selection_epoch
    }

    fn revalidate_runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<crate::XaiRevalidatedSubscriptionAuthority, RuntimeModelAuthorityError> {
        self.core
            .ensure_route_current(&self.expected_receipt, true)
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if checked_at < self.expected_receipt.public.observed_at
            || checked_at >= self.expected_receipt.public.expires_at
        {
            crate::route::log_authority_rejection("native.catalog_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let stored = self
            .core
            .read_stored(&self.expected_receipt.fence)
            .map_err(|_| {
                crate::route::log_authority_rejection("native.stored_selection");
                RuntimeModelAuthorityError::Stale
            })?;
        if stored.selection_epoch != self.expected_receipt.selection_epoch {
            crate::route::log_authority_rejection("native.stored_selection");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let current_unix =
            system_unix_seconds().map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        if access_needs_refresh(stored.expires_at, current_unix, ACCESS_REFRESH_LEAD_SECONDS) {
            crate::route::log_authority_rejection("native.credential_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let credential_valid_until = Timestamp::new(
            stored
                .expires_at
                .saturating_sub(ACCESS_REFRESH_LEAD_SECONDS),
            0,
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        if credential_valid_until <= checked_at {
            crate::route::log_authority_rejection("native.credential_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        Ok(crate::XaiRevalidatedSubscriptionAuthority::new(
            credential_valid_until,
        ))
    }

    fn refresh_if_needed(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), crate::XaiAuthError>> + Send + '_>> {
        Box::pin(self.route_refresh(false))
    }

    fn refresh_after_rejection(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), crate::XaiAuthError>> + Send + '_>> {
        Box::pin(self.route_refresh(true))
    }

    fn renew<'a>(
        &'a self,
        floor: &'a crate::XaiRenewalFloor,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<crate::XaiRenewedAuthority, crate::XaiAuthError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(self.renew_route(floor))
    }

    fn preflight(&self) -> Result<(), crate::XaiAuthError> {
        self.core
            .ensure_route_current(&self.expected_receipt, false)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)
    }

    fn acquire(&self) -> Result<crate::XaiSubscriptionCredentialGrant, crate::XaiAuthError> {
        self.core.acquire_grant(&self.expected_receipt)
    }

    fn acquire_inference_permit(
        &self,
        input_liability: u64,
    ) -> Result<Option<Box<dyn SubscriptionRequestPermit>>, crate::XaiAuthError> {
        self.core
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire_inference(input_liability))
            .transpose()
            .map_err(|_| crate::XaiAuthError::CredentialSource)
    }

    fn revalidate(
        &self,
        _outgoing: &crate::XaiSubscriptionCredentialGrant,
    ) -> Result<(), crate::XaiAuthError> {
        self.core
            .ensure_route_receipt_current_while_leased(&self.expected_receipt)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)
    }
}

impl fmt::Debug for NativeRouteAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeRouteAuthority(<redacted selected slot>)")
    }
}
