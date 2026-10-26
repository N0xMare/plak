//! Gated native model and provider resolution.

use super::{
    Arc, ConfiguredLanguageModel, MODEL_CATALOG_DEADLINE, NativeModelResolutionError,
    NativeModelSelector, NativeProvider, NativeSubscriptionError, NativeSubscriptions,
    ResolvedNativeModel, ResolvedProvider, RuntimeModelAuthority, SelectedProviderConfiguration,
    SessionLineage, SubscriptionRequestGate, SubscriptionRequestLimits,
    SystemSubscriptionGateClock, TurnScopedSubscriptionRequestGate,
    apply_turn_scoped_provider_gate, catalog_operation_deadline, native_provider_route_selection,
    timestamp_now,
};

/// The pinned native provider whose exact route `selected` names, with the
/// exact model selector for it.
fn gated_provider_selection(
    selected: &SelectedProviderConfiguration,
) -> Result<(NativeProvider, NativeModelSelector), NativeModelResolutionError> {
    let requested_model = selected.route().requested_model().clone();
    let provider = [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ]
    .into_iter()
    .find(|provider| {
        native_provider_route_selection(*provider, requested_model.clone())
            .is_ok_and(|route| &route == selected.route())
    })
    .ok_or_else(|| {
        NativeModelResolutionError::after_selection(
            requested_model.clone(),
            NativeSubscriptionError::ProviderConfigurationRejected,
        )
    })?;
    let selector = NativeModelSelector::Exact(requested_model.clone());
    Ok((provider, selector))
}

/// Compiles one resolved native model and its runtime authority into the
/// provider's configured model, the step both gated resolvers share.
fn configure_gated_provider(
    selected: &SelectedProviderConfiguration,
    provider: NativeProvider,
    resolved: ResolvedNativeModel,
    authority: RuntimeModelAuthority,
) -> Result<ResolvedProvider, NativeModelResolutionError> {
    let requested_model = selected.route().requested_model().clone();
    if let plakous_model::ProcessingTierDisposition::ProviderNamed(tier) =
        &selected.behavior().processing_tier
        && !resolved.service_tiers.iter().any(|id| id == tier.as_str())
    {
        return Err(NativeModelResolutionError::after_selection(
            requested_model,
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch("processing_tier"),
        ));
    }
    let vertical = super::vertical::require_vertical(provider)
        .map_err(NativeModelResolutionError::before_selection)?;
    let native_tool_mode = vertical.tool_mode(&resolved);
    let configuration = vertical.configure(selected, &resolved, &authority)?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        resolved.model(),
        configuration,
        authority,
    )
    .map_err(|_| {
        NativeModelResolutionError::after_selection(
            resolved.selected_model.clone(),
            NativeSubscriptionError::ConfiguredAggregateRejected,
        )
    })?;
    Ok(ResolvedProvider {
        model: resolved,
        configured,
        native_tool_mode,
        reresolution: None,
    })
}

impl NativeSubscriptions {
    /// Resolves one exact native model under a classified physical-request gate.
    ///
    /// This is the library **admit** seam for any caller that needs the
    /// fail-closed catalog + transport gate. It is not a general uncapped model
    /// client: the same gate is installed on auth refresh, catalog discovery, and
    /// every resolved inference transport.
    ///
    /// # Errors
    ///
    /// Returns [`NativeModelResolutionError`] when compatibility locks, online
    /// admission, catalog selection, or evidence binding fail. `selected_model`
    /// is set only after a concrete id was chosen.
    pub(crate) async fn resolve_gated_native_model(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedNativeModel, NativeModelResolutionError> {
        self.resolve_gated_native_model_phased(provider, model_selector, gate, false, lineage)
            .await
            .map(|(model, _)| model)
    }

    async fn resolve_gated_native_model_for_attach(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<(ResolvedNativeModel, RuntimeModelAuthority), NativeModelResolutionError> {
        let (model, compile_authority) = self
            .resolve_gated_native_model_phased(
                provider,
                model_selector,
                gate,
                true,
                SessionLineage::Root,
            )
            .await?;
        let compile_authority = compile_authority.ok_or_else(|| {
            NativeModelResolutionError::after_selection(
                model.selected_model.clone(),
                NativeSubscriptionError::Internal,
            )
        })?;
        Ok((model, compile_authority))
    }

    async fn resolve_gated_native_model_phased(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        attach: bool,
        lineage: SessionLineage,
    ) -> Result<(ResolvedNativeModel, Option<RuntimeModelAuthority>), NativeModelResolutionError>
    {
        model_selector
            .validate_for_provider(provider)
            .map_err(|_| {
                NativeModelResolutionError::before_selection(
                    NativeSubscriptionError::ModelUnavailable,
                )
            })?;
        let _lock = self
            .acquire_compatibility_lock(provider)
            .await
            .map_err(NativeModelResolutionError::before_selection)?;
        super::vertical::require_vertical(provider)
            .map_err(NativeModelResolutionError::before_selection)?
            .resolve(self, model_selector, gate, attach, lineage)
            .await
    }

    /// Resolves one complete explicit native provider configuration under the
    /// supplied gate: the dispatch path of [`Self::resolve_turn_scoped_provider`]
    /// and of a windowed re-resolution.
    ///
    /// The literal provider route, model, credential selection, behavior, and
    /// product policy are supplied by the caller. Resolution accepts only an
    /// exact pinned native route, obtains fresh authority from the same model
    /// object that will execute, and compiles through that provider's actual
    /// Responses policy table. It fails when the selected route is not exact,
    /// live authority cannot be revalidated, catalog facts reject the requested
    /// behavior, or family compilation fails.
    pub(super) async fn resolve_gated_provider(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let (provider, selector) = gated_provider_selection(&selected)?;
        let resolved = self
            .resolve_gated_native_model(provider, &selector, gate, lineage)
            .await?;
        let requested_model = selected.route().requested_model().clone();
        let checked_at = timestamp_now().map_err(|error| {
            NativeModelResolutionError::after_selection(requested_model.clone(), error)
        })?;
        let authority = resolved
            .model()
            .revalidate_runtime_authority(checked_at)
            .await
            .map_err(|_| {
                NativeModelResolutionError::after_selection(
                    requested_model,
                    NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
                )
            })?;
        configure_gated_provider(&selected, provider, resolved, authority)
    }

    /// Local-first attach variant of [`Self::resolve_gated_provider`], behind
    /// [`Self::resolve_turn_scoped_provider_for_attach`].
    ///
    /// Skips attach-time `revalidate_runtime_authority` and Claude OAuth
    /// `refresh_if_needed`. Codex and Grok still perform authenticated catalog
    /// discovery. First dispatch still revalidates.
    async fn resolve_gated_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let (provider, selector) = gated_provider_selection(&selected)?;
        let (resolved, authority) = self
            .resolve_gated_native_model_for_attach(provider, &selector, gate)
            .await?;
        configure_gated_provider(&selected, provider, resolved, authority)
    }

    /// Resolves an ordinary native provider with one nonrenewable inference
    /// budget per admitted logical root.
    ///
    /// Authentication and catalog requests share the supplied process-level
    /// limits. Every root turn or session workflow receives an isolated copy of
    /// the inference allowance and retains it until the runtime publishes that
    /// root's terminal result.
    ///
    /// # Errors
    ///
    /// Returns a typed native resolution failure when the selected route is not
    /// exact, live authority cannot be revalidated, catalog facts reject the
    /// requested behavior, family compilation fails, or the catalog deadline
    /// passes.
    pub async fn resolve_turn_scoped_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        self.resolve_turn_scoped_provider_once(
            selected,
            limits,
            lineage,
            tokio::time::Instant::now() + MODEL_CATALOG_DEADLINE,
        )
        .await
    }

    /// Resolves a spawned child's provider within its caller's startup deadline.
    ///
    /// Uses one catalog GET per operation and retries a transient lookup once,
    /// releasing the failed operation before backoff. Reserves ten seconds for
    /// creation, acknowledgement and registration within `startup_deadline`.
    ///
    /// # Errors
    ///
    /// Returns the same typed failures as [`Self::resolve_turn_scoped_provider`],
    /// retaining the lookup cause when the startup budget cannot fit a retry.
    pub async fn resolve_spawned_child_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        startup_deadline: std::time::Instant,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let single = self.clone().without_model_lookup_retries();
        super::child::resolve(
            |deadline| {
                single.resolve_turn_scoped_provider_once(
                    selected.clone(),
                    limits,
                    SessionLineage::SpawnedChild,
                    deadline,
                )
            },
            selected.route().requested_model(),
            Some(tokio::time::Instant::from_std(startup_deadline)),
            |delay| self.child_lookup_backoff(delay),
        )
        .await
    }

    async fn resolve_turn_scoped_provider_once(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        lineage: SessionLineage,
        deadline: tokio::time::Instant,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = tokio::time::timeout_at(
            deadline,
            self.resolve_gated_provider(selected.clone(), gate, lineage),
        )
        .await
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                selected.route().requested_model().clone(),
                catalog_operation_deadline(turn_gate.as_ref()),
            )
        })??;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, lineage, turn_gate))
    }

    /// Local-first attach variant of [`Self::resolve_turn_scoped_provider`].
    ///
    /// Same turn gate and signature as dispatch resolution. Attach skips Claude
    /// `refresh_if_needed` and runtime authority revalidation; Codex/Grok catalog
    /// HTTP may still run. First dispatch still revalidates.
    ///
    /// # Errors
    ///
    /// Returns the same typed native resolution failures as
    /// [`Self::resolve_turn_scoped_provider`], except authority revalidation.
    pub async fn resolve_turn_scoped_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = tokio::time::timeout(
            MODEL_CATALOG_DEADLINE,
            self.resolve_gated_provider_for_attach(selected.clone(), gate),
        )
        .await
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                selected.route().requested_model().clone(),
                catalog_operation_deadline(turn_gate.as_ref()),
            )
        })??;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, SessionLineage::Root, turn_gate))
    }
}

#[cfg(all(test, feature = "codex", feature = "grok", feature = "claude"))]
mod tests {
    use super::super::NativeProvider;
    use super::super::reresolve::tests::{BASE, resolved};
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// A scripted resolution and the selection it was configured under, with
    /// no Codex or SuperGrok catalog evidence.
    fn inputs() -> TestResult<(
        SelectedProviderConfiguration,
        ResolvedNativeModel,
        RuntimeModelAuthority,
    )> {
        let provider = resolved(&BASE)?;
        let selected = provider.configured.configuration().selected().clone();
        let authority = provider.configured.initial_authority().clone();
        Ok((selected, provider.model, authority))
    }

    #[test]
    fn missing_catalog_evidence_is_internal_for_each_responses_provider() -> TestResult {
        for provider in [NativeProvider::Codex, NativeProvider::Xai] {
            let (selected, resolved, authority) = inputs()?;
            let requested = selected.route().requested_model().clone();
            let error = configure_gated_provider(&selected, provider, resolved, authority)
                .err()
                .ok_or("configuration without catalog evidence")?;
            assert_eq!(
                error.kind(),
                NativeSubscriptionError::Internal,
                "{provider:?}"
            );
            assert_eq!(error.selected_model, Some(requested));
        }
        Ok(())
    }

    #[test]
    fn a_named_tier_requires_the_exact_fresh_catalog_row() -> TestResult {
        let (selected, resolved, authority) = inputs()?;
        let mut behavior = selected.behavior().clone();
        behavior.processing_tier = plakous_model::ProcessingTierDisposition::ProviderNamed(
            plakous_model::ProviderServiceTierId::new("priority")?,
        );
        let selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
        let error = configure_gated_provider(&selected, NativeProvider::Codex, resolved, authority)
            .err()
            .ok_or("unadvertised tier was admitted")?;
        assert_eq!(
            error.kind(),
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch("processing_tier")
        );
        let (_, mut resolved, authority) = inputs()?;
        resolved.service_tiers.push("priority".into());
        let error = configure_gated_provider(&selected, NativeProvider::Codex, resolved, authority)
            .err()
            .ok_or("fixture without authenticated evidence was admitted")?;
        assert_eq!(error.kind(), NativeSubscriptionError::Internal);
        Ok(())
    }

    #[test]
    fn a_claude_selection_outside_the_messages_policy_is_rejected() -> TestResult {
        // The scripted selection disables replay, which the Claude OAuth
        // Messages policy requires: a policy mismatch, not an identity error.
        let (selected, resolved, authority) = inputs()?;
        let requested = selected.route().requested_model().clone();
        let error =
            configure_gated_provider(&selected, NativeProvider::Claude, resolved, authority)
                .err()
                .ok_or("Claude configuration outside the Messages policy")?;
        assert_eq!(
            error.kind(),
            NativeSubscriptionError::ProviderConfigurationRejected
        );
        assert_eq!(error.selected_model, Some(requested));
        Ok(())
    }
}
