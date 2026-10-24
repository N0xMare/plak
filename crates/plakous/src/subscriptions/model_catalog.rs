//! Gated model catalog reads and their persisted snapshots.

use super::{
    Arc, CompatibilityStatus, Future, InMemorySubscriptionRequestGate, MODEL_CATALOG_DEADLINE,
    ModelId, NativeAuthStatus, NativeModelCatalog, NativeModelResolutionError, NativeModelSummary,
    NativeModelVisibility, NativeProvider, NativeServiceTierSummary, NativeSubscriptionError,
    NativeSubscriptionStatus, NativeSubscriptions, ResolvedModelDescriptor, RuntimeModelAuthority,
    SubscriptionLimitSnapshot, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestLimits, SystemSubscriptionGateClock, codex_native_http_config,
    dispatch_credential_admitted, facade_codex_visibility, facade_native_tool_mode,
    facade_xai_model_is_selectable, map_claude_error, map_codex_error, map_xai_error, run_blocking,
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
        cause: "operation deadline",
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
                match provider {
                    NativeProvider::Codex => {
                        let online = self.codex_online_with_gate(Some(gate), true).await?;
                        let resolver = plakous_openai::CodexModelResolver::new(
                            Arc::clone(&online.manager),
                            online.client.with_observer(self.exchange_observer.clone()),
                            codex_native_http_config()?,
                        )
                        .map_err(map_codex_error)?;
                        let resolver = if self.catalog_retries {
                            resolver
                        } else {
                            resolver.without_catalog_retries()
                        };
                        let catalog = resolver.refresh_catalog().await.map_err(map_codex_error)?;
                        Ok(catalog
                            .entries()
                            .iter()
                            .filter(|entry| entry.supports_text())
                            .map(|entry| NativeModelSummary {
                                exact_id: entry.model_id().to_owned(),
                                visibility: entry
                                    .visibility()
                                    .map_or(NativeModelVisibility::None, facade_codex_visibility),
                                native_tool_mode: facade_native_tool_mode(entry.tool_mode()),
                                streaming: true,
                                tools: entry.supports_tools(),
                                reasoning_efforts: entry.reasoning_effort_options().to_vec(),
                                default_reasoning_effort: entry.default_reasoning_effort(),
                                service_tiers: entry
                                    .service_tiers()
                                    .iter()
                                    .map(|tier| NativeServiceTierSummary {
                                        id: tier.id().to_owned(),
                                        name: tier.name().to_owned(),
                                        description: tier.description().to_owned(),
                                    })
                                    .collect(),
                                default_service_tier: entry
                                    .default_service_tier()
                                    .map(str::to_owned),
                                reported_context_length: entry
                                    .capacity_facts()
                                    .context_window()
                                    .map(|window| u64::from(window.get())),
                            })
                            .collect())
                    }
                    NativeProvider::Xai => {
                        {
                            let online = self.xai_online_with_gate(None, true).await?;
                            online
                                .manager
                                .refresh_if_needed()
                                .await
                                .map_err(map_xai_error)?;
                        }
                        let online = self.xai_online_with_gate(Some(gate), true).await?;
                        let receipt = online
                            .manager
                            .refresh_catalog()
                            .await
                            .map_err(map_xai_error)?;
                        Ok(receipt
                            .entries()
                            .iter()
                            .filter(|entry| facade_xai_model_is_selectable(entry.model().as_str()))
                            .map(|entry| NativeModelSummary {
                                exact_id: entry.model().as_str().to_owned(),
                                visibility: NativeModelVisibility::List,
                                native_tool_mode: None,
                                streaming: true,
                                tools: true,
                                reasoning_efforts:
                                    plakous_xai::subscription_model_reasoning_effort_options(
                                        entry.model().as_str(),
                                    )
                                    .to_vec(),
                                default_reasoning_effort: None,
                                service_tiers: Vec::new(),
                                default_service_tier: None,
                                reported_context_length: Some(u64::from(
                                    entry.diagnostics().reported_context_length().get(),
                                )),
                            })
                            .collect())
                    }
                    NativeProvider::Claude => {
                        let online = self
                            .claude_online_with_gate(Some(gate.clone()), true)
                            .await?;
                        let mut permit = gate
                            .acquire(SubscriptionRequestClass::Catalog)
                            .map_err(|_| NativeSubscriptionError::Internal)?;
                        permit
                            .authorize_dispatch()
                            .map_err(|_| NativeSubscriptionError::Internal)?;
                        Ok(online
                            .manager
                            .enrolled_catalog()
                            .map_err(map_claude_error)?
                            .into_iter()
                            .map(|entry| NativeModelSummary {
                                exact_id: entry.exact_id().to_owned(),
                                visibility: NativeModelVisibility::List,
                                native_tool_mode: None,
                                streaming: true,
                                tools: true,
                                reasoning_efforts:
                                    plakous_anthropic::claude_oauth_reasoning_effort_options(
                                        entry.exact_id(),
                                    )
                                    .to_vec(),
                                default_reasoning_effort: None,
                                service_tiers: Vec::new(),
                                default_service_tier: None,
                                // Claude publishes no OAuth catalog window; this is
                                // the enrolled row's window, the one the session runs at.
                                reported_context_length: Some(entry.input_tokens()),
                            })
                            .collect())
                    }
                }
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

    pub(super) fn attach_compile_authority_from_codex(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_openai::CodexModelResolutionEvidence,
    ) -> Result<RuntimeModelAuthority, NativeModelResolutionError> {
        RuntimeModelAuthority::new(
            descriptor,
            evidence.account_fingerprint(),
            evidence.credential_authority_binding(),
            evidence.selected_credential_generation(),
            evidence.catalog_observed_at(),
            Some(evidence.catalog_expires_at()),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
            )
        })
    }

    pub(super) fn attach_compile_authority_from_xai(
        requested_model: &ModelId,
        descriptor: &ResolvedModelDescriptor,
        evidence: &plakous_xai::XaiModelResolutionEvidence,
    ) -> Result<RuntimeModelAuthority, NativeModelResolutionError> {
        RuntimeModelAuthority::new(
            descriptor,
            evidence.account_fingerprint(),
            evidence.credential_authority_binding(),
            evidence.selected_credential_generation(),
            evidence.catalog_observed_at(),
            Some(evidence.catalog_expires_at()),
        )
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
            )
        })
        .map(|authority| {
            authority.with_admission_witness(evidence.runtime_admission_witness().clone())
        })
    }
}
