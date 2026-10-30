use super::*;

#[tokio::test]
async fn sanitized_production_messages_thinking_signature_validates_for_carry() -> TestResult {
    let policy = behavior()?;
    let bound = policy.output_bound.bound();
    let (configured, model) = AnthropicLanguageModel::build(
        Arc::new(crate::AnthropicApiKey::new("synthetic-carry-key")?),
        "carry-fixture",
        &crate::AnthropicApiModel::bundled("claude-sonnet-4-6")?,
        b"sanitized-carry-facts",
        policy,
        NonZeroU64::new(65536).ok_or("window")?,
        Client::new_http1(Limits::for_streaming_inference())?.pooled(),
        None,
    )?;
    let target = crate::AnthropicMessagesTargetBinding::from_resolved(model.resolved())?;
    let replay_policy = plakous_messages::MessagesRequestPolicy::new(
        target.protocol().clone(),
        target.model().remote_id().clone(),
        target.expected_reported_model().cloned(),
        target.descriptor_binding(),
        target.semantic_descriptor_binding(),
        target.replay_account_binding().ok_or("account")?,
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        target.model().maximum_output_tokens(),
        None,
        target.dialect().api_version(),
        target.dialect().beta_header().unwrap_or_default(),
    );
    let replay = plakous_messages::bind_replay_payload(
        &replay_policy,
        target.replay_model().clone(),
        plakous_messages::SIGNATURE_REPLAY_KIND,
        serde_json::value::RawValue::from_string("\"synthetic-signature\"".to_owned())?,
    )?;
    let segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            "00000000-0000-0000-0000-00000000cafe".parse::<EntryId>()?,
            Actor::Assistant,
            vec![
                ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("sanitized thought".into()),
                    Some(replay),
                )?),
                ContentBlock::Text(TextBlock::new("sanitized answer", None)?),
            ],
            Provenance::ModelOutput,
        )?],
    )?;
    let root = tempfile::tempdir()?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )
    .request_policy(
        AgentRequestPolicy::new(
            GenerationLimits::new(bound, Vec::new())?,
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Low),
                requirement: CapabilityRequirement::Required,
            },
        )
        .with_context_admission_requirement(ContextAdmissionRequirement::AnyBounded),
    );
    plakous_agent::internal::facade_support::validate_seed_replay(&builder, &[segment])
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}
