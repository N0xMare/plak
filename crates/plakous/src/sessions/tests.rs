#[path = "tests/command_ancestry.rs"]
mod command_ancestry;

use plakous_agent::internal::{RuntimeDurability, SessionStoreError};
use std::collections::{BTreeMap, VecDeque};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::fs::{File, OpenOptions};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::io::Write;
use std::num::{NonZeroU32, NonZeroU64};
#[cfg(any(all(unix, feature = "sessions-sqlite"), feature = "composition"))]
use std::path::Path;
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::process::{Child, Command, Stdio};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use std::time::{Duration, Instant};

use super::*;

#[cfg(feature = "sessions-sqlite")]
#[test]
fn export_requires_terminal_process_evidence() {
    assert!(require_export_job_terminals(false).is_ok());
    let error = require_export_job_terminals(true).expect_err("unsettled process export refusal");
    assert_ne!(error.kind(), AgentSessionsErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "background process outcomes are not settled; resume the session and send a prompt to deliver their notices before exporting"
    );
}
use plakous_agent::internal::{
    AcquireBranch, CommitCheckpoint, CreateSession, DurableCommandDisposition,
    DurableCommandRecord, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, FamilyArtifact, LeaseClock, LeaseDomain, LeaseInstant,
    RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION, RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
    ReconstructiveComponent, RecoveryHandlerSignature, RecoveryObligationKind,
    RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeContextTransition,
    RuntimeContextTransitionKind, RuntimeManifest, RuntimeOperationLedger, RuntimeTopology,
    RuntimeWorkingRoot,
};
use plakous_agent::{AgentRequestPolicy, TurnResultOutcome};
use plakous_model::{
    Actor, AttemptId, BlockStart, BlockStarted, Capability, CapabilityConstraints, CapabilitySet,
    ContentBlock, ContextStatusKind, ConversationItem, DigestDomain, EntryId, FactProvenance,
    FactSource, FamilyArtifactProducerFacts, FeatureSupport, FinishDetails, FinishReason,
    FixedClock, GenerationLimits, LanguageModel, LocalDurabilityDisposition, ModelAdapterProfileId,
    ModelError, ModelErrorKind, ModelEvent, ModelFamilyId, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, ModelResponse, ModelStream,
    OutputBoundDisposition, OutputLocation, PreparedCallRecord, PreparedModelCall, Provenance,
    ReasoningPolicy, ResolvedModelDescriptor, ResponseStarted, RunId, RuntimeModelAuthority,
    RuntimeModelAuthorityOperation, SealedFamilyArtifactCandidate, Support, SystemIdGenerator,
    TextBlock, Timestamp, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract, TurnId, ValidatedRequestPlan,
};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use plakous_model::{
    CountObservation, HistorySegmentKind, ModelCallRecoveryOperation, ModelCallRecoveryRequest,
    ModelOperationRoot, Usage,
};
#[cfg(all(unix, feature = "sessions-sqlite"))]
use plakous_store::SqliteSessionStore;
use plakous_store::{MemorySessionStore, MemoryStoreLimits};
use plakous_tools::{
    AlwaysAllowPolicy, FrozenToolRegistry, RegisteredToolId, ToolExecutorId, ToolOutcome,
    ToolPolicy, ToolPolicyDecision, ToolPolicyFuture, ToolPolicyRequest, ToolPolicyRevision,
    ToolRegistration, ToolRegistryBuilder, WorkspacePolicy,
};

#[cfg(all(unix, feature = "sessions-sqlite"))]
mod capacity;
mod debt;
#[cfg(feature = "sessions-sqlite")]
mod feed;
#[cfg(feature = "composition-python")]
mod python;
#[cfg(all(unix, feature = "sessions-sqlite"))]
mod retention;

#[test]
fn facade_open_admits_only_runtime_restorable_working_phases() {
    assert!(runtime_can_restore_phase(
        SessionRecoveryPhase::EffectDecision
    ));
    assert!(runtime_can_restore_phase(
        SessionRecoveryPhase::HostedContinuation
    ));
    for phase in [
        SessionRecoveryPhase::DispatchEvidence,
        SessionRecoveryPhase::EffectReconciliation,
        SessionRecoveryPhase::CommitRecovery,
        SessionRecoveryPhase::Finalization,
    ] {
        assert!(!runtime_can_restore_phase(phase));
    }
}
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn test_lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct CustomRestorablePolicy;

impl ToolPolicy for CustomRestorablePolicy {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision::new("test.custom-restorable", 7).expect("static custom policy revision")
    }

    fn evaluate(&self, _request: ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(async { Ok(ToolPolicyDecision::Allowed) })
    }
}

struct FixedLeaseClock(Timestamp);

impl FixedLeaseClock {
    fn new(value: Timestamp) -> Self {
        Self(value)
    }
}

impl LeaseClock for FixedLeaseClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        LeaseInstant::new(
            LeaseDomain::new([1; 16], [2; 32]),
            self.0.seconds(),
            self.0.nanoseconds(),
        )
    }
}

fn timestamp() -> TestResult<Timestamp> {
    Ok(Timestamp::new(1, 0)?)
}

fn descriptor_for(route: &str, model: &str) -> TestResult<ResolvedModelDescriptor> {
    descriptor_for_adapter(route, model, "test.adapter")
}

fn descriptor_for_adapter(
    route: &str,
    model: &str,
    adapter: &str,
) -> TestResult<ResolvedModelDescriptor> {
    descriptor_for_adapter_at(route, model, adapter, timestamp()?)
}

fn descriptor_for_adapter_at(
    route: &str,
    model: &str,
    adapter: &str,
    observed: Timestamp,
) -> TestResult<ResolvedModelDescriptor> {
    Ok(crate::test_model::with_context_contract(
        descriptor_without_context_contract_for_adapter_at(route, model, adapter, observed)?,
        observed,
    )?)
}

fn descriptor_without_context_contract(
    route: &str,
    model: &str,
) -> TestResult<ResolvedModelDescriptor> {
    descriptor_without_context_contract_for_adapter(route, model, "test.adapter")
}

fn descriptor_without_context_contract_for_adapter(
    route: &str,
    model: &str,
    adapter: &str,
) -> TestResult<ResolvedModelDescriptor> {
    descriptor_without_context_contract_for_adapter_at(route, model, adapter, timestamp()?)
}

fn descriptor_without_context_contract_for_adapter_at(
    route: &str,
    model: &str,
    adapter: &str,
    observed: Timestamp,
) -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, observed, None);
    let features = BTreeMap::from([
        (
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Caching,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Tools,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
    ]);
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new(route)?,
        ModelId::new(model)?,
        None,
        Some(ModelFamilyId::new("test.family")?),
        ModelAdapterProfileId::new(adapter)?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    descriptor_for("test.route", "test.model")
}

fn runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<SessionRuntimeBinding> {
    Ok(runtime_binding_with_model_and_durability(
        workspace,
        descriptor,
        account,
        LocalDurabilityDisposition::Ephemeral,
    )?
    .0)
}

fn custom_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<SessionRuntimeBinding> {
    let binding = runtime_binding_for(workspace, descriptor, account)?;
    Ok(SessionRuntimeBinding::new(
        binding
            .builder
            .tool_policy(Arc::new(CustomRestorablePolicy)),
    ))
}

fn compaction_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_content_sensitive_observation(),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_compaction_model(
        model,
        descriptor,
        authority,
        local_durability,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

#[cfg(feature = "sessions-sqlite")]
fn durable_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<SessionRuntimeBinding> {
    Ok(runtime_binding_with_model_and_durability(
        workspace,
        descriptor,
        account,
        LocalDurabilityDisposition::DurableRequired,
    )?
    .0)
}

fn runtime_binding_with_model(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<(
    SessionRuntimeBinding,
    Arc<plakous_test_support::ScriptedModel>,
)> {
    runtime_binding_with_model_and_durability(
        workspace,
        descriptor,
        account,
        LocalDurabilityDisposition::Ephemeral,
    )
}

#[test]
fn isolated_tool_scheduler_uses_private_budget() -> TestResult<()> {
    use plakous_agent::internal::ToolScheduler;

    let shared = ToolScheduler::new(64)?;
    let isolated = ToolScheduler::new(64)?;
    assert!(
        !shared.shares_budget_with(&isolated),
        "concurrent agents can have independent tool scheduler budgets"
    );
    let directory = TempDir::new()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (binding, _) = runtime_binding_with_model(workspace, descriptor()?, b"isolated-agent")?;
    isolated_tool_scheduler(binding.builder).expect("isolated scheduler install");
    Ok(())
}

fn runtime_binding_with_model_and_durability(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<(
    SessionRuntimeBinding,
    Arc<plakous_test_support::ScriptedModel>,
)> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model.clone(),
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        local_durability,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok((SessionRuntimeBinding::new(builder), Arc::clone(&model)))
}

#[cfg(feature = "native-subscriptions")]
fn runtime_binding_with_turn_scoped_model(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<(
    SessionRuntimeBinding,
    Arc<plakous_test_support::ScriptedModel>,
)> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        crate::subscriptions::wrap_turn_scoped_native_model(Arc::clone(&model) as _),
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok((SessionRuntimeBinding::new(builder), model))
}

fn blocked_runtime_binding(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let scenario = plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::Any,
        plakous_test_support::ScenarioStart::AwaitCancellation,
        Vec::new(),
    );
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [scenario])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        local_durability,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    Ok(SessionRuntimeBinding::new(
        AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            ),
    ))
}

fn runtime_binding_with_registry(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    registry: FrozenToolRegistry,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?),
    );
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

struct CompletingModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
}

struct CapturingCompletingModel {
    inner: Arc<CompletingModel>,
    prepared: Arc<Mutex<Option<PreparedCallRecord>>>,
}

struct HostedPauseModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    calls: AtomicUsize,
}

struct FixtureStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
}

fn hosted_pause_fixture_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::InvalidEvent,
        "hosted pause fixture produced invalid semantic state",
    )
}

impl futures_core::Stream for FixtureStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.events.pop_front())
    }
}

impl LanguageModel for CompletingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted
            .clone()
            .revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        let record = inert.record().clone();
        let mut output_identity = [0_u8; 16];
        output_identity.copy_from_slice(&plan.digest().bytes()[..16]);
        let output = ConversationItem::new(
            EntryId::try_from(Uuid::from_bytes(output_identity)).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "completion fixture entry identity is invalid",
                )
            })?,
            Actor::Assistant,
            vec![ContentBlock::Text(
                TextBlock::new("completed", None).map_err(|_| {
                    ModelPreparationError::rejected(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "completion fixture text is invalid",
                    )
                })?,
            )],
            Provenance::ModelOutput,
        )
        .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "completion fixture output is invalid",
            )
        })?;
        Ok(PreparedModelCall::new(record, move |_| {
            Box::pin(async move {
                let response = ModelResponse::new(
                    vec![output.clone()],
                    FinishDetails::new(FinishReason::Stop, None),
                    None,
                    None,
                    plan.digest(),
                );
                let events = vec![
                    Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                        None,
                        plan.digest(),
                    ))),
                    Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        output.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(output.blocks()[0].clone()),
                    ))),
                    Ok(ModelEvent::ResponseCompleted(response)),
                ];
                Ok(Box::pin(FixtureStream {
                    events: events.into(),
                }) as ModelStream)
            })
        }))
    }
}

impl LanguageModel for CapturingCompletingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.inner.clone().revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let call = self.inner.clone().prepare_call(plan, context)?;
        *test_lock(&self.prepared) = Some(call.record().clone());
        Ok(call)
    }
}

impl LanguageModel for HostedPauseModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted
            .clone()
            .revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let cache_affinity = context
            .cache_affinity()
            .map(plakous_model::ModelCacheAffinity::identity);
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        let record = inert.record().clone();
        Ok(PreparedModelCall::new(record, move |_| {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::AcqRel);
                let entry_id = EntryId::try_from(Uuid::from_u128(10_000_u128.saturating_add(
                    u128::try_from(call).map_err(|_| hosted_pause_fixture_error())?,
                )))
                .map_err(|_| hosted_pause_fixture_error())?;
                let output = ConversationItem::new(
                    entry_id,
                    Actor::Assistant,
                    vec![ContentBlock::Text(
                        TextBlock::new(format!("hosted pause response {call}"), None)
                            .map_err(|_| hosted_pause_fixture_error())?,
                    )],
                    Provenance::ModelOutput,
                )
                .map_err(|_| hosted_pause_fixture_error())?;
                let mut response = ModelResponse::new(
                    vec![output.clone()],
                    FinishDetails::new(
                        if call == 0 {
                            FinishReason::ProviderContinuation
                        } else {
                            FinishReason::Stop
                        },
                        None,
                    ),
                    None,
                    None,
                    plan.digest(),
                );
                if call == 0 {
                    let request = plan.request();
                    let source = request
                        .input()
                        .canonical_item_prefix_identity(request.input().items().len())
                        .map_err(|_| hosted_pause_fixture_error())?;
                    let covered = request
                        .input()
                        .staged_canonical_item_prefix_identity(std::slice::from_ref(&output))
                        .map_err(|_| hosted_pause_fixture_error())?;
                    let metadata = request.metadata();
                    let producer = FamilyArtifactProducerFacts::new(
                        request.identity(),
                        request.model_input_identity(),
                        plan.digest(),
                        cache_affinity,
                        metadata.run_id(),
                        metadata.root(),
                        metadata.attempt_id(),
                        source.identity(),
                        u32::try_from(request.input().items().len()).map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "hosted pause fixture source count overflowed",
                            )
                        })?,
                        1,
                        covered.identity(),
                        u32::try_from(covered.item_count()).map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "hosted pause fixture coverage count overflowed",
                            )
                        })?,
                        Digest::sha256(
                            DigestDomain::ProviderOperation,
                            3,
                            &[
                                b"facade-hosted-pause-physical",
                                metadata.attempt_id().as_uuid().as_bytes(),
                            ],
                        )
                        .map_err(|_| hosted_pause_fixture_error())?,
                        Digest::sha256(
                            DigestDomain::ProviderOperation,
                            3,
                            &[
                                b"facade-hosted-pause-terminal",
                                metadata.attempt_id().as_uuid().as_bytes(),
                            ],
                        )
                        .map_err(|_| hosted_pause_fixture_error())?,
                    )
                    .map_err(|_| hosted_pause_fixture_error())?;
                    let candidate = SealedFamilyArtifactCandidate::new(
                        "responses",
                        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
                        RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
                        Digest::sha256(
                            DigestDomain::ModelBinding,
                            3,
                            &[b"facade-hosted-pause-binding"],
                        )
                        .map_err(|_| hosted_pause_fixture_error())?,
                        producer,
                        b"facade-hosted-pause-artifact".as_slice(),
                    )
                    .map_err(|_| hosted_pause_fixture_error())?;
                    response = response
                        .with_family_artifact_candidate(candidate)
                        .map_err(|_| hosted_pause_fixture_error())?;
                }
                let events = vec![
                    Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                        None,
                        plan.digest(),
                    ))),
                    Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        output.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(output.blocks()[0].clone()),
                    ))),
                    Ok(ModelEvent::ResponseCompleted(response)),
                ];
                Ok(Box::pin(FixtureStream {
                    events: events.into(),
                }) as ModelStream)
            })
        }))
    }
}

fn completing_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<SessionRuntimeBinding> {
    completing_runtime_binding_with_durability(
        workspace,
        descriptor,
        account,
        LocalDurabilityDisposition::Ephemeral,
    )
}

#[cfg(feature = "sessions-sqlite")]
fn durable_completing_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
) -> TestResult<SessionRuntimeBinding> {
    completing_runtime_binding_with_durability(
        workspace,
        descriptor,
        account,
        LocalDurabilityDisposition::DurableRequired,
    )
}

fn completing_runtime_binding_with_durability(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<SessionRuntimeBinding> {
    Ok(completing_runtime_binding_with_capture_and_durability(
        workspace,
        descriptor,
        account,
        local_durability,
    )?
    .0)
}

fn completing_runtime_binding_with_capture_and_durability(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<(
    SessionRuntimeBinding,
    Arc<Mutex<Option<PreparedCallRecord>>>,
)> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let scripted = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let prepared = Arc::new(Mutex::new(None));
    let model: Arc<dyn LanguageModel> = Arc::new(CapturingCompletingModel {
        inner: Arc::new(CompletingModel { scripted }),
        prepared: Arc::clone(&prepared),
    });
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        local_durability,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok((SessionRuntimeBinding::new(builder), prepared))
}

/// A completing runtime whose explicit plan enrolls one instruction source —
/// an `AGENTS.md` stand-in — whose exact content is `instructions`.
fn instructed_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    instructions: &str,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let scripted = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let model: Arc<dyn LanguageModel> = Arc::new(CompletingModel { scripted });
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        Arc::clone(&model),
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let mut catalog = plakous_agent::ContextSourceCatalog::new();
    catalog.insert(plakous_agent::RegisteredContextSource::new(
        "test.agents.v1",
        instructions,
        "observed",
        "test-agents",
    )?)?;
    let selector = plakous_agent::ContextSourceSelector::new("test.agents.v1")?;
    let facts = plakous_agent::ContextSourceResolver::new(catalog)
        .resolve(std::slice::from_ref(&selector))?;
    let spec = plakous_agent::HarnessProfileSpecBuilder::new()
        .id(plakous_agent::HarnessProfileId::new("test.instructed")?)
        .base_instructions("base")
        .instruction_strategy(plakous_agent::InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(vec![selector])
        .cache_strategy(plakous_agent::CachePreferenceStrategy::PreferStablePrefix)
        .build()?;
    let plan = plakous_agent::compile_explicit_runtime_profile(
        &spec,
        model.as_ref(),
        configured.configuration(),
        &registry,
        &workspace,
        facts,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .harness_plan(plan)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

fn hosted_pause_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    model: Arc<HostedPauseModel>,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    Ok(SessionRuntimeBinding::new(
        AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            ),
    ))
}

fn completing_compaction_runtime_binding_for(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
    account: &[u8],
    local_durability: LocalDurabilityDisposition,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let scripted = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_content_sensitive_observation()
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let model: Arc<dyn LanguageModel> = Arc::new(CompletingModel { scripted });
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_compaction_model(
        model,
        descriptor,
        authority,
        local_durability,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

fn runtime_binding() -> TestResult<(TempDir, SessionRuntimeBinding)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let runtime = runtime_binding_for(workspace, descriptor()?, b"account-alpha")?;
    Ok((directory, runtime))
}

#[tokio::test]
async fn clear_runtime_copies_counter_without_job_rows() -> TestResult {
    let (directory, runtime) = runtime_binding()?;
    let runtime = SessionRuntimeBinding::new(runtime.builder.job_counter_seed(17)?);
    let source = runtime.builder.build()?;
    let cleared = runtime_binding_for(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        descriptor()?,
        b"account-alpha",
    )?
    .with_job_counter(&source)?
    .builder
    .build()?;
    assert_eq!(agent_checkpoint(&cleared).manifest().jobs().next_id(), 17);
    assert_eq!(agent_checkpoint(&cleared).manifest().jobs().rows(), []);
    source.shutdown().await?;
    cleared.shutdown().await?;
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
fn sqlite_test_sessions(path: &Path) -> TestResult<AgentSessions> {
    let store =
        SqliteSessionStore::open_with_clock(path, Arc::new(FixedLeaseClock::new(timestamp()?)))?;
    Ok(AgentSessions::new(SessionBackend::from_store(
        Arc::new(store),
        RuntimeDurability::Required,
    ))?)
}

fn unauthenticated_runtime_binding(
    workspace: Arc<WorkspacePolicy>,
    descriptor: ResolvedModelDescriptor,
) -> TestResult<SessionRuntimeBinding> {
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"unauthenticated-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"unauthenticated-credential"],
        )?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_context_observation(crate::test_model::context_observation()?),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

fn one_tool_registry() -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probe one value.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition.clone(),
        Arc::new(plakous_test_support::ScriptedTool::new([
            ToolOutcome::success_text("ok"),
        ])),
    ))?;
    Ok(registry.freeze())
}

fn composition_marker_registry() -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    plakous_compose::ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

struct LoseForkAckStore {
    inner: MemorySessionStore,
    lose_next_fork_ack: AtomicBool,
    stop_after_hosted_working: AtomicBool,
    working_cut_committed: AtomicBool,
    /// Report every forked target checkpoint as pruned and refuse every
    /// lease release, so a fork fails both its publication and its cleanup.
    fail_fork_publication_and_release: AtomicBool,
    forked_targets: Mutex<Vec<CheckpointId>>,
}

impl LoseForkAckStore {
    fn new(inner: MemorySessionStore) -> Self {
        Self {
            inner,
            lose_next_fork_ack: AtomicBool::new(true),
            stop_after_hosted_working: AtomicBool::new(false),
            working_cut_committed: AtomicBool::new(false),
            fail_fork_publication_and_release: AtomicBool::new(false),
            forked_targets: Mutex::new(Vec::new()),
        }
    }

    fn passing_through(inner: MemorySessionStore) -> Self {
        Self {
            inner,
            lose_next_fork_ack: AtomicBool::new(false),
            stop_after_hosted_working: AtomicBool::new(false),
            working_cut_committed: AtomicBool::new(false),
            fail_fork_publication_and_release: AtomicBool::new(false),
            forked_targets: Mutex::new(Vec::new()),
        }
    }

    fn fail_fork_publication_and_release(&self) {
        self.fail_fork_publication_and_release
            .store(true, Ordering::Release);
    }

    fn stopping_after_hosted_working(inner: MemorySessionStore) -> Self {
        Self {
            inner,
            lose_next_fork_ack: AtomicBool::new(false),
            stop_after_hosted_working: AtomicBool::new(true),
            working_cut_committed: AtomicBool::new(false),
            fail_fork_publication_and_release: AtomicBool::new(false),
            forked_targets: Mutex::new(Vec::new()),
        }
    }

    fn resume_commits(&self) {
        self.stop_after_hosted_working
            .store(false, Ordering::Release);
    }
}

impl SessionStore for LoseForkAckStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }

    fn create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        self.inner.create(request)
    }

    fn recover_create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        self.inner.recover_create(request)
    }

    fn load_branch(
        &self,
        session: SessionRef,
    ) -> plakous_agent::internal::StoreFuture<'_, LoadedBranch> {
        self.inner.load_branch(session)
    }

    fn load_checkpoint(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoredCheckpoint> {
        if self
            .fail_fork_publication_and_release
            .load(Ordering::Acquire)
            && test_lock(&self.forked_targets).contains(&checkpoint)
        {
            return Box::pin(async { Err(SessionStoreError::Pruned) });
        }
        self.inner.load_checkpoint(checkpoint)
    }

    fn acquire(
        &self,
        request: AcquireBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        self.inner.acquire(request)
    }

    fn renew(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        self.inner.renew(lease)
    }

    fn release(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, ()> {
        if self
            .fail_fork_publication_and_release
            .load(Ordering::Acquire)
        {
            return Box::pin(async { Err(SessionStoreError::BackendUnavailable) });
        }
        self.inner.release(lease)
    }

    fn commit(
        &self,
        request: CommitCheckpoint,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommitReceipt> {
        Box::pin(async move {
            if self.stop_after_hosted_working.load(Ordering::Acquire)
                && self.working_cut_committed.load(Ordering::Acquire)
            {
                return Err(SessionStoreError::BackendUnavailable);
            }
            let selected_working =
                request
                    .checkpoint()
                    .validated_checkpoint()
                    .is_ok_and(|checkpoint| {
                        let lifecycle = checkpoint.manifest().lifecycle();
                        matches!(
                            lifecycle,
                            RuntimeLifecycle::Working {
                                recovery_phase: WorkingRecoveryPhase::HostedContinuation,
                                ..
                            }
                        ) && self.stop_after_hosted_working.load(Ordering::Acquire)
                    });
            let receipt = self.inner.commit(request).await?;
            if selected_working {
                self.working_cut_committed.store(true, Ordering::Release);
            }
            Ok(receipt)
        })
    }

    fn recover_commit(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommittedCheckpoint>
    {
        self.inner.recover_commit(checkpoint)
    }

    fn fork(
        &self,
        request: plakous_agent::internal::ForkBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        Box::pin(async move {
            let receipt = self.inner.fork(request).await?;
            test_lock(&self.forked_targets).push(receipt.target().checkpoint().checkpoint_id());
            if self.lose_next_fork_ack.swap(false, Ordering::AcqRel) {
                Err(SessionStoreError::BackendUnavailable)
            } else {
                Ok(receipt)
            }
        })
    }

    fn recover_fork(
        &self,
        request: plakous_agent::internal::RecoverFork,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        self.inner.recover_fork(request)
    }

    fn list(
        &self,
        query: plakous_agent::internal::SessionQuery,
    ) -> plakous_agent::internal::StoreFuture<
        '_,
        plakous_agent::internal::Page<plakous_agent::internal::SessionSummary>,
    > {
        self.inner.list(query)
    }

    fn usage(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoreUsage> {
        self.inner.usage()
    }

    fn archive(
        &self,
        request: ArchiveSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ArchiveReceipt> {
        self.inner.archive(request)
    }

    fn delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.delete(request)
    }

    fn recover_delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.recover_delete(request)
    }

    fn read_feed(
        &self,
        session_id: SessionId,
        after: plakous_agent::internal::FeedSeq,
        max_records: std::num::NonZeroUsize,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }

    fn feed_tail(
        &self,
        session_id: SessionId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedTail> {
        self.inner.feed_tail(session_id)
    }

    fn flush(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.flush()
    }

    fn shutdown(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.shutdown()
    }
}

async fn seeded_service() -> TestResult<(
    TempDir,
    Arc<WorkspacePolicy>,
    AgentSessions,
    MemorySessionStore,
    SessionLocator,
)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:seeded-service")?,
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let locator = session.release().await?;
    Ok((directory, workspace, sessions, store, locator))
}

#[tokio::test]
async fn derived_genesis_locator_equals_created_session_without_mutating_store() -> TestResult {
    let (_directory, workspace, sessions, _store, _) = seeded_service().await?;
    let operation = SessionOperationId::new("create:derived-genesis")?;
    let expected = AgentSessions::genesis_locator_for(&operation)?;
    assert!(sessions.accepted_create(&operation).await?.is_none());
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            operation.clone(),
            runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    assert_eq!(session.locator(), expected);
    assert_eq!(AgentSessions::genesis_locator_for(&operation)?, expected);
    session.release().await?;
    Ok(())
}

async fn install_family_artifact(
    store: &dyn SessionStore,
    source: SessionLocator,
) -> TestResult<SessionLocator> {
    let loaded = store.load_branch(source.session_ref()).await?;
    let source_checkpoint = loaded.checkpoint().checkpoint();
    let source_manifest = source_checkpoint.manifest();
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            RunId::try_from(Uuid::from_u128(950))?,
            NonZeroU64::new(30).ok_or("lease duration must be nonzero")?,
        ))
        .await?;
    let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(951))?;
    let artifact = FamilyArtifact::new(
        "responses",
        RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
        RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
        source_manifest.binding().model_binding(),
        b"unproved-family-state".as_slice(),
    )?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        source_manifest.provenance().clone(),
        RuntimeTopology {
            session_id: source.session_id(),
            branch_id: source.branch_id(),
            checkpoint_id,
            parent_checkpoint_id: Some(source.checkpoint_id()),
            branch_revision: loaded.head().revision().get() + 1,
            fork: source_manifest.topology().fork,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"family-artifact-fixture"],
            )?,
        },
        RuntimeLifecycle::Quiescent,
        source_manifest.harness_plan_component().clone(),
        source_manifest.provider_configuration().clone(),
        source_manifest.canonical().clone(),
        source_manifest.binding().clone(),
        source_manifest.tools().clone(),
        source_manifest.ledger().clone(),
        Some(artifact),
        source_manifest.interpreter_observation().cloned(),
        plakous_agent::internal::JobLedger::default(),
        source_manifest.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, loaded.checkpoint().bundle())?;
    let committed = store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        committed.checkpoint().checkpoint_id(),
    ))
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
const PAID_COMPACTION_CRASH_TEST: &str =
    "sessions::tests::sqlite_paid_compaction_sigkill_reopens_exactly_once";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const PAID_COMPACTION_OPERATION: &str = "compact:sqlite-sigkill-paid";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const PAID_COMPACTION_CHILD_ENV: &str = "PLAKOUS_PAID_COMPACTION_CRASH_CHILD";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const PAID_COMPACTION_ROOT_ENV: &str = "PLAKOUS_PAID_COMPACTION_CRASH_ROOT";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const NO_LOCATOR_CRASH_TEST: &str =
    "sessions::tests::sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const NO_LOCATOR_OPERATION: &str = "compact:sqlite-sigkill-no-locator";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const NO_LOCATOR_RETRY_OPERATION: &str = "compact:sqlite-sigkill-explicit-retry";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const NO_LOCATOR_CHILD_ENV: &str = "PLAKOUS_NO_LOCATOR_CRASH_CHILD";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const NO_LOCATOR_ROOT_ENV: &str = "PLAKOUS_NO_LOCATOR_CRASH_ROOT";
#[cfg(all(unix, feature = "sessions-sqlite"))]
const PAID_COMPACTION_WAIT: Duration = Duration::from_secs(30);

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionCrashCut {
    ProviderOutcomeNotCommitted,
    CompletedReceiptLost,
    SuccessNotCommitted,
    SuccessReceiptLost,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl CompactionCrashCut {
    const ALL: [Self; 4] = [
        Self::ProviderOutcomeNotCommitted,
        Self::CompletedReceiptLost,
        Self::SuccessNotCommitted,
        Self::SuccessReceiptLost,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::ProviderOutcomeNotCommitted => "provider-outcome-before-commit",
            Self::CompletedReceiptLost => "completed-after-commit",
            Self::SuccessNotCommitted => "success-before-commit",
            Self::SuccessReceiptLost => "success-after-commit",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cut| cut.name() == value)
    }

    const fn target(self) -> (CompactionCommitStage, CommitCutLocation) {
        match self {
            Self::ProviderOutcomeNotCommitted => (
                CompactionCommitStage::ProviderCompletedWorking,
                CommitCutLocation::Before,
            ),
            Self::CompletedReceiptLost => (
                CompactionCommitStage::ProviderCompletedWorking,
                CommitCutLocation::After,
            ),
            Self::SuccessNotCommitted => (
                CompactionCommitStage::FinalizedSuccess,
                CommitCutLocation::Before,
            ),
            Self::SuccessReceiptLost => (
                CompactionCommitStage::FinalizedSuccess,
                CommitCutLocation::After,
            ),
        }
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionCommitStage {
    ProviderCompletedWorking,
    OutcomeUnknownWorking,
    FinalizedSuccess,
    FinalizedSourcePreserving,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoLocatorCrashCut {
    /// Exact public model-start seam after durable `Dispatched` Working publication.
    PhysicalDispatch,
    /// Exact SQLite after-commit seam. The provider-outcome/private-publication
    /// interval has no public hook and is therefore not claimed as an exact cut.
    OutcomeUnknown,
    /// Exact SQLite after-commit seam for the one-commit abandonment closure.
    /// There is no working abandonment-evidence head to cut.
    Abandoned,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl NoLocatorCrashCut {
    const ALL: [Self; 3] = [
        Self::PhysicalDispatch,
        Self::OutcomeUnknown,
        Self::Abandoned,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::PhysicalDispatch => "physical-dispatch",
            Self::OutcomeUnknown => "outcome-unknown",
            Self::Abandoned => "abandoned",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cut| cut.name() == value)
    }

    const fn target(self) -> Option<CompactionCommitStage> {
        match self {
            Self::PhysicalDispatch => None,
            Self::OutcomeUnknown => Some(CompactionCommitStage::OutcomeUnknownWorking),
            Self::Abandoned => Some(CompactionCommitStage::FinalizedSourcePreserving),
        }
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitCutLocation {
    Before,
    After,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone)]
struct CrashOracle {
    recovery: plakous_test_support::ScriptedRecoveryState,
    starts: Arc<AtomicU32>,
    oracle_path: PathBuf,
    count_path: PathBuf,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl CrashOracle {
    fn persist(&self) -> Result<(), SessionStoreError> {
        let snapshot = self
            .recovery
            .snapshot()
            .map_err(|_| SessionStoreError::corrupt())?;
        durable_test_write(&self.oracle_path, &snapshot)
            .map_err(|_| SessionStoreError::BackendUnavailable)?;
        durable_test_write(
            &self.count_path,
            self.starts.load(Ordering::Acquire).to_string().as_bytes(),
        )
        .map_err(|_| SessionStoreError::BackendUnavailable)
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn durable_test_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn compaction_commit_stage(
    request: &CommitCheckpoint,
) -> Result<Option<CompactionCommitStage>, SessionStoreError> {
    let checkpoint = request
        .checkpoint()
        .validated_checkpoint()
        .map_err(|_| SessionStoreError::corrupt())?;
    let manifest = checkpoint.manifest();
    let compactions = manifest
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|record| match record.kind() {
            DurableOperationKind::Compaction { workflow_id } => {
                Some((*workflow_id, record.state()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [(workflow_id, compaction_state)] = compactions.as_slice() else {
        return Ok(None);
    };
    let providers = manifest
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|record| match record.kind() {
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(candidate),
                ..
            } if candidate == workflow_id => Some(record.state()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [provider_state] = providers.as_slice() else {
        return Ok(None);
    };
    let both_completed = matches!(compaction_state, DurableOperationState::Completed(_))
        && matches!(provider_state, DurableOperationState::Completed(_));
    if both_completed
        && matches!(
            manifest.lifecycle(),
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::SessionWorkflow(candidate),
                ..
            } if candidate == workflow_id
        )
    {
        return Ok(Some(CompactionCommitStage::ProviderCompletedWorking));
    }
    let both_unknown = matches!(compaction_state, DurableOperationState::OutcomeUnknown(_))
        && matches!(provider_state, DurableOperationState::OutcomeUnknown(_));
    if both_unknown
        && matches!(
            manifest.lifecycle(),
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::SessionWorkflow(candidate),
                ..
            } if candidate == workflow_id
        )
    {
        return Ok(Some(CompactionCommitStage::OutcomeUnknownWorking));
    }
    let both_finalized = matches!(compaction_state, DurableOperationState::Finalized(_))
        && matches!(provider_state, DurableOperationState::Finalized(_));
    if both_finalized
        && matches!(
            manifest.lifecycle(),
            RuntimeLifecycle::Quiescent | RuntimeLifecycle::Working { .. }
        )
        && manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_some()
        && manifest
            .canonical()
            .context()
            .transition()
            .is_some_and(|transition| {
                transition.kind() == RuntimeContextTransitionKind::SuccessfulCompaction
            })
    {
        return Ok(Some(CompactionCommitStage::FinalizedSuccess));
    }
    if both_finalized
        && matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent)
        && manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_none()
    {
        return Ok(Some(CompactionCommitStage::FinalizedSourcePreserving));
    }
    Ok(None)
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone)]
struct CrashCutStore {
    inner: SqliteSessionStore,
    cut: Option<CompactionCrashCut>,
    no_locator_cut: Option<NoLocatorCrashCut>,
    armed: Arc<AtomicBool>,
    marker: Option<PathBuf>,
    oracle: Option<CrashOracle>,
    commits: Arc<AtomicUsize>,
    branch_loads: Arc<AtomicUsize>,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl CrashCutStore {
    fn armed(
        inner: SqliteSessionStore,
        cut: CompactionCrashCut,
        marker: PathBuf,
        oracle: CrashOracle,
    ) -> Self {
        Self {
            inner,
            cut: Some(cut),
            no_locator_cut: None,
            armed: Arc::new(AtomicBool::new(true)),
            marker: Some(marker),
            oracle: Some(oracle),
            commits: Arc::new(AtomicUsize::new(0)),
            branch_loads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn observing(inner: SqliteSessionStore) -> Self {
        Self {
            inner,
            cut: None,
            no_locator_cut: None,
            armed: Arc::new(AtomicBool::new(false)),
            marker: None,
            oracle: None,
            commits: Arc::new(AtomicUsize::new(0)),
            branch_loads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn armed_no_locator(
        inner: SqliteSessionStore,
        cut: NoLocatorCrashCut,
        marker: PathBuf,
        oracle: CrashOracle,
    ) -> Self {
        Self {
            inner,
            cut: None,
            no_locator_cut: Some(cut),
            armed: Arc::new(AtomicBool::new(true)),
            marker: Some(marker),
            oracle: Some(oracle),
            commits: Arc::new(AtomicUsize::new(0)),
            branch_loads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn commit_count(&self) -> usize {
        self.commits.load(Ordering::Acquire)
    }

    fn should_cut(
        &self,
        stage: Option<CompactionCommitStage>,
        location: CommitCutLocation,
    ) -> bool {
        let Some(stage) = stage else {
            return false;
        };
        (self
            .cut
            .is_some_and(|cut| cut.target() == (stage, location))
            || (location == CommitCutLocation::After
                && self
                    .no_locator_cut
                    .and_then(NoLocatorCrashCut::target)
                    .is_some_and(|target| target == stage)))
            && self
                .armed
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    async fn mark_and_park(&self) -> Result<(), SessionStoreError> {
        let oracle = self.oracle.as_ref().ok_or(SessionStoreError::corrupt())?;
        let marker = self.marker.as_ref().ok_or(SessionStoreError::corrupt())?;
        oracle.persist()?;
        durable_test_write(marker, b"ready").map_err(|_| SessionStoreError::BackendUnavailable)?;
        std::future::pending::<Result<(), SessionStoreError>>().await
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl SessionStore for CrashCutStore {
    fn lease_now(&self) -> Result<LeaseInstant, SessionStoreError> {
        self.inner.lease_now()
    }

    fn create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        self.inner.create(request)
    }

    fn recover_create(
        &self,
        request: CreateSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CreateReceipt> {
        self.inner.recover_create(request)
    }

    fn load_branch(
        &self,
        session: SessionRef,
    ) -> plakous_agent::internal::StoreFuture<'_, LoadedBranch> {
        self.branch_loads.fetch_add(1, Ordering::AcqRel);
        self.inner.load_branch(session)
    }

    fn load_checkpoint(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoredCheckpoint> {
        self.inner.load_checkpoint(checkpoint)
    }

    fn acquire(
        &self,
        request: AcquireBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        self.inner.acquire(request)
    }

    fn renew(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::BranchLease> {
        self.inner.renew(lease)
    }

    fn release(
        &self,
        lease: plakous_agent::internal::BranchLease,
    ) -> plakous_agent::internal::StoreFuture<'_, ()> {
        self.inner.release(lease)
    }

    fn commit(
        &self,
        request: CommitCheckpoint,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommitReceipt> {
        self.commits.fetch_add(1, Ordering::AcqRel);
        let stage = compaction_commit_stage(&request);
        Box::pin(async move {
            let stage = stage?;
            if self.should_cut(stage, CommitCutLocation::Before) {
                self.mark_and_park().await?;
            }
            let receipt = self.inner.commit(request).await?;
            if self.should_cut(stage, CommitCutLocation::After) {
                self.mark_and_park().await?;
            }
            Ok(receipt)
        })
    }

    fn recover_commit(
        &self,
        checkpoint: CheckpointId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::CommittedCheckpoint>
    {
        self.inner.recover_commit(checkpoint)
    }

    fn fork(
        &self,
        request: plakous_agent::internal::ForkBranch,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        self.inner.fork(request)
    }

    fn recover_fork(
        &self,
        request: plakous_agent::internal::RecoverFork,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ForkReceipt> {
        self.inner.recover_fork(request)
    }

    fn list(
        &self,
        query: plakous_agent::internal::SessionQuery,
    ) -> plakous_agent::internal::StoreFuture<
        '_,
        plakous_agent::internal::Page<plakous_agent::internal::SessionSummary>,
    > {
        self.inner.list(query)
    }

    fn usage(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::StoreUsage> {
        self.inner.usage()
    }

    fn archive(
        &self,
        request: ArchiveSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::ArchiveReceipt> {
        self.inner.archive(request)
    }

    fn delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.delete(request)
    }

    fn recover_delete(
        &self,
        request: DeleteSession,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DeleteReceipt> {
        self.inner.recover_delete(request)
    }

    fn read_feed(
        &self,
        session_id: SessionId,
        after: plakous_agent::internal::FeedSeq,
        max_records: std::num::NonZeroUsize,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedPage> {
        self.inner.read_feed(session_id, after, max_records)
    }

    fn feed_tail(
        &self,
        session_id: SessionId,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::FeedTail> {
        self.inner.feed_tail(session_id)
    }

    fn flush(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.flush()
    }

    fn shutdown(
        &self,
    ) -> plakous_agent::internal::StoreFuture<'_, plakous_agent::internal::DurabilityReport> {
        self.inner.shutdown()
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
struct CrashRecoveryModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    observation: CountObservation,
    recovery: plakous_test_support::ScriptedRecoveryState,
    starts: Arc<AtomicU32>,
    count_path: PathBuf,
    recovery_locator: bool,
    outcome_unknown: bool,
    start_marker: Option<PathBuf>,
    preparations: AtomicUsize,
    recoveries: AtomicUsize,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl CrashRecoveryModel {
    fn new(
        descriptor: ResolvedModelDescriptor,
        account: &[u8],
        recovery: plakous_test_support::ScriptedRecoveryState,
        starts: Arc<AtomicU32>,
        count_path: PathBuf,
    ) -> TestResult<Arc<Self>> {
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
            1,
            timestamp()?,
            None,
        )?;
        Ok(Arc::new(Self {
            descriptor,
            authority,
            observation: crate::test_model::context_observation()?,
            recovery,
            starts,
            count_path,
            recovery_locator: true,
            outcome_unknown: false,
            start_marker: None,
            preparations: AtomicUsize::new(0),
            recoveries: AtomicUsize::new(0),
        }))
    }

    fn no_locator(
        descriptor: ResolvedModelDescriptor,
        account: &[u8],
        starts: Arc<AtomicU32>,
        count_path: PathBuf,
        outcome_unknown: bool,
        start_marker: Option<PathBuf>,
    ) -> TestResult<Arc<Self>> {
        let model = Self::new(
            descriptor,
            account,
            plakous_test_support::ScriptedRecoveryState::new(),
            starts,
            count_path,
        )?;
        let model = Arc::into_inner(model).ok_or("new crash model was unexpectedly shared")?;
        Ok(Arc::new(Self {
            recovery_locator: false,
            outcome_unknown,
            start_marker,
            ..model
        }))
    }

    fn work_counts(&self) -> (usize, usize) {
        (
            self.preparations.load(Ordering::Acquire),
            self.recoveries.load(Ordering::Acquire),
        )
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl LanguageModel for CrashRecoveryModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let result = self
            .authority
            .validate(&self.descriptor, checked_at)
            .map(|()| self.authority.clone());
        Box::pin(async move { result })
    }

    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        self.recoveries.fetch_add(1, Ordering::AcqRel);
        let scripted = Arc::new(
            plakous_test_support::ScriptedModel::new(self.descriptor.clone(), [])
                .with_runtime_authority(self.authority.clone())
                .with_context_observation(self.observation.clone())
                .with_call_recovery(self.recovery.clone()),
        );
        scripted.recover_call(request)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.preparations.fetch_add(1, Ordering::AcqRel);
        let output = ConversationItem::new(
            EntryId::try_from(Uuid::from_bytes(
                plan.digest().bytes()[..16].try_into().map_err(|_| {
                    ModelPreparationError::rejected(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "compaction fixture output identity is invalid",
                    )
                })?,
            ))
            .map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "compaction fixture output identity is invalid",
                )
            })?,
            Actor::Assistant,
            vec![ContentBlock::Text(
                TextBlock::new("durable paid summary", None).map_err(|_| {
                    ModelPreparationError::rejected(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "compaction fixture summary is invalid",
                    )
                })?,
            )],
            Provenance::ModelOutput,
        )
        .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "compaction fixture output is invalid",
            )
        })?;
        let response = ModelResponse::new(
            vec![output.clone()],
            FinishDetails::new(FinishReason::Stop, None),
            Some(Usage::new(11, 3, 0, 0, 0)),
            None,
            plan.digest(),
        );
        let steps = if self.outcome_unknown {
            vec![plakous_test_support::ScenarioStep::Emit(
                ModelEvent::ResponseStarted(ResponseStarted::new(None, plan.digest())),
            )]
        } else {
            vec![
                plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                    ResponseStarted::new(None, plan.digest()),
                )),
                plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(
                    BlockStarted::new(
                        output.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(output.blocks()[0].clone()),
                    ),
                )),
                plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
            ]
        };
        let scenario = plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::PlanDigest(plan.digest()),
            plakous_test_support::ScenarioStart::Immediate,
            steps,
        );
        let scripted =
            plakous_test_support::ScriptedModel::new(self.descriptor.clone(), [scenario])
                .with_runtime_authority(self.authority.clone())
                .with_context_observation(self.observation.clone())
                .with_content_sensitive_observation();
        let scripted = if self.recovery_locator {
            scripted.with_call_recovery(self.recovery.clone())
        } else {
            scripted
        };
        let scripted = Arc::new(scripted);
        let prepared = scripted.prepare_call(plan, context)?;
        let starts = Arc::clone(&self.starts);
        let count_path = self.count_path.clone();
        let start_marker = self.start_marker.clone();
        Ok(prepared.map_start_with_control(move |start| {
            Box::pin(async move {
                let next = starts.fetch_add(1, Ordering::AcqRel).saturating_add(1);
                durable_test_write(&count_path, next.to_string().as_bytes()).map_err(|_| {
                    ModelError::new(
                        ModelErrorKind::Protocol,
                        "physical-start oracle could not be made durable",
                    )
                })?;
                if let Some(marker) = start_marker {
                    durable_test_write(&marker, b"ready").map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::Protocol,
                            "physical-start latch could not be made durable",
                        )
                    })?;
                    std::future::pending::<()>().await;
                }
                start.await
            })
        }))
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[derive(Clone)]
struct CompactionCrashPaths {
    root: PathBuf,
    database: PathBuf,
    marker: PathBuf,
    oracle: PathBuf,
    physical_starts: PathBuf,
    source: PathBuf,
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl CompactionCrashPaths {
    fn new(root: PathBuf) -> Self {
        Self {
            database: root.join("sessions.sqlite3"),
            marker: root.join("cut.reached"),
            oracle: root.join("provider-oracle.json"),
            physical_starts: root.join("physical-starts"),
            source: root.join("source-locator"),
            root,
        }
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
struct KillCrashChild(Option<Child>);

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl KillCrashChild {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("live paid-compaction child")
    }

    fn sigkill(mut self) -> std::io::Result<std::process::ExitStatus> {
        let child = self.0.as_mut().expect("live paid-compaction child");
        child.kill()?;
        let status = child.wait()?;
        let _ = self.0.take();
        Ok(status)
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
impl Drop for KillCrashChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn wait_for_compaction_cut(child: &mut Child, marker: &Path) -> TestResult {
    let deadline = Instant::now() + PAID_COMPACTION_WAIT;
    loop {
        if marker.is_file() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(std::io::Error::other(format!(
                "paid-compaction crash child exited before its cut: {status}"
            ))
            .into());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "paid-compaction crash child did not reach its cut",
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn crash_recovery_runtime_binding(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<CrashRecoveryModel>,
) -> TestResult<SessionRuntimeBinding> {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let model: Arc<dyn LanguageModel> = model;
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_compaction_model(
        model,
        descriptor,
        authority,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(timestamp()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
async fn run_paid_compaction_crash_child(
    cut: CompactionCrashCut,
    paths: &CompactionCrashPaths,
) -> TestResult {
    durable_test_write(&paths.physical_starts, b"0")?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let starts = Arc::new(AtomicU32::new(0));
    let sqlite = SqliteSessionStore::open_with_clock(
        &paths.database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let store = Arc::new(CrashCutStore::armed(
        sqlite,
        cut,
        paths.marker.clone(),
        CrashOracle {
            recovery: recovery.clone(),
            starts: Arc::clone(&starts),
            oracle_path: paths.oracle.clone(),
            count_path: paths.physical_starts.clone(),
        },
    ));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Required,
    ))?;
    let workspace = Arc::new(WorkspacePolicy::host(&paths.root, ".")?);
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:sqlite-sigkill-paid")?,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"sqlite-sigkill-account",
            )?,
        ))
        .await?;
    for input in ["older compactable intent", "latest protected intent"] {
        let result = session.agent().prompt(input).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = install_family_artifact(store.as_ref(), session.release().await?).await?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:sqlite-sigkill-paid")?,
            source,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"sqlite-sigkill-account",
            )?,
        ))
        .await?
    else {
        return Err("family-artifact source did not reopen as an attached driver".into());
    };
    durable_test_write(&paths.source, source.to_string().as_bytes())?;
    let model = CrashRecoveryModel::new(
        descriptor()?,
        b"sqlite-sigkill-account",
        recovery,
        starts,
        paths.physical_starts.clone(),
    )?;
    let result = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new(PAID_COMPACTION_OPERATION)?,
            source,
            CompactReason::Manual,
            crash_recovery_runtime_binding(workspace, model)?,
        ))
        .await;
    let _ = session.release().await;
    Err(format!("configured paid-compaction crash cut returned: {result:?}").into())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
async fn run_no_locator_crash_child(
    cut: NoLocatorCrashCut,
    paths: &CompactionCrashPaths,
) -> TestResult {
    durable_test_write(&paths.physical_starts, b"0")?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let starts = Arc::new(AtomicU32::new(0));
    let sqlite = SqliteSessionStore::open_with_clock(
        &paths.database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let store = Arc::new(CrashCutStore::armed_no_locator(
        sqlite,
        cut,
        paths.marker.clone(),
        CrashOracle {
            recovery,
            starts: Arc::clone(&starts),
            oracle_path: paths.oracle.clone(),
            count_path: paths.physical_starts.clone(),
        },
    ));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Required,
    ))?;
    let workspace = Arc::new(WorkspacePolicy::host(&paths.root, ".")?);
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:sqlite-sigkill-no-locator")?,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"sqlite-sigkill-no-locator-account",
            )?,
        ))
        .await?;
    for input in ["older compactable intent", "latest protected intent"] {
        let result = session.agent().prompt(input).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = install_family_artifact(store.as_ref(), session.release().await?).await?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:sqlite-sigkill-no-locator")?,
            source,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"sqlite-sigkill-no-locator-account",
            )?,
        ))
        .await?
    else {
        return Err("no-locator source did not reopen as an attached driver".into());
    };
    durable_test_write(&paths.source, source.to_string().as_bytes())?;
    let model = CrashRecoveryModel::no_locator(
        descriptor()?,
        b"sqlite-sigkill-no-locator-account",
        starts,
        paths.physical_starts.clone(),
        true,
        (cut == NoLocatorCrashCut::PhysicalDispatch).then(|| paths.marker.clone()),
    )?;
    let compacted = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new(NO_LOCATOR_OPERATION)?,
            source,
            CompactReason::Manual,
            crash_recovery_runtime_binding(Arc::clone(&workspace), model)?,
        ))
        .await?;
    assert!(matches!(
        compacted,
        CompactSessionOutcome::RecoveryRequired {
            ref operation_id,
            ..
        } if operation_id.as_str() == NO_LOCATOR_OPERATION
    ));
    let released = session.release().await?;
    assert_ne!(released, source);
    if cut == NoLocatorCrashCut::Abandoned {
        let result = sessions
            .abandon_compaction(AbandonCompactionRequest::new(
                SessionOperationId::new(NO_LOCATOR_OPERATION)?,
                source,
            ))
            .await;
        return Err(format!("configured no-locator abandonment cut returned: {result:?}").into());
    }
    Err(format!("configured no-locator compaction cut returned: {compacted:?}").into())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn compaction_workflow_states(
    manifest: &RuntimeManifest,
) -> TestResult<(
    plakous_model::SessionWorkflowRootId,
    &DurableOperationState,
    &DurableOperationState,
)> {
    let compactions = manifest
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|record| match record.kind() {
            DurableOperationKind::Compaction { workflow_id } => {
                Some((*workflow_id, record.state()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [(workflow_id, compaction)] = compactions.as_slice() else {
        return Err("checkpoint does not contain exactly one compaction workflow".into());
    };
    let providers = manifest
        .ledger()
        .operations()
        .into_iter()
        .filter_map(|record| match record.kind() {
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(candidate),
                ..
            } if candidate == workflow_id => Some(record.state()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [provider] = providers.as_slice() else {
        return Err("compaction workflow does not contain exactly one provider operation".into());
    };
    Ok((*workflow_id, compaction, provider))
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn assert_unpublished_compaction_preserves_source(
    source: &RuntimeCheckpoint,
    current: &RuntimeCheckpoint,
    provider_outcome_recorded: bool,
) -> TestResult {
    let (workflow_id, compaction, provider) = compaction_workflow_states(current.manifest())?;
    assert!(matches!(
        current.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::SessionWorkflow(candidate),
            ..
        } if *candidate == workflow_id
    ));
    if provider_outcome_recorded {
        let (
            DurableOperationState::Completed(compaction_evidence),
            DurableOperationState::Completed(provider_evidence),
        ) = (compaction, provider)
        else {
            return Err("paid provider completion was not durably exact at the cut".into());
        };
        assert_eq!(compaction_evidence, provider_evidence);
    } else if !matches!(
        (compaction, provider),
        (
            DurableOperationState::Dispatched,
            DurableOperationState::Dispatched
        )
    ) {
        return Err("pre-publication cut did not retain both dispatched obligations".into());
    }
    let source_manifest = source.manifest();
    let current_manifest = current.manifest();
    assert_eq!(
        current_manifest.canonical().stable_prefix(),
        source_manifest.canonical().stable_prefix()
    );
    assert_eq!(
        current_manifest.canonical().history().identity(),
        source_manifest.canonical().history().identity()
    );
    assert_eq!(
        current_manifest
            .canonical()
            .history()
            .segments()
            .iter()
            .map(plakous_model::HistorySegment::identity)
            .collect::<Vec<_>>(),
        source_manifest
            .canonical()
            .history()
            .segments()
            .iter()
            .map(plakous_model::HistorySegment::identity)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        current_manifest.canonical().context(),
        source_manifest.canonical().context()
    );
    assert_eq!(
        current_manifest.family_artifact(),
        source_manifest.family_artifact()
    );
    assert!(
        current_manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_none()
    );
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn assert_complete_compaction_success(
    source: &RuntimeCheckpoint,
    target: &RuntimeCheckpoint,
) -> TestResult {
    let source_manifest = source.manifest();
    let target_manifest = target.manifest();
    assert!(matches!(
        target_manifest.lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let (_, compaction, provider) = compaction_workflow_states(target_manifest)?;
    assert!(matches!(compaction, DurableOperationState::Finalized(_)));
    assert!(matches!(provider, DurableOperationState::Finalized(_)));
    assert_eq!(
        target_manifest.canonical().stable_prefix(),
        source_manifest.canonical().stable_prefix()
    );
    assert_eq!(target_manifest.binding(), source_manifest.binding());
    assert_eq!(target_manifest.tools(), source_manifest.tools());
    assert_eq!(target_manifest.canonical().working().segments(), []);

    let source_history = source_manifest.canonical().history();
    let target_history = target_manifest.canonical().history();
    let summary = target_history
        .segments()
        .get(0)
        .ok_or("compaction successor has no summary segment")?;
    assert_eq!(summary.kind(), HistorySegmentKind::Compaction);
    assert_eq!(
        target_history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count(),
        1
    );
    assert!(summary.items().iter().any(|item| {
        item.blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Compaction(_)))
    }));

    let context = target_manifest.canonical().context();
    let provenance = context
        .compaction_provenance()
        .ok_or("compaction successor lacks exact provenance")?;
    assert_eq!(
        provenance.source_checkpoint(),
        source_manifest.topology().checkpoint_id
    );
    assert_eq!(
        provenance.source_history_identity(),
        source_history.identity()
    );
    assert_eq!(provenance.summary_segment_identity(), summary.identity());
    assert_eq!(provenance.usage(), Some(Usage::new(11, 3, 0, 0, 0)));
    assert!(provenance.after_audit().input_tokens() > 0);
    let (selected_start, selected_end, selected_segments) = provenance.selected_range();
    assert_eq!(selected_start, 0);
    assert_eq!(selected_end, selected_segments);
    let (retained_tail, retained_segments) = provenance.retained_tail();
    assert_eq!(
        target_history.segments().len(),
        usize::try_from(retained_segments)?.saturating_add(1)
    );
    assert_eq!(
        target_history
            .segments()
            .iter()
            .skip(1)
            .map(plakous_model::HistorySegment::identity)
            .collect::<Vec<_>>(),
        source_history
            .segments()
            .iter()
            .skip(usize::try_from(selected_segments)?)
            .map(plakous_model::HistorySegment::identity)
            .collect::<Vec<_>>()
    );
    assert_eq!(retained_tail.domain(), DigestDomain::ContextPlan);

    let source_context = source_manifest.canonical().context();
    let transition = context
        .transition()
        .ok_or("successful compaction lacks its cache rotation proof")?;
    assert_eq!(
        transition.kind(),
        RuntimeContextTransitionKind::SuccessfulCompaction
    );
    assert_eq!(
        transition.source_session(),
        source_manifest.topology().session_id
    );
    assert_eq!(
        transition.source_branch(),
        source_manifest.topology().branch_id
    );
    assert_eq!(
        transition.source_checkpoint(),
        source_manifest.topology().checkpoint_id
    );
    assert_eq!(
        transition.source_context_identity(),
        source_context.identity()
    );
    assert_eq!(
        transition.source_affinity_identity(),
        source_context.affinity().identity()
    );
    assert_eq!(transition.source_epoch(), source_context.affinity().epoch());
    assert_eq!(
        transition.target_session(),
        source_manifest.topology().session_id
    );
    assert_eq!(
        transition.target_branch(),
        source_manifest.topology().branch_id
    );
    assert_eq!(
        transition.target_affinity_identity(),
        context.affinity().identity()
    );
    assert_eq!(transition.target_epoch(), context.affinity().epoch());
    assert_eq!(
        context.affinity().epoch(),
        source_context.affinity().epoch() + 1
    );
    assert_ne!(
        context.affinity().identity(),
        source_context.affinity().identity()
    );
    assert!(context.occupancy_anchor().is_none());
    let invalidation = provenance.invalidation();
    assert_eq!(
        invalidation.source_epoch(),
        source_context.affinity().epoch()
    );
    assert_eq!(invalidation.target_epoch(), context.affinity().epoch());
    assert!(invalidation.clears_capacity_observations());
    assert!(invalidation.clears_continuation_anchor());
    assert!(source_manifest.family_artifact().is_some());
    assert!(target_manifest.family_artifact().is_none());
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
async fn verify_paid_compaction_reopen(
    cut: CompactionCrashCut,
    paths: &CompactionCrashPaths,
) -> TestResult {
    let source: SessionLocator = std::str::from_utf8(&std::fs::read(&paths.source)?)?.parse()?;
    let physical_starts =
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?;
    assert_eq!(physical_starts, 1);
    let recovery =
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&std::fs::read(&paths.oracle)?)?;
    let starts = Arc::new(AtomicU32::new(physical_starts));
    let sqlite = SqliteSessionStore::open_with_clock(
        &paths.database,
        Arc::new(FixedLeaseClock::new(Timestamp::new(100, 0)?)),
    )?;
    let store = Arc::new(CrashCutStore::observing(sqlite));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Required,
    ))?;
    let source_stored = store.load_checkpoint(source.checkpoint_id()).await?;
    let source_checkpoint = source_stored.checkpoint();
    assert!(source_checkpoint.manifest().family_artifact().is_some());
    let before = store.load_branch(source.session_ref()).await?;
    if cut == CompactionCrashCut::SuccessReceiptLost {
        assert_complete_compaction_success(source_checkpoint, before.checkpoint().checkpoint())?;
    } else {
        assert_unpublished_compaction_preserves_source(
            source_checkpoint,
            before.checkpoint().checkpoint(),
            cut != CompactionCrashCut::ProviderOutcomeNotCommitted,
        )?;
    }

    let workspace = Arc::new(WorkspacePolicy::host(&paths.root, ".")?);
    let model = CrashRecoveryModel::new(
        descriptor()?,
        b"sqlite-sigkill-account",
        recovery,
        starts,
        paths.physical_starts.clone(),
    )?;
    let recovered = Box::pin(sessions.recover(RecoverSessionRequest::new(
        SessionOperationId::new(PAID_COMPACTION_OPERATION)?,
        source,
        crash_recovery_runtime_binding(Arc::clone(&workspace), Arc::clone(&model))?,
    )))
    .await?;
    let target = match recovered {
        RecoverSessionOutcome::Compacted {
            source: recovered_source,
            target,
            ..
        } => {
            assert_eq!(recovered_source, source);
            target
        }
        other => return Err(format!("compaction did not recover success: {other:?}").into()),
    };
    let after = store.load_branch(source.session_ref()).await?;
    assert_eq!(
        after.head().checkpoint().checkpoint_id(),
        target.checkpoint_id()
    );
    assert_complete_compaction_success(source_checkpoint, after.checkpoint().checkpoint())?;
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        1
    );

    let commits = store.commit_count();
    let model_work = model.work_counts();
    let repeated = Box::pin(sessions.recover(RecoverSessionRequest::new(
        SessionOperationId::new(PAID_COMPACTION_OPERATION)?,
        source,
        crash_recovery_runtime_binding(workspace, Arc::clone(&model))?,
    )))
    .await?;
    assert!(matches!(
        repeated,
        RecoverSessionOutcome::Compacted {
            source: repeated_source,
            target: repeated_target,
            ..
        } if repeated_source == source && repeated_target == target
    ));
    assert_eq!(store.commit_count(), commits);
    assert_eq!(model.work_counts(), model_work);
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        1
    );
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
async fn verify_no_locator_reopen(
    cut: NoLocatorCrashCut,
    paths: &CompactionCrashPaths,
) -> TestResult {
    let operation_id = SessionOperationId::new(NO_LOCATOR_OPERATION)?;
    let source: SessionLocator = std::str::from_utf8(&std::fs::read(&paths.source)?)?.parse()?;
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        1,
        "the ambiguous operation did not have exactly one physical start"
    );
    let sqlite = SqliteSessionStore::open_with_clock(
        &paths.database,
        Arc::new(FixedLeaseClock::new(Timestamp::new(100, 0)?)),
    )?;
    let store = Arc::new(CrashCutStore::observing(sqlite));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Required,
    ))?;
    let source_stored = store.load_checkpoint(source.checkpoint_id()).await?;
    let source_checkpoint = source_stored.checkpoint();
    assert!(source_checkpoint.manifest().family_artifact().is_some());
    let before = store.load_branch(source.session_ref()).await?;
    assert_eq!(
        before
            .checkpoint()
            .checkpoint()
            .manifest()
            .canonical()
            .history(),
        source_checkpoint.manifest().canonical().history(),
        "the crash branch rewrote its immutable source semantics"
    );

    let workspace = Arc::new(WorkspacePolicy::host(&paths.root, ".")?);
    let starts = Arc::new(AtomicU32::new(1));
    let recovery_model = CrashRecoveryModel::no_locator(
        descriptor()?,
        b"sqlite-sigkill-no-locator-account",
        Arc::clone(&starts),
        paths.physical_starts.clone(),
        false,
        None,
    )?;
    let recovered = Box::pin(sessions.recover(RecoverSessionRequest::new(
        operation_id.clone(),
        source,
        crash_recovery_runtime_binding(Arc::clone(&workspace), Arc::clone(&recovery_model))?,
    )))
    .await?;
    let mut abandoned_target = match recovered {
        RecoverSessionOutcome::Unavailable {
            operation_id: recovered_operation,
            recovery,
            ..
        } => {
            assert_eq!(recovered_operation, operation_id);
            assert_eq!(recovery.locator().session_ref(), source.session_ref());
            assert_ne!(recovery.locator(), source);
            let working = store.load_branch(source.session_ref()).await?;
            let (_, compaction, provider) =
                compaction_workflow_states(working.checkpoint().checkpoint().manifest())?;
            let expected_working_state =
                matches!(compaction, DurableOperationState::OutcomeUnknown(_))
                    && matches!(provider, DurableOperationState::OutcomeUnknown(_));
            assert!(
                expected_working_state,
                "{cut:?}: compaction={compaction:?}, provider={provider:?}"
            );
            assert_eq!(
                working
                    .checkpoint()
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .history(),
                source_checkpoint.manifest().canonical().history()
            );
            None
        }
        RecoverSessionOutcome::Abandoned {
            operation_id: recovered_operation,
            source: recovered_source,
            target,
        } if cut == NoLocatorCrashCut::Abandoned => {
            assert_eq!(recovered_operation, operation_id);
            assert_eq!(recovered_source, source);
            Some(target)
        }
        other => return Err(format!("unexpected no-locator recovery: {other:?}").into()),
    };
    assert_eq!(recovery_model.work_counts(), (0, 0));
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        1
    );

    if abandoned_target.is_none() {
        abandoned_target = Some(
            match sessions
                .abandon_compaction(AbandonCompactionRequest::new(operation_id.clone(), source))
                .await?
            {
                AbandonCompactionOutcome::Abandoned {
                    operation_id: abandoned_operation,
                    source: abandoned_source,
                    target,
                } => {
                    assert_eq!(abandoned_operation, operation_id);
                    assert_eq!(abandoned_source, source);
                    target
                }
                other => return Err(format!("unexpected abandonment result: {other:?}").into()),
            },
        );
    }
    let abandoned_target = abandoned_target.ok_or("abandonment target is missing")?;
    let target_stored = store
        .load_checkpoint(abandoned_target.checkpoint_id())
        .await?;
    assert!(matches!(
        target_stored.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(
        target_stored.checkpoint().manifest().canonical().history(),
        source_checkpoint.manifest().canonical().history()
    );
    let commits = store.commit_count();
    let repeated = sessions
        .abandon_compaction(AbandonCompactionRequest::new(operation_id.clone(), source))
        .await?;
    assert!(matches!(
        repeated,
        AbandonCompactionOutcome::Abandoned {
            operation_id: repeated_operation,
            source: repeated_source,
            target: repeated_target,
        } if repeated_operation == operation_id
            && repeated_source == source
            && repeated_target == abandoned_target
    ));
    assert_eq!(store.commit_count(), commits);
    assert_eq!(recovery_model.work_counts(), (0, 0));

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:sqlite-sigkill-explicit-retry")?,
            abandoned_target,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"sqlite-sigkill-no-locator-account",
            )?,
        ))
        .await?
    else {
        return Err("abandonment target did not reopen as quiescent".into());
    };
    let retry_model = CrashRecoveryModel::no_locator(
        descriptor()?,
        b"sqlite-sigkill-no-locator-account",
        starts,
        paths.physical_starts.clone(),
        false,
        None,
    )?;
    let retry_operation = SessionOperationId::new(NO_LOCATOR_RETRY_OPERATION)?;
    let retry = sessions
        .compact(CompactSessionRequest::new(
            retry_operation.clone(),
            abandoned_target,
            CompactReason::Manual,
            crash_recovery_runtime_binding(workspace, Arc::clone(&retry_model))?,
        ))
        .await?;
    let retry_target = match retry {
        CompactSessionOutcome::Compacted {
            operation_id,
            source: retry_source,
            target,
            ..
        } => {
            assert_eq!(operation_id, retry_operation);
            assert_eq!(retry_source, abandoned_target);
            target
        }
        other => {
            return Err(format!("explicit new operation did not execute: {other:?}").into());
        }
    };
    assert_ne!(retry_operation, operation_id);
    assert_ne!(retry_target, abandoned_target);
    assert_ne!(retry_target, source);
    let retry_work = retry_model.work_counts();
    assert!(retry_work.0 > 0);
    assert_eq!(retry_work.1, 0);
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        2
    );
    let repeat_retry = sessions
        .compact(CompactSessionRequest::new(
            retry_operation.clone(),
            abandoned_target,
            CompactReason::Manual,
            crash_recovery_runtime_binding(
                Arc::new(WorkspacePolicy::host(&paths.root, ".")?),
                Arc::clone(&retry_model),
            )?,
        ))
        .await?;
    assert!(matches!(
        repeat_retry,
        CompactSessionOutcome::Compacted {
            operation_id,
            source: retry_source,
            target,
            ..
        } if operation_id == retry_operation
            && retry_source == abandoned_target
            && target == retry_target
    ));
    assert_eq!(retry_model.work_counts(), retry_work);
    assert_eq!(
        std::str::from_utf8(&std::fs::read(&paths.physical_starts)?)?.parse::<u32>()?,
        2
    );
    let _ = session.release().await;
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[tokio::test]
async fn sqlite_paid_compaction_sigkill_reopens_exactly_once() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    if let Some(cut) = std::env::var(PAID_COMPACTION_CHILD_ENV)
        .ok()
        .as_deref()
        .and_then(CompactionCrashCut::parse)
    {
        let root = std::env::var_os(PAID_COMPACTION_ROOT_ENV)
            .map(PathBuf::from)
            .ok_or("paid-compaction crash child root is missing")?;
        return Box::pin(run_paid_compaction_crash_child(
            cut,
            &CompactionCrashPaths::new(root),
        ))
        .await;
    }

    for cut in CompactionCrashCut::ALL {
        let directory = tempfile::tempdir()?;
        let paths = CompactionCrashPaths::new(directory.path().join(cut.name()));
        std::fs::create_dir(&paths.root)?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(PAID_COMPACTION_CRASH_TEST)
            .arg("--nocapture")
            .env(PAID_COMPACTION_CHILD_ENV, cut.name())
            .env(PAID_COMPACTION_ROOT_ENV, &paths.root)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillCrashChild::new(child);
        wait_for_compaction_cut(child.child(), &paths.marker)?;
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));
        Box::pin(verify_paid_compaction_reopen(cut, &paths)).await?;
    }
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[tokio::test]
async fn sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;

    if let Some(cut) = std::env::var(NO_LOCATOR_CHILD_ENV)
        .ok()
        .as_deref()
        .and_then(NoLocatorCrashCut::parse)
    {
        let root = std::env::var_os(NO_LOCATOR_ROOT_ENV)
            .map(PathBuf::from)
            .ok_or("no-locator crash child root is missing")?;
        return Box::pin(run_no_locator_crash_child(
            cut,
            &CompactionCrashPaths::new(root),
        ))
        .await;
    }

    for cut in NoLocatorCrashCut::ALL {
        let directory = tempfile::tempdir()?;
        let paths = CompactionCrashPaths::new(directory.path().join(cut.name()));
        std::fs::create_dir(&paths.root)?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(NO_LOCATOR_CRASH_TEST)
            .arg("--nocapture")
            .env(NO_LOCATOR_CHILD_ENV, cut.name())
            .env(NO_LOCATOR_ROOT_ENV, &paths.root)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = KillCrashChild::new(child);
        wait_for_compaction_cut(child.child(), &paths.marker)?;
        let status = child.sigkill()?;
        assert_eq!(status.signal(), Some(9));
        Box::pin(verify_no_locator_reopen(cut, &paths)).await?;
    }
    Ok(())
}

#[test]
fn operation_and_locator_are_portable_and_exact() -> Result<(), Box<dyn std::error::Error>> {
    let operation: SessionOperationId = "product:session:create:42".parse()?;
    assert_eq!(operation.as_str(), "product:session:create:42");
    assert!(SessionOperationId::new("contains space").is_err());
    assert!(SessionOperationId::new("x".repeat(MAX_OPERATION_ID_BYTES)).is_ok());
    assert!(SessionOperationId::new("x".repeat(MAX_OPERATION_ID_BYTES + 1)).is_err());

    let locator: SessionLocator = concat!(
        "00000000-0000-0000-0000-000000000001/",
        "00000000-0000-0000-0000-000000000002/",
        "00000000-0000-0000-0000-000000000003"
    )
    .parse()?;
    assert_eq!(locator.to_string().parse::<SessionLocator>()?, locator);
    assert!("not/a/locator".parse::<SessionLocator>().is_err());
    assert!(
        concat!(
            "00000000-0000-0000-0000-000000000000/",
            "00000000-0000-0000-0000-000000000002/",
            "00000000-0000-0000-0000-000000000003"
        )
        .parse::<SessionLocator>()
        .is_err()
    );
    Ok(())
}

#[test]
fn cancelled_workflow_does_not_strand_operation_reservation() -> TestResult {
    let registry = Arc::new(DriverRegistry::default());
    let operation = SessionOperationId::new("cancelled:create")?;
    let reservation = registry.reserve(&operation)?;
    assert!(matches!(
        registry.reserve(&operation),
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership
    ));

    drop(reservation);
    let retry = registry.reserve(&operation)?;
    drop(retry);
    Ok(())
}

#[tokio::test]
async fn compact_endpoint_unavailable_is_definitive_and_unknown_is_ambiguous() -> TestResult {
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let locator: SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse()?;
    for endpoint_error in [
        AgentSessionEndpointError::Backpressure,
        AgentSessionEndpointError::Unavailable,
    ] {
        let outcome = sessions
            .map_compact_outcome(
                SessionOperationId::new("compact:unavailable")?,
                locator,
                Err(endpoint_error),
            )
            .await?;
        assert!(
            matches!(outcome, CompactSessionOutcome::Unavailable { source, .. } if source == locator)
        );
    }
    let error = sessions
        .map_compact_outcome(
            SessionOperationId::new("compact:lost-reply")?,
            locator,
            Err(AgentSessionEndpointError::OutcomeUnknown),
        )
        .await
        .expect_err("a lost reply must not claim no admission");
    assert_eq!(error.kind(), AgentSessionsErrorKind::OutcomeUnknown);
    Ok(())
}

#[tokio::test]
async fn compact_endpoint_busy_and_stale_are_typed_pre_admission_refusals() -> TestResult {
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let locator: SessionLocator = "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003".parse()?;
    for (endpoint_error, kind) in [
        (
            AgentSessionEndpointError::Busy,
            AgentSessionsErrorKind::Busy,
        ),
        (
            AgentSessionEndpointError::Stale {
                session_id: locator.session_id(),
                branch_id: locator.branch_id(),
                checkpoint_id: locator.checkpoint_id(),
            },
            AgentSessionsErrorKind::StaleLocator,
        ),
        (
            AgentSessionEndpointError::Rejected {
                reason: "not a no-admission proof".into(),
            },
            AgentSessionsErrorKind::InvalidInput,
        ),
    ] {
        let error = sessions
            .map_compact_outcome(
                SessionOperationId::new("compact:typed-refusal")?,
                locator,
                Err(endpoint_error),
            )
            .await
            .expect_err("a refused endpoint must not return a compact result");
        assert_eq!(error.kind(), kind);
    }
    Ok(())
}

#[tokio::test]
async fn dropping_attached_session_unregisters_even_with_retained_agent() -> TestResult {
    let (_directory, binding) = runtime_binding()?;
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("drop:registration")?,
            binding,
        ))
        .await?;
    let agent = session.agent().clone();
    let branch = SessionBranchKey::new(agent.session_id(), agent.branch_id());
    assert!(sessions.drivers.endpoint(branch).is_some());
    drop(session);
    assert!(sessions.drivers.endpoint(branch).is_none());
    agent.shutdown().await?;
    Ok(())
}

#[test]
fn destructive_session_reservation_is_atomic_and_released() -> TestResult {
    let registry = Arc::new(DriverRegistry::default());
    let session_id = "00000000-0000-0000-0000-000000015f91".parse::<SessionId>()?;
    let reservation = registry.reserve_session(session_id)?;
    assert!(matches!(
        registry.reserve_session(session_id),
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership
    ));
    drop(reservation);
    let retry = registry.reserve_session(session_id)?;
    drop(retry);
    Ok(())
}

#[test]
fn pending_context_debug_redacts_user_content() -> TestResult {
    let locator: SessionLocator = concat!(
        "00000000-0000-0000-0000-000000000001/",
        "00000000-0000-0000-0000-000000000002/",
        "00000000-0000-0000-0000-000000000003"
    )
    .parse()?;
    let request = ContextStatusRequest::new(
        SessionOperationId::new("context:redaction")?,
        locator,
        ContextQuery::WithPendingUserInput("sensitive prompt sentinel".to_owned().into()),
    );
    let debug = format!("{request:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("sensitive prompt sentinel"));
    Ok(())
}

#[test]
fn compact_request_has_no_caller_summary_cut_or_count_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let locator: SessionLocator = concat!(
        "00000000-0000-0000-0000-000000000001/",
        "00000000-0000-0000-0000-000000000002/",
        "00000000-0000-0000-0000-000000000003"
    )
    .parse()?;
    let (_directory, runtime) = runtime_binding()?;
    let request = CompactSessionRequest::new(
        SessionOperationId::new("compact:1")?,
        locator,
        CompactReason::Manual,
        runtime,
    );
    assert_eq!(request.locator, locator);
    assert_eq!(request.reason, CompactReason::Manual);

    let (_directory, runtime) = runtime_binding()?;
    let request = CompactSessionRequest::new(
        SessionOperationId::new("compact:scheduled-pending")?,
        locator,
        CompactReason::Scheduled,
        runtime,
    )
    .with_pending_user_input("sensitive queued prompt".to_owned().into());
    assert!(matches!(
        request.qualification,
        ContextQuery::WithPendingUserInput(_)
    ));
    let debug = format!("{request:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("sensitive queued prompt"));
    Ok(())
}

#[tokio::test]
async fn pending_compaction_qualification_requires_scheduled_reason()
-> Result<(), Box<dyn std::error::Error>> {
    let locator: SessionLocator = concat!(
        "00000000-0000-0000-0000-000000000001/",
        "00000000-0000-0000-0000-000000000002/",
        "00000000-0000-0000-0000-000000000003"
    )
    .parse()?;
    let (_directory, runtime) = runtime_binding()?;
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let error = sessions
        .compact(
            CompactSessionRequest::new(
                SessionOperationId::new("compact:manual-pending-invalid")?,
                locator,
                CompactReason::Manual,
                runtime,
            )
            .with_pending_user_input("ephemeral pending input".to_owned().into()),
        )
        .await
        .expect_err("manual compaction accepted pending-input qualification");
    assert_eq!(error.kind(), AgentSessionsErrorKind::InvalidInput);
    assert_eq!(error.current_locator(), None);
    Ok(())
}

#[tokio::test]
async fn create_repeat_exact_open_and_release_use_one_public_workflow() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let create_operation = SessionOperationId::new("create:lost-ack-stable")?;

    let created = sessions
        .create(CreateSessionRequest::new(
            create_operation.clone(),
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await?;
    let CreateSessionOutcome::Created {
        operation_id,
        session,
    } = created;
    assert_eq!(operation_id, create_operation);
    let genesis = session.locator();
    assert_eq!(session.binding().protocol().as_str(), "test.protocol");
    let result = session
        .agent()
        .prompt("advance the exact durable locator")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_ne!(session.locator(), genesis);

    let active_repeat = sessions
        .create(CreateSessionRequest::new(
            create_operation.clone(),
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?,
        ))
        .await;
    let Err(active_repeat) = active_repeat else {
        return Err("one operation attached a second active private driver".into());
    };
    assert_eq!(active_repeat.kind(), AgentSessionsErrorKind::Ownership);
    let active_archive = sessions.archive(session.locator()).await;
    let Err(active_archive) = active_archive else {
        return Err("an attached private driver was archived".into());
    };
    assert_eq!(active_archive.kind(), AgentSessionsErrorKind::Ownership);
    let locator = session.release().await?;
    assert_ne!(locator, genesis);
    assert_eq!(
        sessions.accepted_create(&create_operation).await?,
        Some(genesis)
    );
    let stale = sessions
        .created_job_config(genesis)
        .await
        .expect_err("genesis must not adopt a later head");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(locator));

    let repeated = sessions
        .create(CreateSessionRequest::new(
            create_operation.clone(),
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?,
        ))
        .await;
    let Err(repeated) = repeated else {
        return Err("stale genesis create receipt attached an ordinary driver".into());
    };
    assert_eq!(repeated.kind(), AgentSessionsErrorKind::Integrity);

    let open_operation = SessionOperationId::new("open:exact-after-release")?;
    let opened = sessions
        .open(OpenSessionRequest::new(
            open_operation.clone(),
            locator,
            runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let OpenSessionOutcome::Opened {
        operation_id,
        session,
    } = opened
    else {
        return Err("quiescent exact open unexpectedly required recovery".into());
    };
    assert_eq!(operation_id, open_operation);
    assert_eq!(session.locator(), locator);
    let released = session.release().await?;
    let archived = sessions.archive(released).await?;
    assert_eq!(archived.session_id(), released.session_id());
    assert_eq!(
        archived.archived_revision(),
        archived.previous_revision() + 1
    );
    let deleted = sessions
        .delete(archived.session_id(), archived.archived_revision())
        .await?;
    assert_eq!(deleted.session_id(), released.session_id());
    assert_eq!(deleted.archived_revision(), archived.archived_revision());
    assert_eq!(
        sessions
            .recover_delete(archived.session_id(), archived.archived_revision())
            .await?,
        deleted
    );
    Ok(())
}

#[tokio::test]
async fn recover_effect_evidence_on_quiescent_session_returns_not_required() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let operation = SessionOperationId::new("recover:effect-not-required")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            operation.clone(),
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"recover-effect-not-required",
            )?,
        ))
        .await?;
    let _ = session
        .agent()
        .prompt("advance to a quiescent checkpoint")
        .await?
        .result()
        .await?;
    let locator = session.release().await?;
    let executor = ToolExecutorId::new("test.recover-effect-executor.v1")?;
    let outcome = ToolOutcome::success_text("terminal evidence")?;
    let request = RecoverEffectEvidenceRequest::new(
        operation.clone(),
        locator,
        1,
        SessionEffectEvidence::prepared(),
        executor.clone(),
        outcome,
    );
    let first = sessions.recover_effect_evidence(request).await?;
    let RecoverEffectEvidenceOutcome::NotRequired {
        ref operation_id,
        source,
    } = first
    else {
        return Err(format!("quiescent effect recovery returned: {first:?}").into());
    };
    assert_eq!(operation_id, &operation);
    assert_eq!(source, locator);
    let retry = RecoverEffectEvidenceRequest::new(
        operation,
        locator,
        1,
        SessionEffectEvidence::prepared(),
        executor,
        ToolOutcome::success_text("terminal evidence")?,
    );
    assert_eq!(sessions.recover_effect_evidence(retry).await?, first);
    Ok(())
}

#[tokio::test]
async fn history_page_on_created_session_returns_page_or_honest_resnapshot() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let operation = SessionOperationId::new("history:page-quiescent")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            operation,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"history-page-quiescent",
            )?,
        ))
        .await?;
    let _ = session
        .agent()
        .prompt("record one completed turn")
        .await?
        .result()
        .await?;
    let locator = session.release().await?;
    let stale_revision = SessionHistoryPageRequest::new(
        locator,
        locator.checkpoint_id(),
        SessionHistoryVisibility::Metadata,
        SESSION_HISTORY_VISIBILITY_REVISION.saturating_add(1),
        SessionHistoryDirection::Forward,
        32,
        4096,
        None,
    );
    assert_eq!(
        sessions.history_page(stale_revision).await?,
        SessionHistoryPageOutcome::Resnapshot {
            locator,
            reason: SessionHistoryResnapshot::VisibilityRevision,
        }
    );
    let page = sessions
        .history_page(SessionHistoryPageRequest::new(
            locator,
            locator.checkpoint_id(),
            SessionHistoryVisibility::Metadata,
            SESSION_HISTORY_VISIBILITY_REVISION,
            SessionHistoryDirection::Forward,
            32,
            4096,
            None,
        ))
        .await?;
    match page {
        SessionHistoryPageOutcome::Page {
            locator: page_locator,
            visibility,
            history_identity: _,
            items,
            next,
        } => {
            assert_eq!(page_locator, locator);
            assert_eq!(visibility, SessionHistoryVisibility::Metadata);
            assert_ne!(items, [] as [SessionHistoryItem; 0]);
            assert!(items.iter().all(|item| item.display_text().is_none()));
            assert_eq!(next, None);
        }
        other => {
            return Err(format!("quiescent history page returned: {other:?}").into());
        }
    }
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "both paging directions, visibility, exactness and unchanged store authority share one SQLite fixture"
)]
async fn sqlite_history_inspection_pages_committed_content_without_mutation() -> TestResult {
    let request = |locator: SessionLocator, visibility, cursor| {
        SessionHistoryPageRequest::new(
            locator,
            locator.checkpoint_id(),
            visibility,
            SESSION_HISTORY_VISIBILITY_REVISION,
            SessionHistoryDirection::Forward,
            1,
            65_536,
            cursor,
        )
    };
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("history-inspect.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = SqliteSessionStore::open_with_clock(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("history:inspect:create")?,
            durable_completing_runtime_binding_for(workspace, descriptor()?, b"history-inspect")?,
        ))
        .await?;
    session
        .agent()
        .prompt("read-only committed prompt")
        .await?
        .result()
        .await?;
    let source = session.release().await?;
    let before = store.stats().await?;
    let mut cursor = None;
    let mut first_cursor = None;
    let mut content = Vec::new();
    loop {
        let page = AgentSessions::inspect_history_page_sqlite(
            &database,
            &request(source, SessionHistoryVisibility::Content, cursor),
        )?;
        let SessionHistoryPageOutcome::Page { items, next, .. } = page else {
            return Err("exact inspection requested a resnapshot".into());
        };
        assert_eq!(items.len(), 1);
        content.extend(items);
        first_cursor = first_cursor.or(next);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(content.len() >= 2);
    let mut cursor = None;
    let mut reverse = Vec::new();
    for _ in 0..content.len() {
        let mut backward = request(source, SessionHistoryVisibility::Content, cursor);
        backward.direction = SessionHistoryDirection::Backward;
        let page = AgentSessions::inspect_history_page_sqlite(&database, &backward)?;
        let SessionHistoryPageOutcome::Page { items, next, .. } = page else {
            return Err("backward inspection requested a resnapshot".into());
        };
        assert_eq!(items.len(), 1);
        reverse.extend(items.into_iter().map(|item| item.entry_id()));
        cursor = next;
    }
    assert!(
        cursor.is_none(),
        "backward EOF must not mint another cursor"
    );
    reverse.reverse();
    assert_eq!(
        reverse,
        content
            .iter()
            .map(SessionHistoryItem::entry_id)
            .collect::<Vec<_>>()
    );
    assert!(
        content
            .iter()
            .any(|item| item.display_text() == Some("read-only committed prompt"))
    );
    let metadata = AgentSessions::inspect_history_page_sqlite(
        &database,
        &request(source, SessionHistoryVisibility::Metadata, None),
    )?;
    let SessionHistoryPageOutcome::Page { items, .. } = metadata else {
        return Err("metadata inspection requested a resnapshot".into());
    };
    assert!(items.iter().all(|item| item.display_text().is_none()));
    let missing = directory.path().join("absent.sqlite3");
    assert_eq!(
        AgentSessions::inspect_history_page_sqlite(
            &missing,
            &request(source, SessionHistoryVisibility::Content, None),
        )
        .expect_err("read-only inspection must not create a missing database")
        .kind(),
        AgentSessionsErrorKind::NotFound
    );
    assert!(!missing.exists());
    assert_eq!(store.stats().await?, before);
    let current = install_family_artifact(&store, source).await?;
    let before = store.stats().await?;
    let stale = AgentSessions::inspect_history_page_sqlite(
        &database,
        &request(source, SessionHistoryVisibility::Content, None),
    )
    .expect_err("read-only history inspection must not adopt a stale locator");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(current));
    assert_eq!(
        AgentSessions::inspect_history_page_sqlite(
            &database,
            &request(current, SessionHistoryVisibility::Content, first_cursor),
        )?,
        SessionHistoryPageOutcome::Resnapshot {
            locator: current,
            reason: SessionHistoryResnapshot::Checkpoint,
        }
    );
    assert_eq!(store.stats().await?, before);
    Ok(())
}

#[tokio::test]
async fn facade_open_restores_hosted_working_turn_and_cleans_driver_registration() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = descriptor()?;
    let account = b"hosted-working-account";
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(HostedPauseModel {
        scripted: Arc::new(
            plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
                .with_runtime_authority(authority)
                .with_context_observation(crate::test_model::context_observation()?),
        ),
        calls: AtomicUsize::new(0),
    });
    let memory = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let store = Arc::new(LoseForkAckStore::stopping_after_hosted_working(memory));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:hosted-working")?,
            hosted_pause_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor.clone(),
                account,
                Arc::clone(&model),
            )?,
        ))
        .await?;
    let original = session.agent().prompt("pause on the provider").await?;
    let original_turn = original.id();
    let interrupted = original.result().await?;
    assert!(matches!(
        interrupted.outcome(),
        TurnResultOutcome::RecoveryRequired(error)
            if error.kind() == AgentErrorKind::OutcomeUnknown
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    let working = session.locator();
    assert!(matches!(
        store
            .load_branch(working.session_ref())
            .await?
            .checkpoint()
            .bundle()
            .validated_checkpoint()?
            .manifest()
            .lifecycle(),
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::HostedContinuation,
            ..
        }
    ));
    let _ = session.release().await;
    store.resume_commits();

    let open_operation = SessionOperationId::new("open:hosted-working")?;
    let OpenSessionOutcome::RestoredWorking {
        operation_id,
        session,
        turn,
    } = sessions
        .open(OpenSessionRequest::new(
            open_operation.clone(),
            working,
            hosted_pause_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor.clone(),
                account,
                Arc::clone(&model),
            )?,
        ))
        .await?
    else {
        return Err("supported facade did not restore hosted Working state".into());
    };
    assert_eq!(operation_id, open_operation);
    assert_eq!(turn.turn_id(), original_turn);
    assert!(turn.pending_decision().is_none());

    let repeated = sessions
        .open(OpenSessionRequest::new(
            open_operation,
            working,
            hosted_pause_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor.clone(),
                account,
                Arc::clone(&model),
            )?,
        ))
        .await;
    assert!(matches!(
        repeated,
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership
    ));
    let blocked_operation = SessionOperationId::new("open:hosted-working-blocked")?;
    let blocked = sessions
        .open(OpenSessionRequest::new(
            blocked_operation.clone(),
            working,
            hosted_pause_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor.clone(),
                account,
                Arc::clone(&model),
            )?,
        ))
        .await;
    assert!(matches!(
        blocked,
        Err(error) if error.kind() == AgentSessionsErrorKind::Ownership
    ));

    let recovered = turn.result().result().await?;
    assert!(matches!(
        recovered.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    let quiescent = session.release().await?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            blocked_operation,
            quiescent,
            hosted_pause_runtime_binding_for(workspace, descriptor, account, model)?,
        ))
        .await?
    else {
        return Err("released recovery registration or failed reservation was retained".into());
    };
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn native_shaped_reopen_accepts_freshness_only_descriptor_rotation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let created_descriptor = descriptor_for_adapter_at(
        "openai.codex.subscription.responses",
        "gpt-5.3-codex-spark",
        "openai.codex.responses.v1",
        Timestamp::new(0, 0)?,
    )?;
    let reopened_descriptor = descriptor_for_adapter_at(
        "openai.codex.subscription.responses",
        "gpt-5.3-codex-spark",
        "openai.codex.responses.v1",
        Timestamp::new(1, 0)?,
    )?;
    assert_ne!(
        created_descriptor.binding_digest()?,
        reopened_descriptor.binding_digest()?,
        "attempt/freshness-sensitive descriptor identity must rotate"
    );
    assert_eq!(
        created_descriptor.semantic_binding_digest()?,
        reopened_descriptor.semantic_binding_digest()?,
        "behavior-bearing descriptor identity must remain exact"
    );

    let create_runtime = completing_runtime_binding_for(
        Arc::clone(&workspace),
        created_descriptor,
        b"native-shaped-codex-account",
    )?;
    let create_binding = create_runtime.target_binding();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:native-shaped-freshness")?,
            create_runtime,
        ))
        .await?;
    let result = session
        .agent()
        .prompt("persist one native-shaped turn")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let locator = session.release().await?;

    let reopen_runtime = completing_runtime_binding_for(
        workspace,
        reopened_descriptor,
        b"native-shaped-codex-account",
    )?;
    let reopen_binding = reopen_runtime.target_binding();
    assert_eq!(create_binding.protocol(), reopen_binding.protocol());
    assert_eq!(create_binding.route(), reopen_binding.route());
    assert_eq!(create_binding.model(), reopen_binding.model());
    assert_eq!(
        create_binding.selected_provider_configuration(),
        reopen_binding.selected_provider_configuration()
    );
    assert_eq!(
        create_binding.effective_provider_configuration(),
        reopen_binding.effective_provider_configuration()
    );
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:native-shaped-freshness")?,
            locator,
            reopen_runtime,
        ))
        .await?
    else {
        return Err("freshness-only native-shaped reopen was not attached".into());
    };
    assert_eq!(session.locator(), locator);
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn context_status_current_and_pending_are_exact_and_read_only() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let (runtime, model) = runtime_binding_with_model(workspace, descriptor()?, b"context-status")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:context-status")?,
            runtime,
        ))
        .await?;
    let locator = session.locator();
    let checkpoint = agent_checkpoint(session.agent());
    let history_identity = checkpoint.manifest().canonical().history().identity();
    let before = store.load_branch(locator.session_ref()).await?;
    let before_stats = store.stats()?;

    let current = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:current")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    let pending = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:pending")?,
            locator,
            ContextQuery::WithPendingUserInput("ephemeral pending input".to_owned().into()),
        ))
        .await?;
    let SessionContextStatusKind::Available(current) = current.kind() else {
        return Err("current exact status was unavailable".into());
    };
    let SessionContextStatusKind::Available(pending) = pending.kind() else {
        return Err("pending exact status was unavailable".into());
    };
    assert!(matches!(current.kind(), ContextStatusKind::Fits { .. }));
    assert!(matches!(pending.kind(), ContextStatusKind::Fits { .. }));
    assert_ne!(
        current.model_input_identity(),
        pending.model_input_identity()
    );
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    let after_checkpoint = agent_checkpoint(session.agent());
    assert_eq!(after_checkpoint.id(), checkpoint.id());
    assert_eq!(
        after_checkpoint.manifest().canonical().history().identity(),
        history_identity
    );
    let after = store.load_branch(locator.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats()?, before_stats);
    session.release().await?;
    Ok(())
}

#[cfg(feature = "native-subscriptions")]
#[tokio::test]
async fn context_status_turn_scoped_inspect_is_available_without_operation_root() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let (runtime, model) =
        runtime_binding_with_turn_scoped_model(workspace, descriptor()?, b"turn-scoped-inspect")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:turn-scoped-inspect")?,
            runtime,
        ))
        .await?;
    let locator = session.locator();
    let before = store.load_branch(locator.session_ref()).await?;
    let before_stats = store.stats()?;

    let current = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:turn-scoped-current")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    let pending = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:turn-scoped-pending")?,
            locator,
            ContextQuery::WithPendingUserInput("ephemeral pending input".to_owned().into()),
        ))
        .await?;
    let SessionContextStatusKind::Available(current) = current.kind() else {
        return Err("turn-scoped current exact status was unavailable".into());
    };
    let SessionContextStatusKind::Available(pending) = pending.kind() else {
        return Err("turn-scoped pending exact status was unavailable".into());
    };
    assert!(matches!(current.kind(), ContextStatusKind::Fits { .. }));
    assert!(matches!(pending.kind(), ContextStatusKind::Fits { .. }));
    assert_ne!(
        current.model_input_identity(),
        pending.model_input_identity()
    );
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    let after = store.load_branch(locator.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats()?, before_stats);
    session.release().await?;
    Ok(())
}

#[cfg(feature = "native-subscriptions")]
#[tokio::test]
async fn context_status_turn_scoped_inspect_reports_overflow_without_operation_root() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let observed = timestamp()?;
    let descriptor = crate::test_model::with_shared_window_context_contract(
        descriptor_without_context_contract("test.route", "test.model")?,
        observed,
        80,
    )?;
    let (runtime, model) = runtime_binding_with_turn_scoped_model(
        workspace,
        descriptor,
        b"turn-scoped-overflow-inspect",
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:turn-scoped-overflow-inspect")?,
            runtime,
        ))
        .await?;
    let locator = session.locator();
    let status = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:turn-scoped-overflow")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    let SessionContextStatusKind::Available(status) = status.kind() else {
        return Err("turn-scoped overflow inspect was unavailable".into());
    };
    assert!(
        matches!(
            status.kind(),
            ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_)
        ),
        "turn-scoped overflow inspect returned {:?}",
        status.kind()
    );
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn context_status_missing_contract_is_available_unavailable_without_start() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let (runtime, model) = runtime_binding_with_model(
        workspace,
        descriptor_without_context_contract("test.route", "test.model")?,
        b"missing-contract",
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:missing-contract")?,
            runtime,
        ))
        .await?;
    let locator = session.locator();
    let before = store.load_branch(locator.session_ref()).await?;
    let before_stats = store.stats()?;
    let status = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:missing-contract")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    assert!(matches!(
        status.kind(),
        SessionContextStatusKind::Available(status)
            if matches!(status.kind(), ContextStatusKind::Unavailable(_))
    ));
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    let after = store.load_branch(locator.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats()?, before_stats);
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn exact_open_refuses_a_session_this_build_would_rebuild_differently() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:build-drift")?,
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let CreateSessionOutcome::Created { session, .. } = created;
    let locator = session.release().await?;

    // The same model and account, but a tool policy a later build would bind.
    let drifted =
        custom_runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?;
    let error = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:build-drift")?,
            locator,
            drifted,
        ))
        .await
        .expect_err("a session this build rebuilds differently opened");
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::IncompatibleBuild,
        "{error}"
    );
    assert_eq!(error.incompatible_session(), Some(locator.session_id()));
    assert!(error.to_string().starts_with(&format!(
        "session {} was created by an incompatible build; start a new session",
        locator.session_id()
    )));
    assert!(error.to_string().contains("(reconstruction:"));

    // The refusal released its lease and left the session as it was.
    let reopened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:same-build")?,
            locator,
            runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = reopened else {
        return Err("the writing build could not reopen its own session".into());
    };
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn exact_open_rejects_auth_and_model_drift_and_releases_failed_leases() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:drift")?,
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let CreateSessionOutcome::Created { session, .. } = created;
    let locator = session.release().await?;

    let auth_error = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:auth-drift")?,
            locator,
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-beta")?,
        ))
        .await;
    let Err(auth_error) = auth_error else {
        return Err("changed account authority opened an exact checkpoint".into());
    };
    assert_eq!(auth_error.kind(), AgentSessionsErrorKind::Revalidation);

    let model_error = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:model-drift")?,
            locator,
            runtime_binding_for(
                Arc::clone(&workspace),
                descriptor_for("test.other-route", "test.other-model")?,
                b"account-alpha",
            )?,
        ))
        .await;
    let Err(model_error) = model_error else {
        return Err("changed model binding opened an exact checkpoint".into());
    };
    assert_eq!(model_error.kind(), AgentSessionsErrorKind::Revalidation);
    assert_eq!(model_error.incompatible_session(), None);

    let reopened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:after-drift")?,
            locator,
            runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = reopened else {
        return Err("failed attachment retained the branch lease".into());
    };
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn unavailable_compaction_leaves_exact_source_unchanged_and_retries_stable() -> TestResult {
    let (_directory, workspace, sessions, store, locator) = Box::pin(seeded_service()).await?;
    let before = store.load_branch(locator.session_ref()).await?;
    let before_stats = store.stats()?;

    let context = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("context:1")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    assert!(matches!(
        context.kind(),
        SessionContextStatusKind::Unavailable(SessionWorkflowUnavailable::ContextStatus)
    ));
    assert_eq!(context.operation_id().as_str(), "context:1");

    for operation in ["compact:lost-ack", "compact:lost-ack"] {
        let runtime = runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha")?;
        let compact = sessions
            .compact(CompactSessionRequest::new(
                SessionOperationId::new(operation)?,
                locator,
                CompactReason::Scheduled,
                runtime,
            ))
            .await?;
        assert!(matches!(
            compact,
            CompactSessionOutcome::Unavailable {
                source,
                reason: SessionWorkflowUnavailable::Compaction,
                ..
            } if source == locator
        ));
    }

    let runtime = runtime_binding_for(workspace, descriptor()?, b"account-alpha")?;
    let recovered = Box::pin(sessions.recover(RecoverSessionRequest::new(
        SessionOperationId::new("recover:1")?,
        locator,
        runtime,
    )))
    .await?;
    assert!(matches!(
        recovered,
        RecoverSessionOutcome::NotRequired { source, .. } if source == locator
    ));

    let after = store.load_branch(locator.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats()?, before_stats);
    Ok(())
}

#[tokio::test]
async fn attached_compaction_maps_no_dispatch_driver_outcomes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:compact-no-dispatch")?,
            runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"compact-account")?,
        ))
        .await?;
    let source = session.locator();

    let scheduled = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new("compact:scheduled-fits")?,
            source,
            CompactReason::Scheduled,
            compaction_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"compact-account",
                LocalDurabilityDisposition::Ephemeral,
            )?,
        ))
        .await?;
    assert!(matches!(
        scheduled,
        CompactSessionOutcome::NoEligibleRange {
            operation_id,
            source: unchanged,
        } if operation_id.as_str() == "compact:scheduled-fits" && unchanged == source
    ));

    let manual = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new("compact:manual-empty")?,
            source,
            CompactReason::Manual,
            compaction_runtime_binding_for(
                workspace,
                descriptor()?,
                b"compact-account",
                LocalDurabilityDisposition::Ephemeral,
            )?,
        ))
        .await?;
    assert!(matches!(
        manual,
        CompactSessionOutcome::NoEligibleRange {
            operation_id,
            source: unchanged,
        } if operation_id.as_str() == "compact:manual-empty" && unchanged == source
    ));
    assert_eq!(session.locator(), source);
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn compact_and_recover_return_finalized_receipt_before_source_freshness() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:compact-finalized")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"compact-finalized-account",
            )?,
        ))
        .await?;
    for input in ["older intent", "latest protected intent"] {
        let result = session.agent().prompt(input).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = session.locator();
    let operation_id = SessionOperationId::new("compact:finalized-receipt")?;

    let compacted = sessions
        .compact(CompactSessionRequest::new(
            operation_id.clone(),
            source,
            CompactReason::Manual,
            completing_compaction_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"compact-finalized-account",
                LocalDurabilityDisposition::Ephemeral,
            )?,
        ))
        .await?;
    let CompactSessionOutcome::Compacted {
        source: receipt_source,
        target,
        ..
    } = compacted
    else {
        return Err("driver compaction did not publish one complete successor".into());
    };
    assert_eq!(receipt_source, source);
    assert_ne!(target, source);
    assert_eq!(session.locator(), target);

    let repeated = sessions
        .compact(CompactSessionRequest::new(
            operation_id.clone(),
            source,
            CompactReason::Manual,
            completing_compaction_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"compact-finalized-account",
                LocalDurabilityDisposition::Ephemeral,
            )?,
        ))
        .await?;
    assert!(matches!(
        repeated,
        CompactSessionOutcome::Compacted {
            source: repeated_source,
            target: repeated_target,
            ..
        } if repeated_source == source && repeated_target == target
    ));

    let recovered = Box::pin(sessions.recover(RecoverSessionRequest::new(
        operation_id,
        source,
        completing_runtime_binding_for(workspace, descriptor()?, b"compact-finalized-account")?,
    )))
    .await?;
    assert!(matches!(
        recovered,
        RecoverSessionOutcome::Compacted {
            source: recovered_source,
            target: recovered_target,
            ..
        } if recovered_source == source && recovered_target == target
    ));
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_fork_is_exact_source_preserving_and_clears_unproved_state() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fork-source")?,
            completing_runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"fork-account")?,
        ))
        .await?;
    let result = session
        .agent()
        .prompt("retain canonical history")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let source = install_family_artifact(&store, session.release().await?).await?;
    let before = store.load_branch(source.session_ref()).await?;
    let before_stats = store.stats()?;
    assert!(
        before
            .checkpoint()
            .checkpoint()
            .manifest()
            .family_artifact()
            .is_some()
    );

    let operation = SessionOperationId::new("fork:ordinary:stable")?;
    let ForkSessionOutcome::Forked {
        operation_id,
        source: reported_source,
        target,
        portability,
        ..
    } = sessions
        .fork(ForkSessionRequest::new(operation.clone(), source))
        .await?
    else {
        return Err("quiescent ordinary fork did not publish".into());
    };
    assert_eq!(operation_id, operation);
    assert_eq!(reported_source, source);
    assert_eq!(portability, ArtifactPortability::ClearedUnproved);
    assert_ne!(target.branch_id(), source.branch_id());
    let source_after = store.load_branch(source.session_ref()).await?;
    assert_eq!(source_after.head(), before.head());
    assert_eq!(
        source_after.checkpoint().reference(),
        before.checkpoint().reference()
    );

    let target_loaded = store.load_branch(target.session_ref()).await?;
    assert_eq!(
        target_loaded.checkpoint().bundle().base_root(),
        Some(before.checkpoint().reference().root())
    );
    assert!(
        target_loaded
            .checkpoint()
            .bundle()
            .stats()
            .new_object_count()
            < target_loaded.checkpoint().bundle().stats().object_count()
    );
    assert_eq!(
        target_loaded
            .checkpoint()
            .bundle()
            .stats()
            .new_object_count(),
        target_loaded.checkpoint().bundle().supplied_objects().len()
    );
    let source_checkpoint = before.checkpoint().checkpoint();
    let target_checkpoint = target_loaded.checkpoint().checkpoint();
    let source_manifest = source_checkpoint.manifest();
    let target_manifest = target_checkpoint.manifest();
    assert_eq!(target_loaded.head().revision().get(), 0);
    assert_eq!(
        target_manifest.topology().parent_checkpoint_id,
        Some(source.checkpoint_id())
    );
    assert_eq!(
        target_manifest.topology().fork,
        Some(plakous_agent::internal::ForkLineage {
            session_id: source.session_id(),
            branch_id: source.branch_id(),
            checkpoint_id: source.checkpoint_id(),
            branch_revision: before.head().revision().get(),
        })
    );
    assert_eq!(
        target_manifest.canonical().history(),
        source_manifest.canonical().history()
    );
    assert_eq!(
        target_manifest.canonical().working(),
        source_manifest.canonical().working()
    );
    assert_eq!(
        target_manifest.canonical().stable_prefix(),
        source_manifest.canonical().stable_prefix()
    );
    assert_eq!(target_manifest.binding(), source_manifest.binding());
    assert_eq!(target_manifest.tools(), source_manifest.tools());
    assert_eq!(target_manifest.ledger(), source_manifest.ledger());
    assert!(target_manifest.family_artifact().is_none());
    let target_context = target_manifest.canonical().context();
    assert_ne!(
        target_context.affinity().identity(),
        source_manifest.canonical().context().affinity().identity()
    );
    assert!(target_context.occupancy_anchor().is_none());
    assert_eq!(
        target_context
            .transition()
            .map(RuntimeContextTransition::kind),
        Some(RuntimeContextTransitionKind::OrdinaryFork)
    );
    let after_stats = store.stats()?;
    assert_eq!(after_stats.sessions(), before_stats.sessions());
    assert_eq!(after_stats.branches(), before_stats.branches() + 1);
    assert_eq!(after_stats.checkpoints(), before_stats.checkpoints() + 1);

    let repeated = sessions
        .fork(ForkSessionRequest::new(operation, source))
        .await?;
    assert!(matches!(
        repeated,
        ForkSessionOutcome::Forked {
            source: exact_source,
            target: exact_target,
            portability: ArtifactPortability::ClearedUnproved,
            ..
        } if exact_source == source && exact_target == target
    ));
    assert_eq!(store.stats()?, after_stats);

    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:fork-target")?,
            target,
            runtime_binding_for(workspace, descriptor()?, b"fork-account")?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("compatible fork target did not reopen".into());
    };
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_fork_recovers_an_accepted_lost_ack_without_republication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let inner = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let store = Arc::new(LoseForkAckStore::new(inner.clone()));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store,
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fork-lost-ack")?,
            runtime_binding_for(workspace, descriptor()?, b"lost-ack-account")?,
        ))
        .await?;
    let source = session.release().await?;
    let before = inner.stats()?;
    let ForkSessionOutcome::Forked { target, .. } = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:lost-ack")?,
            source,
        ))
        .await?
    else {
        return Err("accepted fork ack loss was not exactly recovered".into());
    };
    let after = inner.stats()?;
    assert_ne!(target.branch_id(), source.branch_id());
    assert_eq!(after.branches(), before.branches() + 1);
    assert_eq!(after.checkpoints(), before.checkpoints() + 1);
    Ok(())
}

/// A fork whose publication and lease releases both fail reports the
/// publication's cause, not the release failure.
#[tokio::test]
async fn ordinary_fork_keeps_its_publication_cause_when_releases_fail_too() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let inner = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let store = Arc::new(LoseForkAckStore::passing_through(inner));
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::clone(&store) as Arc<dyn SessionStore>,
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fork-cleanup")?,
            runtime_binding_for(workspace, descriptor()?, b"cleanup-account")?,
        ))
        .await?;
    let source = session.release().await?;
    store.fail_fork_publication_and_release();
    let Err(error) = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:cleanup")?,
            source,
        ))
        .await
    else {
        return Err("a fork whose target cannot be loaded published".into());
    };
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::Pruned,
        "the release failure replaced the publication cause: {error}"
    );
    Ok(())
}

/// Reading a session's created output bound names the head when the locator lags.
///
/// The hosted API lane reads this before it compiles anything, so it is the
/// first place a record that has fallen behind the durable head is noticed. If
/// the failure arrived without the head, the caller's stale-head adoption could
/// not retry and every API session whose record lagged would refuse to reopen,
/// which is the one case adoption exists for.
#[tokio::test]
async fn construction_fact_reads_name_the_current_head_when_the_locator_lags() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:created-bound-read")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"created-bound-account",
            )?,
        ))
        .await?;
    let created = session.release().await?;

    // At the head, the read answers with the bound the session was compiled
    // under rather than failing.
    let at_head = sessions.created_request_output_bound(created).await?;
    let jobs_at_head = sessions.created_job_config(created).await?;
    assert_eq!(jobs_at_head, plakous_tools::JobConfig::default());

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:advance-created-bound")?,
            created,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"created-bound-account",
            )?,
        ))
        .await?
    else {
        return Err("the session did not reopen".into());
    };
    let outcome = session
        .agent()
        .prompt("advance the head")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let advanced = session.release().await?;
    assert_ne!(advanced, created);

    // The record now lags. The read must say so in a way the caller can act on.
    let lagging = sessions
        .created_request_output_bound(created)
        .await
        .expect_err("a lagging locator is not the head");
    assert_eq!(lagging.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(lagging.current_locator(), Some(advanced));
    let lagging_jobs = sessions
        .created_job_config(created)
        .await
        .expect_err("job bounds must not fall back when the locator lags");
    assert_eq!(lagging_jobs.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(lagging_jobs.current_locator(), Some(advanced));

    // And adopting that head answers, with the same bound: advancing a session
    // never changes what it was created under.
    assert_eq!(
        sessions.created_request_output_bound(advanced).await?,
        at_head
    );
    assert_eq!(sessions.created_job_config(advanced).await?, jobs_at_head);
    Ok(())
}

#[cfg(feature = "composition")]
#[tokio::test]
async fn stored_head_uses_exact_historical_evaluator_not_byte_identical_sibling() -> TestResult {
    historical_evaluator_round_trip(0).await
}

#[cfg(feature = "composition")]
#[tokio::test]
async fn product_javascript_receipt_round_trips_through_stored_binding() -> TestResult {
    historical_evaluator_round_trip(1).await
}

#[cfg(feature = "composition-python")]
#[tokio::test]
async fn monty_receipt_round_trips_through_stored_binding() -> TestResult {
    historical_evaluator_round_trip(2).await
}

#[cfg(feature = "composition")]
async fn historical_evaluator_round_trip(kind: u8) -> TestResult {
    use crate::local_composition::{
        LocalCompositionAttachment, LocalCompositionEngineArtifact, LocalCompositionProfile,
        ProductExtensionEvidence, ProductExtensionLimits,
    };
    use plakous_compose::{CodexMinimalLocalCodeModeV1, ProductJavaScriptCompositionV1};
    #[cfg(feature = "composition-python")]
    use plakous_compose::{PythonPtcPresentationV1, PythonPtcV1};
    let directory = tempfile::tempdir()?;
    let historical = directory.path().join("historical-host");
    let current = directory.path().join("current-host");
    std::fs::write(&historical, b"offline evaluator fixture")?;
    std::fs::copy(&historical, &current)?;
    let historical = historical.canonicalize()?;
    let current = current.canonicalize()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let mut registry = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &crate::tools::standard_coding_packs_in_memory()?,
        &mut registry,
    )?;
    match kind {
        0 => CodexMinimalLocalCodeModeV1::register_markers_into(&mut registry)?,
        1 => ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?,
        #[cfg(feature = "composition-python")]
        2 => PythonPtcV1::register_markers_into(&mut registry)?,
        _ => return Err("unknown composition fixture".into()),
    }
    let registry = registry.freeze();
    let compile = |artifact, candidate: &[u8]| -> TestResult<_> {
        let base = descriptor()?;
        let facts = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new(plakous_compose::CODEX_PROJECTION_PROTOCOL)?,
            RouteId::new(plakous_compose::CODEX_PROJECTION_ROUTE)?,
            ModelId::new("offline-historical-code-mode")?,
            None,
            Some(ModelFamilyId::new("responses-family")?),
            ModelAdapterProfileId::new(plakous_compose::CODEX_PROJECTION_PROFILE)?,
            base.capabilities().clone(),
        );
        let facts = crate::test_model::with_context_contract(facts, timestamp()?)?;
        let authority = RuntimeModelAuthority::new(
            &facts,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"historical-host"])?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"credential", b"historical-host"],
            )?,
            1,
            timestamp()?,
            None,
        )?;
        let model = Arc::new(
            plakous_test_support::ScriptedModel::new(facts.clone(), [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?),
        );
        let markers = registry
            .definitions()
            .iter()
            .filter(|tool| {
                let names: &[&str] = match kind {
                    0 => &["exec", "wait"],
                    1 => &["exec_js", "wait_js"],
                    _ => &["exec_py", "wait_py"],
                };
                names.contains(&tool.name().as_str())
            })
            .cloned();
        let configured = crate::test_model::configured_model_for_operation(
            model.clone(),
            facts.clone(),
            authority,
            markers,
            LocalDurabilityDisposition::Ephemeral,
            OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output bound")?),
            plakous_model::ToolPresentationMode::LocalCompositionOnly,
        )?;
        let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
            facts.integration_class(),
            facts.protocol().as_str(),
            facts.route().as_str(),
            facts.profile().as_str(),
            facts.model_family().ok_or("model family")?.as_str(),
            facts.requested_model().as_str(),
            facts.requested_model().as_str(),
            configured.configuration().effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let profile = match kind {
            0 => LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(
                &CodexMinimalLocalCodeModeV1::compile(
                    plakous_tools::JobConfig::default(),
                    registry.clone(),
                    target,
                )?,
            )?,
            1 => LocalCompositionProfile::product_javascript_v1(
                &ProductJavaScriptCompositionV1::compile_for_kind(
                    plakous_tools::JobConfig::default(),
                    registry.clone(),
                    target,
                    plakous_tools::CodingPresentation::Codex,
                    [ToolName::new("read_file")?],
                )?,
            )?,
            #[cfg(feature = "composition-python")]
            2 => LocalCompositionProfile::python_ptc_v1(&PythonPtcV1::compile_for_kind(
                plakous_tools::JobConfig::default(),
                registry.clone(),
                target,
                PythonPtcPresentationV1::StructuredFunctions,
                plakous_tools::CodingPresentation::Codex,
                [ToolName::new("read_file")?],
            )?)?,
            _ => return Err("unknown composition fixture".into()),
        };
        let evidence = if kind == 0 {
            None
        } else {
            let issuer = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"product"])?;
            let candidate = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[candidate])?;
            Some(if kind == 1 {
                ProductExtensionEvidence::product_javascript_v1(
                    &profile,
                    &artifact,
                    ProductExtensionLimits {
                        max_source_bytes: 1_048_576,
                        max_yield_time_ms: 10_000,
                        max_output_tokens: 10_000,
                    },
                    issuer,
                    candidate,
                    configured.configuration(),
                )?
            } else {
                #[cfg(feature = "composition-python")]
                {
                    ProductExtensionEvidence::python_ptc_v1(
                        &profile,
                        &artifact,
                        issuer,
                        candidate,
                        configured.configuration(),
                    )?
                }
                #[cfg(not(feature = "composition-python"))]
                {
                    return Err("Python fixture needs composition-python".into());
                }
            })
        };
        let plan_model: Arc<dyn LanguageModel> = model.clone();
        let plan = crate::construction::compile_local_composition_plan(
            plakous_agent::HarnessProfileId::new("test.historical-host")?,
            &plan_model,
            configured.configuration(),
            &registry,
            &workspace,
        )?;
        let mut attachment = LocalCompositionAttachment::builder(profile).engine_artifact(artifact);
        if let Some(evidence) = evidence {
            attachment = attachment
                .product_extension_evidence(evidence)
                .coding_presentation_kind(plakous_tools::CodingPresentation::Codex);
        }
        let attachment = attachment.build()?;
        let builder = AgentBuilder::new(configured, registry.clone(), Arc::clone(&workspace))
            .harness_plan(plan)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            );
        Ok((
            SessionRuntimeBinding::new(crate::local_composition::attach_local_composition(
                builder,
                Some(attachment),
            )?),
            model,
        ))
    };
    let artifact = |path: &Path| {
        #[cfg(feature = "composition-python")]
        if kind == 2 {
            return LocalCompositionEngineArtifact::monty_pool_v1(path);
        }
        LocalCompositionEngineArtifact::rquickjs_sidecar_v1(path)
    };
    let saved_artifact = artifact(&historical)?;
    let saved_identity = saved_artifact.identity();
    let (binding, model) = compile(saved_artifact, b"qualified")?;
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let create_id = SessionOperationId::new("create:historical-host")?;
    assert_eq!(sessions.accepted_create(&create_id).await?, None);
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(create_id.clone(), binding))
        .await?;
    let head = session.release().await?;
    let before = store.stats()?;
    let current_artifact = artifact(&current)?;
    #[cfg(feature = "sessions-sqlite")]
    {
        let inspection = rehearsal::inspect_exact(&sessions.load_exact(head).await?);
        assert_eq!(
            inspection.outcome,
            rehearsal::OfflineReopenOutcome::OpensQuiescent
        );
        assert!(inspection.evaluator_verified);
    }
    assert_ne!(saved_identity, current_artifact.identity());
    // The runtime create was accepted, but the host never published its locator.
    // Recover it before compiling, even with a different current sidecar.
    assert_eq!(sessions.accepted_create(&create_id).await?, Some(head));
    let restored = sessions
        .created_composition_artifact(
            sessions
                .accepted_create(&create_id)
                .await?
                .ok_or("receipt")?,
        )
        .await?
        .ok_or("missing saved artifact")?;
    assert_eq!(restored.path(), historical);
    assert_eq!(restored.identity(), saved_identity);
    assert_eq!(store.stats()?, before);
    let (replay, _) = compile(restored, b"qualified")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(create_id, replay))
        .await?;
    assert_eq!(session.release().await?, head);
    if kind != 0 {
        let restored = sessions
            .created_composition_artifact(head)
            .await?
            .ok_or("artifact")?;
        let (mismatched, _) = compile(restored, b"different qualification")?;
        let refused = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("open:mismatched-receipt")?,
                head,
                mismatched,
            ))
            .await
            .expect_err("foreign receipt reopened stored attachment");
        assert_eq!(refused.kind(), AgentSessionsErrorKind::IncompatibleBuild);
        assert_eq!(store.stats()?, before);
    }
    let restored = sessions
        .created_composition_artifact(head)
        .await?
        .ok_or("artifact")?;
    let (runtime, reopened_model) = compile(restored, b"qualified")?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:historical-host")?,
            head,
            runtime,
        ))
        .await?
    else {
        return Err("historical head did not reopen".into());
    };
    assert_eq!(session.release().await?, head);
    assert_eq!(model.captured_plans(), [] as [ValidatedRequestPlan; 0]);
    assert_eq!(
        reopened_model.captured_plans(),
        [] as [ValidatedRequestPlan; 0]
    );
    std::fs::write(&historical, b"changed evaluator")?;
    #[cfg(feature = "sessions-sqlite")]
    assert!(!rehearsal::inspect_exact(&sessions.load_exact(head).await?).evaluator_verified);
    assert_eq!(
        sessions
            .created_composition_artifact(head)
            .await
            .expect_err("changed bytes accepted")
            .kind(),
        AgentSessionsErrorKind::IncompatibleBuild
    );
    std::fs::remove_file(&historical)?;
    #[cfg(feature = "sessions-sqlite")]
    assert!(!rehearsal::inspect_exact(&sessions.load_exact(head).await?).evaluator_verified);
    assert_eq!(
        sessions
            .created_composition_artifact(head)
            .await
            .expect_err("missing artifact accepted")
            .kind(),
        AgentSessionsErrorKind::IncompatibleBuild
    );
    assert_eq!(store.stats()?, before);
    Ok(())
}

#[tokio::test]
async fn fork_receipt_retry_survives_later_heads_and_conflicting_reuse_fails() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:historical-fork")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"historical-account",
            )?,
        ))
        .await?;
    let source = session.release().await?;
    let operation = SessionOperationId::new("fork:historical-retry")?;
    let ForkSessionOutcome::Forked { target, .. } = sessions
        .fork(ForkSessionRequest::new(operation.clone(), source))
        .await?
    else {
        return Err("initial fork did not publish".into());
    };

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:advance-source")?,
            source,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"historical-account",
            )?,
        ))
        .await?
    else {
        return Err("source did not reopen".into());
    };
    let outcome = session
        .agent()
        .prompt("advance source head")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let active_source = session.locator();
    let active_error = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:while-attached")?,
            active_source,
        ))
        .await
        .expect_err("an attached source published a second branch owner");
    assert_eq!(active_error.kind(), AgentSessionsErrorKind::Ownership);
    let current_source = session.release().await?;

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:advance-target")?,
            target,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"historical-account",
            )?,
        ))
        .await?
    else {
        return Err("target did not reopen".into());
    };
    let outcome = session
        .agent()
        .prompt("advance target head")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let current_target = session.release().await?;
    assert_ne!(current_source, source);
    assert_ne!(current_target, target);
    let before_retry = store.stats()?;

    let recovered = sessions
        .fork(ForkSessionRequest::new(operation.clone(), source))
        .await?;
    assert!(matches!(
        recovered,
        ForkSessionOutcome::Forked {
            source: exact_source,
            target: exact_target,
            ..
        } if exact_source == source && exact_target == target
    ));
    assert_eq!(store.stats()?, before_retry);

    let stale = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:new-from-stale")?,
            source,
        ))
        .await
        .expect_err("a new operation published from a stale locator");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(current_source));
    assert_eq!(store.stats()?, before_retry);

    let conflicting_runtime = runtime_binding_for(
        workspace,
        descriptor_for("conflicting.route", "conflicting.model")?,
        b"conflicting-account",
    )?;
    let conflicting_target = conflicting_runtime.target_binding();
    let conflicting = Box::pin(sessions.rebind(RebindSessionRequest::new(
        operation.clone(),
        source,
        conflicting_target,
        conflicting_runtime,
    )))
    .await
    .expect_err("one operation identity accepted contradictory branch semantics");
    assert_eq!(conflicting.kind(), AgentSessionsErrorKind::IdentityConflict);
    assert_eq!(store.stats()?, before_retry);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "fresh target authority, exact publication, restart recovery, and prepublication continuity failures are one rebind contract"
)]
async fn rebind_requires_fresh_matching_authority_and_recovers_without_reauth() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:rebind-source")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await?;
    let outcome = session
        .agent()
        .prompt("preserve history on rebind")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let source_loaded = store.load_branch(source.session_ref()).await?;
    let source_reference = source_loaded.checkpoint().reference();
    let source_manifest = source_loaded.checkpoint().checkpoint().manifest();
    let target_descriptor =
        descriptor_for_adapter("target.route", "target.model", "target.adapter")?;
    let target_runtime = runtime_binding_for(
        Arc::clone(&workspace),
        target_descriptor.clone(),
        b"account-beta",
    )?;
    let target_selector = target_runtime.target_binding();
    let operation = SessionOperationId::new("rebind:exact-stable")?;

    let RebindSessionOutcome::Rebound {
        operation_id,
        source: reported_source,
        target,
        portability,
        binding,
        ..
    } = Box::pin(sessions.rebind(RebindSessionRequest::new(
        operation.clone(),
        source,
        target_selector.clone(),
        target_runtime,
    )))
    .await?
    else {
        return Err("quiescent explicit rebind did not publish".into());
    };
    assert_eq!(operation_id, operation);
    assert_eq!(reported_source, source);
    assert_eq!(portability, ArtifactPortability::ClearedUnproved);
    assert_eq!(binding.protocol(), target_selector.protocol());
    assert_eq!(binding.route(), target_selector.route());
    assert_eq!(binding.model(), target_selector.model());
    assert_ne!(
        binding.account_reference(),
        source_manifest.binding().account().subject()
    );
    assert_eq!(binding.profile_reference(), "target.adapter");

    let source_after = store.load_branch(source.session_ref()).await?;
    assert_eq!(source_after.head(), source_loaded.head());
    assert_eq!(source_after.checkpoint().reference(), source_reference);
    let target_loaded = store.load_branch(target.session_ref()).await?;
    let target_manifest = target_loaded.checkpoint().checkpoint().manifest();
    assert_eq!(target_loaded.head().revision().get(), 0);
    assert_eq!(
        target_manifest.binding().protocol(),
        target_selector.protocol()
    );
    assert_eq!(target_manifest.binding().route(), target_selector.route());
    assert_eq!(
        target_manifest.binding().requested_model(),
        target_selector.model()
    );
    assert_eq!(
        target_manifest.binding().account().subject(),
        binding.account_reference()
    );
    assert_eq!(
        target_manifest.canonical().history(),
        source_manifest.canonical().history()
    );
    assert_eq!(
        target_manifest.canonical().working(),
        source_manifest.canonical().working()
    );
    assert_eq!(target_manifest.ledger(), source_manifest.ledger());
    assert_eq!(target_manifest.tools(), source_manifest.tools());
    assert_ne!(
        target_manifest.harness_plan_component(),
        source_manifest.harness_plan_component()
    );
    assert_ne!(
        target_manifest.canonical().stable_prefix(),
        source_manifest.canonical().stable_prefix()
    );
    assert!(target_manifest.family_artifact().is_none());
    assert!(
        target_manifest
            .canonical()
            .context()
            .occupancy_anchor()
            .is_none()
    );
    assert_eq!(
        target_manifest
            .canonical()
            .context()
            .transition()
            .map(RuntimeContextTransition::kind),
        Some(RuntimeContextTransitionKind::Rebind)
    );
    assert_ne!(
        target_manifest.canonical().context().affinity().identity(),
        source_manifest.canonical().context().affinity().identity()
    );

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:rebound-target")?,
            target,
            runtime_binding_for(
                Arc::clone(&workspace),
                target_descriptor.clone(),
                b"account-beta",
            )?,
        ))
        .await?
    else {
        return Err("fresh rebind target did not reopen with its exact builder".into());
    };
    session.release().await?;
    let accepted_stats = store.stats()?;

    let recovered = Box::pin(sessions.rebind(RebindSessionRequest::new(
        operation.clone(),
        source,
        target_selector.clone(),
        unauthenticated_runtime_binding(Arc::clone(&workspace), target_descriptor.clone())?,
    )))
    .await?;
    assert!(matches!(
        recovered,
        RebindSessionOutcome::Rebound {
            source: exact_source,
            target: exact_target,
            binding: recovered_binding,
            ..
        } if exact_source == source
            && exact_target == target
            && recovered_binding == binding
    ));
    assert_eq!(store.stats()?, accepted_stats);

    let changed_account_runtime = runtime_binding_for(
        Arc::clone(&workspace),
        target_descriptor.clone(),
        b"account-gamma",
    )?;
    let changed_account_target = changed_account_runtime.target_binding();
    let changed_account = Box::pin(sessions.rebind(RebindSessionRequest::new(
        operation.clone(),
        source,
        changed_account_target,
        changed_account_runtime,
    )))
    .await
    .expect_err("same operation accepted a changed target account authority");
    assert_eq!(
        changed_account.kind(),
        AgentSessionsErrorKind::IdentityConflict
    );
    assert_eq!(store.stats()?, accepted_stats);

    let mismatched_selector = SessionTargetBinding::new(
        ProtocolId::new("test.protocol")?,
        RouteId::new("wrong.selector")?,
        ModelId::new("wrong.model")?,
        target_selector.selected_provider_configuration(),
        target_selector.effective_provider_configuration(),
        target_selector.account_binding(),
    );
    let selector_error = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("rebind:wrong-selector")?,
        source,
        mismatched_selector,
        runtime_binding_for(
            Arc::clone(&workspace),
            target_descriptor.clone(),
            b"account-beta",
        )?,
    )))
    .await
    .expect_err("selector-only authority published a rebind");
    assert_eq!(selector_error.kind(), AgentSessionsErrorKind::Revalidation);

    let auth_error = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("rebind:missing-auth")?,
        source,
        target_selector.clone(),
        unauthenticated_runtime_binding(Arc::clone(&workspace), target_descriptor.clone())?,
    )))
    .await
    .expect_err("missing fresh target auth published a rebind");
    assert_eq!(auth_error.kind(), AgentSessionsErrorKind::Revalidation);

    let other_directory = tempfile::tempdir()?;
    let other_workspace = Arc::new(WorkspacePolicy::host(other_directory.path(), ".")?);
    let workspace_error = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("rebind:workspace-drift")?,
        source,
        target_selector.clone(),
        runtime_binding_for(other_workspace, target_descriptor.clone(), b"account-beta")?,
    )))
    .await
    .expect_err("workspace drift published a rebind");
    assert_eq!(workspace_error.kind(), AgentSessionsErrorKind::Revalidation);

    let unauthorized_tool_runtime = runtime_binding_with_registry(
        Arc::clone(&workspace),
        target_descriptor.clone(),
        b"account-beta",
        one_tool_registry()?,
    )?;
    let unauthorized_tool_target = unauthorized_tool_runtime.target_binding();
    let tool_error = Box::pin(sessions.rebind(RebindSessionRequest::recompiling_target(
        SessionOperationId::new("rebind:canonical-tool-drift")?,
        source,
        unauthorized_tool_target,
        unauthorized_tool_runtime,
    )))
    .await
    .expect_err("target presentation added canonical tool authority");
    assert_eq!(tool_error.kind(), AgentSessionsErrorKind::Revalidation);

    let marker_runtime = runtime_binding_with_registry(
        Arc::clone(&workspace),
        target_descriptor.clone(),
        b"account-beta",
        composition_marker_registry()?,
    )?;
    let marker_target = marker_runtime.target_binding();
    let tool_rebind = Box::pin(sessions.rebind(RebindSessionRequest::recompiling_target(
        SessionOperationId::new("rebind:target-presentation")?,
        source,
        marker_target,
        marker_runtime,
    )))
    .await?;
    let RebindSessionOutcome::Rebound {
        target: tool_locator,
        ..
    } = tool_rebind
    else {
        return Err("exact target tool presentation did not publish a rebind".into());
    };
    let tool_branch = store.load_branch(tool_locator.session_ref()).await?;
    assert_ne!(
        tool_branch.checkpoint().checkpoint().manifest().tools(),
        source_manifest.tools()
    );
    let accepted_stats_after_tool_rebind = store.stats()?;

    let policy_binding = runtime_binding_for(workspace, target_descriptor, b"account-beta")?;
    let policy_error = Box::pin(
        sessions.rebind(RebindSessionRequest::new(
            SessionOperationId::new("rebind:policy-drift")?,
            source,
            target_selector,
            SessionRuntimeBinding::new(
                policy_binding
                    .builder
                    .tool_policy(Arc::new(AlwaysAllowPolicy)),
            ),
        )),
    )
    .await
    .expect_err("effect-policy drift published a rebind");
    assert_eq!(policy_error.kind(), AgentSessionsErrorKind::Revalidation);
    assert_eq!(store.stats()?, accepted_stats_after_tool_rebind);
    Ok(())
}

/// Instruction files that changed since the source was compiled reach a
/// rebind that recompiles for its target, exactly as they are now; an exact
/// rebind still refuses them, and its refusal names the cause.
#[tokio::test]
async fn recompiling_rebind_rereads_changed_instruction_sources() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:instructed-source")?,
            instructed_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
                "instructions as the session started",
            )?,
        ))
        .await?;
    let outcome = session.agent().prompt("work").await?.result().await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let source_head = store.load_branch(source.session_ref()).await?.head();
    let target_descriptor =
        descriptor_for_adapter("target.route", "target.model", "target.adapter")?;
    let edited = || {
        instructed_runtime_binding_for(
            Arc::clone(&workspace),
            target_descriptor.clone(),
            b"account-beta",
            "instructions after the file was edited",
        )
    };

    let exact = edited()?;
    let exact_selector = exact.target_binding();
    let refused = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("rebind:exact-instructions")?,
        source,
        exact_selector,
        exact,
    )))
    .await
    .expect_err("an exact rebind re-read edited instruction files");
    assert_eq!(refused.kind(), AgentSessionsErrorKind::Revalidation);
    assert!(
        refused.to_string().contains("instruction-source authority")
            && refused.to_string().contains("AGENTS.md"),
        "{refused}"
    );
    assert_eq!(
        store.load_branch(source.session_ref()).await?.head(),
        source_head
    );

    let recompiled = edited()?;
    let selector = recompiled.target_binding();
    let RebindSessionOutcome::Rebound { target, .. } =
        Box::pin(sessions.rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new("rebind:recompiled-instructions")?,
            source,
            selector,
            recompiled,
        )))
        .await?
    else {
        return Err("a recompiling rebind refused edited instruction files".into());
    };
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:recompiled-instructions")?,
            target,
            runtime_binding_for(
                Arc::clone(&workspace),
                target_descriptor.clone(),
                b"account-beta",
            )?,
        ))
        .await?
    else {
        return Err("the rebound target did not reopen".into());
    };
    let contents = session
        .agent()
        .harness_plan()
        .instructions()
        .sources()
        .iter()
        .map(|source| source.content().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["instructions after the file was edited"]);
    session.release().await?;
    assert_eq!(
        store.load_branch(source.session_ref()).await?.head(),
        source_head,
        "the source branch is never rewritten"
    );
    Ok(())
}

/// A fork that recompiles lands on the instruction files as they are now and
/// keeps the source's history and binding; an exact fork and an exact reopen
/// keep the pinned sources, and a recompiling fork onto another target is a
/// rebind, not a fork.
#[tokio::test]
async fn recompiling_fork_rereads_changed_instruction_sources() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let instructed = |instructions: &str| {
        instructed_runtime_binding_for(
            Arc::clone(&workspace),
            descriptor()?,
            b"account-alpha",
            instructions,
        )
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fork-instructed-source")?,
            instructed("instructions as the session started")?,
        ))
        .await?;
    let outcome = session.agent().prompt("work").await?.result().await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let source_head = store.load_branch(source.session_ref()).await?.head();
    let opened_sources = |locator: SessionLocator, operation: &'static str| {
        let sessions = &sessions;
        let workspace = Arc::clone(&workspace);
        async move {
            let history = sessions
                .load_exact(locator)
                .await?
                .checkpoint
                .manifest()
                .canonical()
                .history()
                .identity();
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new(operation)?,
                    locator,
                    runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
                ))
                .await?
            else {
                return Err::<_, Box<dyn std::error::Error>>("the fork did not reopen".into());
            };
            let contents = session
                .agent()
                .harness_plan()
                .instructions()
                .sources()
                .iter()
                .map(|source| source.content().to_owned())
                .collect::<Vec<_>>();
            session.release().await?;
            Ok((contents, history))
        }
    };
    let source_history = sessions
        .load_exact(source)
        .await?
        .checkpoint
        .manifest()
        .canonical()
        .history()
        .identity();

    let forked = |operation: &'static str, runtime| {
        let sessions = &sessions;
        Box::pin(async move {
            let ForkSessionOutcome::Forked { target, .. } = sessions
                .fork(ForkSessionRequest::recompiling(
                    SessionOperationId::new(operation)?,
                    source,
                    runtime,
                ))
                .await?
            else {
                return Err::<_, Box<dyn std::error::Error>>("the fork did not land".into());
            };
            Ok(target)
        })
    };
    let unchanged = forked(
        "fork:recompiled-unchanged",
        instructed("instructions as the session started")?,
    )
    .await?;
    assert_eq!(
        opened_sources(unchanged, "open:recompiled-unchanged").await?,
        (
            vec!["instructions as the session started".to_owned()],
            source_history
        )
    );

    let edited = forked(
        "fork:recompiled-edited",
        instructed("instructions after the file was edited")?,
    )
    .await?;
    assert_eq!(
        opened_sources(edited, "open:recompiled-edited").await?,
        (
            vec!["instructions after the file was edited".to_owned()],
            source_history
        ),
        "the fork re-reads the instruction files and keeps the source's history"
    );
    for instructions in [
        "instructions after the file was edited",
        "instructions as the session started",
    ] {
        assert_eq!(
            forked("fork:recompiled-edited", instructed(instructions)?).await?,
            edited,
            "a replayed fork recovers its published receipt"
        );
    }
    assert_eq!(
        forked(
            "fork:recompiled-unchanged",
            instructed("instructions after the file was edited")?,
        )
        .await?,
        unchanged
    );

    let exact = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:exact-instructions")?,
            source,
        ))
        .await?;
    let ForkSessionOutcome::Forked { target: exact, .. } = exact else {
        return Err("the exact fork did not land".into());
    };
    assert_eq!(
        opened_sources(exact, "open:exact-fork").await?.0,
        ["instructions as the session started"]
    );

    let elsewhere = instructed_runtime_binding_for(
        Arc::clone(&workspace),
        descriptor_for_adapter("target.route", "target.model", "target.adapter")?,
        b"account-beta",
        "instructions after the file was edited",
    )?;
    let refused = sessions
        .fork(ForkSessionRequest::recompiling(
            SessionOperationId::new("fork:recompiled-elsewhere")?,
            source,
            elsewhere,
        ))
        .await
        .expect_err("a recompiling fork changed its binding");
    assert!(refused.to_string().contains("rebind"), "{refused}");

    assert_eq!(
        store.load_branch(source.session_ref()).await?.head(),
        source_head,
        "the source branch is never rewritten"
    );
    assert_eq!(
        opened_sources(source, "open:fork-source-after").await?.0,
        ["instructions as the session started"],
        "an exact reopen keeps the pinned instruction sources"
    );
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
#[tokio::test]
async fn sqlite_reboot_retries_attach_fresh_runtime_without_republishing_history() -> TestResult {
    struct BootClock(LeaseInstant);
    impl LeaseClock for BootClock {
        fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
            Ok(self.0)
        }
    }

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("reboot-workflows.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let open = |epoch, seconds| -> TestResult<(SqliteSessionStore, AgentSessions)> {
        let clock = BootClock(LeaseInstant::new(
            LeaseDomain::new([epoch; 16], [2; 32]),
            seconds,
            0,
        )?);
        let store = SqliteSessionStore::open_with_clock(&database, Arc::new(clock))?;
        let sessions = AgentSessions::new(SessionBackend::from_store(
            Arc::new(store.clone()),
            RuntimeDurability::Required,
        ))?;
        Ok((store, sessions))
    };
    let binding =
        || durable_runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"reboot-account");
    let create_operation = SessionOperationId::new("reboot:create")?;
    let fork_operation = SessionOperationId::new("reboot:fork")?;
    let (store, sessions) = open(1, 100)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            create_operation.clone(),
            binding()?,
        ))
        .await?;
    let source = session.release().await?;
    let ForkSessionOutcome::Forked { target, .. } = sessions
        .fork(ForkSessionRequest::new(fork_operation.clone(), source))
        .await?
    else {
        return Err("initial reboot fixture fork did not publish".into());
    };
    let before = store.stats().await?;
    drop((sessions, store));

    // Exact old receipts retain their original boot's grants. These facade
    // calls must tolerate retired-grant cleanup and acquire fresh authority.
    let (store, sessions) = open(2, 1)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(create_operation, binding()?))
        .await?;
    assert_eq!(session.locator(), source);
    assert_eq!(session.release().await?, source);
    let recovered = sessions
        .fork(ForkSessionRequest::new(fork_operation, source))
        .await?;
    assert!(
        matches!(recovered, ForkSessionOutcome::Forked { target: recovered, .. } if recovered == target)
    );
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("reboot:open-target")?,
            target,
            binding()?,
        ))
        .await?
    else {
        return Err("recovered target did not attach a runtime".into());
    };
    assert_eq!(session.release().await?, target);
    let after = store.stats().await?;
    assert_eq!(after.sessions(), before.sessions());
    assert_eq!(after.branches(), before.branches());
    assert_eq!(after.checkpoints(), before.checkpoints());
    assert_eq!(after.objects(), before.objects());
    assert_eq!(after.object_bytes(), before.object_bytes());
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the exact Working checkpoint and read-only diagnostic form one fixture"
)]
async fn sqlite_recovery_inspection_reports_working_obligations_without_mutation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("working-inspect.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = SqliteSessionStore::open_with_clock(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("inspect:working:create")?,
            durable_runtime_binding_for(workspace, descriptor()?, b"working-inspect-account")?,
        ))
        .await?;
    let source = session.release().await?;
    let genesis_loaded = store.load_branch(source.session_ref()).await?;
    let genesis = Arc::clone(genesis_loaded.checkpoint().checkpoint());
    let source_topology = genesis.manifest().topology();
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            RunId::try_from(Uuid::from_u128(0x5111))?,
            NonZeroU64::new(30).ok_or("lease duration must be nonzero")?,
        ))
        .await?;
    let turn_id = TurnId::try_from(Uuid::from_u128(0x5112))?;
    let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(0x5113))?;
    let root = RuntimeWorkingRoot::Turn {
        turn_id,
        next_round: 1,
    };
    let canonical_input = b"diagnostic-working-root";
    let command_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "canonical_input": canonical_input,
        "effect": "Root",
        "rejection": null,
    }))?;
    let command = DurableCommandRecord::new(
        "diagnostic-working-root",
        1,
        Some(turn_id),
        Digest::sha256(DigestDomain::Blob, 1, &[canonical_input])?,
        ReconstructiveComponent::new(RuntimeComponentKind::Command, 1, command_bytes.as_slice())?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let command_operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, command_bytes.as_slice())?,
        vec![
            DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(checkpoint_id, DurableOperationState::Dispatched),
        ],
    )?;
    let provider_operation = DurableOperationRecord::new(
        2,
        DurableOperationKind::Provider {
            root: root.operation_root(),
            attempt_id: AttemptId::try_from(Uuid::from_u128(0x5114))?,
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, [1_u8].as_slice())?,
        vec![DurableOperationTransition::new(
            checkpoint_id,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger =
        RuntimeOperationLedger::new(vec![command], vec![command_operation, provider_operation])?;
    let command_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::Command,
        RecoveryAction::FinalizationRequired,
    );
    let provider_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::ProviderEvidenceRequired,
    );
    let manifest = genesis.manifest();
    let working = Arc::new(RuntimeCheckpoint::new(RuntimeManifest::new(
        manifest.provenance().clone(),
        RuntimeTopology {
            session_id: source_topology.session_id,
            branch_id: source_topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(source_topology.checkpoint_id),
            branch_revision: 1,
            fork: None,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"working-inspection"],
            )?,
        },
        RuntimeLifecycle::Working {
            root,
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            obligations: vec![
                RecoveryObligation::new(1, command_signature),
                RecoveryObligation::new(2, provider_signature),
            ],
        },
        manifest.harness_plan_component().clone(),
        manifest.provider_configuration().clone(),
        manifest.canonical().clone(),
        manifest.binding().clone(),
        manifest.tools().clone(),
        ledger,
        manifest.family_artifact().cloned(),
        manifest.interpreter_observation().cloned(),
        plakous_agent::internal::JobLedger::default(),
        manifest.durability(),
    )?)?);
    let bundle = RuntimeCheckpointCodec::advance(&working, genesis_loaded.checkpoint().bundle())?;
    let committed = store
        .commit(CommitCheckpoint::new(
            lease.clone(),
            genesis_loaded.head(),
            bundle,
        ))
        .await?;
    store.release(lease).await?;
    let head = SessionLocator::new(
        committed.head().session().session_id(),
        committed.head().session().branch_id(),
        committed.head().checkpoint().checkpoint_id(),
    );
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    assert!(
        wal.metadata()?.len() > 0,
        "working head must remain in a live WAL"
    );
    let before = store.stats().await?;
    let report = AgentSessions::inspect_recovery_sqlite(&database, head)?
        .ok_or("Working head reported quiescent")?;
    let page = AgentSessions::inspect_history_page_sqlite(
        &database,
        &SessionHistoryPageRequest::new(
            head,
            head.checkpoint_id(),
            SessionHistoryVisibility::Content,
            SESSION_HISTORY_VISIBILITY_REVISION,
            SessionHistoryDirection::Forward,
            32,
            4096,
            None,
        ),
    )?;
    let SessionHistoryPageOutcome::Page { items, next, .. } = page else {
        return Err("Working inspection requested a resnapshot".into());
    };
    assert!(
        items.is_empty(),
        "inspection invented uncommitted Working content"
    );
    assert!(next.is_none());
    assert_eq!(report.locator(), head);
    assert_eq!(report.phase(), SessionRecoveryPhase::DispatchEvidence);
    assert_eq!(report.obligations().len(), 2);
    #[cfg(feature = "sessions-sqlite")]
    {
        let inspection = rehearsal::inspect_sqlite(&database, head)?;
        assert_eq!(
            inspection.outcome,
            rehearsal::OfflineReopenOutcome::RecoversAbandonedTurn
        );
        assert!(inspection.evaluator_verified);
        assert!(inspection.job_config_verified);
    }
    assert_eq!(report.obligations()[0].operation_id(), 1);
    assert_eq!(
        report.obligations()[0].action(),
        SessionRecoveryAction::FinalizationRequired
    );
    assert_eq!(report.obligations()[1].operation_id(), 2);
    assert_eq!(
        report.obligations()[1].action(),
        SessionRecoveryAction::ProviderEvidenceRequired
    );
    assert_eq!(store.stats().await?, before);
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
#[tokio::test]
async fn sqlite_recovery_inspection_is_exact_and_does_not_change_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("inspect.sqlite3");
    let absent: SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse()?;
    let missing =
        AgentSessions::inspect_recovery_sqlite(directory.path().join("missing.sqlite3"), absent)
            .expect_err("inspection created an absent database");
    assert_eq!(missing.kind(), AgentSessionsErrorKind::NotFound);
    assert!(!directory.path().join("missing.sqlite3").exists());

    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = SqliteSessionStore::open_with_clock(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("inspect:create")?,
            durable_runtime_binding_for(workspace, descriptor()?, b"inspect-account")?,
        ))
        .await?;
    let source = session.release().await?;
    let current = install_family_artifact(&store, source).await?;
    let before = store.load_branch(current.session_ref()).await?;
    let before_stats = store.stats().await?;
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let before_domain: Vec<u8> = connection.query_row(
        "SELECT lease_domain FROM store_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let before_lease: (Option<String>, Option<Vec<u8>>) = connection.query_row(
        "SELECT active_owner, active_fence FROM branches WHERE session_id=?1 AND branch_id=?2",
        [
            current.session_id().to_string(),
            current.branch_id().to_string(),
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    drop(connection);

    let stale = AgentSessions::inspect_recovery_sqlite(&database, source)
        .expect_err("stale locator was silently adopted");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(current));
    assert_eq!(
        AgentSessions::inspect_recovery_sqlite(&database, current)?,
        None
    );

    let after = store.load_branch(current.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats().await?, before_stats);
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let after_domain: Vec<u8> = connection.query_row(
        "SELECT lease_domain FROM store_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let after_lease: (Option<String>, Option<Vec<u8>>) = connection.query_row(
        "SELECT active_owner, active_fence FROM branches WHERE session_id=?1 AND branch_id=?2",
        [
            current.session_id().to_string(),
            current.branch_id().to_string(),
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(after_domain, before_domain);
    assert_eq!(after_lease, before_lease);
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "fixture sizing, real capacity failure, cold exact recovery and next-turn continuity are one regression"
)]
async fn sqlite_terminal_capacity_failure_recovers_exactly_within_the_reserve() -> TestResult {
    use std::num::NonZeroUsize;

    use plakous_store::SqliteStoreOptions;

    fn options(bytes: usize) -> TestResult<SqliteStoreOptions> {
        let nonzero = |value| NonZeroUsize::new(value).ok_or("nonzero SQLite fixture limit");
        Ok(SqliteStoreOptions::new(
            MemoryStoreLimits::new(
                nonzero(2)?,
                nonzero(2)?,
                nonzero(64)?,
                nonzero(4096)?,
                nonzero(bytes)?,
                nonzero(16)?,
            ),
            nonzero(8)?,
        ))
    }

    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let runtime = |first: u128| -> TestResult<_> {
        let (binding, model) = completing_runtime_binding_with_capture_and_durability(
            Arc::clone(&workspace),
            descriptor()?,
            b"sqlite-capacity-account",
            LocalDurabilityDisposition::DurableRequired,
        )?;
        let ids =
            plakous_model::SequenceIdGenerator::new((first..first + 128).map(Uuid::from_u128))?;
        Ok((
            SessionRuntimeBinding::new(
                binding
                    .builder
                    .identity_sources(Arc::new(FixedClock::new(timestamp()?)), Arc::new(ids)),
            ),
            model,
        ))
    };
    let create_id = SessionOperationId::new("capacity:create")?;
    let input = "preserve this intent through exact recovery";
    let sufficient = options(4 * 1024 * 1024)?;

    // Measure the retained graph before and after the final publication, not defaults
    // or a guessed admission budget. The midpoint leaves room for digest-dependent
    // JSON lengths while rejecting the final publication in the second fresh fixture.
    let calibration = SqliteSessionStore::open_with_clock_and_options(
        directory.path().join("capacity-sizing.sqlite3"),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        sufficient,
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(calibration.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(create_id.clone(), runtime(1)?.0))
        .await?;
    let completed = session.agent().prompt(input).await?.result().await?;
    assert!(matches!(
        completed.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    session.release().await?;
    let measured = calibration.stats().await?;
    let terminal = calibration
        .load_checkpoint(completed.checkpoint_id())
        .await?;
    let mut parent = terminal
        .checkpoint()
        .manifest()
        .topology()
        .parent_checkpoint_id;
    let mut retained = BTreeMap::new();
    while let Some(checkpoint_id) = parent {
        let checkpoint = calibration.load_checkpoint(checkpoint_id).await?;
        for object in checkpoint.bundle().supplied_objects() {
            retained.insert(object.address(), object.retained_bytes());
        }
        parent = checkpoint
            .checkpoint()
            .manifest()
            .topology()
            .parent_checkpoint_id;
    }
    let before_final: usize = retained.values().sum();
    let after_final = usize::try_from(measured.object_bytes())?;
    assert!(after_final > before_final);
    // Terminal publication is settlement work, which may fill the bound to
    // its threshold rather than the bound itself, so the midpoint is placed
    // on that threshold.
    let midpoint = u64::try_from(before_final + (after_final - before_final) / 2)?;
    let mut bound = midpoint;
    while plakous_agent::internal::StoreAdmission::Settlement.threshold(bound) < midpoint {
        bound += 1;
    }
    let tiny = options(usize::try_from(bound)?)?;
    drop(completed);
    drop(sessions);
    calibration.shutdown().await?;
    drop(calibration);

    let database = directory.path().join("capacity-recovery.sqlite3");
    let store = SqliteSessionStore::open_with_clock_and_options(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        tiny,
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let (binding, model) = runtime(1)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(create_id, binding))
        .await?;
    let genesis = session.locator();
    let result = session.agent().prompt(input).await?.result().await?;
    let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
        return Err(format!("expected terminal capacity failure: {:?}", result.outcome()).into());
    };
    assert!(
        error
            .message()
            .contains("authoritative terminal publication requires exact recovery"),
        "{error}"
    );
    assert!(test_lock(&model).is_some());
    let source = session.release().await?;
    assert_ne!(
        source, genesis,
        "admission must have published Working state"
    );
    let failed_stats = store.stats().await?;
    assert_eq!(
        failed_stats.checkpoints() + 1,
        measured.checkpoints(),
        "before={before_final} after={after_final} failed={failed_stats:?}"
    );
    assert!(failed_stats.object_bytes() <= u64::try_from(tiny.limits().max_object_bytes().get())?);
    let report = AgentSessions::inspect_recovery_sqlite(&database, source)?
        .ok_or("capacity failure did not retain a recovery checkpoint")?;
    assert_eq!(report.phase(), SessionRecoveryPhase::Finalization);
    assert_ne!(report.obligations(), []);
    assert!(
        report.obligations().iter().all(|obligation| {
            obligation.action() == SessionRecoveryAction::FinalizationRequired
        })
    );
    // This fixture registers no tools and its one provider call has already completed.
    drop(result);
    drop(model);
    drop(sessions);
    store.shutdown().await?;
    drop(store);

    // The limits are unchanged: recovery writes into the reserve the
    // refused settlement could not reach.
    let store = SqliteSessionStore::open_with_clock_and_options(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        tiny,
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    assert_eq!(store.stats().await?, failed_stats);
    let (binding, model) = runtime(129)?;
    // Exact facade open owns local terminal recovery; no caller-built checkpoint repair.
    let outcome = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:recover")?,
            source,
            binding,
        ))
        .await?;
    let session = match outcome {
        OpenSessionOutcome::Opened { session, .. } => session,
        OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
            let result = turn.result().result().await?;
            assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
            session
        }
        OpenSessionOutcome::RecoveryRequired { recovery, .. } => {
            return Err(format!("recovery did not fit the reserve: {recovery:?}").into());
        }
    };
    let target = session.release().await?;
    assert_ne!(target, source);
    assert!(test_lock(&model).is_none(), "recovery replayed the model");
    let recovered_stats = store.stats().await?;
    assert_eq!(
        recovered_stats.checkpoints(),
        failed_stats.checkpoints() + 1
    );
    assert_eq!(
        AgentSessions::inspect_recovery_sqlite(&database, target)?,
        None
    );
    let recovered = store.load_branch(target.session_ref()).await?;
    assert!(
        recovered
            .checkpoint()
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .iter()
            .all(|record| {
                matches!(
                    record.kind(),
                    DurableOperationKind::Command { .. } | DurableOperationKind::Provider { .. }
                )
            }),
        "terminal recovery must not introduce tool work"
    );
    let history = recovered
        .checkpoint()
        .checkpoint()
        .manifest()
        .canonical()
        .history();
    let texts = history
        .segments()
        .iter()
        .flat_map(plakous_model::HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec![input, "completed"]);

    let stale = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:stale-retry")?,
            source,
            runtime(257)?.0,
        ))
        .await
        .expect_err("recovery republished from a stale Working selector");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(target));
    // New work needs room the reserve does not lend it; with the limits
    // raised the recovered session continues where it left off.
    drop(sessions);
    store.shutdown().await?;
    drop(store);
    let store = SqliteSessionStore::open_with_clock_and_options(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        sufficient,
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let (binding, next_model) = runtime(385)?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:open-recovered")?,
            target,
            binding,
        ))
        .await?
    else {
        return Err("recovered checkpoint did not reopen quiescently".into());
    };
    let reopened_stats = store.stats().await?;
    assert_eq!(reopened_stats.checkpoints(), recovered_stats.checkpoints());
    assert_eq!(reopened_stats.objects(), recovered_stats.objects());
    assert_eq!(
        reopened_stats.object_bytes(),
        recovered_stats.object_bytes()
    );
    assert!(test_lock(&next_model).is_none());
    let next = session
        .agent()
        .prompt("continue the preserved intent")
        .await?
        .result()
        .await?;
    assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
    assert!(test_lock(&next_model).is_some());
    let final_locator = session.release().await?;
    let final_checkpoint = store.load_branch(final_locator.session_ref()).await?;
    let final_history = final_checkpoint
        .checkpoint()
        .checkpoint()
        .manifest()
        .canonical()
        .history();
    assert_eq!(
        final_history
            .segments()
            .iter()
            .take(history.segments().len())
            .collect::<Vec<_>>(),
        history.segments().iter().collect::<Vec<_>>()
    );
    assert!(final_history.segments().len() > history.segments().len());
    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
#[tokio::test]
async fn sqlite_reopen_exactly_recovers_fork_and_rebind_receipts() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("branch-workflows.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = sqlite_test_sessions(&database)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("sqlite:create-source")?,
            durable_runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"sqlite-alpha")?,
        ))
        .await?;
    let source = session.release().await?;
    let fork_operation = SessionOperationId::new("sqlite:fork-stable")?;
    let ForkSessionOutcome::Forked {
        target: fork_target,
        ..
    } = sessions
        .fork(ForkSessionRequest::new(fork_operation.clone(), source))
        .await?
    else {
        return Err("SQLite fork did not publish".into());
    };
    drop(sessions);

    let sessions = sqlite_test_sessions(&database)?;
    let recovered_fork = sessions
        .fork(ForkSessionRequest::new(fork_operation, source))
        .await?;
    assert!(matches!(
        recovered_fork,
        ForkSessionOutcome::Forked { target, .. } if target == fork_target
    ));

    let target_descriptor = descriptor_for_adapter(
        "sqlite.target.route",
        "sqlite.target.model",
        "sqlite.target.adapter",
    )?;
    let target_runtime = durable_runtime_binding_for(
        Arc::clone(&workspace),
        target_descriptor.clone(),
        b"sqlite-beta",
    )?;
    let selector = target_runtime.target_binding();
    let rebind_operation = SessionOperationId::new("sqlite:rebind-stable")?;
    let RebindSessionOutcome::Rebound {
        target: rebind_target,
        binding,
        ..
    } = Box::pin(sessions.rebind(RebindSessionRequest::new(
        rebind_operation.clone(),
        source,
        selector.clone(),
        target_runtime,
    )))
    .await?
    else {
        return Err("SQLite rebind did not publish".into());
    };
    drop(sessions);

    let sessions = sqlite_test_sessions(&database)?;
    let recovered_rebind = Box::pin(sessions.rebind(RebindSessionRequest::new(
        rebind_operation,
        source,
        selector,
        unauthenticated_runtime_binding(Arc::clone(&workspace), target_descriptor.clone())?,
    )))
    .await?;
    assert!(matches!(
        recovered_rebind,
        RebindSessionOutcome::Rebound {
            target,
            binding: recovered_binding,
            ..
        } if target == rebind_target && recovered_binding == binding
    ));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("sqlite:open-rebound")?,
            rebind_target,
            durable_runtime_binding_for(workspace, target_descriptor, b"sqlite-beta")?,
        ))
        .await?
    else {
        return Err("SQLite rebound target did not reopen".into());
    };
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn exact_open_reports_stale_head_and_never_substitutes_current() -> TestResult {
    let (_directory, workspace, sessions, store, current) = Box::pin(seeded_service()).await?;
    let stale = SessionLocator::new(
        current.session_id(),
        current.branch_id(),
        CheckpointId::try_from(Uuid::from_u128(999))?,
    );
    let before = store.load_branch(current.session_ref()).await?;
    let status_error = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:stale")?,
            stale,
            ContextQuery::Current,
        ))
        .await;
    let Err(status_error) = status_error else {
        return Err("a stale selector produced context status".into());
    };
    assert_eq!(status_error.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(status_error.current_locator(), Some(current));
    let runtime = runtime_binding_for(workspace, descriptor()?, b"account-alpha")?;
    let result = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:stale")?,
            stale,
            runtime,
        ))
        .await;
    let Err(error) = result else {
        return Err("a stale selector opened the current head".into());
    };
    assert_eq!(error.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(error.current_locator(), Some(current));
    let fork_error = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:stale")?,
            stale,
        ))
        .await
        .expect_err("a stale selector published a fork");
    assert_eq!(fork_error.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(fork_error.current_locator(), Some(current));
    let after = store.load_branch(current.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the scripted fixture, persisted safe point, recovery classification, and no-mutation proof are one scenario"
)]
async fn scripted_working_checkpoint_is_recovery_only_and_never_mutated() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"working-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"working-credential"])?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone()),
    );
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = crate::test_model::configured_model(
        model,
        descriptor.clone(),
        authority.clone(),
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero session output bound")?,
        ),
    )?;
    let runtime = SessionRuntimeBinding::new(
        AgentBuilder::new(configured, registry, Arc::clone(&workspace)).request_policy(
            AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ),
        ),
    );
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("working:create")?,
            runtime,
        ))
        .await?;
    let source = session.locator();
    session.release().await?;
    let loaded_genesis = store.load_branch(source.session_ref()).await?;
    let genesis = Arc::clone(loaded_genesis.checkpoint().checkpoint());
    let topology = genesis.manifest().topology();
    let genesis_bundle = loaded_genesis.checkpoint().bundle().clone();
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            RunId::try_from(Uuid::from_u128(901))?,
            NonZeroU64::new(30).ok_or("lease duration must be nonzero")?,
        ))
        .await?;

    let turn_id = TurnId::try_from(Uuid::from_u128(902))?;
    let working_checkpoint_id = CheckpointId::try_from(Uuid::from_u128(903))?;
    let working_root = RuntimeWorkingRoot::Turn {
        turn_id,
        next_round: 1,
    };
    let canonical_input = b"ordinary-recovery-root";
    let command_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "canonical_input": canonical_input,
        "effect": "Root",
        "rejection": null,
    }))?;
    let command_component =
        ReconstructiveComponent::new(RuntimeComponentKind::Command, 1, command_bytes.as_slice())?;
    let command_binding = Digest::sha256(DigestDomain::Blob, 1, &[canonical_input])?;
    let command = DurableCommandRecord::new(
        "ordinary-recovery-root",
        1,
        Some(turn_id),
        command_binding,
        command_component,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let root_operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, command_bytes.as_slice())?,
        vec![
            DurableOperationTransition::new(
                topology.checkpoint_id,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                working_checkpoint_id,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let operation = DurableOperationRecord::new(
        2,
        DurableOperationKind::Provider {
            root: working_root.operation_root(),
            attempt_id: AttemptId::try_from(Uuid::from_u128(904))?,
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, [1_u8].as_slice())?,
        vec![DurableOperationTransition::new(
            working_checkpoint_id,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(vec![command], vec![root_operation, operation])?;
    let command_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::Command,
        RecoveryAction::FinalizationRequired,
    );
    let provider_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::ProviderEvidenceRequired,
    );
    let lifecycle = RuntimeLifecycle::Working {
        root: working_root,
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: vec![
            RecoveryObligation::new(1, command_signature),
            RecoveryObligation::new(2, provider_signature),
        ],
    };
    let working_manifest = genesis.manifest();
    let working = Arc::new(RuntimeCheckpoint::new(RuntimeManifest::new(
        working_manifest.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id: working_checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision: 1,
            fork: None,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"working-fixture"],
            )?,
        },
        lifecycle,
        working_manifest.harness_plan_component().clone(),
        working_manifest.provider_configuration().clone(),
        working_manifest.canonical().clone(),
        working_manifest.binding().clone(),
        working_manifest.tools().clone(),
        ledger,
        working_manifest.family_artifact().cloned(),
        working_manifest.interpreter_observation().cloned(),
        plakous_agent::internal::JobLedger::default(),
        working_manifest.durability(),
    )?)?);
    let working_bundle = RuntimeCheckpointCodec::advance(&working, &genesis_bundle)?;
    let committed = store
        .commit(CommitCheckpoint::new(
            lease.clone(),
            loaded_genesis.head(),
            working_bundle,
        ))
        .await?;
    store.release(lease).await?;
    let committed_head = committed.head();
    let working_locator = SessionLocator::new(
        committed_head.session().session_id(),
        committed_head.session().branch_id(),
        committed_head.checkpoint().checkpoint_id(),
    );
    let before = store.load_branch(working_locator.session_ref()).await?;
    let before_stats = store.stats()?;
    let (_, runtime) = runtime_binding()?;
    // Reopening is the one verb this checkpoint is no longer recovery-only for.
    // Its outstanding work is the turn's own dispatched command and one provider
    // attempt, which is exactly what a user reopening the session may abandon
    // (spec 01 §13.1), so the open no longer turns the caller away; this
    // fixture's hand-built branch simply has no environment to build on. Every
    // other verb below still refuses, and nothing is mutated either way.
    let open = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("working:open")?,
            working_locator,
            runtime,
        ))
        .await;
    assert!(
        !matches!(open, Ok(OpenSessionOutcome::RecoveryRequired { .. })),
        "a reopen may abandon an unfinished model request"
    );

    let (_directory, compact_runtime) = runtime_binding()?;
    let compact = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new("working:compact")?,
            working_locator,
            CompactReason::Manual,
            compact_runtime,
        ))
        .await?;
    let CompactSessionOutcome::RecoveryRequired {
        operation_id,
        recovery,
        ..
    } = &compact
    else {
        return Err("working compact returned ordinary runtime authority".into());
    };
    assert_eq!(operation_id.as_str(), "working:compact");
    assert_eq!(recovery.locator(), working_locator);
    let provider_obligation = recovery
        .obligations()
        .iter()
        .copied()
        .find(|obligation| obligation.action() == SessionRecoveryAction::ProviderEvidenceRequired)
        .ok_or("working recovery omitted provider obligation")?;
    let fork = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("working:fork")?,
            working_locator,
        ))
        .await?;
    assert!(matches!(
        fork,
        ForkSessionOutcome::RecoveryRequired {
            operation_id,
            recovery,
        } if operation_id.as_str() == "working:fork"
            && recovery.locator() == working_locator
    ));
    let target_descriptor = descriptor_for("working.target", "working.target")?;
    let target_runtime = runtime_binding_for(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        target_descriptor,
        b"working-target-account",
    )?;
    let target_binding = target_runtime.target_binding();
    let rebind = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("working:rebind")?,
        working_locator,
        target_binding,
        target_runtime,
    )))
    .await?;
    assert!(matches!(
        rebind,
        RebindSessionOutcome::RecoveryRequired {
            operation_id,
            recovery,
        } if operation_id.as_str() == "working:rebind"
            && recovery.locator() == working_locator
    ));
    let after = store.load_branch(working_locator.session_ref()).await?;
    assert_eq!(after.head(), before.head());
    assert_eq!(
        after.checkpoint().reference(),
        before.checkpoint().reference()
    );
    assert_eq!(store.stats()?, before_stats);

    assert_eq!(provider_obligation.operation_id(), 2);

    Ok(())
}

#[cfg(feature = "sessions-sqlite")]
mod portable_history {
    use super::*;
    use plakous_model::{
        CallId, CompactionBlock, DataClassification, HistorySegment, OpaqueBlock, OpaqueFallback,
        ReasoningBlock, ReasoningVisibility, ReplayEnvelope, ReplayKindId, ReplayScope,
        ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolResultBlock,
        ToolResultKind, ToolResultPart, hex_lower,
    };

    struct PlanCapturingModel {
        inner: Arc<CompletingModel>,
        history: Arc<Mutex<Option<Vec<HistorySegment>>>>,
    }

    impl LanguageModel for PlanCapturingModel {
        fn revalidate_runtime_authority(
            self: Arc<Self>,
            checked_at: Timestamp,
        ) -> RuntimeModelAuthorityOperation {
            self.inner.clone().revalidate_runtime_authority(checked_at)
        }

        fn prepare_call(
            self: Arc<Self>,
            plan: ValidatedRequestPlan,
            context: ModelPreparationContext,
        ) -> Result<PreparedModelCall, ModelPreparationError> {
            *test_lock(&self.history) = Some(plan.request().input().history().segments().to_vec());
            self.inner.clone().prepare_call(plan, context)
        }
    }

    type CapturedHistory = Arc<Mutex<Option<Vec<HistorySegment>>>>;

    pub(super) fn capturing_binding(
        workspace: Arc<WorkspacePolicy>,
        descriptor: ResolvedModelDescriptor,
        account: &[u8],
    ) -> TestResult<(SessionRuntimeBinding, CapturedHistory)> {
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
            1,
            timestamp()?,
            None,
        )?;
        let scripted = Arc::new(
            plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?)
                .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
        );
        let history = Arc::new(Mutex::new(None));
        let model: Arc<dyn LanguageModel> = Arc::new(PlanCapturingModel {
            inner: Arc::new(CompletingModel { scripted }),
            history: Arc::clone(&history),
        });
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = crate::test_model::configured_model(
            model,
            descriptor,
            authority,
            registry.definitions().iter().cloned(),
            LocalDurabilityDisposition::DurableRequired,
            OutputBoundDisposition::Enforced(
                NonZeroU32::new(128).ok_or("nonzero session output bound")?,
            ),
        )?;
        let builder = AgentBuilder::new(configured, registry, workspace)
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::default(),
            ))
            .identity_sources(
                Arc::new(FixedClock::new(timestamp()?)),
                Arc::new(SystemIdGenerator),
            );
        Ok((SessionRuntimeBinding::new(builder), history))
    }

    fn entry(value: u128, actor: Actor, blocks: Vec<ContentBlock>) -> TestResult<ConversationItem> {
        let provenance = if actor == Actor::User {
            Provenance::UserInput
        } else {
            Provenance::ModelOutput
        };
        Ok(ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(value))?,
            actor,
            blocks,
            provenance,
        )?)
    }

    fn replay(kind: &str) -> TestResult<ReplayEnvelope> {
        Ok(ReplayEnvelope::new(
            ProtocolId::new("test.protocol")?,
            None,
            ReplayScope::ExactModel(ModelId::new("test.model")?),
            ReplayKindId::new(kind)?,
            1,
            DataClassification::Sensitive,
            serde_json::value::RawValue::from_string("\"opaque-bytes\"".to_owned())?,
        )?)
    }

    fn text(content: &str) -> TestResult<ContentBlock> {
        Ok(ContentBlock::Text(TextBlock::new(content, None)?))
    }

    /// A conversation with every carried shape: a native compaction item,
    /// user and assistant text, visible and opaque reasoning with replay,
    /// opaque content with and without a fallback summary, and a tool
    /// exchange.
    pub(super) fn carried_segments() -> TestResult<Vec<HistorySegment>> {
        let call_id = CallId::try_from(Uuid::from_u128(0xca11))?;
        let correlation = ToolCorrelation::default();
        Ok(vec![
            HistorySegment::new(
                HistorySegmentKind::Compaction,
                vec![entry(
                    0x10,
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(
                        CompactionBlock::new("opaque-compaction-ciphertext")?
                            .with_native_item("native-compaction".to_owned()),
                    )],
                )?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(0x11, Actor::User, vec![text("read the seed file")?])?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![entry(
                    0x12,
                    Actor::Assistant,
                    vec![
                        ContentBlock::Reasoning(ReasoningBlock::new(
                            ReasoningVisibility::Summary,
                            Some("look at the file first".to_owned()),
                            Some(replay("test.reasoning")?),
                        )?),
                        ContentBlock::Reasoning(ReasoningBlock::new(
                            ReasoningVisibility::OpaqueReplay,
                            None,
                            Some(replay("test.encrypted")?),
                        )?),
                        ContentBlock::Opaque(OpaqueBlock::new(
                            replay("test.annotation")?,
                            OpaqueFallback::SemanticSummary("cited seed.txt".to_owned()),
                        )?),
                        ContentBlock::Opaque(OpaqueBlock::new(
                            replay("test.private")?,
                            OpaqueFallback::Omit,
                        )?),
                    ],
                )?],
            )?,
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    entry(
                        0x13,
                        Actor::Assistant,
                        vec![ContentBlock::ToolCall(ToolCallBlock::new(
                            call_id,
                            ToolName::new("read")?,
                            ToolExecutionOwner::Local,
                            ToolCallState::Requested,
                            correlation.clone(),
                            "{\"path\":\"seed.txt\"}",
                        )?)],
                    )?,
                    entry(
                        0x14,
                        Actor::Tool,
                        vec![ContentBlock::ToolResult(ToolResultBlock::new(
                            call_id,
                            ToolExecutionOwner::Local,
                            ToolResultKind::Success,
                            vec![ToolResultPart::Text(TextBlock::new("seed contents", None)?)],
                            correlation,
                        )?)],
                    )?,
                ],
            )?,
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(
                    0x15,
                    Actor::Assistant,
                    vec![text("the seed file says hello")?],
                )?],
            )?,
        ])
    }

    /// `carried_segments` as a target with another account or model sees it.
    fn stripped_segments() -> TestResult<Vec<HistorySegment>> {
        let mut segments = carried_segments()?;
        segments[2] = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![entry(
                0x12,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(ReasoningBlock::new(
                        ReasoningVisibility::Summary,
                        Some("look at the file first".to_owned()),
                        None,
                    )?),
                    text("cited seed.txt")?,
                ],
            )?],
        )?;
        segments[0] = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![entry(
                0x10,
                Actor::Assistant,
                vec![ContentBlock::Compaction(CompactionBlock::new(
                    "[earlier context was natively compacted and cannot be carried to this binding]",
                )?)],
            )?],
        )?;
        Ok(segments)
    }

    /// Checks that the request history is `expected` followed only by the new
    /// prompt, comparing canonical JSON so a mismatch reads.
    fn assert_next_request_history(
        seen: &[HistorySegment],
        expected: &[HistorySegment],
    ) -> TestResult {
        let (carried, new) = seen
            .split_at_checked(expected.len())
            .ok_or("history was cut")?;
        assert_eq!(
            new.len(),
            1,
            "only the new prompt follows the carried history"
        );
        assert!(serde_json::to_string(new)?.contains("continue"));
        assert_same_history(carried, expected)
    }

    /// Compares histories by their canonical JSON, so a mismatch reads.
    fn assert_same_history(seen: &[HistorySegment], expected: &[HistorySegment]) -> TestResult {
        let seen = serde_json::to_string_pretty(seen)?;
        let expected = serde_json::to_string_pretty(expected)?;
        if let Some((line, (seen, expected))) = seen
            .lines()
            .zip(expected.lines())
            .enumerate()
            .find(|(_, (seen, expected))| seen != expected)
        {
            return Err(format!("history differs at line {line}: {seen} != {expected}").into());
        }
        assert_eq!(seen.lines().count(), expected.lines().count());
        Ok(())
    }

    pub(super) fn account_reference(account: &[u8]) -> TestResult<String> {
        Ok(hex_lower(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?.bytes(),
        ))
    }

    /// Seeds a fresh SQLite session, prompts it once, and returns what the
    /// next model request saw as committed history plus the released head.
    async fn seed_and_prompt(
        directory: &Path,
        name: &str,
        history: PortableSessionHistory,
        descriptor: ResolvedModelDescriptor,
        account: &[u8],
    ) -> TestResult<(Vec<HistorySegment>, PathBuf, SessionLocator)> {
        let database = directory.join(format!("{name}.sqlite"));
        let sessions = sqlite_test_sessions(&database)?;
        let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
        let (runtime, captured) = capturing_binding(workspace, descriptor, account)?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(
                CreateSessionRequest::new(
                    SessionOperationId::new(format!("create:{name}"))?,
                    runtime,
                )
                .with_history(history),
            )
            .await?;
        let result = session.agent().prompt("continue").await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        let locator = session.release().await?;
        let seen = test_lock(&captured)
            .take()
            .ok_or("the next turn issued no model request")?;
        Ok((seen, database, locator))
    }

    #[tokio::test]
    async fn carried_history_round_trips_and_reaches_the_next_request() -> TestResult {
        let directory = tempfile::tempdir()?;
        let account = b"carry-account";
        let carried = PortableSessionHistory::new(
            carried_segments()?,
            account_reference(account)?,
            ModelId::new("test.model")?,
        );
        let (seen, database, locator) =
            seed_and_prompt(directory.path(), "same", carried, descriptor()?, account).await?;
        assert_next_request_history(&seen, &carried_segments()?)?;

        let exported = AgentSessions::export_history_sqlite(&database, locator)?;
        assert_eq!(exported.replay_account(), account_reference(account)?);
        assert_eq!(exported.replay_model().as_str(), "test.model");
        let segments = exported.segments();
        assert!(segments.len() > carried_segments()?.len());
        assert_same_history(&segments[..carried_segments()?.len()], &carried_segments()?)?;

        let (reimported, _, _) = seed_and_prompt(
            directory.path(),
            "again",
            exported.clone(),
            descriptor()?,
            account,
        )
        .await?;
        assert_next_request_history(&reimported, exported.segments())?;
        Ok(())
    }

    /// A carried tool exchange is canonical history of the new session, so its
    /// call detail reads from there; an unknown id or another session's call
    /// is never answered from anywhere else.
    #[tokio::test]
    async fn call_detail_reads_only_the_addressed_sessions_canonical_history() -> TestResult {
        let directory = tempfile::tempdir()?;
        let account = b"carry-account";
        let carried = |segments: Vec<HistorySegment>| -> TestResult<PortableSessionHistory> {
            Ok(PortableSessionHistory::new(
                segments,
                account_reference(account)?,
                ModelId::new("test.model")?,
            ))
        };
        let (_, database, locator) = seed_and_prompt(
            directory.path(),
            "with-call",
            carried(carried_segments()?)?,
            descriptor()?,
            account,
        )
        .await?;
        let sessions = sqlite_test_sessions(&database)?;
        let call_id = CallId::try_from(Uuid::from_u128(0xca11))?;
        let SessionCallDetailOutcome::Found(detail) =
            sessions.call_detail(locator, call_id).await?
        else {
            return Err("carried call is canonical history of the new session".into());
        };
        assert_eq!(detail.call_id(), call_id);
        assert_eq!(detail.exposed_name().as_str(), "read");
        assert_eq!(detail.input().text(), "{\n  \"path\": \"seed.txt\"\n}");
        assert_eq!(detail.output().text(), "seed contents");
        assert!(!detail.input().truncated() && !detail.output().truncated());
        assert_eq!(
            sessions
                .call_detail(locator, CallId::try_from(Uuid::from_u128(0xdead))?)
                .await?,
            SessionCallDetailOutcome::NotInCanonicalHistory
        );

        let segments = carried_segments()?;
        let (_, other_database, other_locator) = seed_and_prompt(
            directory.path(),
            "without-call",
            carried(vec![segments[1].clone(), segments[4].clone()])?,
            descriptor()?,
            account,
        )
        .await?;
        let other = sqlite_test_sessions(&other_database)?;
        assert_eq!(
            other.call_detail(other_locator, call_id).await?,
            SessionCallDetailOutcome::NotInCanonicalHistory
        );
        assert!(other.call_detail(locator, call_id).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn carried_replay_is_dropped_for_another_model_or_account() -> TestResult {
        let directory = tempfile::tempdir()?;
        let account = b"carry-account";
        let carried = || -> TestResult<PortableSessionHistory> {
            Ok(PortableSessionHistory::new(
                carried_segments()?,
                account_reference(account)?,
                ModelId::new("test.model")?,
            ))
        };
        let (other_model, _, _) = seed_and_prompt(
            directory.path(),
            "other-model",
            carried()?,
            descriptor_for("test.route", "test.other-model")?,
            account,
        )
        .await?;
        assert_next_request_history(&other_model, &stripped_segments()?)?;
        let (other_account, _, _) = seed_and_prompt(
            directory.path(),
            "other-account",
            carried()?,
            descriptor()?,
            b"another-account",
        )
        .await?;
        assert_next_request_history(&other_account, &stripped_segments()?)?;
        for history in [&other_model, &other_account] {
            let serialized = serde_json::to_string(history)?;
            assert!(!serialized.contains("opaque-compaction-ciphertext"));
            assert!(serialized.contains("earlier context was natively compacted"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_carried_history_is_refused_before_anything_is_created() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("refused.sqlite");
        let sessions = sqlite_test_sessions(&database)?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let account = b"carry-account";
        let mut segments = carried_segments()?;
        let tool_exchange = segments.remove(3);
        segments.push(tool_exchange.clone());
        segments.push(tool_exchange);
        let error = sessions
            .create(
                CreateSessionRequest::new(
                    SessionOperationId::new("create:refused")?,
                    durable_completing_runtime_binding_for(workspace, descriptor()?, account)?,
                )
                .with_history(PortableSessionHistory::new(
                    segments,
                    account_reference(account)?,
                    ModelId::new("test.model")?,
                )),
            )
            .await
            .err()
            .ok_or("a duplicated carried history created a session")?;
        assert_eq!(error.kind(), AgentSessionsErrorKind::InvalidInput);
        Ok(())
    }
}

/// A recompiling fork tolerates an identity conflict only while it looks for
/// its own receipt; an operation that already published a rebind onto another
/// target is refused, never adopted or duplicated, and so is its lookup.
#[tokio::test]
async fn recompiling_fork_refuses_an_operation_with_other_semantics() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:review-f34-conflict")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await?;
    let outcome = session.agent().prompt("work").await?.result().await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let target_runtime = runtime_binding_for(
        Arc::clone(&workspace),
        descriptor_for_adapter("target.route", "target.model", "target.adapter")?,
        b"account-beta",
    )?;
    let target_selector = target_runtime.target_binding();
    let operation = SessionOperationId::new("op:review-f34-shared")?;
    let RebindSessionOutcome::Rebound { .. } = Box::pin(sessions.rebind(
        RebindSessionRequest::new(operation.clone(), source, target_selector, target_runtime),
    ))
    .await?
    else {
        return Err("the rebind did not publish".into());
    };
    let before = store.stats()?;
    let refused = sessions
        .fork(ForkSessionRequest::recompiling(
            operation,
            source,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await
        .expect_err("a recompiling fork adopted another request's receipt");
    assert_eq!(
        refused.kind(),
        AgentSessionsErrorKind::IdentityConflict,
        "{refused}"
    );
    assert_eq!(store.stats()?, before);
    Ok(())
}

/// A recompiling fork whose only change is its tool set
/// is refused with the canonical-tool-authority refusal Plak maps to
/// "cannot fork: this session's tools changed", and nothing is published.
#[tokio::test]
async fn recompiling_fork_tool_change_refusal_names_canonical_tool_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:recompiling-fork-tools")?,
            runtime_binding_with_registry(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
                ToolRegistryBuilder::default().freeze(),
            )?,
        ))
        .await?;
    let source = session.release().await?;
    let before = store.stats()?;
    let refused = sessions
        .fork(ForkSessionRequest::recompiling(
            SessionOperationId::new("fork:recompiling-tools")?,
            source,
            runtime_binding_with_registry(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
                one_tool_registry()?,
            )?,
        ))
        .await
        .expect_err("a recompiling fork added a tool");
    assert_eq!(
        refused.kind(),
        AgentSessionsErrorKind::Revalidation,
        "{refused}"
    );
    assert!(
        refused
            .to_string()
            .contains("fork changed canonical tool authority"),
        "the tool-change refusal Plak maps for /fork is not what Plakous says: {refused}"
    );
    assert_eq!(store.stats()?, before);
    Ok(())
}

/// The rebind identity a recompiling fork publishes under,
/// derived from the source manifest, is the identity a rebind request with
/// the fork builder's own target selectors names, so a
/// publication recovered either way has the same source and target.
#[tokio::test]
async fn recompiling_fork_source_derived_fork_identity_equals_builder_identity() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let instructed = |instructions: &str| {
        instructed_runtime_binding_for(
            Arc::clone(&workspace),
            descriptor()?,
            b"account-alpha",
            instructions,
        )
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:recompiling-fork-identity")?,
            instructed("instructions as the session started")?,
        ))
        .await?;
    let outcome = session.agent().prompt("work").await?.result().await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let edited = instructed("instructions after the file was edited")?;
    let builder_target = edited.target_binding();
    let ForkSessionOutcome::Forked { target, .. } = sessions
        .fork(ForkSessionRequest::recompiling(
            SessionOperationId::new("fork:recompiling-identity")?,
            source,
            edited,
        ))
        .await?
    else {
        return Err("the fork did not land".into());
    };
    let RebindSessionOutcome::Rebound {
        source: recorded_source,
        target: recorded_target,
        ..
    } = sessions
        .rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new("fork:recompiling-identity")?,
            source,
            builder_target,
            instructed("instructions after the file was edited")?,
        ))
        .await?
    else {
        return Err("the fork's rebind identity differs from its builder's own target".into());
    };
    assert_eq!((recorded_source, recorded_target), (source, target));
    Ok(())
}

#[cfg(all(unix, feature = "sessions-sqlite"))]
#[tokio::test]
async fn attached_context_status_uses_driver_authority_without_store_reads() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = CrashCutStore::observing(SqliteSessionStore::open(
        directory.path().join("context-status.sqlite3"),
    )?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:driver-context")?,
            durable_runtime_binding_for(workspace, descriptor()?, b"driver-context")?,
        ))
        .await?;
    let locator = session.locator();
    let before = store.branch_loads.load(Ordering::Acquire);
    let status = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:driver-context")?,
            locator,
            ContextQuery::Current,
        ))
        .await?;
    assert!(matches!(
        status.kind(),
        SessionContextStatusKind::Available(_)
    ));
    let stale = SessionLocator::new(
        locator.session_id(),
        locator.branch_id(),
        CheckpointId::try_from(Uuid::from_u128(999))?,
    );
    let error = sessions
        .context_status(ContextStatusRequest::new(
            SessionOperationId::new("status:driver-context-stale")?,
            stale,
            ContextQuery::Current,
        ))
        .await
        .expect_err("driver must reject a stale locator");
    assert_eq!(error.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(error.current_locator(), Some(locator));
    assert_eq!(
        store.branch_loads.load(Ordering::Acquire),
        before,
        "context status must not load or decode the canonical head"
    );
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn attached_context_status_revives_expired_but_refuses_taken_over_and_archived_owners() -> TestResult {
    struct AdvancingClock(std::sync::atomic::AtomicU64);
    impl LeaseClock for AdvancingClock {
        fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
            LeaseInstant::new(
                LeaseDomain::new([1; 16], [2; 32]),
                self.0.load(Ordering::Acquire),
                0,
            )
        }
    }
    for disposition in ["revive", "takeover", "archive"] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let clock = Arc::new(AdvancingClock(std::sync::atomic::AtomicU64::new(1)));
        let store = MemorySessionStore::with_clock(MemoryStoreLimits::default(), clock.clone())?;
        let sessions = AgentSessions::new(SessionBackend::from_store(
            Arc::new(store.clone()),
            RuntimeDurability::Disabled,
        ))?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("create:lost-owner-context")?,
                runtime_binding_for(
                    Arc::clone(&workspace),
                    crate::test_model::with_shared_window_context_contract(
                        descriptor_without_context_contract("test.route", "test.model")?,
                        timestamp()?,
                        80,
                    )?,
                    b"lost-owner-context",
                )?,
            ))
            .await?;
        let locator = session.locator();
        let loaded = store.load_branch(locator.session_ref()).await?;
        clock.0.store(10_000, Ordering::Release);
        if disposition == "archive" {
            store
                .archive(ArchiveSession::new(
                    locator.session_id(),
                    loaded.session().revision(),
                ))
                .await?;
        } else if disposition == "takeover" {
            store
                .acquire(AcquireBranch::new(
                    locator.session_ref(),
                    RunId::try_from(Uuid::from_u128(0x9876))?,
                    NonZeroU64::new(30).ok_or("lease duration")?,
                ))
                .await?;
        }
        let status = sessions
            .context_status(ContextStatusRequest::new(
                SessionOperationId::new("status:lost-owner-context")?,
                locator,
                ContextQuery::Current,
            ))
            .await?;
        if disposition == "revive" {
            assert!(matches!(status.kind(), SessionContextStatusKind::Available(_)));
            let pending = "pending after suspend";
            let preflight = sessions.context_status(ContextStatusRequest::new(
                SessionOperationId::new("status:pending-after-suspend")?,
                locator,
                ContextQuery::WithPendingUserInput(pending.to_owned().into()),
            )).await?;
            assert!(matches!(preflight.kind(), SessionContextStatusKind::Available(status)
                if matches!(status.kind(), ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_))));
            let outcome = sessions.compact(CompactSessionRequest::new(
                SessionOperationId::new("compact:after-suspend")?,
                locator,
                CompactReason::Scheduled,
                compaction_runtime_binding_for(
                    workspace,
                    crate::test_model::with_shared_window_context_contract(
                        descriptor_without_context_contract("test.route", "test.model")?,
                        timestamp()?,
                        80,
                    )?,
                    b"lost-owner-context",
                    LocalDurabilityDisposition::Ephemeral,
                )?,
            ).with_pending_user_input(pending.to_owned().into())).await?;
            assert!(matches!(outcome, CompactSessionOutcome::NoEligibleRange { .. }));
            session.release().await?;
        } else {
            assert!(matches!(status.kind(), SessionContextStatusKind::Unavailable(_)));
            // The old owner cannot release the successor's lease.
            drop(session);
        }
    }
    Ok(())
}
