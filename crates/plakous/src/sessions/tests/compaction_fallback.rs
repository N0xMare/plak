use super::*;
use plakous_model::{CallId, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner};

struct EndpointModel {
    scripted: Arc<plakous_test_support::ScriptedModel>,
    calls: Arc<AtomicUsize>,
    compact: bool,
}

impl LanguageModel for EndpointModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted.clone().revalidate_runtime_authority(at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        Ok(PreparedModelCall::new(inert.record().clone(), move |_| {
            Box::pin(async move {
                let ordinal = self.calls.fetch_add(1, Ordering::AcqRel);
                let failure = || {
                    ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "endpoint fixture output failed",
                    )
                };
                let id = Uuid::from_bytes(
                    plan.digest().bytes()[..16]
                        .try_into()
                        .map_err(|_| failure())?,
                );
                let block = if !self.compact && ordinal == 1 {
                    ContentBlock::ToolCall(
                        ToolCallBlock::new(
                            CallId::try_from(id).map_err(|_| failure())?,
                            ToolName::new("probe").map_err(|_| failure())?,
                            ToolExecutionOwner::Local,
                            ToolCallState::Requested,
                            ToolCorrelation::default(),
                            "{}",
                        )
                        .map_err(|_| failure())?,
                    )
                } else {
                    ContentBlock::Text(
                        TextBlock::new(
                            if !self.compact && ordinal == 0 {
                                "x".repeat(60_000)
                            } else {
                                "summary".to_owned()
                            },
                            None,
                        )
                        .map_err(|_| failure())?,
                    )
                };
                let finish = if self.compact {
                    FinishReason::Length
                } else if ordinal == 1 {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                let output = ConversationItem::new(
                    EntryId::try_from(id).map_err(|_| failure())?,
                    Actor::Assistant,
                    vec![block],
                    Provenance::ModelOutput,
                )
                .map_err(|_| failure())?;
                let response = ModelResponse::new(
                    vec![output.clone()],
                    FinishDetails::new(finish, None),
                    None,
                    None,
                    plan.digest(),
                );
                Ok(Box::pin(FixtureStream {
                    events: vec![
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
                    ]
                    .into(),
                }) as ModelStream)
            })
        }))
    }
}

fn binding(
    workspace: Arc<WorkspacePolicy>,
    calls: Arc<AtomicUsize>,
    compact: bool,
) -> TestResult<SessionRuntimeBinding> {
    let descriptor = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"endpoint-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"endpoint-credential"])?,
        1,
        timestamp()?,
        None,
    )?;
    let scripted = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_content_sensitive_observation(),
    );
    let definition = one_tool_registry()?.definitions()[0].clone();
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        Arc::new(plakous_test_support::ScriptedTool::new([
            ToolOutcome::success_text("r".repeat(20_000)),
        ])),
    ))?;
    let registry = registry.freeze();
    let model: Arc<dyn LanguageModel> = Arc::new(EndpointModel {
        scripted,
        calls,
        compact,
    });
    let configured = crate::test_model::configured_model(
        model,
        descriptor,
        authority,
        registry.definitions().iter().cloned(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("output")?),
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
            )
            .with_scheduled_compaction_enabled(false),
    ))
}

#[tokio::test]
async fn facade_idle_and_scheduled_join_free_receipt_and_replay_without_dispatch() -> TestResult {
    for reason in [CompactReason::Manual, CompactReason::Scheduled] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(MemorySessionStore::new(MemoryStoreLimits::default())?);
        let sessions = AgentSessions::new(SessionBackend::from_store(
            store.clone(),
            RuntimeDurability::Disabled,
        ))?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("endpoint:create")?,
                binding(Arc::clone(&workspace), Arc::new(AtomicUsize::new(0)), false)?,
            ))
            .await?;
        for prompt in ["older intent", "latest intent"] {
            let result = session.agent().prompt(prompt).await?.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}",
                result.outcome()
            );
        }
        let source = session.locator();
        let calls = Arc::new(AtomicUsize::new(0));
        let operation = SessionOperationId::new("endpoint:compact")?;
        let make_request = || -> TestResult<_> {
            let request = CompactSessionRequest::new(
                operation.clone(),
                source,
                reason,
                binding(Arc::clone(&workspace), Arc::clone(&calls), true)?,
            );
            Ok(if reason == CompactReason::Scheduled {
                request.with_pending_user_input(UserInput::from("p".repeat(100_000)))
            } else {
                request
            })
        };
        let first = sessions.compact(make_request()?).await?;
        let CompactSessionOutcome::Compacted {
            source: receipt_source,
            target,
            ..
        } = first
        else {
            return Err(format!("endpoint did not commit free successor: {first:?}").into());
        };
        assert_eq!(receipt_source, source);
        assert_eq!(session.locator(), target);
        assert!(session.paid_fold_failed_since_last_round());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let stats = store.stats()?;
        let repeated = sessions.compact(make_request()?).await?;
        assert!(
            matches!(repeated,CompactSessionOutcome::Compacted { source: s, target: t, .. } if s == source && t == target)
        );
        let recovered = sessions
            .recover(RecoverSessionRequest::new(
                operation,
                source,
                binding(workspace, Arc::clone(&calls), true)?,
            ))
            .await?;
        assert!(
            matches!(recovered,RecoverSessionOutcome::Compacted { source: s, target: t, .. } if s == source && t == target)
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            store.stats()?,
            stats,
            "receipt replay must not publish another placeholder"
        );
        session.release().await?;
    }
    Ok(())
}

#[tokio::test]
async fn external_placeholder_suffix_is_refused_before_any_ledger_write() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(MemorySessionStore::new(MemoryStoreLimits::default())?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("reserved:create")?,
            binding(Arc::clone(&workspace), Arc::new(AtomicUsize::new(0)), false)?,
        ))
        .await?;
    let source = session.locator();
    let stats = store.stats()?;
    let calls = Arc::new(AtomicUsize::new(0));
    for reason in [CompactReason::Manual, CompactReason::Scheduled] {
        let error = sessions
            .compact(CompactSessionRequest::new(
                SessionOperationId::new("x:placeholders")?,
                source,
                reason,
                binding(Arc::clone(&workspace), Arc::clone(&calls), true)?,
            ))
            .await
            .expect_err("reserved suffix must be refused");
        assert_eq!(error.kind(), AgentSessionsErrorKind::InvalidInput);
        assert!(error.to_string().contains(":placeholders"));
    }
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(store.stats()?, stats);
    assert_eq!(session.locator(), source);
    session.release().await?;
    Ok(())
}
