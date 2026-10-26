//! Gated model catalog reads and their persisted snapshots.

use super::{
    Arc, CompatibilityStatus, Future, InMemorySubscriptionRequestGate, MODEL_CATALOG_DEADLINE,
    NativeAuthStatus, NativeModelCatalog, NativeModelSummary, NativeProvider,
    NativeSubscriptionError, NativeSubscriptionStatus, NativeSubscriptions,
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
    /// Discovers the current authenticated account-scoped model catalog.
    ///
    /// Compatibility acceptance is validated before constructing an online
    /// provider manager or attempting authenticated discovery. Returned
    /// entries contain exact IDs, conservative portable capability bits, and
    /// exact provider-authored catalog defaults. A reported default is
    /// observational metadata, never a Plakous selection or request default.
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
        tokio::time::timeout(
            MODEL_CATALOG_DEADLINE,
            self.model_catalog_inner(provider, gate.clone()),
        )
        .await
        .map_err(|_| catalog_operation_deadline(gate.as_ref()))?
    }

    async fn model_catalog_inner(
        &self,
        provider: NativeProvider,
        gate: Arc<InMemorySubscriptionRequestGate>,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
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
        lock.verify()?;
        let catalog = NativeModelCatalog { provider, models };
        let _ = self
            .state
            .persist_catalog_snapshot(provider, catalog.models());
        Ok(catalog)
    }

    /// Hydrates the last persisted authenticated catalog without network I/O.
    ///
    /// Used for local-first TUI attach. A missing snapshot fails closed; stale
    /// snapshots remain displayable until a background refresh succeeds.
    ///
    /// # Errors
    ///
    /// Rejects missing compatibility consent, unavailable credentials, absent
    /// snapshots, and malformed persisted data. Never performs catalog refresh.
    pub async fn model_catalog_persisted(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeModelCatalog, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        match status.compatibility() {
            CompatibilityStatus::Current => {}
            CompatibilityStatus::Inactive => return Err(NativeSubscriptionError::ProfileInactive),
            CompatibilityStatus::Required => {
                return Err(NativeSubscriptionError::AcceptanceRequired);
            }
        }
        if status.credential() != NativeAuthStatus::Ready {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        let paths = self.state.clone();
        let catalog = run_blocking(move || paths.load_persisted_catalog(provider)).await??;
        lock.verify()?;
        Ok(catalog)
    }
}
