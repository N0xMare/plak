//! Create, open and shut down exact durable sessions.

use crate::runtime::admission::sealed_runtime_admission;
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::continuation::{
    RestoredContinuationFrame, checkpoint_bundle_error, decode_restored_continuation,
};
use crate::runtime::durable::bootstrap::{
    self, prepare_durable_genesis, recover_or_create, release_store_lease,
};
use crate::runtime::durable::branch::{
    operation_run_id, validate_durable_builder, validate_session_operation_id,
};
use crate::runtime::durable::turn_recovery::{
    checkpoint_requires_closed_recovery, reopen_closes_working_turn, validate_loaded_address,
};
use crate::runtime::durable::{
    DurableGenesisIdentities, DurableRuntimeOpen, DurableSessionAddress, DurableSessionRuntime,
};
use crate::runtime::{Agent, AgentBuilder, DurableRuntimeBootstrap};
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, CreateSession, RuntimeCheckpointCodec, SessionRef,
};
use plakous_model::CanonicalHistory;
use std::sync::Arc;
impl DurableSessionRuntime {
    /// Pure deterministic create address derivation for facade composition.
    ///
    /// # Errors
    /// Returns a typed invalid operation identity failure.
    #[doc(hidden)]
    pub fn genesis_address_for(operation_id: &str) -> Result<DurableSessionAddress, AgentError> {
        validate_session_operation_id(operation_id)?;
        let identities = DurableGenesisIdentities::new(operation_id)?;
        Ok(DurableSessionAddress::new(
            identities.session_id,
            identities.branch_id,
            identities.checkpoint_id,
        ))
    }

    /// Authenticates an accepted deterministic create before rebuilding its runtime.
    /// This is a read-only inspection, not authority to open a mutable head.
    ///
    /// # Errors
    /// Returns store or identity failures; only an absent session means unaccepted.
    pub async fn accepted_create(
        &self,
        operation_id: &str,
    ) -> Result<Option<DurableSessionAddress>, AgentError> {
        validate_session_operation_id(operation_id)?;
        let identities = DurableGenesisIdentities::new(operation_id)?;
        let session = SessionRef::new(identities.session_id, identities.branch_id);
        if bootstrap::recover_persisted_create(
            Arc::clone(&self.store),
            session,
            identities.run_id,
            self.lease_seconds,
            identities.checkpoint_id,
            self.store_timeouts.operation,
        )
        .await?
        .is_none()
        {
            return Ok(None);
        }
        Ok(Some(DurableSessionAddress::new(
            identities.session_id,
            identities.branch_id,
            identities.checkpoint_id,
        )))
    }

    /// Atomically creates an exact genesis checkpoint and attaches its driver.
    ///
    /// The bounded operation identity deterministically selects the session,
    /// run, and checkpoint identities. Exact repetition recovers the
    /// accepted create receipt; changed model/auth/tools/environment/policy
    /// facts conflict instead of selecting defaults.
    ///
    /// # Errors
    ///
    /// Returns a typed validation, fresh-authority, identity, store, or driver
    /// construction error. Any acquired lease is released on validation/build
    /// failure.
    pub async fn create(
        &self,
        operation_id: &str,
        builder: AgentBuilder,
    ) -> Result<Agent, AgentError> {
        Box::pin(self.create_seeded(operation_id, builder, CanonicalHistory::default())).await
    }

    /// Like [`Self::create`], with `history` as the genesis checkpoint's
    /// committed conversation instead of an empty one.
    ///
    /// Nothing in `history` is replayed, re-executed, or charged. Provider
    /// replay material it carries must already suit `builder`'s account and
    /// model.
    ///
    /// # Errors
    ///
    /// As [`Self::create`], plus a validation error when `history` is not a
    /// valid committed conversation.
    pub async fn create_seeded(
        &self,
        operation_id: &str,
        builder: AgentBuilder,
        history: CanonicalHistory,
    ) -> Result<Agent, AgentError> {
        validate_session_operation_id(operation_id)?;
        validate_durable_builder(&builder)?;
        let admission = sealed_runtime_admission(&builder).await?;
        let identities = DurableGenesisIdentities::new(operation_id)?;
        let checkpoint = prepare_durable_genesis(
            &builder,
            identities,
            admission.clone(),
            self.durability,
            history,
        )?;
        let bundle =
            RuntimeCheckpointCodec::bundle(&checkpoint, None).map_err(checkpoint_bundle_error)?;
        let request = CreateSession::new(
            SessionRef::new(identities.session_id, identities.branch_id),
            admission.fact.observed_at(),
            identities.run_id,
            self.lease_seconds,
            bundle,
        )
        .with_feed_facts(crate::feed::facts::genesis_facts(operation_id, &checkpoint));
        let receipt = recover_or_create(
            Arc::clone(&self.store),
            request,
            self.store_timeouts.operation,
        )
        .await?;
        release_store_lease(
            Arc::clone(&self.store),
            receipt.lease().clone(),
            self.store_timeouts.operation,
        )
        .await?;
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                receipt.branch().session(),
                owner,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let address = DurableSessionAddress::new(
            identities.session_id,
            identities.branch_id,
            identities.checkpoint_id,
        );
        let bootstrap = DurableRuntimeBootstrap::open(
            Arc::clone(&self.store),
            lease,
            address,
            self.durability,
            admission,
            self.store_timeouts,
        )
        .await?;
        Box::pin(builder.build_durable(bootstrap)).await
    }

    /// Opens only the exact named checkpoint or returns recovery-only state.
    ///
    /// # Errors
    ///
    /// Returns a typed stale-address, fresh-authority, compatibility, lease,
    /// store, or driver-construction error. It never opens the latest head by
    /// substitution.
    pub async fn open(
        &self,
        operation_id: &str,
        address: DurableSessionAddress,
        builder: AgentBuilder,
    ) -> Result<DurableRuntimeOpen, AgentError> {
        validate_session_operation_id(operation_id)?;
        validate_durable_builder(&builder)?;
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(address.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        validate_loaded_address(&loaded, address, self.durability)?;
        // An unfinished turn this open closes itself is not recovery-only:
        // opening it is how it recovers (spec 01 §13.1). Everything else
        // outstanding still is.
        let abandonable = reopen_closes_working_turn(loaded.checkpoint().checkpoint());
        let restored = decode_restored_continuation(loaded.checkpoint().checkpoint());
        if checkpoint_requires_closed_recovery(loaded.checkpoint().checkpoint())
            && !abandonable
            && restored.is_err()
        {
            return Ok(DurableRuntimeOpen::RecoveryRequired {
                address,
                retry_after_seconds: None,
            });
        }
        if let Ok(frame) = restored
            && matches!(
                &frame,
                RestoredContinuationFrame::EffectDecision(effect)
                    if builder.clock.now() >= effect.decision_expires_at
            )
        {
            return Ok(DurableRuntimeOpen::RecoveryRequired {
                address,
                retry_after_seconds: None,
            });
        }
        let admission = sealed_runtime_admission(&builder).await?;
        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let lease = match store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                address.session(),
                owner,
                self.lease_seconds,
            )),
        )
        .await
        {
            Ok(lease) => lease,
            // The branch is still leased, which after a process loss means the
            // dead holder's lease has not expired yet. Abandoning its turn is
            // this open's to do once it can hold the branch, and until then the
            // session is recovery-only exactly as it was before this open
            // learned it could abandon anything. Every other acquisition
            // failure keeps its own error.
            Err(error) if abandonable && error.is(crate::SessionStoreError::LeaseHeld) => {
                return Ok(DurableRuntimeOpen::RecoveryRequired {
                    address,
                    // The holder renews for at most this long, so a lease left
                    // behind by a dead process is gone by then at the latest.
                    retry_after_seconds: Some(self.lease_seconds.get()),
                });
            }
            Err(error) => return Err(error.into_agent()),
        };
        let bootstrap = DurableRuntimeBootstrap::open(
            Arc::clone(&self.store),
            lease,
            address,
            self.durability,
            admission,
            self.store_timeouts,
        )
        .await?;
        let agent = Box::pin(builder.build_durable(bootstrap)).await?;
        if let Some(turn) = agent.recovered_turn() {
            return Ok(DurableRuntimeOpen::RestoredWorking { agent, turn });
        }
        if checkpoint_requires_closed_recovery(&agent.checkpoint()) {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "restored working runtime omitted its original turn capability",
            ));
        }
        Ok(DurableRuntimeOpen::Opened(agent))
    }

    /// Stops the selected semantic backend under the runtime-owned cleanup bound.
    ///
    /// Callers must first join every agent using this backend. Repeated calls
    /// retain the store contract's exact terminal-barrier semantics.
    ///
    /// # Errors
    ///
    /// Returns a typed redaction-safe store or bounded-cleanup failure.
    #[doc(hidden)]
    pub async fn shutdown(self) -> Result<(), AgentError> {
        let report = store_call(
            StorePhase::Shutdown,
            self.store_timeouts.cleanup,
            self.store.shutdown(),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        if report.is_complete() {
            Ok(())
        } else {
            Err(AgentError::new(
                AgentErrorKind::Session,
                "durable store shutdown retained unresolved checkpoint debt",
            ))
        }
    }
}
