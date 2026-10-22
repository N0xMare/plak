use super::*;

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one native fixture binds exact catalog, credentials, configuration and inert replay"
)]
async fn sanitized_codex_native_compaction_passes_carry_replay_validation() -> TestResult {
    use plakous_agent::{AgentBuilder, AgentRequestPolicy};
    use plakous_model::{
        CompactionBlock, CompactionReplayBinding, ConfiguredLanguageModel,
        ContextAdmissionRequirement, CredentialSelectionId, LocalDurabilityDisposition,
        ModelAdapterProfileId, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition,
        ProcessingTierDisposition, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
        ProviderProfileRevisionId, ProviderRouteSelection, ProviderStorageDisposition,
        ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
        ReplayDisposition, ResponseStateDisposition, RetentionDisposition, RouteId,
        SelectedProviderConfiguration, StandardIntegrationAuthority, SystemIdGenerator, ToolChoice,
        ToolPresentationMode,
    };
    let model_id = "gpt-5.6-luna";
    let mut row = catalog_model_value(model_id);
    row["tool_mode"] = serde_json::json!("direct");
    let (_credentials, resolver, _catalog) =
        resolver_for_catalog_values("carry-native-compaction", vec![row]).await?;
    let (model, evidence) = resolver.resolve_with_evidence(model_id)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(resolver.clock.now())
        .await?;
    let descriptor = evidence.validated_descriptor()?;
    let route = ProviderRouteSelection::new(
        descriptor.protocol().clone(),
        RouteId::new(crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
        ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?,
        ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        ProviderProfileRevisionId::new(OPENAI_CODEX_SOURCE_REVISION)?,
        ProviderAuthorityClassId::new("oauth-subscription")?,
        CredentialSelectionId::new("primary")?,
        descriptor.requested_model().clone(),
    );
    let selected = SelectedProviderConfiguration::new(
        route,
        ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
            reasoning_summary: ReasoningSummaryDisposition::Auto,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
                revision: ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )?;
    let configuration =
        crate::compile_codex_subscription_configuration(&selected, &authority, &evidence)?;
    let native=CompactionBlock::new("synthetic-native-ciphertext")?
        .with_native_item(serde_json::json!({"type":"compaction","encrypted_content":"synthetic-native-ciphertext"}).to_string())
        .with_native_binding(CompactionReplayBinding::new(descriptor.protocol().clone(),descriptor.model_family().cloned(),descriptor.requested_model().clone(),authority.account_binding()));
    let history = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            "00000000-0000-0000-0000-00000000cafe".parse::<EntryId>()?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(native)],
            Provenance::ModelOutput,
        )?],
    )?;
    let configured = ConfiguredLanguageModel::new(
        &StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?;
    let root = TempDir::new()?;
    let builder = AgentBuilder::new(
        configured,
        plakous_tools::ToolRegistryBuilder::default().freeze(),
        Arc::new(plakous_tools::WorkspacePolicy::host(root.path(), ".")?),
    )
    .request_policy(
        AgentRequestPolicy::new(
            GenerationLimits::new(None, Vec::new())?,
            plakous_model::ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Low),
                requirement: CapabilityRequirement::Required,
            },
        )
        .with_context_admission_requirement(ContextAdmissionRequirement::AnyBounded),
    )
    .identity_sources(Arc::clone(&resolver.clock), Arc::new(SystemIdGenerator));
    plakous_agent::internal::facade_support::validate_seed_replay(&builder, &[history])
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
