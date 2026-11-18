//! A mid-turn semantic fold on manual Messages thinking ends the turn.
//!
//! The continuation note merges into the tool-result user message, so the
//! manual loop stays open after compaction omitted its thinking prefix.
//! Preparation returns one protected-history failure and does not dispatch again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_agent::{AgentBuilder, AgentRequestPolicy, TurnResultOutcome};
use plakous_messages::{
    MessagesRequestError, MessagesRequestErrorKind, MessagesRequestPolicy,
    MessagesRequestPolicyInput, SIGNATURE_REPLAY_KIND, ToolOwnership, bind_replay_payload,
    lower_request,
};
use plakous_model::*;
use plakous_tools::{
    AlwaysAllowPolicy, ToolExecutorBindingIdentity, ToolExecutorId, ToolRegistration,
    ToolRegistryBuilder,
};
use serde_json::{json, value::RawValue};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const BUDGET: u32 = 1024;
const WINDOW: u64 = 1_000_000;
const PRESSURE: u64 = 900_000;

struct ManualFoldModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    model_id: ModelId,
    semantic: Digest,
    account: Digest,
    prepares: AtomicUsize,
    ordinary: AtomicUsize,
    summaries: AtomicUsize,
    protected: AtomicUsize,
}

fn observed_at() -> TestResult<Timestamp> {
    Ok(Timestamp::new(1, 0)?)
}

fn context_contract(protocol: ProtocolId) -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(observed_at()?, None)?;
    let authority = StandardIntegrationAuthority::release_owned();
    Ok(ContextContract::new(
        protocol,
        "artifact-accounting-v1",
        "artifact-projection-v1",
        "artifact-reservation-v1",
        CapacityCertificate::new(
            &authority,
            "agent-scripted-capacity",
            "agent-scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(WINDOW),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &authority,
            "agent-scripted-counter",
            "agent-scripted-exact-v1",
            "agent-scripted-complete-projection",
            "agent-scripted-method-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?)
}

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, observed_at()?, None);
    let features = [
        Capability::Streaming,
        Capability::Tools,
        Capability::Reasoning,
    ]
    .into_iter()
    .map(|capability| {
        (
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        )
    })
    .collect();
    let protocol = ProtocolId::new("messages")?;
    let descriptor = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        protocol.clone(),
        RouteId::new("messages.manual-fold")?,
        ModelId::new("claude-manual-fixture")?,
        None,
        Some(ModelFamilyId::new("messages-family")?),
        ModelAdapterProfileId::new("messages.manual-fold")?,
        CapabilitySet::new(features, std::collections::BTreeMap::new()),
    );
    Ok(descriptor.with_context_contract(context_contract(protocol)?))
}

fn authority(descriptor: &ResolvedModelDescriptor) -> TestResult<RuntimeModelAuthority> {
    Ok(RuntimeModelAuthority::new(
        descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"manual-fold-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"manual-fold-credential"])?,
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?)
}

fn probe_registry() -> TestResult<plakous_tools::FrozenToolRegistry> {
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
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        plakous_tools::RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new([
            plakous_tools::ToolOutcome::success_text("ok"),
        ])),
    ))?;
    Ok(registry.freeze())
}

fn reasoning_budget() -> TestResult<ReasoningEffort> {
    Ok(ReasoningEffort::TokenBudget(
        std::num::NonZeroU32::new(BUDGET).ok_or("budget")?,
    ))
}

fn provider_behavior(
    descriptor: &ResolvedModelDescriptor,
    registry: &plakous_tools::FrozenToolRegistry,
) -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Enabled(reasoning_budget()?),
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: registry
            .definitions()
            .iter()
            .cloned()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            std::num::NonZeroU32::new(4_096).ok_or("output")?,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("manual-fold-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

fn configured_model(
    model: Arc<ManualFoldModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    registry: &plakous_tools::FrozenToolRegistry,
) -> TestResult<ConfiguredLanguageModel> {
    let behavior = provider_behavior(&descriptor, registry)?;
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("manual-fold-adapter-v1")?,
            ModelAdapterProfileId::new("manual-fold.auth")?,
            ProviderProfileRevisionId::new("manual-fold-auth-v1")?,
            ProviderAuthorityClassId::new("manual-fold")?,
            CredentialSelectionId::new("manual-fold-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"manual-fold-provider-semantics-v1".to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("manual fold descriptor omitted its family")?,
        1,
        1,
        1,
        b"manual-fold-family-policy-v1".to_vec(),
    )?;
    let implementation = ModelImplementationIdentity::release_owned(
        &StandardIntegrationAuthority::release_owned(),
        &descriptor,
        1,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor,
        implementation,
        stable,
        behavior,
        family,
    )?;
    Ok(ConfiguredLanguageModel::new(
        &StandardIntegrationAuthority::release_owned(),
        model,
        CompiledProviderConfiguration::new(selected, effective)?,
        authority,
    )?)
}

fn manual_policy(
    model: &ManualFoldModel,
    plan: &ValidatedRequestPlan,
) -> TestResult<MessagesRequestPolicy> {
    let request = plan.request();
    Ok(MessagesRequestPolicy::new(MessagesRequestPolicyInput {
        protocol: model.descriptor.protocol().clone(),
        requested_model: model.model_id.clone(),
        expected_reported_model: None,
        descriptor_binding: plan.binding_digest(),
        semantic_descriptor_binding: model.semantic,
        account_binding: model.account,
        selected_provider_configuration: request.selected_provider_configuration_id(),
        effective_provider_configuration: request.effective_provider_configuration_id(),
        maximum_output_tokens: 8_192,
        minimum_thinking_budget: Some(BUDGET),
        api_version: "2023-06-01".to_owned(),
        beta_header: String::new(),
    }))
}

fn preparation_rejection(error: MessagesRequestError) -> ModelPreparationError {
    let kind = match error.kind() {
        MessagesRequestErrorKind::ProtectedHistory => ModelPreparationErrorKind::ProtectedHistory,
        MessagesRequestErrorKind::UnsupportedSemantic => {
            ModelPreparationErrorKind::UnsupportedCapability
        }
        MessagesRequestErrorKind::InvalidRequest => ModelPreparationErrorKind::InvalidRequest,
    };
    ModelPreparationError::rejected(kind, error.message())
}

fn compaction_request(plan: &ValidatedRequestPlan) -> bool {
    plan.request().input().items().iter().any(|item| {
        item.blocks().iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text(text) if text.text() == SEMANTIC_COMPACTION_INSTRUCTION
            )
        })
    })
}

fn counted_tokens(plan: &ValidatedRequestPlan, prepare: usize) -> u64 {
    let workflow = matches!(
        plan.request().metadata().root(),
        ModelOperationRoot::SessionWorkflow(_)
    );
    if workflow && !compaction_request(plan) {
        let bytes = plan
            .request()
            .input()
            .history()
            .segments()
            .iter()
            .fold(0usize, |total, segment| {
                total.saturating_add(segment.canonical_bytes().len())
            });
        return u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(4).max(1);
    }
    if !workflow && prepare >= 4 {
        PRESSURE
    } else {
        64
    }
}

// Shared-window admission reserves the request's enforced output cap.
// Omitting it is ExplicitOutputReservationUnavailable, reported as an
// invalid request before any dispatch.
fn enforced_output_limit(digest: Digest) -> Result<PreparedOutputLimit, PreparedCallError> {
    PreparedOutputLimit::enforced_wire_maximum(
        std::num::NonZeroU32::new(4_096).expect("output cap"),
        "max_output_tokens",
        digest,
        "artifact-lowering-v1",
    )
}

fn token_projection(
    digest: Digest,
    size: u64,
    tokens: u64,
) -> Result<PreparedTokenProjection, ModelPreparationError> {
    let observation =
        CountObservation::new("agent-scripted-counter", "agent-scripted-exact-v1", tokens)
            .map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "manual fold count observation is invalid",
                )
            })?;
    let parts_bytes = usize::try_from(tokens.saturating_mul(4)).unwrap_or(usize::MAX);
    let body = format!(r#"{{"input":"{}"}}"#, "x".repeat(parts_bytes));
    Ok(
        PreparedTokenProjection::new(digest, size, Some(observation))?
            .with_input_parts(ContextInputParts::measure_wire(body.as_bytes())),
    )
}

fn text_block(text: &str) -> Result<ContentBlock, ModelError> {
    TextBlock::new(text, None)
        .map(ContentBlock::Text)
        .map_err(|_| ModelError::new(ModelErrorKind::InvalidEvent, "manual fold text is invalid"))
}

fn tool_turn(
    policy: &MessagesRequestPolicy,
    model_id: &ModelId,
    call: usize,
) -> Result<Vec<ContentBlock>, ModelError> {
    let payload =
        RawValue::from_string("\"synthetic-tool-signature\"".to_owned()).map_err(|_| {
            ModelError::new(
                ModelErrorKind::InvalidEvent,
                "manual fold signature is invalid",
            )
        })?;
    let envelope = bind_replay_payload(policy, model_id.clone(), SIGNATURE_REPLAY_KIND, payload)
        .map_err(|_| {
            ModelError::new(
                ModelErrorKind::InvalidEvent,
                "manual fold thinking replay is invalid",
            )
        })?;
    let reasoning = ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("fixture thought".to_owned()),
        Some(envelope),
    )
    .map_err(|_| {
        ModelError::new(
            ModelErrorKind::InvalidEvent,
            "manual fold thinking block is invalid",
        )
    })?;
    let call_id = CallId::try_from(uuid::Uuid::from_u128(8_000 + call as u128)).map_err(|_| {
        ModelError::new(
            ModelErrorKind::InvalidEvent,
            "manual fold call id is invalid",
        )
    })?;
    // Messages replays `tool_use.id` from the remote correlation. A default
    // correlation has none, so the post-tool prepare is an invalid request
    // before the fence can fold.
    let correlation = ToolCorrelation::new(
        Some(format!("call_{}", 8_000 + call)),
        None,
        ToolCallerLineage::Direct,
    )
    .map_err(|_| {
        ModelError::new(
            ModelErrorKind::InvalidEvent,
            "manual fold tool correlation is invalid",
        )
    })?;
    let tool = ToolCallBlock::new(
        call_id,
        ToolName::new("probe").map_err(|_| {
            ModelError::new(
                ModelErrorKind::InvalidEvent,
                "manual fold tool name is invalid",
            )
        })?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        correlation,
        "{}",
    )
    .map_err(|_| {
        ModelError::new(
            ModelErrorKind::InvalidEvent,
            "manual fold tool call is invalid",
        )
    })?;
    Ok(vec![
        ContentBlock::Reasoning(reasoning),
        ContentBlock::ToolCall(tool),
    ])
}

fn response_events(
    plan: &ValidatedRequestPlan,
    blocks: Vec<ContentBlock>,
    finish: FinishReason,
    call: usize,
) -> Result<Vec<Result<ModelEvent, ModelError>>, ModelError> {
    let item = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(9_000 + call as u128)).map_err(|_| {
            ModelError::new(ModelErrorKind::InvalidEvent, "manual fold entry is invalid")
        })?,
        Actor::Assistant,
        blocks.clone(),
        Provenance::ModelOutput,
    )
    .map_err(|_| ModelError::new(ModelErrorKind::InvalidEvent, "manual fold item is invalid"))?;
    let block = blocks.into_iter().next().ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::InvalidEvent,
            "manual fold response is empty",
        )
    })?;
    Ok(vec![
        Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
            None,
            plan.digest(),
        ))),
        Ok(ModelEvent::BlockStarted(BlockStarted::new(
            item.id(),
            OutputLocation::new(0, 0),
            BlockStart::Complete(block),
        ))),
        Ok(ModelEvent::ResponseCompleted(ModelResponse::new(
            vec![item],
            FinishDetails::new(finish, None),
            None,
            None,
            plan.digest(),
        ))),
    ])
}

impl LanguageModel for ManualFoldModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.authority.validate(&self.descriptor, checked_at)?;
            Ok(self.authority.clone())
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let policy = manual_policy(&self, &plan).map_err(|error| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                error.to_string(),
            )
        })?;
        if let Err(error) = lower_request(&plan, &policy, ToolOwnership::default()) {
            if error.kind() == MessagesRequestErrorKind::ProtectedHistory {
                self.protected.fetch_add(1, Ordering::AcqRel);
            }
            return Err(preparation_rejection(error));
        }
        let prepare = self.prepares.fetch_add(1, Ordering::AcqRel);
        let tokens = counted_tokens(&plan, prepare);
        let wire = plan.digest();
        let digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"manual-fold-wire-v1", wire.bytes()],
        )
        .map_err(PreparedCallError::from)?;
        let size =
            u64::try_from(digest.bytes().len()).map_err(|_| PreparedCallError::InvalidWireSize)?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "artifact-lowering-v1",
            "artifact-projection-v1",
            "artifact-accounting-v1",
            digest,
            size,
            token_projection(digest, size, tokens)?,
            PreparedCacheUse::None,
            Some(enforced_output_limit(digest)?),
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let compaction = compaction_request(&plan);
        let model_id = self.model_id.clone();
        Ok(PreparedModelCall::new(record, move |_call_context| {
            let plan = plan.clone();
            let policy = policy.clone();
            let model_id = model_id.clone();
            Box::pin(async move {
                let (blocks, finish, call) = if compaction {
                    let call = self.summaries.fetch_add(1, Ordering::AcqRel);
                    (
                        vec![text_block("folded earlier turns")?],
                        FinishReason::Stop,
                        call,
                    )
                } else {
                    let call = self.ordinary.fetch_add(1, Ordering::AcqRel);
                    if call == 2 {
                        (
                            tool_turn(&policy, &model_id, call)?,
                            FinishReason::ToolCalls,
                            call,
                        )
                    } else {
                        (
                            vec![text_block(&format!("artifact response {call}"))?],
                            FinishReason::Stop,
                            call,
                        )
                    }
                };
                let events = response_events(&plan, blocks, finish, call)?;
                Ok(Box::pin(futures_util::stream::iter(events)) as ModelStream)
            })
        }))
    }
}

#[tokio::test]
async fn manual_mid_turn_fold_ends_on_protected_history() -> TestResult {
    let descriptor = descriptor()?;
    let authority = authority(&descriptor)?;
    let registry = probe_registry()?;
    let model = Arc::new(ManualFoldModel {
        model_id: descriptor.requested_model().clone(),
        semantic: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"manual-fold-semantic"])?,
        account: Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"manual-fold-account-replay"],
        )?,
        descriptor: descriptor.clone(),
        authority: authority.clone(),
        prepares: AtomicUsize::new(0),
        ordinary: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
        protected: AtomicUsize::new(0),
    });
    let configured = configured_model(Arc::clone(&model), descriptor, authority, &registry)?;
    let root = tempfile::tempdir()?;
    let agent = AgentBuilder::new(
        configured,
        registry,
        Arc::new(plakous_tools::WorkspacePolicy::host(root.path(), ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(std::num::NonZeroU32::new(4_096), Vec::new())?,
        ReasoningPolicy::Enabled {
            effort: Some(reasoning_budget()?),
            requirement: CapabilityRequirement::Required,
        },
    ))
    .tool_policy(Arc::new(AlwaysAllowPolicy))
    .identity_sources(
        Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
        Arc::new(SystemIdGenerator),
    )
    .with_scheduled_compaction_enabled(true)
    .build()?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        if let TurnResultOutcome::Failed(error) = result.outcome() {
            return Err(format!(
                "setup turn failed kind={:?} model={:?} message={} prepares={} ordinary={} summaries={} protected={}",
                error.kind(),
                error.model_kind(),
                error.message(),
                model.prepares.load(Ordering::Acquire),
                model.ordinary.load(Ordering::Acquire),
                model.summaries.load(Ordering::Acquire),
                model.protected.load(Ordering::Acquire),
            )
            .into());
        }
    }
    let result = agent
        .prompt("use the growing probe at the tool-group fence")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "turn did not end on protected history: outcome={:?} prepares={} ordinary={} summaries={} protected={}",
            result.outcome(),
            model.prepares.load(Ordering::Acquire),
            model.ordinary.load(Ordering::Acquire),
            model.summaries.load(Ordering::Acquire),
            model.protected.load(Ordering::Acquire),
        )
        .into());
    };
    if error.model_kind() != Some(ModelErrorKind::ProtectedHistory) {
        return Err(format!(
            "turn ended on {:?} ({}) prepares={} ordinary={} summaries={} protected={}",
            error.model_kind(),
            error.message(),
            model.prepares.load(Ordering::Acquire),
            model.ordinary.load(Ordering::Acquire),
            model.summaries.load(Ordering::Acquire),
            model.protected.load(Ordering::Acquire),
        )
        .into());
    }
    assert_eq!(model.summaries.load(Ordering::Acquire), 1);
    assert_eq!(model.ordinary.load(Ordering::Acquire), 3);
    assert_eq!(model.protected.load(Ordering::Acquire), 1);
    assert!(
        agent.conversation().working().segments().is_empty(),
        "a protected-history terminal publishes an empty working projection"
    );
    agent.shutdown().await?;
    Ok(())
}
