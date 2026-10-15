use super::*;

pub(super) fn fake_process_projection_registry() -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    for registration in Arc::new(ProcessSessionTools::posix_sh()?).registrations()? {
        registry.register(registration)?;
    }
    Ok(registry.freeze())
}

#[test]
fn v2_projection_carries_two_names_lowers_raises_and_reopens_exactly() -> TestResult {
    let registry = fake_process_projection_registry()?;
    let bindings = ToolExecutorBindings::capture(&registry);
    let effective_configuration: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"agent-v2-projection-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "test.responses",
        "test.route",
        "test.profile.r1",
        "responses-family",
        "exact-model",
        "exact-model",
        effective_configuration,
        2,
    )?;
    let projection = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    let presentation = ToolPresentation::V2(Arc::new(projection.clone()));
    let compiled = projection.compile_request_view_v2()?;
    assert_eq!(compiled.definitions().len(), 2);
    assert_eq!(
        compiled.definitions()[0].exposed_name().as_str(),
        "exec_command"
    );
    assert_eq!(
        compiled.definitions()[1].exposed_name().as_str(),
        "write_stdin"
    );

    let run_id = RunId::try_from(uuid::Uuid::from_u128(101))?;
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let projected_arguments = serde_json::to_string(&json!({
        "cmd": "printf ok",
        "workdir": environment.cwd(),
    }))?;
    let prepared = presentation.prepare(
        ToolInvocationRequest::new(
            run_id,
            TurnId::try_from(uuid::Uuid::from_u128(102))?,
            AttemptId::try_from(uuid::Uuid::from_u128(103))?,
            CallId::try_from(uuid::Uuid::from_u128(104))?,
            ToolName::new("exec_command")?,
            ToolCorrelation::default(),
            Arc::<str>::from(projected_arguments),
        ),
        &environment,
    )?;
    assert_eq!(
        prepared.invocation.identity().name().as_str(),
        PROCESS_SESSION_CAPABILITY_NAME
    );
    assert_eq!(prepared.invocation.arguments().value()["workdir"], ".");
    assert_eq!(
        prepared
            .invocation
            .exposure()
            .expect("projected exposure")
            .exposed_name()
            .as_str(),
        "exec_command"
    );
    let raised = presentation.raise(
        prepared.projection_receipt.as_ref(),
        ToolOutcome::success_text(
            r#"{"status":"exited","exit_code":0,"session_id":null,"cursor":2,"output":"ok"}"#,
        )?,
    )?;
    assert_eq!(raised.kind(), ToolOutcomeKind::Success);

    let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let registry_bytes = registry_descriptor.encode_v2()?;
    let registry_descriptor = FrozenToolRegistryDescriptor::decode_v2(&registry_bytes)?;
    let descriptor = FrozenToolProjectionDescriptorV2::capture(&projection)?;
    let projection_bytes = descriptor.encode_v2()?;
    let descriptor =
        FrozenToolProjectionDescriptorV2::decode_v2(&projection_bytes, &registry_descriptor)?;
    let reopened_registry = registry_descriptor.resolve(&bindings)?;
    let reopened = descriptor.resolve(reopened_registry)?;
    assert_eq!(reopened.digest(), projection.digest());
    assert_eq!(reopened.compile_request_view_v2()?, compiled);

    let mut tampered = projection_bytes;
    let last = tampered.last_mut().expect("descriptor bytes");
    *last ^= 1;
    assert!(FrozenToolProjectionDescriptorV2::decode_v2(&tampered, &registry_descriptor,).is_err());
    Ok(())
}

fn canonically_named_registry_with_inverse_durable_ids() -> TestResult<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    for (name, tool_id) in [("alpha", "z.registration"), ("zeta", "a.registration")] {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            format!("Run {name}."),
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
        registry.register(ToolRegistration::new(
            RegisteredToolId::new(tool_id)?,
            ToolExecutorId::new(format!("{name}.executor"))?,
            plakous_tools::ToolExecutorBindingIdentity::new(
                1,
                &[format!("{name}.executor").as_bytes()],
            )?,
            definition,
            Arc::new(plakous_test_support::ScriptedTool::new([
                ToolOutcome::success_text(name),
            ])),
        ))?;
    }
    Ok(registry.freeze())
}

fn revalidation_witness_for(
    builder: &AgentBuilder,
    run_id: RunId,
    admission: SealedRuntimeAdmission,
) -> Result<RuntimeRevalidationWitness, AgentError> {
    let environment = run_environment(
        builder.workspace.as_ref(),
        run_id,
        builder.host_process_options.clone(),
        builder.job_config,
    )?;
    let tools = ToolRuntime::new(
        builder.tools.clone(),
        Arc::new(environment),
        Arc::clone(&builder.policy),
    );
    let (_, _, _, exposure) = resolve_construction_harness_plan(builder, &tools)?;
    let presentation = ToolPresentation::V1(exposure);
    runtime_revalidation_witness(
        builder.model.as_ref(),
        &builder.provider_configuration,
        &tools,
        &presentation,
        builder.workspace.as_ref(),
        builder.local_composition.as_ref(),
        admission.fact,
        builder.clock.now(),
    )
}

#[tokio::test]
async fn durable_genesis_is_exact_reconstructive_and_revalidates_all_live_facts() -> TestResult {
    let first_workspace = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(first_workspace.path(), ".")?);
    let empty = ToolRegistryBuilder::default().freeze();
    let builder = durable_test_builder(
        Arc::clone(&workspace),
        empty.clone(),
        "test.route",
        b"alpha",
        false,
    )?;
    let identities = DurableGenesisIdentities::new("create:test-1")?;
    let admission = sealed_runtime_admission(&builder).await?;
    let checkpoint = prepare_durable_genesis(
        &builder,
        identities,
        admission.clone(),
        RuntimeDurability::Required,
        CanonicalHistory::default(),
    )?;
    let repeated = prepare_durable_genesis(
        &builder,
        identities,
        admission,
        RuntimeDurability::Required,
        CanonicalHistory::default(),
    )?;
    assert_eq!(checkpoint.id(), identities.checkpoint_id);
    assert_eq!(repeated.id(), identities.checkpoint_id);
    assert_eq!(
        checkpoint.manifest().topology(),
        repeated.manifest().topology()
    );
    assert_ne!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .affinity()
            .identity(),
        repeated
            .manifest()
            .canonical()
            .context()
            .affinity()
            .identity(),
        "independent genesis construction reused runtime cache-affinity entropy"
    );
    assert_eq!(
        checkpoint.manifest().durability(),
        RuntimeDurability::Required
    );
    let restored = checkpoint.manifest().rehydrate_harness_plan()?;
    assert_eq!(restored, repeated.manifest().rehydrate_harness_plan()?);

    let current = sealed_runtime_admission(&builder).await?;
    let witness = revalidation_witness_for(&builder, identities.run_id, current)?;
    assert!(matches!(
        checkpoint.rehydrate(&witness)?,
        RuntimeRehydration::Quiescent { .. }
    ));

    let changed_workspace_dir = tempfile::tempdir()?;
    let changed_workspace = Arc::new(WorkspacePolicy::host(changed_workspace_dir.path(), ".")?);
    let variants = [
        durable_test_builder(
            Arc::clone(&workspace),
            empty.clone(),
            "test.route",
            b"beta",
            false,
        )?,
        durable_test_builder(
            Arc::clone(&workspace),
            empty.clone(),
            "test.other-route",
            b"alpha",
            false,
        )?,
        durable_test_builder(
            changed_workspace,
            empty.clone(),
            "test.route",
            b"alpha",
            false,
        )?,
        durable_test_builder(Arc::clone(&workspace), empty, "test.route", b"alpha", true)?,
        durable_test_builder(
            workspace,
            one_tool_registry()?,
            "test.route",
            b"alpha",
            false,
        )?,
    ];
    for changed in variants {
        let fresh = sealed_runtime_admission(&changed).await?;
        let witness = revalidation_witness_for(&changed, identities.run_id, fresh)?;
        assert!(checkpoint.rehydrate(&witness).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn durable_create_recovers_lost_ack_repeats_opens_and_releases_on_drift() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let tools = ToolRegistryBuilder::default().freeze();
    let store = Arc::new(TestCreateStore::with_lost_create_ack());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;

    let first = sessions
        .create(
            "create:lost-ack",
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let first_owner = durable_owner(&first)?;
    first.shutdown().await?;
    let current_address = DurableSessionAddress::new(
        first.session_id(),
        first.branch_id(),
        first.checkpoint().id(),
    );
    assert_eq!(store.create_calls.load(Ordering::Relaxed), 1);
    assert!(store.recover_calls.load(Ordering::Relaxed) >= 2);

    let repeated = sessions
        .create(
            "create:lost-ack",
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    assert_eq!(durable_owner(&repeated)?, first_owner);
    repeated.shutdown().await?;
    assert_eq!(store.create_calls.load(Ordering::Relaxed), 1);

    let opened = sessions
        .open(
            "open:exact",
            current_address,
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("quiescent genesis unexpectedly required recovery".into());
    };
    opened.shutdown().await?;
    let latest_address = DurableSessionAddress::new(
        opened.session_id(),
        opened.branch_id(),
        opened.checkpoint().id(),
    );

    let releases_before = store.releases.load(Ordering::Relaxed);
    let drifted = sessions
        .open(
            "open:drifted-auth",
            latest_address,
            durable_test_builder(workspace, tools, "test.route", b"beta", false)?,
        )
        .await;
    assert!(drifted.is_err());
    assert_eq!(
        store.releases.load(Ordering::Relaxed),
        releases_before.saturating_add(1)
    );
    Ok(())
}

#[tokio::test]
async fn durable_reopen_resolves_exact_fresh_tool_bindings_and_ignores_extras() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let created = sessions
        .create(
            "create:tool-bindings",
            durable_test_builder(
                Arc::clone(&workspace),
                one_tool_registry()?,
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    created.shutdown().await?;
    let address = DurableSessionAddress::new(
        created.session_id(),
        created.branch_id(),
        created.checkpoint().id(),
    );

    let reopened = sessions
        .open(
            "open:fresh-tool-binding",
            address,
            durable_test_builder(
                Arc::clone(&workspace),
                one_tool_registry()?,
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let DurableRuntimeOpen::Opened(reopened) = reopened else {
        return Err("quiescent tool checkpoint unexpectedly required recovery".into());
    };
    reopened.shutdown().await?;

    let missing = ToolExecutorBindingsBuilder::new();
    let error = sessions
        .open(
            "open:missing-tool-binding",
            address,
            durable_test_builder(
                Arc::clone(&workspace),
                one_tool_registry()?,
                "test.route",
                b"alpha",
                false,
            )?
            .tool_executor_bindings(missing.freeze()),
        )
        .await
        .expect_err("missing executor binding must fail before open");
    assert_eq!(error.kind(), AgentErrorKind::ExternalBindingUnavailable);
    let named = error
        .message()
        .strip_prefix("a required tool executor binding is unavailable: ")
        .expect("reopen names the executor id it could not rebind");
    assert!(!named.is_empty(), "{}", error.message());

    let mut superset = ToolExecutorBindingsBuilder::new();
    superset.bind(
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        Arc::new(plakous_test_support::ScriptedTool::new([
            ToolOutcome::success_text("fresh"),
        ])),
    )?;
    superset.bind(
        ToolExecutorId::new("unreachable.extra")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"unreachable.extra"])?,
        Arc::new(plakous_test_support::ScriptedTool::new([
            ToolOutcome::success_text("must remain unreachable"),
        ])),
    )?;
    let reopened = sessions
        .open(
            "open:superset-tool-bindings",
            address,
            durable_test_builder(
                workspace,
                one_tool_registry()?,
                "test.route",
                b"alpha",
                false,
            )?
            .tool_executor_bindings(superset.freeze()),
        )
        .await?;
    let DurableRuntimeOpen::Opened(reopened) = reopened else {
        return Err("extra tool binding changed durable reachability".into());
    };
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_reopen_keys_plan_exposure_by_canonical_tool_not_registry_order() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let created = sessions
        .create(
            "create:inverse-tool-ids",
            durable_test_builder(
                Arc::clone(&workspace),
                canonically_named_registry_with_inverse_durable_ids()?,
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await
        .map_err(|error| format!("inverse-ID create failed: {error}"))?;
    created.shutdown().await?;
    let address = DurableSessionAddress::new(
        created.session_id(),
        created.branch_id(),
        created.checkpoint().id(),
    );

    let reopened = sessions
        .open(
            "open:inverse-tool-ids",
            address,
            durable_test_builder(
                workspace,
                canonically_named_registry_with_inverse_durable_ids()?,
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await
        .map_err(|error| format!("inverse-ID open failed: {error}"))?;
    let DurableRuntimeOpen::Opened(reopened) = reopened else {
        return Err("quiescent inverse-ID checkpoint unexpectedly required recovery".into());
    };
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_reopen_rejects_unknown_family_revision_against_current_compiler() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let tools = ToolRegistryBuilder::default().freeze();
    let historical = sessions
        .create(
            "create:unknown-family-revision",
            durable_test_builder_with_family_revision(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
                99,
            )?,
        )
        .await?;
    historical.shutdown().await?;
    let address = DurableSessionAddress::new(
        historical.session_id(),
        historical.branch_id(),
        historical.checkpoint().id(),
    );

    assert!(
        sessions
            .open(
                "open:current-family-revision",
                address,
                durable_test_builder(workspace, tools, "test.route", b"alpha", false,)?,
            )
            .await
            .is_err(),
        "an unknown persisted family revision reopened against current compiler output"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_instance_owner_prevents_cross_runtime_operation_aliasing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let tools = ToolRegistryBuilder::default().freeze();
    let store = Arc::new(TestCreateStore::default());
    let first_runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let second_runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let third_runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let operation_id = "create:runtime-instance-owner";
    let first_derived = operation_run_id(&first_runtime.instance_nonce, operation_id)?;
    assert_eq!(
        first_derived,
        operation_run_id(&first_runtime.instance_nonce, operation_id)?
    );
    assert_ne!(
        first_derived,
        operation_run_id(&second_runtime.instance_nonce, operation_id)?
    );

    let first = first_runtime
        .create(
            operation_id,
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let first_owner = durable_owner(&first)?;
    let concurrent = second_runtime
        .create(
            operation_id,
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await
        .expect_err("a foreign runtime instance reused an attached operation owner");
    assert_eq!(concurrent.store_error(), Some(SessionStoreError::LeaseHeld));

    store.expire_active_lease();
    let successor = second_runtime
        .create(
            operation_id,
            durable_test_builder(
                Arc::clone(&workspace),
                tools.clone(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let successor_owner = durable_owner(&successor)?;
    assert_ne!(successor_owner, first_owner);
    let stale_cleanup = first
        .shutdown()
        .await
        .expect_err("an expired owner released a successor lease");
    assert_eq!(
        stale_cleanup.store_error(),
        Some(SessionStoreError::FenceSuperseded)
    );
    assert!(
        lock_mutex(&store.state).last_fence > 2,
        "expired ownership was not superseded by a new fence"
    );
    successor.shutdown().await?;

    let released_successor = third_runtime
        .create(
            operation_id,
            durable_test_builder(workspace, tools, "test.route", b"alpha", false)?,
        )
        .await?;
    let released_successor_owner = durable_owner(&released_successor)?;
    assert_ne!(released_successor_owner, first_owner);
    assert_ne!(released_successor_owner, successor_owner);
    released_successor.shutdown().await?;
    Ok(())
}

/// Every publication renews immediately before its commit, even when the
/// heartbeat left most of the lease. A commit may block on slow storage, so
/// spending an older lease's remainder here can strand the exact head during
/// bounded shutdown.
#[tokio::test(start_paused = true)]
async fn every_publication_renews_before_slow_storage_can_consume_its_lease() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 45_000)?;
    let agent = runtime
        .create(
            "create:commit-only-publication",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 30,
    ));
    let renewals = store.renew_calls.load(Ordering::Acquire);
    let commits = store.commit_calls.load(Ordering::Acquire);
    let result = agent.prompt("one commit burst").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected candidate/tool outcome: {:?}",
        result.outcome()
    );
    let committed = store
        .commit_calls
        .load(Ordering::Acquire)
        .saturating_sub(commits);
    let renewed = store
        .renew_calls
        .load(Ordering::Acquire)
        .saturating_sub(renewals);
    assert!(committed > 1);
    assert_eq!(renewed, committed, "each commit gets fresh authority");
    let _ = agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn rejected_publication_renewal_does_not_commit_or_dispatch_model() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 45_000)?;
    let agent = runtime
        .create(
            "create:rejected-publication-renewal",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let prior = agent.checkpoint().id();
    let commits = store.commit_calls.load(Ordering::Acquire);
    store.reject_renewal.store(true, Ordering::Release);
    let result = match agent.prompt("must not dispatch with old lease").await {
        Ok(turn) => turn.result().await,
        Err(error) => Err(error),
    };
    let _ = agent.shutdown().await;
    assert!(
        result.is_err()
            || result
                .as_ref()
                .is_ok_and(|turn| matches!(turn.outcome(), TurnResultOutcome::RecoveryRequired(_)))
    );
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits);
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert_eq!(agent.checkpoint().id(), prior);
    assert!(store.renew_calls.load(Ordering::Acquire) > 0);
    Ok(())
}

/// A holder that outlived its lease without being replaced — a machine that
/// slept — gets it back under the same fence and carries on; nothing is left
/// for recovery and the turn runs.
#[tokio::test]
async fn an_expired_but_untaken_lease_is_revived_and_the_turn_runs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 45_100)?;
    let agent = runtime
        .create(
            "create:revived-publication-renewal",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    store.reject_renewal.store(true, Ordering::Release);
    store.allow_revival.store(true, Ordering::Release);
    let result = agent
        .prompt("runs on a revived lease")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(store.revive_calls.load(Ordering::Acquire) > 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

/// When the lease cannot be revived — another owner may have it — the start
/// is refused before anything is dispatched, as a definite failure rather
/// than an unknown outcome.
#[tokio::test]
async fn an_unrevivable_lease_refuses_the_start_definitely() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 45_200)?;
    let agent = runtime
        .create(
            "create:unrevivable-renewal",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    store.reject_renewal.store(true, Ordering::Release);
    let result = match agent.prompt("must not dispatch").await {
        Ok(turn) => turn.result().await,
        Err(error) => Err(error),
    };
    let _ = agent.shutdown().await;
    assert!(store.revive_calls.load(Ordering::Acquire) > 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    if let Ok(turn) = result {
        assert!(
            !matches!(turn.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            turn.outcome()
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn aged_lease_publication_preserves_authority_across_slow_commit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = runtime
        .create(
            "create:aged-publication",
            durable_artifact_test_builder(workspace, artifact_test_model(0, 45_000)?),
        )
        .await?;
    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 30,
    ));
    // Past a third of the lease, so the publication renews before it commits.
    tokio::time::advance(Duration::from_secs(11)).await;
    store.delay_next_publication.store(true, Ordering::Release);
    let task = tokio::spawn(async move {
        let result = agent.prompt("one slow publication").await?.result().await;
        Ok::<_, AgentError>((agent, result))
    });
    store.publication_started.notified().await;
    // The store already accepted the fenced commit, then its acknowledgement
    // arrives after the old lease expires but before a refreshed lease would.
    tokio::time::advance(Duration::from_secs(28)).await;
    store.publication_release.notify_one();
    let (agent, result) = task.await??;
    let shutdown = agent.shutdown().await;
    let result = result?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(store.expired_renewals.load(Ordering::Acquire), 0);
    let released = store.released_lease().ok_or("released current lease")?;
    assert_eq!(released.fence(), initial.fence());
    assert_eq!(released.owner(), initial.owner());
    assert_eq!(
        released
            .expires_at()
            .same_domain_cmp(initial.expires_at())?,
        std::cmp::Ordering::Greater
    );
    shutdown?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn delayed_renewal_ack_does_not_add_another_heartbeat_interval() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    *lock_mutex(&store.renewal_clock) = Some(tokio::time::Instant::now());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("nonzero lease")?,
    )?;
    let agent = runtime
        .create(
            "create:delayed-renewal-ack",
            durable_test_builder(
                workspace,
                ToolRegistryBuilder::default().freeze(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    store.renew_started.notified().await;
    // First renewal starts at t=1 and stamps expiry t=4; ACK arrives at t=3.
    tokio::time::advance(Duration::from_secs(2)).await;
    store.renew_gate.notify_one();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let expiry = store.active_lease().map(|lease| lease.expires_at());
    let shutdown = agent.shutdown().await;
    assert_eq!(
        store.expired_renewals.load(Ordering::Acquire),
        0,
        "a delayed renewal acknowledgement must not cause the next renewal to expire"
    );
    assert!(store.renew_calls.load(Ordering::Acquire) >= 2);
    assert!(
        expiry.is_some_and(|expiry| expiry.seconds() > 1001),
        "next renewal must run before the acknowledged lease expires"
    );
    shutdown?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_the_lease_a_heartbeat_renewed_while_cleanup_joined_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.extend_lease_on_renew();
    let renew_started = store.renew_started();
    let renew_gate = store.renew_gate();
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("nonzero lease")?,
    )?;
    let agent = runtime
        .create(
            "create:heartbeat-renewal-at-cleanup",
            durable_test_builder(
                workspace,
                ToolRegistryBuilder::default().freeze(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    // Hold the heartbeat's renewal inside the store so the shutdown below runs
    // while exactly one renewal is in flight.
    renew_started.notified().await;
    let held = store
        .active_lease()
        .ok_or("the store must hold an active lease while renewing it")?;
    let joined = tokio::spawn(async move { agent.shutdown().await });
    // Paused time advances only when every task is parked, so this sleep
    // proves the cleanup already reached its heartbeat join. Releasing the
    // renewal now supersedes any branch context read before that join.
    tokio::time::sleep(Duration::from_millis(50)).await;
    renew_gate.notify_one();
    joined
        .await
        .map_err(|_| "joined shutdown task did not complete")??;
    assert_eq!(
        store.renew_calls.load(Ordering::Acquire),
        1,
        "the test must exercise exactly one in-flight lease renewal"
    );
    let released = store
        .released_lease()
        .ok_or("cleanup did not release the branch lease")?;
    assert_eq!(
        released.expires_at().seconds(),
        held.expires_at().seconds().saturating_add(1),
        "cleanup must release the renewed lease, not the lease it read before \
             joining the heartbeat"
    );
    Ok(())
}

#[tokio::test]
async fn hanging_store_cleanup_and_shutdown_are_bounded_and_redacted() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.hang_release_on(2);
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?
    .with_store_timeouts(Duration::from_millis(100), Duration::from_millis(25))?;
    let agent = runtime
        .create(
            "create:hanging-cleanup",
            durable_test_builder(
                workspace,
                ToolRegistryBuilder::default().freeze(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let cleanup_error = tokio::time::timeout(Duration::from_secs(1), agent.shutdown())
        .await
        .map_err(|_| "agent shutdown outlived its store cleanup bound")?
        .expect_err("hanging lease release unexpectedly succeeded");
    assert_eq!(
        cleanup_error.store_error(),
        Some(SessionStoreError::BackendBusy)
    );
    assert!(cleanup_error.message().contains("lease-release phase"));
    assert!(!cleanup_error.message().contains("alpha"));

    store.hang_shutdown.store(true, Ordering::Release);
    let shutdown_error = tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .map_err(|_| "store shutdown outlived its cleanup bound")?
        .expect_err("hanging store shutdown unexpectedly succeeded");
    assert_eq!(
        shutdown_error.store_error(),
        Some(SessionStoreError::BackendBusy)
    );
    assert!(shutdown_error.message().contains("store shutdown phase"));
    Ok(())
}

#[tokio::test]
async fn prepared_record_checkpoint_precedes_dispatched_and_one_shot_start() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_000,
            false,
        )
        .with_dispatch_probe(Arc::clone(&store.provider_dispatched)),
    );
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:prepared-order",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let result = agent
        .prompt("ordered provider call")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected provider-dispatch outcome: {:?}",
        result.outcome()
    );
    assert_eq!(model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    assert!(store.provider_dispatched.load(Ordering::Acquire));

    let (prepared_revision, dispatched_revision, persisted_bytes) = {
        let state = lock_mutex(&store.state);
        let mut prepared = None;
        let mut dispatched = None;
        let mut bytes = None;
        for stored in state.stored_commits.values() {
            let checkpoint = stored.checkpoint();
            let revision = checkpoint.manifest().topology().branch_revision;
            let Some(provider) = checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
            else {
                continue;
            };
            match provider.state() {
                DurableOperationState::Prepared => {
                    prepared = Some(revision);
                    bytes = Some(provider.request().bytes().to_vec());
                }
                DurableOperationState::Dispatched => dispatched = Some(revision),
                DurableOperationState::DecisionRequired(_)
                | DurableOperationState::ContinuationRequired(_)
                | DurableOperationState::Claimed(_)
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::Completed(_)
                | DurableOperationState::ProvenNotDispatched(_)
                | DurableOperationState::OutcomeUnknown(_)
                | DurableOperationState::Finalized(_) => {}
            }
        }
        (
            prepared.ok_or("provider Prepared checkpoint was not committed")?,
            dispatched.ok_or("provider Dispatched checkpoint was not committed")?,
            bytes.ok_or("provider Prepared record bytes were not retained")?,
        )
    };
    assert!(prepared_revision < dispatched_revision);
    let persisted: PreparedCallRecord = serde_json::from_slice(&persisted_bytes)?;
    assert_eq!(model.prepared_records(), vec![persisted.clone()]);
    assert_eq!(serde_json::to_vec(&persisted)?, persisted_bytes);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn final_authority_drift_drops_prepared_capability_without_start() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_100,
            false,
        )
        .with_dispatch_authority(RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"drifted-account"])?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"artifact-credential-authority"],
            )?,
            2,
            Timestamp::new(9, 0)?,
            Some(Timestamp::new(20, 0)?),
        )?),
    );
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:authority-drift",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let result = agent
        .prompt("authority must stay fixed")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(model.prepares.load(Ordering::Acquire), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    let provider = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("authority drift omitted its prepared provider operation")?;
    assert!(
        provider
            .transitions()
            .iter()
            .any(|transition| matches!(transition.state(), DurableOperationState::Prepared))
    );
    assert!(
        !provider
            .transitions()
            .iter()
            .any(|transition| matches!(transition.state(), DurableOperationState::Dispatched))
    );
    let persisted: PreparedCallRecord = serde_json::from_slice(provider.request().bytes())?;
    assert_eq!(model.prepared_records(), vec![persisted]);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn certified_required_rejects_reference_client_envelope_before_authority_or_start()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = reference_client_envelope_test_descriptor("test.route")?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        9_150,
        false,
    ));
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::ReleaseVerifiedRequired,
    )?;
    let policy = AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
        ReasoningPolicy::Disabled,
    )
    .with_context_admission_requirement(ContextAdmissionRequirement::ReleaseVerifiedRequired);
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(policy)
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
    let result = agent.prompt("must be certified").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(model.authority_checks.load(Ordering::Acquire), 0);
    assert_eq!(model.prepares.load(Ordering::Acquire), 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn candidate_tool_round_admits_then_promotes_before_one_continuation_dispatch() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_200,
            false,
        )
        .with_tool_first(),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
    let result = agent.prompt("use the probe").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected candidate/tool outcome: {:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    // The pending exchange is lowered once for exact next-round capacity
    // admission before its atomic promotion, then lowered once more at the
    // physical continuation fence. Only the latter is dispatched.
    assert_eq!(model.prepares.load(Ordering::Acquire), 3);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    let observed = model.observed();
    assert_eq!(observed.len(), 3);
    assert!(observed[0].is_none());
    let affinities = model.observed_affinities();
    assert_eq!(affinities.len(), 3);
    let first_affinity = affinities[0]
        .as_ref()
        .ok_or("first ordinary prepare omitted runtime cache affinity")?;
    assert_eq!(affinities[1].as_ref(), Some(first_affinity));
    assert_eq!(affinities[2].as_ref(), Some(first_affinity));
    let admitted = observed[1]
        .as_ref()
        .ok_or("pending tool-exchange admission omitted the candidate artifact")?;
    let promoted = observed[2]
        .as_ref()
        .ok_or("tool continuation prepare omitted the promoted artifact")?;
    assert_eq!(promoted, admitted);
    let records = model.prepared_records();
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0].projection().cache_affinity(),
        Some(first_affinity.identity())
    );
    assert_eq!(
        records[1].projection().cache_affinity(),
        Some(first_affinity.identity())
    );
    assert_eq!(
        records[2].projection().cache_affinity(),
        Some(first_affinity.identity())
    );
    assert_eq!(
        records[1].projection().active_artifact(),
        Some(admitted.integrity())
    );
    assert_eq!(
        records[2].projection().active_artifact(),
        Some(promoted.integrity())
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn provider_programmatic_mcp_call_uses_registered_tool_pipeline() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_225,
            false,
        )
        .with_tool_first()
        .with_programmatic_mcp_tool()?,
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
    let result = agent.prompt("use the MCP probe").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected MCP outcome: {:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn mixed_direct_and_provider_programmatic_calls_share_one_hosted_batch() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            9_250,
            false,
        )
        .with_tool_first()
        .with_mixed_programmatic_and_direct_tools()?,
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;

    let result = agent
        .prompt("use both probe routes")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), 2);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tool_growth_terminalizes_with_exact_compactable_checkpoint() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.capacity-growth")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            9_225,
            false,
        )
        .with_tool_call_on(1)
        .with_context_tokens_from_prepare(2, 1_000_000),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;

    let seed = agent
        .prompt("older compactable context")
        .await?
        .result()
        .await?;
    assert!(matches!(seed.outcome(), TurnResultOutcome::Completed(_)));
    let result = agent
        .prompt("use the growing probe")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    let capacity = result
        .context_capacity()
        .ok_or("capacity-limited turn omitted its typed terminal boundary")?;
    assert_eq!(capacity.source_checkpoint(), result.checkpoint_id());
    assert_eq!(capacity.source_checkpoint(), agent.checkpoint_id());
    assert!(capacity.failure().is_compactable());
    assert!(capacity.failure().overflow().shared().is_some());
    assert_eq!(
        result.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::Compact)
    );
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(model.prepares.load(Ordering::Acquire), 4);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert!(result.conversation().items().iter().any(|item| {
        item.blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult(_)))
    }));
    assert!(matches!(
        result.checkpoint().manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    agent.shutdown().await?;
    Ok(())
}
