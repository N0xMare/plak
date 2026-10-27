use super::*;

async fn source() -> TestResult<(
    tempfile::TempDir,
    Arc<WorkspacePolicy>,
    Arc<TestCreateStore>,
    DurableSessionRuntime,
    Agent,
)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, _) = placeholder_fixture_with_descriptor(
        Arc::clone(&workspace),
        193_000,
        durable_test_descriptor("test.route")?,
        &[8_000],
        &[1],
        LocalDurabilityDisposition::DurableRequired,
        0,
        &[],
    )?;
    let agent = sessions
        .create(
            "create:endpoint-receipt",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    for prompt in ["older intent", "read result"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    Ok((directory, workspace, store, sessions, agent))
}

fn runtime(
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<(AgentBuilder, Arc<RecoveryCompactionModel>)> {
    let (mut builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::ProviderError(ModelError::new(
            ModelErrorKind::RateLimited,
            "definite 429",
        )),
        None,
    )?;
    let registry = oversized_tool_registry(&[])?;
    let configured = configured_test_model(
        model.clone(),
        model.scripted.descriptor().clone(),
        builder.initial_model_authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    builder.provider_configuration = Arc::new(configured.configuration().clone());
    builder.tool_bindings = ToolExecutorBindings::capture(&registry);
    builder.tools = registry;
    builder = builder.tool_policy(Arc::new(AlwaysAllowPolicy));
    Ok((builder, model))
}

#[tokio::test]
async fn joined_receipt_refuses_paid_wrong_operation_and_wrong_failure_source() -> TestResult {
    let (_directory, workspace, store, sessions, agent) = source().await?;
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let operation = "compact:join-refusals";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    let (builder, model) = runtime(workspace)?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("endpoint refused: {error:?}"))?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::Compacted { .. }
    ));
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    let failed = DurableSessionAddress::new(
        source.session_id,
        source.branch_id,
        identity.failure_checkpoint,
    );
    let failure = store.load_checkpoint(failed.checkpoint_id).await?;
    let original = decode_paid_compaction_operations(failure.checkpoint(), identity)?.request;
    let child_operation = format!("{operation}:placeholders");
    let child = DurableCompactionIdentity::new(&child_operation, failed)?;
    let successor = agent.checkpoint();
    let operations = successor.manifest().ledger().operations();
    let child_record = operations.iter().find(|record| matches!(record.kind(),DurableOperationKind::Compaction { workflow_id } if *workflow_id == child.workflow_id)).ok_or("child record")?;
    let validate = |component: &ReconstructiveComponent, child_source| {
        super::super::compaction_fallback::validate_join(
            &original,
            failure.checkpoint(),
            component,
            child,
            &child_operation,
            child_source,
        )
    };
    validate(child_record.request(), failed)?;
    for (field, value) in [
        (
            "operation_id",
            serde_json::json!("user-created:placeholders"),
        ),
        (
            "source_checkpoint",
            serde_json::to_value(source.checkpoint_id)?,
        ),
    ] {
        let mut request: serde_json::Value =
            serde_json::from_slice(child_record.request().bytes())?;
        request[field] = value;
        let component = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&request)?,
        )?;
        assert_eq!(
            validate(&component, failed)
                .expect_err("unrelated offline request")
                .store_error(),
            Some(SessionStoreError::corrupt())
        );
    }
    let paid = runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&original)?,
    )?;
    assert_eq!(
        validate(&paid, failed)
            .expect_err("paid suffix request")
            .store_error(),
        Some(SessionStoreError::corrupt())
    );
    assert_eq!(
        validate(child_record.request(), source)
            .expect_err("wrong failure address")
            .store_error(),
        Some(SessionStoreError::corrupt())
    );
    assert!(
        sessions
            .joined_compaction_source(
                operation,
                source,
                source,
                DurableSessionAddress::new(
                    source.session_id,
                    source.branch_id,
                    child.success_checkpoint
                )
            )
            .await
            .is_err()
    );
    agent.shutdown().await?;
    Ok(())
}

mod receipt_cuts {
    use super::*;

    #[tokio::test]
    async fn endpoint_fallback_receipt_cuts_recover_the_same_unpaid_target() -> TestResult {
        for after_placeholder in [false, true] {
            let (_directory, workspace, store, sessions, agent) = source().await?;
            let source = DurableSessionAddress::new(
                agent.session_id(),
                agent.branch_id(),
                agent.checkpoint().id(),
            );
            let operation = "compact:endpoint-cut";
            let identity = DurableCompactionIdentity::new(operation, source)?;
            let failed = DurableSessionAddress::new(
                source.session_id,
                source.branch_id,
                identity.failure_checkpoint,
            );
            let child =
                DurableCompactionIdentity::new(&format!("{operation}:placeholders"), failed)?;
            let target = DurableSessionAddress::new(
                source.session_id,
                source.branch_id,
                child.success_checkpoint,
            );
            *lock_mutex(&store.pause_after_checkpoint) = Some(if after_placeholder {
                child.success_checkpoint
            } else {
                identity.failure_checkpoint
            });
            let (builder, model) = runtime(Arc::clone(&workspace))?;
            let endpoint = agent.session_endpoint();
            let task = tokio::spawn(async move {
                endpoint
                    .compact_session(AgentCompactSessionRequest::new(
                        operation.to_owned(),
                        source.session_id,
                        source.branch_id,
                        source.checkpoint_id,
                        AgentCompactionReason::Manual,
                        builder,
                    ))
                    .await
            });
            store.delayed_recovery_started.notified().await;
            agent.abort_driver_for_test();
            assert_eq!(task.await?, Err(AgentSessionEndpointError::OutcomeUnknown));
            drop(agent);
            *lock_mutex(&store.pause_after_checkpoint) = None;
            store.expire_active_lease();
            let restarted = DurableSessionRuntime::current(
                store.clone(),
                RuntimeDurability::Required,
                NonZeroU64::new(30).ok_or("lease")?,
            )?;
            let (builder, recovery_model) = runtime(Arc::clone(&workspace))?;
            let result = Box::pin(restarted.recover_compaction(operation, source, builder))
                .await
                .map_err(|error| error.message().to_owned())?;
            assert_eq!(
                result,
                DurableCompactionRecoveryOutcome::Compacted { source, target }
            );
            assert_eq!(
                store
                    .load_branch(source.session())
                    .await?
                    .head()
                    .checkpoint()
                    .checkpoint_id(),
                target.checkpoint_id
            );
            assert_eq!(model.starts.load(Ordering::Acquire), 1);
            assert_eq!(
                recovery_model.starts.load(Ordering::Acquire),
                0,
                "recovery must be unpaid"
            );
            let commits = store.commit_calls.load(Ordering::Acquire);
            let (builder, repeated_model) = runtime(Arc::clone(&workspace))?;
            assert_eq!(
                Box::pin(restarted.recover_compaction(operation, source, builder)).await?,
                result
            );
            assert_eq!(repeated_model.starts.load(Ordering::Acquire), 0);
            assert_eq!(
                store.commit_calls.load(Ordering::Acquire),
                commits,
                "no second placeholder publication"
            );
            assert_eq!(
                sessions.compaction_receipt(operation, source).await?,
                DurableCompactionReceipt::Compacted { source, target }
            );
            let committed = store.load_checkpoint(target.checkpoint_id).await?;
            assert_eq!(
                placeholder_harness_notes(committed.checkpoint().manifest().canonical().history())
                    .len(),
                1
            );
            assert!(crate::runtime::driver::paid_fold_failed_since_last_round(
                committed.checkpoint()
            ));
        }
        Ok(())
    }
}
