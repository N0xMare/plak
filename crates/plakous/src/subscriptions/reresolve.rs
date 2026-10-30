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
        if evidence.exact_provider.is_some() {
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
            && previous_evidence.exact_provider == fresh_evidence.exact_provider
            && self.native_tool_mode == fresh.native_tool_mode
            && self.model().content_filter_retry_instruction()
                == fresh.model().content_filter_retry_instruction()
    }
}

#[cfg(all(test, feature = "codex", feature = "grok", feature = "claude"))]
pub(super) mod tests;
