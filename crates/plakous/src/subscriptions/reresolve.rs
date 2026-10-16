//! Fresh exact re-resolution of a windowed subscription model.
//!
//! SuperGrok and Codex OAuth catalog evidence lasts a fixed window. Inside it a
//! running model renews its own receipt; past it only a new descriptor can
//! continue. This resolves the same selected configuration again through the
//! same exact path as the original resolution, charges it to the same
//! turn-scoped gate, and returns it only when nothing that identifies the
//! session, the login or the model changed.

use super::{
    Arc, NativeModelResolutionError, NativeSubscriptionError, NativeSubscriptions,
    ResolvedProvider, SelectedProviderConfiguration, SessionLineage, SubscriptionRequestGate,
    Timestamp, TurnScopedSubscriptionRequestGate, scope::apply_turn_scoped_provider_gate,
};

/// What a fresh resolution of the same model needs: the facade, the exact
/// selection, its lineage and the turn-scoped gate whose permits and live
/// inference roots the fresh model must share.
pub(super) struct Reresolution {
    subscriptions: NativeSubscriptions,
    selected: SelectedProviderConfiguration,
    lineage: SessionLineage,
    gate: Arc<TurnScopedSubscriptionRequestGate>,
}

impl NativeSubscriptions {
    /// Attaches a re-resolution handle to a turn-scoped resolution whose
    /// evidence is windowed (Codex, SuperGrok). Claude OAuth has none.
    pub(super) fn with_reresolution(
        &self,
        mut resolved: ResolvedProvider,
        selected: SelectedProviderConfiguration,
        lineage: SessionLineage,
        gate: Arc<TurnScopedSubscriptionRequestGate>,
    ) -> ResolvedProvider {
        let evidence = &resolved.model.evidence;
        if evidence.codex_exact.is_some() || evidence.xai_exact.is_some() {
            resolved.reresolution = Some(Arc::new(Reresolution {
                subscriptions: self.clone(),
                selected,
                lineage,
                gate,
            }));
        }
        resolved
    }
}

impl ResolvedProvider {
    /// Returns the end of the admitted catalog evidence window of a
    /// turn-scoped Codex or SuperGrok resolution, past which only
    /// [`Self::reresolve`] can continue. `None` for routes without a window.
    #[must_use]
    pub fn admitted_until(&self) -> Option<Timestamp> {
        self.reresolution
            .as_ref()
            .map(|_| self.model.evidence.catalog_expires_at)
    }

    /// Resolves the same exact selected configuration afresh.
    ///
    /// The resolution runs the same exact path as the original one (claimed
    /// refresh when due, authenticated catalog at the exact route) and spends
    /// its auth and catalog permits from the same turn-scoped gate. The fresh
    /// resolution is returned only when the selected and effective
    /// configuration identities, account, login selection epoch, credential
    /// authority, route fingerprint (Codex: the whole admitted catalog row),
    /// native tool mode and content-filter guidance are unchanged and the
    /// credential generation has not decreased. Nothing falls back.
    ///
    /// # Errors
    ///
    /// Returns [`NativeSubscriptionError::ReresolutionBindingChanged`] when
    /// any of those changed, [`NativeSubscriptionError::ProviderConfigurationRejected`]
    /// for a resolution without a window, or the fresh resolution's own failure.
    pub async fn reresolve(&self) -> Result<Self, NativeModelResolutionError> {
        let Some(handle) = self.reresolution.as_ref() else {
            return Err(NativeModelResolutionError::after_selection(
                self.selected_model().clone(),
                NativeSubscriptionError::ProviderConfigurationRejected,
            ));
        };
        let gate: Arc<dyn SubscriptionRequestGate> = handle.gate.clone();
        let fresh = handle
            .subscriptions
            .resolve_gated_provider(handle.selected.clone(), gate, handle.lineage)
            .await?;
        let mut fresh = apply_turn_scoped_provider_gate(fresh, Arc::clone(&handle.gate))?;
        if !self.admits_fresh(&fresh) {
            return Err(NativeModelResolutionError::after_selection(
                self.selected_model().clone(),
                NativeSubscriptionError::ReresolutionBindingChanged,
            ));
        }
        fresh.reresolution = Some(Arc::clone(handle));
        Ok(fresh)
    }

    fn admits_fresh(&self, fresh: &Self) -> bool {
        let (previous_authority, fresh_authority) =
            (self.initial_authority(), fresh.initial_authority());
        let (previous_evidence, fresh_evidence) = (&self.model.evidence, &fresh.model.evidence);
        let same_selection = matches!(
            (self.credential_selection_binding(), fresh.credential_selection_binding()),
            (Ok(previous), Ok(current)) if previous == current
        );
        same_selection
            && self.configuration().selected_identity() == fresh.configuration().selected_identity()
            && self.configuration().effective_identity()
                == fresh.configuration().effective_identity()
            && previous_authority.account_binding() == fresh_authority.account_binding()
            && previous_authority.credential_authority_binding()
                == fresh_authority.credential_authority_binding()
            && previous_authority.credential_generation() <= fresh_authority.credential_generation()
            && previous_evidence.route_fingerprint == fresh_evidence.route_fingerprint
            && previous_evidence.codex_exact.is_some() == fresh_evidence.codex_exact.is_some()
            && previous_evidence.xai_exact.is_some() == fresh_evidence.xai_exact.is_some()
            && self.native_tool_mode == fresh.native_tool_mode
            && self.model().content_filter_retry_instruction()
                == fresh.model().content_filter_retry_instruction()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use plakous_model::{
        Capability, CapabilityConstraints, CapabilitySet, Digest, DigestDomain, FactProvenance,
        FactSource, FeatureSupport, LanguageModel, LocalDurabilityDisposition,
        ModelAdapterProfileId, ModelFamilyId, ModelId, OutputBoundDisposition, ProtocolId,
        ResolvedModelDescriptor, RouteId, RuntimeModelAuthority, Support, Timestamp,
    };

    use super::super::{
        NativeToolMode, ResolvedNativeModel, ResolvedProvider, SubscriptionResolutionEvidence,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    struct Fixture {
        observed: u64,
        epoch: &'static str,
        route: &'static str,
        account: &'static [u8],
        generation: u64,
        tool_mode: NativeToolMode,
    }

    const BASE: Fixture = Fixture {
        observed: 1,
        epoch: "login-epoch-a",
        route: "route-fingerprint-a",
        account: b"account-a",
        generation: 3,
        tool_mode: NativeToolMode::Direct,
    };

    fn resolved(fixture: &Fixture) -> TestResult<ResolvedProvider> {
        let observed = Timestamp::new(fixture.observed, 0)?;
        let expires = Timestamp::new(fixture.observed + 21_600, 0)?;
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                FactProvenance::new(FactSource::RouteEvidence, observed, Some(expires)),
            ),
        );
        let descriptor = crate::test_model::with_context_contract(
            ResolvedModelDescriptor::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                ProtocolId::new("test.protocol")?,
                RouteId::new("test.route")?,
                ModelId::new("test.model")?,
                Some(ModelId::new("test.model")?),
                Some(ModelFamilyId::new("test.family")?),
                ModelAdapterProfileId::new("test.profile")?,
                CapabilitySet::new(features, BTreeMap::new()),
            ),
            observed,
        )?;
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[fixture.account])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential-authority"])?,
            fixture.generation,
            observed,
            Some(expires),
        )?;
        let model: Arc<dyn LanguageModel> = Arc::new(
            plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
                .with_runtime_authority(authority.clone()),
        );
        let configured = crate::test_model::configured_model(
            Arc::clone(&model),
            descriptor.clone(),
            authority,
            std::iter::empty(),
            LocalDurabilityDisposition::Ephemeral,
            OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("nonzero bound")?),
        )?;
        Ok(ResolvedProvider {
            model: ResolvedNativeModel {
                selected_model: descriptor.requested_model().clone(),
                descriptor,
                model,
                evidence: SubscriptionResolutionEvidence {
                    account_fingerprint: "fixture-account".to_owned(),
                    credential_authority_binding: "fixture-credential".to_owned(),
                    credential_generation: fixture.generation,
                    credential_selection_epoch: fixture.epoch.to_owned(),
                    route_fingerprint: fixture.route.to_owned(),
                    descriptor_digest: "fixture-descriptor".to_owned(),
                    dialect: "test.protocol".to_owned(),
                    adapter_revision: "fixture-adapter".to_owned(),
                    catalog_observed_at: observed,
                    catalog_expires_at: expires,
                    runtime_admission_witness: None,
                    profile_revision: "fixture-profile".to_owned(),
                    codex_exact: None,
                    xai_exact: None,
                },
                retrieval: None,
                image_gen: None,
            },
            configured,
            native_tool_mode: Some(fixture.tool_mode),
            reresolution: None,
        })
    }

    #[test]
    fn a_later_observation_of_the_same_login_and_model_is_admitted() -> TestResult {
        let previous = resolved(&BASE)?;
        let fresh = resolved(&Fixture {
            observed: 30_000,
            generation: 4,
            ..BASE
        })?;
        assert_ne!(
            previous.model.descriptor.binding_digest()?,
            fresh.model.descriptor.binding_digest()?,
            "the fresh descriptor is a new descriptor"
        );
        assert_eq!(
            previous.configuration().effective_identity(),
            fresh.configuration().effective_identity(),
            "observation and expiry stay out of the effective identity"
        );
        assert!(previous.admits_fresh(&fresh));
        Ok(())
    }

    #[test]
    fn a_new_login_another_account_a_changed_row_or_an_older_generation_is_refused() -> TestResult {
        let previous = resolved(&BASE)?;
        for (case, fresh) in [
            (
                "new login of the same account",
                Fixture {
                    epoch: "login-epoch-b",
                    ..BASE
                },
            ),
            (
                "another account",
                Fixture {
                    account: b"account-b",
                    ..BASE
                },
            ),
            (
                "changed route or catalog row",
                Fixture {
                    route: "route-fingerprint-b",
                    ..BASE
                },
            ),
            (
                "changed native tool mode",
                Fixture {
                    tool_mode: NativeToolMode::CodeMode,
                    ..BASE
                },
            ),
            (
                "older credential generation",
                Fixture {
                    generation: 2,
                    ..BASE
                },
            ),
        ] {
            assert!(
                !previous.admits_fresh(&resolved(&fresh)?),
                "{case} must fail closed"
            );
        }
        Ok(())
    }

    #[test]
    fn a_resolution_without_a_window_has_no_reresolution() -> TestResult {
        let provider = resolved(&BASE)?;
        assert_eq!(provider.admitted_until(), None);
        Ok(())
    }
}
