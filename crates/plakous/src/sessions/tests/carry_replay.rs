//! Sanitized canonical replay through production dialect lowering, never dispatch.
use super::*;
use plakous_model::{
    CachePolicy, ConfiguredLanguageModel, DataClassification, HistorySegment, HistorySegmentKind,
    OpaqueBlock, OpaqueFallback, OutputPolicy, ParallelToolDisposition, ProcessingTierDisposition,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderStorageDisposition,
    ReasoningBlock, ReasoningEffort, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReasoningVisibility, ReplayDisposition, ReplayEnvelope,
    ReplayKindId, ReplayScope, ResponseStateDisposition, RetentionDisposition, SystemClock,
    ToolChoice, ToolPresentationMode,
};

fn behavior() -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
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
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output")?),
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new("openai.platform.responses")?,
            revision: ProviderProfileRevisionId::new("carry-fixture-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: plakous_model::ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}
async fn responses() -> TestResult<ConfiguredLanguageModel> {
    responses_with_key("synthetic-carry-key").await
}
async fn responses_with_key(key: &str) -> TestResult<ConfiguredLanguageModel> {
    use plakous_openai::*;
    let reference = CredentialReference::application("carry-sanitized-fixture")?;
    let source = Arc::new(StaticPlatformCredentialSource::new(
        SecretString::new(key)?,
        reference.clone(),
        CredentialAuthorityId::new("carry-fixture")?,
    ));
    let authority = OpenAiPlatformModelBuilder::new(
        reference,
        source,
        OpenAiPlatformModelFacts::Gpt56Luna,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .context_envelope(NonZeroU64::new(65536).ok_or("window")?)
    .seal()?;
    let mut policy = behavior()?;
    policy.retention = RetentionDisposition::RouteContract {
        profile: authority.descriptor().profile().clone(),
        revision: ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?,
    };
    let selected = authority.selected_configuration(policy)?;
    Ok(Arc::new(OpenAiPlatformModel::with_exchange_observer(
        authority, selected, None,
    )?)
    .configured()
    .await?)
}
fn reasoning_segment(id: u128, blocks: Vec<ContentBlock>) -> TestResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(id))?,
            Actor::Assistant,
            blocks,
            Provenance::ModelOutput,
        )?],
    )?)
}
fn envelope(
    configured: &ConfiguredLanguageModel,
    kind: &str,
    version: u32,
    payload: &serde_json::Value,
) -> TestResult<ReplayEnvelope> {
    let descriptor = configured.registered().descriptor();
    Ok(ReplayEnvelope::new(
        descriptor.protocol().clone(),
        None,
        ReplayScope::ExactAccountModel {
            model: descriptor.requested_model().clone(),
            account: configured.initial_authority().account_binding(),
        },
        ReplayKindId::new(kind)?,
        version,
        DataClassification::Sensitive,
        serde_json::value::RawValue::from_string(payload.to_string())?,
    )?)
}
fn builder(
    configured: ConfiguredLanguageModel,
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<AgentBuilder> {
    let behavior = configured.configuration().effective().behavior();
    let request = AgentRequestPolicy::new(
        GenerationLimits::new(behavior.output_bound.bound(), Vec::new())?,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: plakous_model::CapabilityRequirement::Required,
        },
    )
    .with_context_admission_requirement(behavior.context_evidence_requirement);
    Ok(AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .request_policy(request)
    .identity_sources(Arc::new(SystemClock), Arc::new(SystemIdGenerator)))
}

#[tokio::test]
async fn sanitized_production_responses_encrypted_reasoning_validates() -> TestResult {
    let root = TempDir::new()?;
    let configured = responses().await?;
    let replay = envelope(
        &configured,
        "responses.output_item",
        1,
        &serde_json::json!({"type":"reasoning","id":"rs_carry_fixture","status":"completed","summary":[],"encrypted_content":"synthetic-ciphertext"}),
    )?;
    let reasoning = reasoning_segment(
        1,
        vec![ContentBlock::Reasoning(ReasoningBlock::new(
            ReasoningVisibility::OpaqueReplay,
            None,
            Some(replay),
        )?)],
    )?;
    let builder = builder(
        configured,
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )?;
    plakous_agent::internal::facade_support::validate_seed_replay(&builder, &[reasoning])
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
#[tokio::test]
async fn invalid_local_native_replay_creates_a_stripped_successor_instead_of_refusing() -> TestResult
{
    let root = TempDir::new()?;
    let configured = responses().await?;
    let account =
        plakous_model::hex_lower(configured.initial_authority().account_binding().bytes());
    let model = configured
        .registered()
        .descriptor()
        .requested_model()
        .clone();
    let native = plakous_model::CompactionBlock::new("synthetic-ciphertext")?
        .with_native_item("[]".to_owned());
    let source = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(4))?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("visible carried answer", None)?),
                ContentBlock::Compaction(native),
            ],
            Provenance::ModelOutput,
        )?],
    )?;
    let runtime = SessionRuntimeBinding::new(builder(
        configured,
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )?);
    let history =
        PortableSessionHistory::new(vec![source], account, model).with_replay_validation();
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(SessionOperationId::new("carry-invalid-replay")?, runtime)
                .with_history(history),
        )
        .await?;
    assert!(session.replay_stripped());
    assert!(
        agent_checkpoint(session.agent())
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .all(
                |b| !matches!(b, ContentBlock::Compaction(native) if native.native_item().is_some())
            )
    );
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn unsupported_nested_opaque_result_successfully_carries_with_stripping() -> TestResult {
    use plakous_model::{
        CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolResultBlock,
        ToolResultKind, ToolResultPart,
    };
    let root = TempDir::new()?;
    let configured = responses().await?;
    let replay = envelope(
        &configured,
        "responses.output_item",
        1,
        &serde_json::json!({"type":"message","role":"assistant","id":"msg_carry_fixture","status":"completed","content":[{"type":"output_text","text":"sanitized result","annotations":[]}]}),
    )?;
    let call = CallId::try_from(Uuid::from_u128(10))?;
    let request = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(11))?,
        Actor::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock::new(
            call,
            ToolName::new("lookup")?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )?)],
        Provenance::ModelOutput,
    )?;
    let result = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(12))?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Opaque(OpaqueBlock::new(
                replay,
                OpaqueFallback::SemanticSummary("sanitized result".into()),
            )?)],
            ToolCorrelation::default(),
        )?)],
        Provenance::ToolExecution,
    )?;
    let segment = HistorySegment::new(HistorySegmentKind::ToolExchange, vec![request, result])?;
    let account =
        plakous_model::hex_lower(configured.initial_authority().account_binding().bytes());
    let model = configured
        .registered()
        .descriptor()
        .requested_model()
        .clone();
    let builder = builder(
        configured,
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )?;
    let error = plakous_agent::internal::facade_support::validate_seed_replay(
        &builder,
        std::slice::from_ref(&segment),
    )
    .await
    .err()
    .ok_or("nested opaque part unexpectedly admitted")?;
    assert_eq!(error.kind(), AgentErrorKind::InvalidInput);
    let history =
        PortableSessionHistory::new(vec![segment], account, model).with_replay_validation();
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("carry-nested-opaque")?,
                SessionRuntimeBinding::new(builder),
            )
            .with_history(history),
        )
        .await?;
    assert!(session.replay_stripped());
    let checkpoint = agent_checkpoint(session.agent());
    let result = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .find_map(|block| {
            if let ContentBlock::ToolResult(result) = block {
                Some(result)
            } else {
                None
            }
        })
        .ok_or("result missing")?;
    assert!(
        matches!(result.parts(),[ToolResultPart::Text(text)] if text.text()=="sanitized result")
    );
    session.release().await?;
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one offline workflow pins account scope through validated import, rebind and ordinary fork"
)]
async fn responses_account_scope_survives_import_rebind_and_fork() -> TestResult {
    let root = TempDir::new()?;
    let workspace = Arc::new(WorkspacePolicy::host(root.path(), ".")?);
    let configured_a = responses_with_key("synthetic-account-A").await?;
    let configured_b = responses_with_key("synthetic-account-B").await?;
    let account_a = configured_a.initial_authority().account_binding();
    let account_b = configured_b.initial_authority().account_binding();
    assert_ne!(account_a, account_b);
    let model = configured_a
        .registered()
        .descriptor()
        .requested_model()
        .clone();
    let replay = envelope(
        &configured_a,
        "responses.output_item",
        1,
        &serde_json::json!({"type":"reasoning","id":"rs_account_A","status":"completed","summary":[],"encrypted_content":"synthetic-account-A-ciphertext"}),
    )?;
    let segment = reasoning_segment(
        70_001,
        vec![ContentBlock::Reasoning(ReasoningBlock::new(
            ReasoningVisibility::OpaqueReplay,
            None,
            Some(replay.clone()),
        )?)],
    )?;
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("scope-import-A")?,
                SessionRuntimeBinding::new(builder(configured_a, Arc::clone(&workspace))?),
            )
            .with_history(
                PortableSessionHistory::new(
                    vec![segment.clone()],
                    plakous_model::hex_lower(account_a.bytes()),
                    model.clone(),
                )
                .with_replay_validation(),
            ),
        )
        .await?;
    assert!(!session.replay_stripped());
    let source = session.release().await?;
    let runtime_b =
        SessionRuntimeBinding::new(builder(configured_b.clone(), Arc::clone(&workspace))?);
    let RebindSessionOutcome::Rebound {
        target: rebound, ..
    } = Box::pin(sessions.rebind(RebindSessionRequest::new(
        SessionOperationId::new("scope-rebind-B")?,
        source,
        runtime_b.target_binding(),
        runtime_b,
    )))
    .await?
    else {
        return Err("rebind did not publish".into());
    };
    let ForkSessionOutcome::Forked { target: fork, .. } = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("scope-fork-B")?,
            rebound,
        ))
        .await?
    else {
        return Err("fork did not publish".into());
    };
    for locator in [source, rebound, fork] {
        let loaded = store.load_branch(locator.session_ref()).await?;
        let blocks = loaded
            .checkpoint()
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .collect::<Vec<_>>();
        let retained = blocks
            .iter()
            .find_map(|block| match block {
                ContentBlock::Reasoning(r) => r.replay(),
                _ => None,
            })
            .ok_or("reasoning lost")?;
        assert_eq!(retained, &replay);
        assert!(retained.is_compatible(
            replay.protocol(),
            replay.model_family(),
            &model,
            Some(account_a)
        ));
        assert!(!retained.is_compatible(
            replay.protocol(),
            replay.model_family(),
            &model,
            Some(account_b)
        ));
    }
    // Even a locally validated import declaring B in its outer metadata cannot
    // retag an A-bound item. Fresh omission does not grant later replay authority.
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("scope-import-declared-B")?,
                SessionRuntimeBinding::new(builder(configured_b, workspace)?),
            )
            .with_history(
                PortableSessionHistory::new(
                    vec![segment],
                    plakous_model::hex_lower(account_b.bytes()),
                    model.clone(),
                )
                .with_replay_validation(),
            ),
        )
        .await?;
    let checkpoint = agent_checkpoint(session.agent());
    let retained = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::Reasoning(r) => r.replay(),
            _ => None,
        })
        .ok_or("retained imported item missing")?;
    assert_eq!(retained, &replay);
    assert!(!retained.is_compatible(
        replay.protocol(),
        replay.model_family(),
        &model,
        Some(account_b)
    ));
    session.release().await?;
    Ok(())
}
