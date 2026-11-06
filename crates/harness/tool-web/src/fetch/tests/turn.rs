//! An actual pack fetch through an agent turn, without provider traffic.

use super::*;
use plakous_agent::{AgentBuilder, AgentRequestPolicy};
use plakous_model::{
    Actor, BlockStart, BlockStarted, CachePolicy, CallId, Capability, CapabilityConstraints,
    CapabilitySet, CapacityCertificate, CapacityRule, CompiledProviderConfiguration,
    ConfiguredLanguageModel, ConfiguredTool, ContentBlock, ContextAdmissionRequirement,
    ContextContract, ContextEvidenceSource, CountCertificateKind, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration, EntryId,
    EvidenceFreshness, FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport,
    FinishDetails, FinishReason, FixedClock, GenerationLimits, LocalDurabilityDisposition,
    MeasurementCertificate, ModelAdapterProfileId, ModelEvent, ModelFamilyId, ModelId,
    ModelImplementationIdentity, ModelResponse, NamespaceId, OutputBoundDisposition,
    OutputLocation, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition, ProtocolId,
    Provenance, ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStarted, ResponseStateDisposition,
    RetentionDisposition, RouteId, RuntimeModelAuthority, SelectedProviderConfiguration, Support,
    SystemIdGenerator, TextBlock, Timestamp, ToolCallBlock, ToolCallState, ToolCallerDisposition,
    ToolChoice, ToolCorrelation, ToolExecutionDisposition, ToolExecutionOwner,
    ToolPresentationMode, ToolResultKind,
};
use plakous_test_support::{
    ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep, ScriptedModel,
};
use plakous_tools::{
    ImageNormalizer, PROCESS_TOOL_PROJECTION_REVISION, PreparedImage,
    ProviderModelProjectionIdentityV2, ToolRegistryBuilder, WorkspacePolicy, compile,
};

struct UnusedNormalizer;

impl ImageNormalizer for UnusedNormalizer {
    fn normalize(&self, _: &[u8]) -> Result<PreparedImage, String> {
        Err("fetch_url uses its bounded image bytes directly".to_owned())
    }
}

fn descriptor(accepts_images: bool) -> TestResult<ResolvedModelDescriptor> {
    let time = Timestamp::new(1, 0)?;
    let mut capabilities = vec![Capability::Tools, Capability::Streaming];
    if accepts_images {
        capabilities.push(Capability::ImageInput);
    }
    let descriptor = ResolvedModelDescriptor::new_custom(
        ProtocolId::new("test.fetch")?,
        RouteId::new("test.fetch")?,
        ModelId::new("test.fetch")?,
        None,
        Some(ModelFamilyId::new("test.fetch")?),
        ModelAdapterProfileId::new("test.fetch")?,
        CapabilitySet::new(
            capabilities
                .into_iter()
                .map(|capability| {
                    (
                        capability,
                        FeatureSupport::new(
                            Support::Native,
                            CapabilityConstraints::default(),
                            FactProvenance::new(FactSource::TestFixture, time, None),
                        ),
                    )
                })
                .collect(),
            BTreeMap::new(),
        ),
    );
    let freshness = EvidenceFreshness::new(time, None)?;
    let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"test-fetch"])?;
    let class = RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?
        .authority_class_binding()?;
    Ok(descriptor.with_context_contract(ContextContract::new(
        ProtocolId::new("test.fetch")?,
        "test-accounting",
        "test-projection",
        "test-reservation",
        CapacityCertificate::custom_envelope(
            "test-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            class,
            freshness,
            binding,
            "v1",
        )?,
        vec![MeasurementCertificate::new(
            "test-counter",
            "v1",
            "test-complete",
            "v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound {
                declaration: binding,
            },
            Some(class),
            freshness,
        )?],
    )?))
}

fn configuration(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    definitions: &[ToolDefinition],
) -> TestResult<CompiledProviderConfiguration> {
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: definitions
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
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    compile_configuration(descriptor, authority, behavior)
}

fn compile_configuration(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    behavior: ProviderBehaviorConfiguration,
) -> TestResult<CompiledProviderConfiguration> {
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("v1")?,
            ModelAdapterProfileId::new("test.auth")?,
            ProviderProfileRevisionId::new("v1")?,
            ProviderAuthorityClassId::new("test")?,
            CredentialSelectionId::new("test")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::custom(
            NamespaceId::new("test.fetch")?,
            1,
            NamespaceId::new("test.fetch")?,
        )?,
        ResolvedProviderAuthorityBinding::new(
            descriptor,
            authority,
            ProviderSemanticEvidence::new(1, b"test-fetch".to_vec())?,
        )?,
        behavior,
        FamilyPolicyEnvelope::new(
            descriptor.model_family().ok_or("missing family")?.clone(),
            1,
            1,
            1,
            b"test-fetch".to_vec(),
        )?,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

fn projection_target(
    configuration: &CompiledProviderConfiguration,
) -> TestResult<ProviderModelProjectionIdentityV2> {
    let descriptor = configuration.effective().descriptor();
    Ok(ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor.model_family().ok_or("missing family")?.as_str(),
        descriptor.requested_model().as_str(),
        configuration.effective().effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )?)
}

fn scenario(
    block: ContentBlock,
    finish: FinishReason,
    sequence: u128,
) -> TestResult<ModelScenario> {
    use std::str::FromStr;
    let item = plakous_model::ConversationItem::new(
        EntryId::from_str(&format!("00000000-0000-0000-0000-{sequence:012x}"))?,
        Actor::Assistant,
        vec![block.clone()],
        Provenance::ModelOutput,
    )?;
    let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"placeholder"])?;
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![
            ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                None, digest,
            ))),
            ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                OutputLocation::new(0, 0),
                BlockStart::Complete(block),
            ))),
            ScenarioStep::Emit(ModelEvent::ResponseCompleted(ModelResponse::new(
                vec![item],
                FinishDetails::new(finish, None),
                None,
                None,
                digest,
            ))),
        ],
    ))
}

#[tokio::test]
async fn pack_fetch_image_turn_completes_with_route_appropriate_model_content() -> TestResult {
    use std::str::FromStr;
    let peer = peer(vec![(
        "/image",
        reply("200 OK", &[("content-type", "image/png")], &png(2, 3)),
    )])
    .await?;
    for presentation in [CodingPresentation::Grok, CodingPresentation::Claude] {
        for (accepts_images, route_images) in [(true, true), (false, true), (true, false)] {
            let descriptor = descriptor(accepts_images)?;
            let time = Timestamp::new(1, 0)?;
            let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"test-fetch"])?;
            let authority =
                RuntimeModelAuthority::new(&descriptor, binding, binding, 1, time, None)?;
            let pack: Arc<dyn ToolPack> = Arc::new(
                FetchUrlPack::with_client(peer.client.clone(), FetchUrlLimits::default(), UA)?
                    .with_image_results(route_images),
            );
            let mut registry = ToolRegistryBuilder::default();
            plakous_tools::register_packs(&[Arc::clone(&pack)], &mut registry)?;
            let definitions = registry.freeze().definitions().to_vec();
            let configuration = configuration(&descriptor, &authority, &definitions)?;
            let target = projection_target(&configuration)?;
            let surface = compile(
                &[pack],
                presentation,
                ToolRegistryBuilder::default(),
                target,
                None,
            )?;
            let call = ToolCallBlock::new(
                CallId::from_str("00000000-0000-0000-0000-000000000100")?,
                ToolName::new("fetch_url")?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                json!({"url":format!("{}/image", peer.base)}).to_string(),
            )?;
            let model = Arc::new(
                ScriptedModel::new(
                    descriptor,
                    [
                        scenario(ContentBlock::ToolCall(call), FinishReason::ToolCalls, 1000)?,
                        scenario(
                            ContentBlock::Text(TextBlock::new("done", None)?),
                            FinishReason::Stop,
                            1001,
                        )?,
                    ],
                )
                .with_runtime_authority(authority.clone())
                .with_context_observation(CountObservation::new("test-counter", "v1", 64)?),
            );
            let configured =
                ConfiguredLanguageModel::custom(model.clone(), configuration, authority)?;
            let workspace = tempfile::tempdir()?;
            let agent = AgentBuilder::new(
                configured,
                surface.registry().clone(),
                Arc::new(WorkspacePolicy::host(workspace.path(), ".")?),
            )
            .tool_projection(surface.into_projection())
            .image_normalizer(Arc::new(UnusedNormalizer))
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::default(),
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(Arc::new(FixedClock::new(time)), Arc::new(SystemIdGenerator))
            .build()?;
            let result = agent.prompt("fetch the image").await?.result().await?;
            assert_eq!(
                result
                    .response()
                    .ok_or("missing final response")?
                    .output_text(),
                "done"
            );
            let plans = model.captured_plans();
            assert_eq!(plans.len(), 2);
            assert_model_content(plans[1].request(), accepts_images && route_images)?;
            agent.shutdown().await?;
        }
    }
    Ok(())
}

fn assert_model_content(request: &plakous_model::ModelRequest, images: bool) -> TestResult {
    let protocol = ProtocolId::new("test.fetch")?;
    let family = ModelFamilyId::new("test.fetch")?;
    let model_id = ModelId::new("test.fetch")?;
    let lowered = plakous_responses::lower_responses_request(
        request,
        plakous_responses::ResponsesRequestConfig::new(
            &protocol,
            Some(&family),
            &model_id,
            plakous_responses::ResponsesRequestPolicy::supergrok_stateless(),
            None,
        ),
    )?;
    let output = lowered.body()["input"]
        .as_array()
        .ok_or("missing wire input")?
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .ok_or("missing lowered tool result")?;
    if images {
        assert_eq!(output["output"][1]["type"], "input_image");
        assert!(
            output["output"][1]["image_url"]
                .as_str()
                .ok_or("missing image URL")?
                .starts_with("data:image/png;base64,")
        );
    } else {
        assert!(output["output"].is_string());
    }
    let results = request
        .input()
        .items()
        .iter()
        .flat_map(plakous_model::ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].kind(), ToolResultKind::Success);
    let [ToolResultPart::Text(text), rest @ ..] = results[0].parts() else {
        return Err("missing raised text".into());
    };
    let body: Value = serde_json::from_str(text.text())?;
    let head = body["result"].as_str().ok_or("missing result string")?;
    assert!(head.contains("image: media_type=image/png bytes=29 dimensions=2x3"));
    if images {
        assert!(matches!(rest, [ToolResultPart::Image(_)]));
        assert!(!head.contains("omitted"));
    } else {
        assert_eq!(rest, []);
        assert!(
            head.ends_with("\n[1 image(s) omitted: this tool's result carries text only here]")
        );
    }
    Ok(())
}
