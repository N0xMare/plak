//! Exact branch and rebind preparation.

use crate::recovery_scope::require_recoverable;
use crate::runtime::admission::{recorded_admission_authority, sealed_runtime_admission};
use crate::runtime::build::{
    caller_instructions_match, construction_tool_presentation, harness_plan_component,
    resolve_construction_harness_plan, runtime_manifest_binding_and_tools,
    stable_prefix_from_effective_configuration, stable_workspace_environment_bytes,
    tool_registry_descriptor,
};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::continuation::{
    checkpoint_bundle_error, runtime_manifest_error, tool_descriptor_error,
};
use crate::runtime::control::run_environment;
use crate::runtime::durable::bootstrap::release_store_lease;
use crate::runtime::durable::turn_recovery::{
    checkpoint_requires_closed_recovery, validate_historical_address, validate_loaded_address,
};
use crate::runtime::durable::{
    DurableBranchMaterial, DurableBranchPublication, DurableBranchRequest,
    DurableBranchRequestIdentity, DurableRebindRequest, DurableRebindToolTransition,
    DurableRuntimeBranchOutcome, DurableSessionAddress, DurableSessionRuntime,
    DurableTargetSelectors, PreparedRebindTarget,
};
use crate::runtime::{AgentBuilder, RuntimeInstanceNonce};
use crate::tool_runtime::ToolRuntime;
use crate::{
    AcquireBranch, AgentError, AgentErrorKind, BranchHead, BranchLease, BranchTransitionMode,
    CheckpointId, ForkBranch, ForkLineage, ForkReceipt, ModelRouteBinding, ProducerProvenance,
    RecoverFork, RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec,
    RuntimeLifecycle, RuntimeManifest, RuntimeTopology, SessionStoreError, SessionWorkflowId,
    StoredCheckpoint,
};
use plakous_model::{
    BranchId, Digest, DigestDomain, EffectiveProviderConfigurationId, ModelId, RunId,
    SelectedProviderConfigurationId,
};
use plakous_tools::{AlwaysAllowPolicy, ReadOnlyToolPolicy, ToolPolicy};
use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;
impl DurableSessionRuntime {
    /// Creates or exactly recovers one source-preserving ordinary fork.
    #[doc(hidden)]
    pub async fn ordinary_fork(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
    ) -> Result<DurableRuntimeBranchOutcome, AgentError> {
        self.branch(operation_id, source, DurableBranchRequest::OrdinaryFork)
            .await
    }

    /// Creates or exactly recovers one fork that keeps the source's history
    /// and binding and recompiles everything else from `builder`, instruction
    /// sources included. An unchanged compilation is an exact ordinary fork;
    /// a changed one is a rebind onto the source's own target.
    #[doc(hidden)]
    pub async fn recompiling_fork(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        builder: AgentBuilder,
    ) -> Result<DurableRuntimeBranchOutcome, AgentError> {
        self.branch(
            operation_id,
            source,
            DurableBranchRequest::RecompilingFork(Box::new(builder)),
        )
        .await
    }

    /// Creates or exactly recovers one explicitly selected, freshly admitted rebind.
    #[doc(hidden)]
    #[allow(
        clippy::too_many_arguments,
        reason = "selectors and sealed builder are distinct rebind authority"
    )]
    pub async fn rebind(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        protocol: plakous_model::ProtocolId,
        route: plakous_model::RouteId,
        model: ModelId,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        account_binding: Digest,
        recompile_target_tools: bool,
        builder: AgentBuilder,
    ) -> Result<DurableRuntimeBranchOutcome, AgentError> {
        self.branch(
            operation_id,
            source,
            DurableBranchRequest::Rebind(Box::new(DurableRebindRequest {
                target: DurableTargetSelectors {
                    protocol,
                    route,
                    model,
                    selected_provider_configuration,
                    effective_provider_configuration,
                    account_binding,
                    tool_transition: if recompile_target_tools {
                        DurableRebindToolTransition::RecompileForTarget
                    } else {
                        DurableRebindToolTransition::PreserveExact
                    },
                },
                builder,
            })),
        )
        .await
    }

    pub(super) async fn branch(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        request: DurableBranchRequest,
    ) -> Result<DurableRuntimeBranchOutcome, AgentError> {
        validate_session_operation_id(operation_id)?;
        let historical = store_call(
            StorePhase::LoadCheckpoint,
            self.store_timeouts.operation,
            self.store.load_checkpoint(source.checkpoint_id()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let historical_source = validate_historical_address(&historical, source, self.durability)?;
        let historical_checkpoint = historical.checkpoint();
        if checkpoint_requires_closed_recovery(historical_checkpoint) {
            return Ok(DurableRuntimeBranchOutcome::RecoveryRequired { address: source });
        }
        let target = request.publication_target(historical_checkpoint)?;

        let owner = operation_run_id(&self.instance_nonce, operation_id)?;
        let source_lease = store_call(
            StorePhase::Acquire,
            self.store_timeouts.operation,
            self.store.acquire(AcquireBranch::new(
                source.session(),
                owner,
                self.lease_seconds,
            )),
        )
        .await
        .map_err(StoreCallError::into_agent)?;

        let receipt = self
            .branch_receipt(
                operation_id,
                source,
                historical_source,
                &historical,
                &source_lease,
                request,
            )
            .await;
        let target_lease = receipt.as_ref().ok().map(|receipt| receipt.lease().clone());
        let publication = match receipt {
            Ok(receipt) => {
                self.branch_publication(source, &receipt, target.as_ref())
                    .await
            }
            Err(error) => Err(error),
        };
        let target_cleanup = match target_lease {
            Some(lease) => {
                release_store_lease(Arc::clone(&self.store), lease, self.store_timeouts.cleanup)
                    .await
            }
            None => Ok(()),
        };
        let source_cleanup = release_store_lease(
            Arc::clone(&self.store),
            source_lease,
            self.store_timeouts.cleanup,
        )
        .await;
        // A failed branch keeps its own cause; a release failure is reported
        // only for an otherwise published branch.
        let publication = publication?;
        target_cleanup?;
        source_cleanup?;
        Ok(DurableRuntimeBranchOutcome::Published(Box::new(
            publication,
        )))
    }

    /// Recovers the receipt published under one exact request identity.
    async fn recover_branch_receipt(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        historical_source: BranchHead,
        source_lease: &BranchLease,
        transition: BranchTransitionMode,
        selectors: Option<&DurableTargetSelectors>,
    ) -> Result<Result<ForkReceipt, StoreCallError>, AgentError> {
        let identity =
            DurableBranchRequestIdentity::new(operation_id, source, transition, selectors)?;
        let recovery = RecoverFork::new(
            identity.workflow_id,
            identity.semantic_request,
            source_lease.clone(),
            historical_source,
            identity.target_branch,
        )
        .with_transition(transition);
        Ok(store_call(
            StorePhase::RecoverFork,
            self.store_timeouts.operation,
            self.store.recover_fork(recovery),
        )
        .await)
    }

    pub(super) async fn branch_receipt(
        &self,
        operation_id: &str,
        source: DurableSessionAddress,
        historical_source: BranchHead,
        historical: &StoredCheckpoint,
        source_lease: &BranchLease,
        request: DurableBranchRequest,
    ) -> Result<ForkReceipt, AgentError> {
        let source_checkpoint = historical.checkpoint();

        // A recompiling fork publishes as an ordinary fork when its
        // compilation is the source's exactly, otherwise as a rebind onto the
        // source's own target, so either receipt may already exist. Every
        // candidate shares one workflow id, so another request under this
        // operation conflicts with all of them.
        let candidates = request.receipt_candidates(source_checkpoint)?;
        let mut conflict = None;
        for (transition, selectors) in &candidates {
            match self
                .recover_branch_receipt(
                    operation_id,
                    source,
                    historical_source,
                    source_lease,
                    *transition,
                    selectors.as_ref(),
                )
                .await?
            {
                Ok(receipt) => return Ok(receipt),
                Err(error) if error.is(SessionStoreError::NotFound) => {}
                Err(error) if error.is(SessionStoreError::IdentityConflict) => {
                    conflict = Some(error);
                }
                Err(error) => return Err(error.into_agent()),
            }
        }
        if let Some(conflict) = conflict {
            return Err(conflict.into_agent());
        }

        let (transition, target_selectors, prepared_target) = match request {
            DurableBranchRequest::OrdinaryFork => (BranchTransitionMode::OrdinaryFork, None, None),
            DurableBranchRequest::RecompilingFork(builder) => {
                let own = source_target_selectors(source_checkpoint)?;
                match Box::pin(prepare_fork_target(
                    source_checkpoint,
                    *builder,
                    own.clone(),
                    source_lease.owner(),
                ))
                .await?
                {
                    None => (BranchTransitionMode::OrdinaryFork, None, None),
                    Some(target) => (BranchTransitionMode::Rebind, Some(own), Some(target)),
                }
            }
            DurableBranchRequest::Rebind(request) => {
                let selectors = request.target.clone();
                let target = Box::pin(prepare_rebind_target(
                    source_checkpoint,
                    *request,
                    source_lease.owner(),
                ))
                .await?;
                (BranchTransitionMode::Rebind, Some(selectors), Some(target))
            }
        };
        let identity = DurableBranchRequestIdentity::new(
            operation_id,
            source,
            transition,
            target_selectors.as_ref(),
        )?;
        let recovery = RecoverFork::new(
            identity.workflow_id,
            identity.semantic_request,
            source_lease.clone(),
            historical_source,
            identity.target_branch,
        )
        .with_transition(transition);
        // The chosen transition and selectors are one of the candidates
        // probed above, and the held source lease keeps anyone else from
        // publishing this workflow since: `recovery` serves only an
        // ambiguous fork acknowledgement below.
        let loaded = store_call(
            StorePhase::LoadBranch,
            self.store_timeouts.operation,
            self.store.load_branch(source.session()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        validate_loaded_address(&loaded, source, self.durability)?;
        if loaded.head() != historical_source {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "fork source changed before exact publication",
            ));
        }

        let material = DurableBranchMaterial::new(operation_id)?;
        let target_checkpoint = prepare_branch_checkpoint(
            source_checkpoint,
            &identity,
            material,
            transition,
            prepared_target,
        )?;
        let target_bundle =
            RuntimeCheckpointCodec::advance(&target_checkpoint, loaded.checkpoint().bundle())
                .map_err(checkpoint_bundle_error)?;
        let fork = ForkBranch::new(
            identity.workflow_id,
            identity.semantic_request,
            source_lease.clone(),
            loaded.session().revision(),
            loaded.head(),
            identity.target_branch,
            source_lease.owner(),
            self.lease_seconds,
            target_bundle,
        )
        .with_transition(transition)
        .with_feed_facts(crate::feed::facts::branch_facts(
            identity.workflow_id,
            operation_id,
            transition,
            &target_checkpoint,
        ));
        match store_call(
            StorePhase::Fork,
            self.store_timeouts.operation,
            self.store.fork(fork),
        )
        .await
        {
            Ok(receipt) => Ok(receipt),
            Err(error) if error.is_retryable() || error.is(SessionStoreError::IdentityConflict) => {
                match store_call(
                    StorePhase::RecoverFork,
                    self.store_timeouts.operation,
                    self.store.recover_fork(recovery),
                )
                .await
                {
                    Ok(receipt) => Ok(receipt),
                    Err(recovered) if recovered.is(SessionStoreError::NotFound) => {
                        Err(error.into_ambiguous_agent())
                    }
                    Err(recovered) => Err(recovered.into_agent()),
                }
            }
            Err(error) => Err(error.into_agent()),
        }
    }

    pub(super) async fn branch_publication(
        &self,
        source: DurableSessionAddress,
        receipt: &ForkReceipt,
        target: Option<&DurableTargetSelectors>,
    ) -> Result<DurableBranchPublication, AgentError> {
        let stored = store_call(
            StorePhase::LoadCheckpoint,
            self.store_timeouts.operation,
            self.store
                .load_checkpoint(receipt.operation().checkpoint().checkpoint_id()),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        if stored.reference() != receipt.operation().checkpoint()
            || receipt.source().session() != source.session()
            || receipt.source().checkpoint().checkpoint_id() != source.checkpoint_id()
            || receipt.target().checkpoint() != stored.reference()
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let checkpoint = stored.checkpoint();
        require_recoverable(checkpoint).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "fork target scope is not recoverable by this build",
            )
        })?;
        if target.is_some_and(|target| {
            let binding = checkpoint.manifest().binding();
            binding.protocol() != &target.protocol
                || binding.route() != &target.route
                || binding.requested_model() != &target.model
        }) {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let target = DurableSessionAddress::new(
            receipt.target().session().session_id(),
            receipt.target().session().branch_id(),
            receipt.target().checkpoint().checkpoint_id(),
        );
        Ok(DurableBranchPublication {
            source,
            target,
            binding: checkpoint.manifest().binding().clone(),
            feed_seq: Some(receipt.feed_seq()),
        })
    }
}

impl fmt::Debug for DurableRuntimeBranchOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Published(publication) => formatter
                .debug_tuple("Published")
                .field(publication)
                .finish(),
            Self::RecoveryRequired { address } => formatter
                .debug_struct("RecoveryRequired")
                .field("address", address)
                .finish(),
        }
    }
}

impl DurableBranchPublication {
    /// Returns the sequence of the `Branched` record that published the
    /// target, when the store keeps a feed.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<crate::feed::FeedSeq> {
        self.feed_seq
    }

    /// Returns the unchanged exact source.
    #[must_use]
    pub const fn source(&self) -> DurableSessionAddress {
        self.source
    }

    /// Returns the exact originally published target checkpoint.
    #[must_use]
    pub const fn target(&self) -> DurableSessionAddress {
        self.target
    }

    /// Returns the complete target model/route/account binding.
    #[must_use]
    pub const fn binding(&self) -> &ModelRouteBinding {
        &self.binding
    }
}

impl DurableBranchRequest {
    /// Every request identity this request may have published under. A
    /// recompiling fork's rebind identity comes from its source alone, so a
    /// lookup without a runtime derives the same one.
    fn receipt_candidates(
        &self,
        source: &RuntimeCheckpoint,
    ) -> Result<Vec<(BranchTransitionMode, Option<DurableTargetSelectors>)>, AgentError> {
        Ok(match self {
            Self::OrdinaryFork => vec![(BranchTransitionMode::OrdinaryFork, None)],
            Self::RecompilingFork(_) => vec![
                (BranchTransitionMode::OrdinaryFork, None),
                (
                    BranchTransitionMode::Rebind,
                    Some(source_target_selectors(source)?),
                ),
            ],
            Self::Rebind(request) => {
                vec![(BranchTransitionMode::Rebind, Some(request.target.clone()))]
            }
        })
    }

    /// The target a published receipt's manifest must name.
    fn publication_target(
        &self,
        source: &RuntimeCheckpoint,
    ) -> Result<Option<DurableTargetSelectors>, AgentError> {
        match self {
            Self::OrdinaryFork => Ok(None),
            Self::RecompilingFork(_) => source_target_selectors(source).map(Some),
            Self::Rebind(request) => Ok(Some(request.target.clone())),
        }
    }
}

impl DurableBranchRequestIdentity {
    pub(in crate::runtime) fn new(
        operation_id: &str,
        source: DurableSessionAddress,
        transition: BranchTransitionMode,
        target: Option<&DurableTargetSelectors>,
    ) -> Result<Self, AgentError> {
        let workflow = Digest::sha256(
            DigestDomain::ProviderOperation,
            5,
            &[
                b"plakous-agent/session-branch-workflow-v1",
                operation_id.as_bytes(),
            ],
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Identity, "fork workflow identity failed"))?;
        let workflow_id = SessionWorkflowId::new(*workflow.bytes()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "fork workflow identity was reserved",
            )
        })?;
        let transition_label = match transition {
            BranchTransitionMode::OrdinaryFork => b"ordinary-fork".as_slice(),
            BranchTransitionMode::Rebind => b"rebind".as_slice(),
        };
        let target_protocol = target.map_or(&[][..], |target| target.protocol.as_str().as_bytes());
        let target_route = target.map_or(&[][..], |target| target.route.as_str().as_bytes());
        let target_model = target.map_or(&[][..], |target| target.model.as_str().as_bytes());
        let selected_digest = target.map(|target| target.selected_provider_configuration.digest());
        let effective_digest =
            target.map(|target| target.effective_provider_configuration.digest());
        let target_selected = selected_digest
            .as_ref()
            .map_or(&[][..], |digest| digest.bytes().as_slice());
        let target_effective = effective_digest
            .as_ref()
            .map_or(&[][..], |digest| digest.bytes().as_slice());
        let target_account = target.map_or(&[][..], |target| target.account_binding.bytes());
        let target_tool_transition =
            target.map_or(&[][..], |target| match target.tool_transition {
                DurableRebindToolTransition::PreserveExact => b"preserve-exact".as_slice(),
                DurableRebindToolTransition::RecompileForTarget => {
                    b"recompile-for-target".as_slice()
                }
            });
        let semantic_request = Digest::sha256(
            DigestDomain::RequestPlan,
            6,
            &[
                b"plakous-agent/session-branch-semantic-request-v6",
                operation_id.as_bytes(),
                source.session_id.as_uuid().as_bytes(),
                source.branch_id.as_uuid().as_bytes(),
                source.checkpoint_id.as_uuid().as_bytes(),
                transition_label,
                target_protocol,
                target_route,
                target_model,
                target_selected,
                target_effective,
                target_account,
                target_tool_transition,
            ],
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Identity, "fork request identity failed"))?;
        Ok(Self {
            workflow_id,
            target_branch: BranchId::try_from(session_operation_uuid(
                operation_id,
                b"target-branch",
            )?)
            .map_err(|error| AgentError::from_identity(&error))?,
            semantic_request,
        })
    }
}

impl DurableBranchMaterial {
    pub(in crate::runtime) fn new(operation_id: &str) -> Result<Self, AgentError> {
        let entropy = Digest::sha256(
            DigestDomain::CacheAffinity,
            2,
            &[
                b"plakous-agent/session-branch-context-entropy-v1",
                operation_id.as_bytes(),
            ],
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Identity, "fork context identity failed"))?;
        let checkpoint_id =
            CheckpointId::try_from(session_operation_uuid(operation_id, b"target-checkpoint")?)
                .map_err(|error| AgentError::from_identity(&error))?;
        let commit_identity = Digest::sha256(
            DigestDomain::ProviderOperation,
            5,
            &[
                b"plakous-agent/session-branch-checkpoint-commit-v1",
                operation_id.as_bytes(),
                checkpoint_id.as_uuid().as_bytes(),
            ],
        )
        .map_err(|_| AgentError::new(AgentErrorKind::Identity, "fork commit identity failed"))?;
        Ok(Self {
            checkpoint_id,
            context_entropy: *entropy.bytes(),
            commit_identity,
        })
    }
}

const INSTRUCTION_SOURCE_AUTHORITY: &str = "instruction-source authority";

/// A branch target compiled from a builder, with how it relates to its source.
struct CompiledBranchTarget {
    target: PreparedRebindTarget,
    /// Same model binding, account and provider configuration as the source.
    same_binding: bool,
    /// Byte for byte the source's runtime configuration.
    unchanged: bool,
}

async fn prepare_rebind_target(
    source: &RuntimeCheckpoint,
    request: DurableRebindRequest,
    run_id: RunId,
) -> Result<PreparedRebindTarget, AgentError> {
    let compiled = compile_branch_target(source, request, run_id, "rebind").await?;
    if compiled.unchanged {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "rebind target does not change the exact runtime configuration",
        ));
    }
    Ok(compiled.target)
}

/// The builder's own target, recompiled for it.
fn own_target_selectors(builder: &AgentBuilder) -> DurableTargetSelectors {
    let descriptor = builder.provider_configuration.effective().descriptor();
    DurableTargetSelectors {
        protocol: descriptor.protocol().clone(),
        route: descriptor.route().clone(),
        model: descriptor.requested_model().clone(),
        selected_provider_configuration: builder.provider_configuration.selected_identity(),
        effective_provider_configuration: builder.provider_configuration.effective_identity(),
        account_binding: builder.initial_model_authority.account_binding(),
        tool_transition: DurableRebindToolTransition::RecompileForTarget,
    }
}

/// A fork lands on the latest compilation of its source's own target: the
/// builder's current instruction sources and presentation, the source's
/// history and binding. `None` when that compilation is the source's exactly;
/// otherwise the store records the change as a rebind onto that same target,
/// because an ordinary fork never changes the runtime configuration.
async fn prepare_fork_target(
    source: &RuntimeCheckpoint,
    builder: AgentBuilder,
    source_target: DurableTargetSelectors,
    run_id: RunId,
) -> Result<Option<PreparedRebindTarget>, AgentError> {
    let other_target = || {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "a fork keeps its source's model, account and provider configuration; \
             another target is a rebind",
        )
    };
    let target = own_target_selectors(&builder);
    // The provider configuration identities also cover the tool definitions,
    // so they are compared only after the compilation has named a tool change.
    if target.protocol != source_target.protocol
        || target.route != source_target.route
        || target.model != source_target.model
        || target.account_binding != source_target.account_binding
    {
        return Err(other_target());
    }
    let compiled = compile_branch_target(
        source,
        DurableRebindRequest {
            target: target.clone(),
            builder,
        },
        run_id,
        "fork",
    )
    .await?;
    if !compiled.same_binding || target != source_target {
        return Err(other_target());
    }
    Ok((!compiled.unchanged).then_some(compiled.target))
}

/// The source's own target, read from its checkpoint manifest alone.
fn source_target_selectors(
    source: &RuntimeCheckpoint,
) -> Result<DurableTargetSelectors, AgentError> {
    let manifest = source.manifest();
    let binding = manifest.binding();
    let configuration = manifest.provider_configuration();
    let admission =
        recorded_admission_authority(binding.admission(), configuration).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "fork source admission evidence does not name its account",
            )
        })?;
    Ok(DurableTargetSelectors {
        protocol: binding.protocol().clone(),
        route: binding.route().clone(),
        model: binding.requested_model().clone(),
        selected_provider_configuration: configuration.selected_identity(),
        effective_provider_configuration: configuration.effective_identity(),
        account_binding: admission.runtime().account_binding(),
        tool_transition: DurableRebindToolTransition::RecompileForTarget,
    })
}

/// `transition` names the caller's transition in a continuity refusal.
async fn compile_branch_target(
    source: &RuntimeCheckpoint,
    request: DurableRebindRequest,
    run_id: RunId,
    transition: &str,
) -> Result<CompiledBranchTarget, AgentError> {
    validate_durable_builder(&request.builder)?;
    let descriptor = request
        .builder
        .provider_configuration
        .effective()
        .descriptor();
    if descriptor.protocol() != &request.target.protocol
        || descriptor.route() != &request.target.route
        || descriptor.requested_model() != &request.target.model
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "rebind selectors disagree with the sealed target model",
        ));
    }
    if request.builder.provider_configuration.selected_identity()
        != request.target.selected_provider_configuration
        || request.builder.provider_configuration.effective_identity()
            != request.target.effective_provider_configuration
        || request.builder.initial_model_authority.account_binding()
            != request.target.account_binding
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "rebind target authority disagrees with the sealed target runtime",
        ));
    }
    let admission = sealed_runtime_admission(&request.builder).await?;
    let environment = run_environment(
        request.builder.workspace.as_ref(),
        run_id,
        request.builder.host_process_options.clone(),
        request.builder.job_config,
    )?;
    let tools = ToolRuntime::new(
        request.builder.tools.clone(),
        Arc::new(environment),
        Arc::clone(&request.builder.policy),
    );
    let (harness_plan, request_policy, instructions, exposure) =
        resolve_construction_harness_plan(&request.builder, &tools)?;
    let stable_prefix = stable_prefix_from_effective_configuration(
        &request.builder.provider_configuration,
        &request_policy,
        instructions,
        &exposure,
        &harness_plan,
    )?;
    let presentation = construction_tool_presentation(&request.builder, exposure)?;
    let stable_prefix = presentation.request_prefix(stable_prefix)?;
    let observed_at = admission.fact.observed_at();
    let (binding, tool_manifest) = runtime_manifest_binding_and_tools(
        request.builder.model.as_ref(),
        &request.builder.provider_configuration,
        &tools,
        &presentation,
        request.builder.workspace.as_ref(),
        request.builder.local_composition.as_ref(),
        observed_at,
        Some(admission.fact),
    )?;
    let source_manifest = source.manifest();
    let source_plan = source_manifest
        .rehydrate_harness_plan()
        .map_err(runtime_manifest_error)?;
    let source_prefix = source_manifest.canonical().stable_prefix();
    let source_tools = source_manifest.tools();
    let target_registry = tool_registry_descriptor(tool_manifest.registry())?;
    let source_registry = tool_registry_descriptor(source_tools.registry())?;
    let target_workspace = stable_workspace_environment_bytes(&request.builder.workspace)?;
    let exact_tool_continuity = target_registry
        .interface_identity()
        .map_err(tool_descriptor_error)?
        == source_registry
            .interface_identity()
            .map_err(tool_descriptor_error)?;
    let canonical_tool_continuity = target_registry
        .canonical_authority_identity()
        .map_err(tool_descriptor_error)?
        == source_registry
            .canonical_authority_identity()
            .map_err(tool_descriptor_error)?;
    let tool_continuity = match request.target.tool_transition {
        DurableRebindToolTransition::PreserveExact => exact_tool_continuity,
        DurableRebindToolTransition::RecompileForTarget => canonical_tool_continuity,
    };
    let exact_presentation =
        request.target.tool_transition == DurableRebindToolTransition::PreserveExact;
    // A rebind that recompiles for its target recompiles everything the
    // caller compiled for it, instruction sources (`AGENTS.md`) included: the
    // target is constructed from the files as they are now. Only an exact
    // rebind must keep the source's sources byte for byte.
    let continuity_mismatch = if !tool_continuity {
        Some("canonical tool authority")
    } else if exact_presentation
        && harness_plan.execution_environment_digest() != source_plan.execution_environment_digest()
    {
        Some("execution environment")
    } else if exact_presentation
        && harness_plan.instructions().sources() != source_plan.instructions().sources()
    {
        Some(INSTRUCTION_SOURCE_AUTHORITY)
    } else if !caller_instructions_match(stable_prefix.instructions(), source_prefix.instructions())
    {
        Some("caller instructions")
    } else if exact_presentation && stable_prefix.output() != source_prefix.output() {
        Some("output policy")
    } else if exact_presentation && stable_prefix.tool_choice() != source_prefix.tool_choice() {
        Some("tool-choice policy")
    } else if !source_tools
        .environment()
        .bytes()
        .starts_with(&target_workspace)
    {
        Some("workspace environment")
    } else if tool_manifest.effect_policy() != source_tools.effect_policy()
        || tool_manifest.policy() != source_tools.policy()
    {
        Some("effect policy")
    } else {
        None
    };
    if let Some(mismatch) = continuity_mismatch {
        let remedy = if mismatch == INSTRUCTION_SOURCE_AUTHORITY {
            "; instruction files such as AGENTS.md changed since the source was compiled, and \
             only a rebind that recompiles for its target re-reads them"
        } else {
            ""
        };
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            format!("{transition} changed {mismatch} outside its authority{remedy}"),
        ));
    }
    let source_binding = source_manifest.binding();
    let same_binding = binding.model_binding() == source_binding.model_binding()
        && binding.account().authority() == source_binding.account().authority()
        && binding.account().subject() == source_binding.account().subject()
        && request.builder.provider_configuration.selected_identity()
            == source_manifest.provider_configuration().selected_identity()
        && request.builder.provider_configuration.effective_identity()
            == source_manifest
                .provider_configuration()
                .effective_identity();
    let harness = harness_plan_component(&harness_plan)?;
    let unchanged = same_binding
        && harness == *source_manifest.harness_plan_component()
        && stable_prefix == *source_prefix
        && tool_manifest == *source_tools;
    Ok(CompiledBranchTarget {
        target: PreparedRebindTarget {
            harness,
            provider_configuration: request.builder.provider_configuration.as_ref().clone(),
            stable_prefix,
            binding,
            tools: tool_manifest,
        },
        same_binding,
        unchanged,
    })
}

fn prepare_branch_checkpoint(
    source: &RuntimeCheckpoint,
    identity: &DurableBranchRequestIdentity,
    material: DurableBranchMaterial,
    transition: BranchTransitionMode,
    rebind: Option<PreparedRebindTarget>,
) -> Result<RuntimeCheckpoint, AgentError> {
    let source_manifest = source.manifest();
    let source_topology = source_manifest.topology();
    let target_session = source_topology.session_id;
    let context = match transition {
        BranchTransitionMode::OrdinaryFork => source_manifest.canonical().context().ordinary_fork(
            source_topology.checkpoint_id,
            target_session,
            identity.target_branch,
            material.context_entropy,
        ),
        BranchTransitionMode::Rebind => source_manifest.canonical().context().rebind(
            source_topology.checkpoint_id,
            target_session,
            identity.target_branch,
            material.context_entropy,
        ),
    }
    .map_err(runtime_manifest_error)?;
    let (provenance, harness, provider_configuration, stable_prefix, binding, tools) =
        match (transition, rebind) {
            (BranchTransitionMode::OrdinaryFork, None) => (
                source_manifest.provenance().clone(),
                source_manifest.harness_plan_component().clone(),
                source_manifest.provider_configuration().clone(),
                source_manifest.canonical().stable_prefix().clone(),
                source_manifest.binding().clone(),
                source_manifest.tools().clone(),
            ),
            (BranchTransitionMode::Rebind, Some(target)) => (
                ProducerProvenance::new("plakous-agent", env!("CARGO_PKG_VERSION"), "runtime")
                    .map_err(runtime_manifest_error)?,
                target.harness,
                target.provider_configuration,
                target.stable_prefix,
                target.binding,
                target.tools,
            ),
            (BranchTransitionMode::OrdinaryFork, Some(_))
            | (BranchTransitionMode::Rebind, None) => {
                return Err(AgentError::new(
                    AgentErrorKind::Internal,
                    "branch transition target authority is inconsistent",
                ));
            }
        };
    let canonical = RuntimeCanonicalState::new(
        stable_prefix,
        source_manifest.canonical().history().clone(),
        source_manifest.canonical().working().clone(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let manifest = RuntimeManifest::new(
        provenance,
        RuntimeTopology {
            session_id: target_session,
            branch_id: identity.target_branch,
            checkpoint_id: material.checkpoint_id,
            parent_checkpoint_id: Some(source_topology.checkpoint_id),
            branch_revision: 0,
            fork: Some(ForkLineage {
                session_id: source_topology.session_id,
                branch_id: source_topology.branch_id,
                checkpoint_id: source_topology.checkpoint_id,
                branch_revision: source_topology.branch_revision,
            }),
            lease_fence: 1,
            commit_identity: material.commit_identity,
        },
        RuntimeLifecycle::Quiescent,
        harness,
        provider_configuration,
        canonical,
        binding,
        tools,
        source_manifest.ledger().clone(),
        None,
        None,
        source_manifest.jobs().clone(),
        source_manifest.durability(),
    )
    .map_err(runtime_manifest_error)?;
    RuntimeCheckpoint::new(manifest).map_err(runtime_manifest_error)
}

pub(in crate::runtime) fn validate_session_operation_id(
    operation_id: &str,
) -> Result<(), AgentError> {
    if operation_id.is_empty()
        || operation_id.len() > 128
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "session operation identity is invalid",
        ));
    }
    Ok(())
}

pub(in crate::runtime) fn validate_durable_builder(
    builder: &AgentBuilder,
) -> Result<(), AgentError> {
    builder.config.validate()?;
    let revision = builder.policy.revision();
    let concrete = Any::type_id(builder.policy.as_ref());
    let read_only = ReadOnlyToolPolicy.revision();
    let always_allow = AlwaysAllowPolicy.revision();
    if revision.policy_id() == read_only.policy_id()
        && concrete != TypeId::of::<ReadOnlyToolPolicy>()
        || revision.policy_id() == always_allow.policy_id()
            && concrete != TypeId::of::<AlwaysAllowPolicy>()
    {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "reserved tool-policy identity requires its release-owned executable",
        ));
    }
    if builder.instructions.len() > 256 {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "agent instruction prefix exceeds the canonical block limit",
        ));
    }
    tokio::runtime::Handle::try_current().map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "agent construction requires an active Tokio runtime",
        )
    })?;
    Ok(())
}

pub(super) fn session_operation_uuid(
    operation_id: &str,
    role: &[u8],
) -> Result<uuid::Uuid, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        4,
        &[
            b"plakous-agent/session-operation-v1",
            role,
            operation_id.as_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "session operation identity could not be derived",
        )
    })?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes))
}

pub(super) fn operation_run_id(
    instance_nonce: &RuntimeInstanceNonce,
    operation_id: &str,
) -> Result<RunId, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        4,
        &[
            b"plakous-agent/runtime-instance-lease-owner-v1",
            &instance_nonce.0,
            operation_id.as_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "runtime lease owner identity could not be derived",
        )
    })?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RunId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

#[cfg(test)]
mod jobs_carry_tests {
    use super::*;

    #[tokio::test]
    async fn jobs_carry_branch_seed_fork_and_rebind_copy_terminal_rows_and_counter()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = crate::runtime::durable::tests::jobs_carry_fixture::checkpoint().await?;
        let prior = source.manifest();
        for transition in [
            BranchTransitionMode::OrdinaryFork,
            BranchTransitionMode::Rebind,
        ] {
            let operation = if transition == BranchTransitionMode::OrdinaryFork {
                "fork:jobs-carry"
            } else {
                "rebind:jobs-carry"
            };
            let identity = DurableBranchRequestIdentity::new(
                operation,
                DurableSessionAddress::new(
                    prior.topology().session_id,
                    prior.topology().branch_id,
                    source.id(),
                ),
                transition,
                None,
            )?;
            let target =
                (transition == BranchTransitionMode::Rebind).then(|| PreparedRebindTarget {
                    harness: prior.harness_plan_component().clone(),
                    provider_configuration: prior.provider_configuration().clone(),
                    stable_prefix: prior.canonical().stable_prefix().clone(),
                    binding: prior.binding().clone(),
                    tools: prior.tools().clone(),
                });
            let seeded = prepare_branch_checkpoint(
                &source,
                &identity,
                DurableBranchMaterial::new(operation)?,
                transition,
                target,
            )?;
            assert_ne!(
                seeded.manifest().topology().branch_id,
                prior.topology().branch_id
            );
            assert_eq!(seeded.manifest().jobs(), prior.jobs());
            assert_eq!(seeded.manifest().jobs().rows().len(), 1);
        }
        Ok(())
    }
}
