use super::*;

pub(super) fn durable_decision_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
) -> TestResult<AgentBuilder> {
    durable_decision_builder_at(workspace, model, registry, Timestamp::new(10, 0)?)
}

fn durable_decision_builder_at(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
    now: Timestamp,
) -> TestResult<AgentBuilder> {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    Ok(AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(RestoreDecisionPolicy::new()?))
        .identity_sources(Arc::new(FixedClock::new(now)), Arc::new(SystemIdGenerator)))
}

pub(super) async fn reopen_pending_decision(
    allow: bool,
    hosted: bool,
    supersede: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut fixture = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        usize::from(hosted),
        match (allow, hosted) {
            (true, false) => 80_000,
            (false, false) => 81_000,
            (true, true) => 82_000,
            (false, true) => 83_000,
        },
        false,
    )
    .with_tool_first();
    if hosted {
        fixture = fixture.with_programmatic_local_tool()?;
    }
    let model = Arc::new(fixture);
    let registry = one_mutating_tool_registry()?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            if hosted {
                "create:restore-hosted-allow"
            } else if allow {
                "create:restore-allow"
            } else {
                "create:restore-deny"
            },
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?,
        )
        .await
        .map_err(|error| format!("create failed: {}", error.message()))?;
    let command_id = CommandId::new(if allow {
        "root:restore-allow"
    } else {
        "root:restore-deny"
    })?;
    let turn = original
        .start(command_id, "restore pending effect")
        .await?
        .into_turn()
        .ok_or("root unexpectedly duplicated")?;
    let turn_id = turn.id();
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    crate::recovery_scope::require_recoverable(&original.checkpoint())
        .map_err(|error| format!("decision checkpoint recovery scope failed: {error:?}"))?;
    store.expire_active_lease();
    let opened = sessions
        .open(
            if hosted {
                "open:restore-hosted-allow"
            } else if allow {
                "open:restore-allow"
            } else {
                "open:restore-deny"
            },
            address,
            durable_decision_builder(workspace, Arc::clone(&model), registry)?,
        )
        .await
        .map_err(|error| format!("open failed: {}", error.message()))?;
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = opened else {
        return Err("pending decision did not restore as working".into());
    };
    assert_eq!(turn.turn_id(), turn_id);
    assert_eq!(
        turn.pending_decision()
            .ok_or("restored effect decision was absent")?
            .turn_id(),
        turn_id
    );
    let decision = turn
        .pending_decision()
        .ok_or("restored effect decision was absent")?
        .clone();
    if supersede {
        turn.control().steer("supersede approved effect").await?;
    }
    if allow {
        assert_eq!(
            turn.control()
                .allow_effect(decision, EffectDecisionAuditId::new("test:restore-allow")?,)
                .await?,
            EffectDecisionStatus::Allowed
        );
    } else {
        assert_eq!(
            turn.control()
                .deny_tool(
                    decision,
                    ToolPolicyDenial::new("test denied restored effect"),
                )
                .await?,
            EffectDecisionStatus::Denied
        );
    }
    let result = turn.result().result().await?;
    assert_eq!(result.turn_id(), turn_id);
    if supersede {
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text() == "not run: superseded by your message")))));
        assert!(
            result
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .filter(|op| matches!(op.kind(), DurableOperationKind::Effect { .. }))
                .all(|op| !op
                    .transitions()
                    .iter()
                    .any(|t| matches!(t.state(), DurableOperationState::Dispatched)))
        );
    }
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn durable_pending_decision_reopens_allow_without_provider_replay() -> TestResult {
    Box::pin(reopen_pending_decision(true, false, false)).await
}

#[tokio::test]
async fn durable_pending_decision_reopens_deny_without_provider_replay() -> TestResult {
    Box::pin(reopen_pending_decision(false, false, false)).await
}

#[tokio::test]
async fn durable_hosted_pending_decision_reopens_with_baton_and_quiesces() -> TestResult {
    Box::pin(reopen_pending_decision(true, true, false)).await
}

fn v2_direct_patch_decision_builder(
    workspace: Arc<WorkspacePolicy>,
    model: &Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
) -> TestResult<AgentBuilder> {
    v2_direct_builder(workspace, model, registry, CodingPresentation::Codex)
}

fn v2_direct_builder(
    workspace: Arc<WorkspacePolicy>,
    model: &Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
    presentation: CodingPresentation,
) -> TestResult<AgentBuilder> {
    let descriptor = model.descriptor.clone();
    let authority = model.authority.clone();
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        None,
        ToolPresentationMode::Direct,
        false,
    )?;
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("test.direct-patch-decision")?)
        .semantic_version(ProfileSemanticVersion::new(1, 0, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions("")
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::Disabled)
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let plan = compile_explicit_runtime_profile(
        &profile,
        model.as_ref(),
        configured.configuration(),
        &registry,
        workspace.as_ref(),
        Vec::new(),
    )?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let projection = presentation.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    let admission: Arc<dyn ProjectionCandidateAdmission> = FakeProjectionAdmission::new(true);
    Ok(AgentBuilder::new(configured, registry, workspace)
        .tool_projection_v2(projection)
        .projection_admission(admission)
        .harness_plan(plan)
        .tool_policy(Arc::new(RestoreDecisionPolicy::new()?))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        ))
}

#[tokio::test]
async fn pending_v2_custom_patch_reopens_and_executes_without_provider_replay() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = lineage_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let patch = "*** Begin Patch\n*** Add File: restored.txt\n+restored λ\n*** End Patch";
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 0, 0xc0de_7200, false)
            .with_scripted_tool_calls([(0, "apply_patch", serde_json::to_string(patch)?)]),
    );
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:pending-v2-custom-patch",
            v2_direct_patch_decision_builder(Arc::clone(&workspace), &model, registry.clone())?,
        )
        .await?;
    let turn = original
        .start(
            CommandId::new("pending-v2-custom-patch")?,
            "await patch approval",
        )
        .await?
        .into_turn()
        .ok_or("patch turn not admitted")?;
    let turn_id = turn.id();
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    assert!(!directory.path().join("restored.txt").exists());
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    crate::recovery_scope::require_recoverable(&original.checkpoint())?;
    store.expire_active_lease();
    let opened = sessions
        .open(
            "open:pending-v2-custom-patch",
            address,
            v2_direct_patch_decision_builder(workspace, &model, registry)?,
        )
        .await
        .map_err(|error| format!("custom patch reopen failed: {}", error.message()))?;
    let DurableRuntimeOpen::RestoredWorking { agent, turn } = opened else {
        return Err("pending custom patch did not restore as working".into());
    };
    assert_eq!(turn.turn_id(), turn_id);
    let decision = turn
        .pending_decision()
        .ok_or("restored custom patch decision absent")?
        .clone();
    assert_eq!(
        turn.control()
            .allow_effect(
                decision,
                EffectDecisionAuditId::new("test:restore-custom-patch")?
            )
            .await?,
        EffectDecisionStatus::Allowed
    );
    let result = turn.result().result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        std::fs::read(directory.path().join("restored.txt"))?,
        "restored λ\n".as_bytes()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    let _ = original.shutdown().await;
    Ok(())
}

/// Raised tool-result texts committed to `checkpoint`'s canonical history.
fn committed_tool_result_texts(checkpoint: &RuntimeCheckpoint) -> Vec<String> {
    checkpoint
        .manifest()
        .canonical()
        .projection()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.parts().iter())
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .collect()
}

/// A maximum-size read page, of plain long lines and of the most heavily
/// escaped text, raises on every Direct read presentation without an
/// observation cut, commits to canonical history and reopens unchanged.
#[tokio::test]
async fn maximum_read_pages_raise_commit_and_reopen_on_every_direct_read_presentation() -> TestResult
{
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bulk = (0..2_784_usize)
        .map(|line| {
            (0..1_469_usize)
                .map(|column| char::from(ALPHABET[(line * 7 + column) % ALPHABET.len()]))
                .chain(std::iter::once('\n'))
                .collect::<String>()
        })
        .collect::<String>();
    let escaped_line = "\"\\".repeat(150) + "\u{1}\t\r\n";
    let escaped = (0..2_000_usize)
        .flat_map(|line| [line.to_string(), ":".to_owned(), escaped_line.clone()])
        .collect::<String>();
    for (index, (presentation, tool, path_key, continuation)) in [
        (
            CodingPresentation::Claude,
            "Read",
            "file_path",
            "next_offset=",
        ),
        (
            CodingPresentation::Grok,
            "read_file",
            "target_file",
            "next_offset",
        ),
        (CodingPresentation::Pi, "read", "path", "Use offset="),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("bulk.txt"), &bulk)?;
        std::fs::write(directory.path().join("escaped.txt"), &escaped)?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let registry = lineage_registry()?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let read = |file: &str| {
            serde_json::json!({ path_key: file, "offset": 1, "limit": 2_000 }).to_string()
        };
        let model = Arc::new(
            ArtifactModel::new(
                descriptor,
                authority,
                0,
                0xc0de_7300 + u128::try_from(index)?,
                false,
            )
            .with_scripted_tool_calls([
                (0, tool, read("bulk.txt")),
                (1, tool, read("escaped.txt")),
            ]),
        );
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let agent = sessions
            .create(
                &format!("create:maximum-read-{index}"),
                v2_direct_builder(
                    Arc::clone(&workspace),
                    &model,
                    registry.clone(),
                    presentation,
                )?
                .tool_policy(Arc::new(AlwaysAllowPolicy)),
            )
            .await?;
        let result = agent.prompt("read both files").await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{tool}: {:?}",
            result.outcome()
        );
        let checkpoint = agent.checkpoint();
        let texts = committed_tool_result_texts(&checkpoint);
        assert_eq!(texts.len(), 2, "{tool}: one committed result per read");
        for text in &texts {
            assert!(
                text.len() <= crate::runtime::observation::budget::CANONICAL_RESULT_BYTES,
                "{tool}: a whole page fits the per-result bound ({} bytes)",
                text.len()
            );
            let raised: serde_json::Value = serde_json::from_str(text)?;
            let inner = raised["result"]
                .as_str()
                .ok_or("raised result is not a string")?;
            assert_eq!(raised.as_object().map(serde_json::Map::len), Some(1));
            assert!(!inner.contains("bytes omitted"), "{tool}: page was cut");
            assert!(
                !inner.contains("observation_truncated"),
                "{tool}: page was cut"
            );
            assert!(
                inner.contains(continuation),
                "{tool}: page names its continuation"
            );
        }
        assert!(
            texts[0].len() > 200_000,
            "{tool}: the bulk page is near the bound ({} bytes)",
            texts[0].len()
        );
        let address =
            DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
        let history_identity = checkpoint.manifest().canonical().history().identity();
        agent.shutdown().await?;
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open(
                &format!("open:maximum-read-{index}"),
                address,
                v2_direct_builder(workspace, &model, registry, presentation)?
                    .tool_policy(Arc::new(AlwaysAllowPolicy)),
            )
            .await?
        else {
            return Err(format!("{tool}: maximum-read session did not reopen").into());
        };
        let reopened_checkpoint = reopened.checkpoint();
        assert_eq!(
            reopened_checkpoint
                .manifest()
                .canonical()
                .history()
                .identity(),
            history_identity
        );
        assert_eq!(committed_tool_result_texts(&reopened_checkpoint), texts);
        reopened.shutdown().await?;
    }
    Ok(())
}

#[test]
fn pending_custom_patch_refuses_stale_encoded_argument_binding_before_effect() -> TestResult {
    let directory = tempfile::tempdir()?;
    let run_id = RunId::try_from(uuid::Uuid::from_u128(0xc0de_7201))?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        directory.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = lineage_registry()?;
    let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"pending-custom-patch"],
    )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "test.responses",
        "test.route",
        "test.profile.r1",
        "responses-family",
        "exact-model",
        "exact-model",
        configuration,
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let projection = CodingPresentation::Codex.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    let policy = Arc::new(RestoreDecisionPolicy::new()?);
    let runtime = ToolRuntime::new(registry, Arc::clone(&environment), policy);
    let patch = "*** Begin Patch\n*** Add File: stale.txt\n+must not appear\n*** End Patch";
    let encoded = serde_json::to_string(patch)?;
    let request = |text: &str| -> TestResult<ToolInvocationRequest> {
        Ok(ToolInvocationRequest::new(
            run_id,
            TurnId::try_from(uuid::Uuid::from_u128(0xc0de_7202))?,
            AttemptId::try_from(uuid::Uuid::from_u128(0xc0de_7203))?,
            CallId::try_from(uuid::Uuid::from_u128(0xc0de_7204))?,
            ToolName::new("apply_patch")?,
            ToolCorrelation::default(),
            Arc::<str>::from(text),
        ))
    };
    // This is the old pending invocation: its raw input was the canonical
    // JSON string, including quotes, rather than the decoded patch text.
    let old = projection.prepare_in_environment(request(&encoded)?, &environment)?;
    let persisted = plakous_tools::internal::authorization_binding(
        old.invocation(),
        environment.descriptor().clone(),
        runtime.policy_revision().clone(),
    )?;
    let current = projection.prepare_in_environment(request(patch)?, &environment)?;
    assert_ne!(
        old.invocation().arguments().digest(),
        current.invocation().arguments().digest()
    );
    let refused = runtime.restore_pending_effect_decision(
        current.into_parts().0,
        persisted.digest(),
        EffectDecisionPrompt::new("restore stale patch"),
    );
    let error = refused
        .err()
        .ok_or("stale encoded pending patch was restored")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Policy);
    assert!(error.to_string().contains("is not exact"), "{error}");
    assert!(!directory.path().join("stale.txt").exists());
    Ok(())
}

#[tokio::test]
async fn resolved_hosted_decision_without_effect_fails_closed_on_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            83_500,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?,
    );
    let registry = one_mutating_tool_registry()?;
    let store = Arc::new(TestCreateStore::default());
    store.block_after_resolved_hosted_decision();
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:resolved-hosted-decision-gap",
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?,
        )
        .await?;
    let turn = original
        .start(
            CommandId::new("root:resolved-hosted-decision-gap")?,
            "resolve a hosted effect decision",
        )
        .await?
        .into_turn()
        .ok_or("root unexpectedly duplicated")?;
    let decision = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break decode_await_effect_decision_continuation(&checkpoint)?.decision;
        }
        tokio::task::yield_now().await;
    };
    let control = turn.control().clone();
    let audit = EffectDecisionAuditId::new("test:resolved-hosted-gap")?;
    let resolution = tokio::spawn(async move {
        let _ = control.allow_effect(decision, audit).await;
    });
    let address = loop {
        let checkpoint = original.checkpoint();
        if store.commits_blocked.load(Ordering::Acquire) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    original.abort_driver_for_test();
    resolution.abort();
    let _ = resolution.await;
    store.resume_commits();
    store.expire_active_lease();
    assert!(matches!(
        sessions
            .open(
                "open:resolved-hosted-decision-gap",
                address,
                durable_decision_builder(workspace, Arc::clone(&model), registry)?,
            )
            .await?,
        DurableRuntimeOpen::RecoveryRequired { address: recovered, .. } if recovered == address
    ));
    Ok(())
}

#[tokio::test]
async fn expired_durable_effect_decision_returns_recovery_required_without_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            84_000,
            false,
        )
        .with_tool_first(),
    );
    let registry = one_mutating_tool_registry()?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:expired-effect-decision",
            durable_decision_builder(Arc::clone(&workspace), Arc::clone(&model), registry.clone())?,
        )
        .await?;
    let _turn = original
        .start(
            CommandId::new("root:expired-effect-decision")?,
            "persist an expiring effect decision",
        )
        .await?
        .into_turn()
        .ok_or("root unexpectedly duplicated")?;
    let address = loop {
        let checkpoint = original.checkpoint();
        if matches!(
            checkpoint.manifest().lifecycle(),
            RuntimeLifecycle::Working {
                recovery_phase: WorkingRecoveryPhase::EffectDecision,
                ..
            }
        ) {
            break DurableSessionAddress::new(
                original.session_id(),
                original.branch_id(),
                checkpoint.id(),
            );
        }
        tokio::task::yield_now().await;
    };
    store.expire_active_lease();
    assert!(matches!(
        sessions
            .open(
                "open:expired-effect-decision",
                address,
                durable_decision_builder_at(
                    workspace,
                    Arc::clone(&model),
                    registry,
                    Timestamp::new(100_000, 0)?,
                )?,
            )
            .await?,
        DurableRuntimeOpen::RecoveryRequired { address: recovered, .. } if recovered == address
    ));
    let _ = original.shutdown().await;
    Ok(())
}
