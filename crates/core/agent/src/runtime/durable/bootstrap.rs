//! Durable genesis, publication recovery and lease bootstrap.

use crate::recovery_scope::require_recoverable;
use crate::runtime::admission::SealedRuntimeAdmission;
use crate::runtime::build::{
    construction_tool_presentation, initial_memory_checkpoint, resolve_construction_harness_plan,
    stable_prefix_from_effective_configuration,
};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::checkpoint::owner::DurableBranchFacts;
use crate::runtime::control::run_environment;
use crate::runtime::durable::branch::session_operation_uuid;
use crate::runtime::durable::turn_recovery::validate_loaded_address;
use crate::runtime::durable::{
    DurableGenesisIdentities, DurableRuntimeOpen, DurableSessionAddress,
};
use crate::runtime::{
    AgentBuilder, DEFAULT_STORE_CLEANUP_TIMEOUT, DEFAULT_STORE_OPERATION_TIMEOUT,
    DurableRuntimeBootstrap, RuntimeInstanceNonce, StoreTimeouts,
};
use crate::tool_runtime::ToolRuntime;
use crate::{
    AgentError, AgentErrorKind, BranchLease, CheckpointId, CreateSession, DurableOperationKind,
    RuntimeCheckpoint, RuntimeDurability, SessionRef, SessionStore, SessionStoreError,
};
use plakous_model::{BranchId, CanonicalHistory, RunId, SessionId};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
impl Default for StoreTimeouts {
    fn default() -> Self {
        Self {
            operation: DEFAULT_STORE_OPERATION_TIMEOUT,
            cleanup: DEFAULT_STORE_CLEANUP_TIMEOUT,
        }
    }
}

impl RuntimeInstanceNonce {
    pub(in crate::runtime) fn generate() -> Result<Self, AgentError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "runtime instance identity randomness is unavailable",
            )
        })?;
        Ok(Self(nonce))
    }
}

impl Drop for RuntimeInstanceNonce {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl DurableSessionAddress {
    /// Creates an exact session, branch, and immutable-checkpoint address.
    #[must_use]
    pub const fn new(
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
    ) -> Self {
        Self {
            session_id,
            branch_id,
            checkpoint_id,
        }
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the independent branch identity.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }

    /// Returns the exact immutable checkpoint identity.
    #[must_use]
    pub const fn checkpoint_id(self) -> CheckpointId {
        self.checkpoint_id
    }

    pub(in crate::runtime) const fn session(self) -> SessionRef {
        SessionRef::new(self.session_id, self.branch_id)
    }
}

impl fmt::Debug for DurableRuntimeOpen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opened(agent) => formatter
                .debug_tuple("Opened")
                .field(&agent.checkpoint().manifest().topology().checkpoint_id)
                .finish(),
            Self::RestoredWorking { agent, turn } => formatter
                .debug_struct("RestoredWorking")
                .field("checkpoint", &agent.checkpoint().id())
                .field("turn_id", &turn.turn_id())
                .finish(),
            Self::RecoveryRequired { address, .. } => formatter
                .debug_struct("RecoveryRequired")
                .field("address", address)
                .finish(),
        }
    }
}

pub(super) fn checkpoint_has_code_cell_obligation(checkpoint: &RuntimeCheckpoint) -> bool {
    checkpoint
        .manifest()
        .lifecycle()
        .obligations()
        .iter()
        .any(|obligation| {
            checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| record.operation_id() == obligation.operation_id())
                .is_some_and(|record| {
                    matches!(
                        record.kind(),
                        DurableOperationKind::CompositionEvaluation { .. }
                            | DurableOperationKind::CompositionContinuation { .. }
                            | DurableOperationKind::CompositionChild { .. }
                    )
                })
        })
}

impl fmt::Debug for DurableRuntimeBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableRuntimeBootstrap")
            .field("head", &self.branch.head())
            .field("lease", &self.branch.lease())
            .field(
                "checkpoint",
                &self.checkpoint.manifest().topology().checkpoint_id,
            )
            .field("admission", &"<sealed>")
            .finish_non_exhaustive()
    }
}

impl DurableRuntimeBootstrap {
    /// Validates one exact loaded branch for ordinary durable turn execution.
    ///
    /// # Errors
    ///
    /// Returns a typed fail-closed error for topology, object-root,
    /// compatibility, revalidation, durability, or recovery-scope mismatch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact store/open authority tuple"
    )]
    pub(in crate::runtime) async fn open(
        store: Arc<dyn SessionStore>,
        lease: BranchLease,
        expected: DurableSessionAddress,
        durability: RuntimeDurability,
        admission: SealedRuntimeAdmission,
        store_timeouts: StoreTimeouts,
    ) -> Result<Self, AgentError> {
        let release_store = Arc::clone(&store);
        let release_lease = lease.clone();
        match Self::open_owned(
            store,
            lease,
            expected,
            durability,
            admission,
            store_timeouts,
        )
        .await
        {
            Ok(bootstrap) => Ok(bootstrap),
            Err(error) => {
                store_call(
                    StorePhase::Release,
                    store_timeouts.cleanup,
                    release_store.release(release_lease),
                )
                .await
                .map_err(StoreCallError::into_agent)?;
                Err(error)
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the private validator consumes the exact closed store/open authority tuple"
    )]
    pub(in crate::runtime) async fn open_owned(
        store: Arc<dyn SessionStore>,
        lease: BranchLease,
        expected: DurableSessionAddress,
        durability: RuntimeDurability,
        admission: SealedRuntimeAdmission,
        store_timeouts: StoreTimeouts,
    ) -> Result<Self, AgentError> {
        let loaded = store_call(
            StorePhase::LoadBranch,
            store_timeouts.operation,
            store.load_branch(lease.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        validate_loaded_address(&loaded, expected, durability)?;
        let head = loaded.head();
        let stored = loaded.checkpoint();
        let checkpoint = Arc::clone(stored.checkpoint());
        let reference = stored.reference();
        let topology = checkpoint.manifest().topology();
        if lease.session() != head.session()
            || reference != head.checkpoint()
            || topology.session_id != head.session().session_id()
            || topology.branch_id != head.session().branch_id()
            || topology.checkpoint_id != reference.checkpoint_id()
            || topology.branch_revision != head.revision().get()
            || checkpoint.integrity() != reference.integrity()
        {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable runtime bootstrap does not match the loaded branch authority",
            ));
        }
        let bundle = stored.bundle().clone();
        if bundle.root() != reference.root()
            || bundle.checkpoint_id() != reference.checkpoint_id()
            || bundle.checkpoint_integrity() != reference.integrity()
        {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable runtime bootstrap object root disagrees with the branch head",
            ));
        }
        require_recoverable(&checkpoint).map_err(|_| {
            AgentError::new(
                AgentErrorKind::IncompatibleSession,
                "durable checkpoint recovery scope is not recoverable by this build",
            )
        })?;
        Ok(Self {
            checkpoint,
            bundle,
            branch: DurableBranchFacts::new(store, head, lease, store_timeouts)
                .with_pruned_horizon(loaded.horizon()),
            admission: admission.fact,
        })
    }
}

impl DurableGenesisIdentities {
    pub(in crate::runtime) fn new(operation_id: &str) -> Result<Self, AgentError> {
        Ok(Self {
            session_id: SessionId::try_from(session_operation_uuid(operation_id, b"session")?)
                .map_err(|error| AgentError::from_identity(&error))?,
            branch_id: BranchId::try_from(session_operation_uuid(operation_id, b"branch")?)
                .map_err(|error| AgentError::from_identity(&error))?,
            run_id: RunId::try_from(session_operation_uuid(operation_id, b"create-run")?)
                .map_err(|error| AgentError::from_identity(&error))?,
            checkpoint_id: CheckpointId::try_from(session_operation_uuid(
                operation_id,
                b"genesis-checkpoint",
            )?)
            .map_err(|error| AgentError::from_identity(&error))?,
        })
    }
}

pub(in crate::runtime) fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(super) fn prepare_durable_genesis(
    builder: &AgentBuilder,
    identities: DurableGenesisIdentities,
    admission: SealedRuntimeAdmission,
    durability: RuntimeDurability,
    history: CanonicalHistory,
) -> Result<RuntimeCheckpoint, AgentError> {
    let environment = run_environment(
        builder.workspace.as_ref(),
        identities.run_id,
        builder.host_process_options.clone(),
        builder.job_config,
    )?;
    let tools = ToolRuntime::new(
        builder.tools.clone(),
        Arc::new(environment),
        Arc::clone(&builder.policy),
    );
    let (harness_plan, request_policy, instructions, exposure) =
        resolve_construction_harness_plan(builder, &tools)?;
    let stable_prefix = stable_prefix_from_effective_configuration(
        &builder.provider_configuration,
        &request_policy,
        instructions,
        &exposure,
        &harness_plan,
    )?;
    let presentation = construction_tool_presentation(builder, exposure)?;
    let stable_prefix = presentation.request_prefix(stable_prefix)?;
    initial_memory_checkpoint(
        identities.session_id,
        identities.branch_id,
        identities.checkpoint_id,
        &stable_prefix,
        &harness_plan,
        &builder.provider_configuration,
        builder.model.as_ref(),
        &tools,
        &presentation,
        builder.workspace.as_ref(),
        builder.local_composition.as_ref(),
        admission.fact.observed_at(),
        Some(admission.fact),
        durability,
        history,
        builder.job_counter_seed,
    )
}

pub(super) async fn recover_or_create(
    store: Arc<dyn SessionStore>,
    request: CreateSession,
    timeout: Duration,
) -> Result<crate::CreateReceipt, AgentError> {
    match store_call(
        StorePhase::RecoverCreate,
        timeout,
        store.recover_create(request.clone()),
    )
    .await
    {
        Ok(receipt) => return Ok(receipt),
        Err(error) if error.is(SessionStoreError::NotFound) => {}
        Err(error) if error.is(SessionStoreError::IdentityConflict) => {
            return recover_persisted_create(store, &request, timeout).await;
        }
        Err(error) => return Err(error.into_agent()),
    }
    match store_call(StorePhase::Create, timeout, store.create(request.clone())).await {
        Ok(receipt) => Ok(receipt),
        Err(error) if error.is_retryable() => {
            match store_call(
                StorePhase::RecoverCreate,
                timeout,
                store.recover_create(request.clone()),
            )
            .await
            {
                Ok(receipt) => Ok(receipt),
                Err(recovery) if recovery.is(SessionStoreError::IdentityConflict) => {
                    recover_persisted_create(store, &request, timeout).await
                }
                Err(recovery) if recovery.is(SessionStoreError::NotFound) => {
                    Err(error.into_ambiguous_agent())
                }
                Err(recovery) => Err(recovery.into_agent()),
            }
        }
        Err(error) if error.is(SessionStoreError::IdentityConflict) => {
            recover_persisted_create(store, &request, timeout).await
        }
        Err(error) => Err(error.into_agent()),
    }
}

async fn recover_persisted_create(
    store: Arc<dyn SessionStore>,
    request: &CreateSession,
    timeout: Duration,
) -> Result<crate::CreateReceipt, AgentError> {
    let loaded = store_call(
        StorePhase::LoadBranch,
        timeout,
        store.load_branch(request.session()),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    if loaded.session().initial_branch() != request.session().branch_id() {
        return Err(AgentError::from_store(SessionStoreError::IdentityConflict));
    }
    let stored = store_call(
        StorePhase::LoadCheckpoint,
        timeout,
        store.load_checkpoint(request.initial().checkpoint_id()),
    )
    .await
    .map_err(StoreCallError::into_agent)?;
    let bundle = stored.bundle().clone();
    let original = CreateSession::new(
        request.session(),
        loaded.session().created_at(),
        request.owner(),
        request.lease_seconds(),
        bundle,
    );
    store_call(
        StorePhase::RecoverCreate,
        timeout,
        store.recover_create(original),
    )
    .await
    .map_err(StoreCallError::into_agent)
}

pub(super) async fn release_store_lease(
    store: Arc<dyn SessionStore>,
    lease: BranchLease,
    timeout: Duration,
) -> Result<(), AgentError> {
    for attempt in 0..2 {
        match store_call(StorePhase::Release, timeout, store.release(lease.clone())).await {
            Ok(()) => return Ok(()),
            Err(error)
                if error.is(SessionStoreError::LeaseExpired)
                    || error.is(SessionStoreError::FenceSuperseded) =>
            {
                return Ok(());
            }
            Err(error) if attempt == 0 && error.is_retryable() => {}
            Err(error) => return Err(error.into_agent()),
        }
    }
    Err(AgentError::new(
        AgentErrorKind::Internal,
        "session lease cleanup exhausted its bounded retry",
    ))
}
