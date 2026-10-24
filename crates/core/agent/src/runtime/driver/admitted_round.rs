//! The next round's request a tool exchange's capacity admission prepared.

use crate::AgentError;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::driver::Driver;
use plakous_model::{
    ActiveFamilyArtifact, AttemptId, CompiledProviderConfiguration, ContextRequestAnchor,
    ModelCacheAffinity, PreparedModelCall, ResolvedModelDescriptor, Timestamp, TurnId,
    ValidatedRequestPlan,
};
use std::sync::{Arc, Mutex, PoisonError};

/// A round request the capacity admission validated and prepared, with what
/// its preparation read. The round reuses it only when every one of those is
/// still what the round would read: otherwise it prepares afresh.
pub(super) struct AdmittedRound {
    pub(super) turn_id: TurnId,
    pub(super) attempt_id: AttemptId,
    pub(super) plan: ValidatedRequestPlan,
    /// The sole dispatch capability; it is `Send` but not `Sync`, and the
    /// driver is shared by reference across awaits.
    pub(super) prepared: Mutex<PreparedModelCall>,
    pub(super) prepared_at: Timestamp,
    pub(super) provider_configuration: Arc<CompiledProviderConfiguration>,
    pub(super) operation_count: usize,
    pub(super) request_anchor: Option<ContextRequestAnchor>,
    pub(super) family_artifact: Option<ActiveFamilyArtifact>,
    pub(super) cache_affinity: ModelCacheAffinity,
}

impl Driver {
    /// The admitted round request, when the round about to start would
    /// prepare exactly it: the same turn and stable prefix (no wrap-up), the
    /// same conversation (no fold, steer, notice or note since), the same
    /// provider configuration, ledger length and request anchor (no fold
    /// attempt or reresolution since), and the same artifact and cache
    /// affinity. Anything else prepares the round afresh.
    pub(super) fn take_admitted_round(
        &mut self,
        turn_id: TurnId,
        tools_disabled: bool,
    ) -> Result<Option<Box<AdmittedRound>>, AgentError> {
        let Some(round) = self.admitted_round.take() else {
            return Ok(None);
        };
        let checkpoint = self.authority.checkpoint();
        let request = round.plan.request();
        let current = round.turn_id == turn_id
            && !tools_disabled
            && request.stable_prefix().identity() == self.stable_request_prefix.identity()
            && request.input().identity() == self.conversation().identity()
            && Arc::ptr_eq(&round.provider_configuration, &self.provider_configuration)
            && round.operation_count == checkpoint.manifest().ledger().operation_count()
            && round.request_anchor == self.request_anchor;
        if !current {
            return Ok(None);
        }
        let (family_artifact, cache_affinity) = self.active_model_preparation_state()?;
        Ok(
            (round.family_artifact == family_artifact && round.cache_affinity == cache_affinity)
                .then_some(round),
        )
    }

    /// Whether the runtime authority the admission prepared under still
    /// authorizes its preparation, checked once the round holds its request
    /// slot. The window since the admission spans the exchange's promotion,
    /// fence (b), steering and notices, reresolution and the slot queue; a
    /// credential refresh, an account switch or an expiry in it fails the
    /// dispatch-time check of the admitted preparation, while a preparation
    /// under the current authority dispatches.
    pub(super) async fn admitted_round_authorized(
        &self,
        descriptor: &ResolvedModelDescriptor,
        round: &AdmittedRound,
    ) -> bool {
        let now = self.clock.now();
        let Ok(authority) = Arc::clone(&self.model)
            .revalidate_runtime_authority(now)
            .await
        else {
            return false;
        };
        if validate_configuration_runtime_authority(
            &self.provider_configuration,
            descriptor,
            &authority,
            now,
        )
        .is_err()
        {
            return false;
        }
        let prepared = round
            .prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let record = prepared.record();
        record.revalidate(descriptor, &round.plan, now).is_ok()
            && record
                .revalidate_selected_authority(&authority, descriptor, now)
                .is_ok()
    }
}
