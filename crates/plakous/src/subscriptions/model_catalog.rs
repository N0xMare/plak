//! Gated model catalog reads and their persisted snapshots.

use std::path::{Path, PathBuf};

use plakous_auth::custody::AccountFacts;

use super::{
    Arc, CatalogPublicationSkipped, CompatibilityStatus, Future, InMemorySubscriptionRequestGate,
    MODEL_CATALOG_DEADLINE, NativeAuthStatus, NativeModelCatalog, NativeModelSummary,
    NativeProvider, NativeSubscriptionError, NativeSubscriptionStatus, NativeSubscriptions,
    SubscriptionLimitSnapshot, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestLimits, SystemSubscriptionGateClock, dispatch_credential_admitted,
    run_blocking,
};

#[cfg(test)]
pub(super) async fn run_classified_model_catalog<F, Fut>(
    provider: NativeProvider,
    status: NativeSubscriptionStatus,
    operation: F,
) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError>
where
    F: FnOnce(Arc<dyn SubscriptionRequestGate>) -> Fut,
    Fut: Future<Output = Result<Vec<NativeModelSummary>, NativeSubscriptionError>>,
{
    let gate = model_catalog_gate()?;
    run_classified_model_catalog_with_gate(provider, status, gate, operation).await
}

fn model_catalog_gate() -> Result<Arc<InMemorySubscriptionRequestGate>, NativeSubscriptionError> {
    let limits = SubscriptionRequestLimits::new(0, 3, 0, 0, Some(MODEL_CATALOG_DEADLINE), 0)
        .map_err(|_| NativeSubscriptionError::Internal)?;
    Ok(Arc::new(InMemorySubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    )))
}

async fn run_classified_model_catalog_with_gate<F, Fut>(
    provider: NativeProvider,
    status: NativeSubscriptionStatus,
    gate: Arc<InMemorySubscriptionRequestGate>,
    operation: F,
) -> Result<Vec<NativeModelSummary>, NativeSubscriptionError>
where
    F: FnOnce(Arc<dyn SubscriptionRequestGate>) -> Fut,
    Fut: Future<Output = Result<Vec<NativeModelSummary>, NativeSubscriptionError>>,
{
    match status.compatibility() {
        CompatibilityStatus::Current => {}
        CompatibilityStatus::Inactive => return Err(NativeSubscriptionError::ProfileInactive),
        CompatibilityStatus::Required => {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
    }
    if !dispatch_credential_admitted(provider, status.credential()) {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let dispatch_gate: Arc<dyn SubscriptionRequestGate> = gate.clone();
    let models = tokio::time::timeout(MODEL_CATALOG_DEADLINE, operation(dispatch_gate))
        .await
        .map_err(|_| catalog_operation_deadline(gate.as_ref()))??;
    let snapshot = gate
        .snapshot()
        .map_err(|_| NativeSubscriptionError::Internal)?;
    validate_model_catalog_snapshot(
        snapshot,
        gate.dispatch_attempts(SubscriptionRequestClass::Catalog)
            .map_err(|_| NativeSubscriptionError::Internal)?,
    )?;
    Ok(models)
}

pub(super) const OPERATION_DEADLINE_CAUSE: &str = "operation deadline";

pub(super) fn catalog_operation_deadline(
    gate: &dyn SubscriptionRequestGate,
) -> NativeSubscriptionError {
    let attempts = gate.snapshot().map_or(0, |snapshot| {
        snapshot
            .class(SubscriptionRequestClass::Catalog)
            .dispatched()
    });
    NativeSubscriptionError::ModelLookup {
        attempts: u8::try_from(attempts).unwrap_or(u8::MAX),
        cause: OPERATION_DEADLINE_CAUSE,
        retry_after: None,
    }
}

fn validate_model_catalog_snapshot(
    snapshot: SubscriptionLimitSnapshot,
    attempts: u32,
) -> Result<(), NativeSubscriptionError> {
    let auth = snapshot.class(SubscriptionRequestClass::AuthRefresh);
    let catalog = snapshot.class(SubscriptionRequestClass::Catalog);
    let inference = snapshot.class(SubscriptionRequestClass::Inference);
    let exact_zero = |class: plakous_auth::subscription_limit::SubscriptionClassSnapshot| {
        class.allowance() == 0
            && class.reserved() == 0
            && class.dispatched() == 0
            && class.abandoned() == 0
            && class.denied() == 0
            && class.input_token_upper_bound() == 0
    };
    if !exact_zero(auth)
        || catalog.allowance() != 3
        || !(1..=3).contains(&attempts)
        || !(1..=attempts).contains(&catalog.dispatched())
        || catalog.reserved() != catalog.dispatched()
        || catalog.abandoned() != 0
        || catalog.denied() != 0
        || catalog.input_token_upper_bound() != 0
        || !exact_zero(inference)
        || snapshot.retries() != 0
        || snapshot.max_elapsed() != Some(MODEL_CATALOG_DEADLINE)
        || snapshot.deadline_reached()
        || snapshot.elapsed() >= MODEL_CATALOG_DEADLINE
    {
        return Err(NativeSubscriptionError::Internal);
    }
    Ok(())
}

impl NativeSubscriptions {
    /// Use the product home's cache for gateway-custodied catalogs only.
    #[doc(hidden)]
    #[must_use]
    pub fn with_gateway_catalog_cache(mut self, home: &Path) -> Self {
        if let Some(gateway) = &mut self.gateway {
            gateway.catalog_cache = Some(home.join("cache/subscription-catalogs"));
        }
        self
    }

    /// Discovers the current authenticated account-scoped model catalog.
    ///
    /// Compatibility acceptance is validated before constructing an online
    /// provider manager or attempting authenticated discovery. Returned
    /// entries contain exact IDs, conservative portable capability bits, and
    /// exact provider-authored catalog defaults. A reported default is
    /// observational metadata, never a Plakous selection or request default.
    /// An unsuccessful snapshot update does not invalidate a live catalog;
    /// see [`NativeModelCatalog::publication_skipped`].
    ///
    /// # Errors
    ///
    /// Rejects missing or inactive compatibility consent, unavailable
    /// credentials, malformed or stale catalogs, and bounded transport
    /// failures. No unauthenticated or static fallback is attempted.
    pub async fn model_catalog(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let gate = model_catalog_gate()?;
        let deadline = tokio::time::Instant::now() + MODEL_CATALOG_DEADLINE;
        if let Some(gateway) = &self.gateway {
            let (mut catalog, before) = tokio::time::timeout_at(deadline, async {
                let before = self.gateway_facts(gateway, provider).await?;
                let status = Self::gateway_status_of(provider, &before)?;
                let catalog = self
                    .discover_catalog(provider, status, gate.clone())
                    .await?;
                Ok::<_, NativeSubscriptionError>((catalog, before))
            })
            .await
            .map_err(|_| catalog_operation_deadline(gate.as_ref()))??;
            if let Some(directory) = &gateway.catalog_cache {
                let publication = tokio::time::timeout_at(deadline, async {
                    let after = self
                        .gateway_facts(gateway, provider)
                        .await
                        .map_err(|_| CatalogPublicationSkipped::FactsUnavailable)?;
                    publish_gateway_snapshot(directory, &catalog, &before, &after).await
                })
                .await
                .unwrap_or(Err(CatalogPublicationSkipped::Deadline));
                catalog.publication_skipped = publication.err();
            }
            return Ok(catalog);
        }

        tokio::time::timeout_at(deadline, self.local_catalog(provider, gate.clone()))
            .await
            .map_err(|_| catalog_operation_deadline(gate.as_ref()))?
    }

    async fn local_catalog(
        &self,
        provider: NativeProvider,
        gate: Arc<InMemorySubscriptionRequestGate>,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        let mut catalog = self.discover_catalog(provider, status, gate).await?;
        lock.verify()?;
        if self
            .state
            .persist_catalog_snapshot(provider, catalog.models())
            .is_err()
        {
            catalog.publication_skipped = Some(CatalogPublicationSkipped::SnapshotWriteFailed);
        }
        Ok(catalog)
    }

    async fn discover_catalog(
        &self,
        provider: NativeProvider,
        status: NativeSubscriptionStatus,
        gate: Arc<InMemorySubscriptionRequestGate>,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let models =
            run_classified_model_catalog_with_gate(provider, status, gate, |gate| async move {
                super::vertical::require_vertical(provider)?
                    .catalog(self, gate)
                    .await
            })
            .await?;
        if models.is_empty() {
            return Err(NativeSubscriptionError::ModelUnavailable);
        }
        Ok(NativeModelCatalog {
            provider,
            models,
            publication_skipped: None,
        })
    }

    /// Hydrates the last persisted authenticated catalog without upstream discovery.
    ///
    /// Used for local-first TUI attach. A missing snapshot fails closed; stale
    /// snapshots remain displayable until a background refresh succeeds. Under
    /// gateway custody, the gateway supplies current account and consent facts;
    /// only the host-owned snapshot for that exact account is read.
    ///
    /// # Errors
    ///
    /// Rejects missing compatibility consent, unavailable credentials, absent
    /// snapshots, and malformed persisted data. Never performs catalog refresh.
    pub async fn model_catalog_persisted(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        if let Some(gateway) = &self.gateway {
            let directory = gateway
                .catalog_cache
                .as_deref()
                .ok_or(NativeSubscriptionError::GatewayCustody)?;
            let facts = self.gateway_facts(gateway, provider).await?;
            require_ready(Self::gateway_status_of(provider, &facts)?)?;
            let path = gateway_snapshot_path(directory, &facts)?;
            return run_blocking(move || {
                super::state::read_catalog_snapshot(&path, provider)
                    .map_err(|_| NativeSubscriptionError::ModelUnavailable)
            })
            .await?;
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        require_ready(self.status_unlocked(provider).await?)?;
        let paths = self.state.clone();
        let catalog = run_blocking(move || paths.load_persisted_catalog(provider)).await??;
        lock.verify()?;
        Ok(catalog)
    }
}

async fn publish_gateway_snapshot(
    directory: &Path,
    catalog: &NativeModelCatalog,
    before: &AccountFacts,
    after: &AccountFacts,
) -> Result<(), CatalogPublicationSkipped> {
    if before.account_binding != after.account_binding
        || before.selection_epoch_digest != after.selection_epoch_digest
    {
        return Err(CatalogPublicationSkipped::SelectionChanged);
    }
    let status = NativeSubscriptions::gateway_status_of(catalog.provider(), after)
        .map_err(|_| CatalogPublicationSkipped::FactsUnavailable)?;
    require_ready(status).map_err(|_| CatalogPublicationSkipped::AccountNotReady)?;
    let path = gateway_snapshot_path(directory, after)
        .map_err(|_| CatalogPublicationSkipped::FactsUnavailable)?;
    let catalog = catalog.clone();
    run_blocking(move || super::state::write_gateway_catalog_snapshot(&path, &catalog))
        .await
        .flatten()
        .map_err(|_| CatalogPublicationSkipped::SnapshotWriteFailed)
}

fn require_ready(status: NativeSubscriptionStatus) -> Result<(), NativeSubscriptionError> {
    match status.compatibility() {
        CompatibilityStatus::Current => {}
        CompatibilityStatus::Inactive => return Err(NativeSubscriptionError::ProfileInactive),
        CompatibilityStatus::Required => return Err(NativeSubscriptionError::AcceptanceRequired),
    }
    if status.credential() != NativeAuthStatus::Ready {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    Ok(())
}

fn gateway_snapshot_path(
    directory: &Path,
    facts: &AccountFacts,
) -> Result<PathBuf, NativeSubscriptionError> {
    let binding = facts
        .account_binding
        .ok_or(NativeSubscriptionError::AuthenticationBinding)?;
    let account = facts.slot.as_str().replace('/', "~");
    Ok(directory.join(format!("{account}-{}.json", binding.to_hex())))
}
