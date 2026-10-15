use super::*;

async fn delayed_dispatch_ack_admission(
    delay: u64,
    lost: bool,
    clock_failure: bool,
    authority_only: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("nonzero lease")?,
    )?;
    let model = artifact_test_model(0, 45_000)?;
    let agent = runtime
        .create(
            "create:delayed-start-admission",
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 3,
    ));
    if lost {
        store.lose_dispatched_ack.store(true, Ordering::Release);
    } else {
        store.delay_dispatched_ack.store(true, Ordering::Release);
    }
    let prompt_agent = agent.clone();
    let task = tokio::spawn(async move {
        prompt_agent
            .prompt("lease admission witness")
            .await?
            .result()
            .await
    });
    store.delayed_recovery_started.notified().await;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    // Capture the exact accepted operation, including its original lease. A
    // later refusal must neither erase it nor reinterpret a renewed receipt.
    let historical = {
        let state = lock_mutex(&store.state);
        let head = state.loaded.as_ref().ok_or("committed head")?.head();
        state
            .commits
            .get(&head.checkpoint().checkpoint_id())
            .ok_or("accepted operation")?
            .clone()
    };
    if authority_only {
        let mut clock = lock_mutex(&store.publication_clock);
        let (epoch, base) = clock.ok_or("authority clock")?;
        *clock = Some((epoch, base + delay));
    } else {
        tokio::time::advance(Duration::from_secs(delay)).await;
    }
    store
        .fail_lease_clock
        .store(clock_failure, Ordering::Release);
    store.delayed_recovery_release.notify_one();
    let result = task.await?;
    let shutdown = agent.shutdown().await;
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        usize::from(delay < 3 && !clock_failure)
    );
    assert_eq!(
        store.lost_commit_acks.load(Ordering::Acquire),
        usize::from(lost)
    );
    let recovered = store
        .recover_commit(historical.receipt().checkpoint().checkpoint_id())
        .await?;
    assert_eq!(recovered, historical);
    if delay < 3 && !clock_failure {
        let turn = result?;
        assert!(matches!(turn.outcome(), TurnResultOutcome::Completed(_)));
        shutdown?;
    } else if clock_failure {
        // Nothing started, so the refusal is definite, not an unknown outcome.
        let turn = result?;
        assert!(
            matches!(turn.outcome(), TurnResultOutcome::Failed(_)),
            "{:?}",
            turn.outcome()
        );
    } else {
        // The lease expired and could not be revived, so the terminal itself
        // cannot be published under it.
        let turn = result?;
        assert!(matches!(
            turn.outcome(),
            TurnResultOutcome::RecoveryRequired(_)
        ));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn delayed_normal_dispatch_ack_admits_only_before_expiry() -> TestResult {
    for delay in [2, 3, 4] {
        delayed_dispatch_ack_admission(delay, false, false, false).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn delayed_exact_dispatch_recovery_preserves_receipt_but_not_expired_authority() -> TestResult
{
    for delay in [2, 3, 4] {
        delayed_dispatch_ack_admission(delay, true, false, false).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn evaluator_source_wait_rechecks_lease_before_heap_execution_and_joins() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5100, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("store bounded state")?),
                (2, "exec", serde_json::to_string("load bounded state")?),
            ]),
    );
    let configured = configured_test_model_for_operation_with_tools(
        model.clone(),
        descriptor.clone(),
        authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
        1,
        ReasoningEffortDisposition::Disabled,
        Some(&["exec", "wait"]),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let effective = configured.configuration().effective();
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        descriptor
            .model_family()
            .ok_or("neutral composition descriptor omitted its family")?
            .as_str(),
        descriptor.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let composition_descriptor = neutral_composition_descriptor(registry.clone(), target)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = composition_test_builder(
        Arc::clone(&workspace),
        registry.clone(),
        &model,
        descriptor.clone(),
        authority.clone(),
        composition_descriptor.clone(),
        (
            Arc::clone(&evidence),
            LocalCompositionLifecycle::AgentIncarnationPersistent,
        ),
    )?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:delayed-source-admission", builder)
        .await?;

    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 3,
    ));
    evidence.delay_source.store(true, Ordering::Release);
    let prompt_agent = agent.clone();
    let task = tokio::spawn(async move {
        prompt_agent
            .prompt("store evaluator state")
            .await?
            .result()
            .await
    });
    evidence.source_waiting.notified().await;
    assert_eq!(evidence.executes.load(Ordering::Acquire), 1);
    assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 0);
    tokio::time::advance(Duration::from_secs(3)).await;
    evidence.source_release.notify_one();
    let result = task.await?;
    let _shutdown = agent.shutdown().await;
    assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 0);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    assert!(
        result.is_err()
            || result.as_ref().is_ok_and(|turn| {
                matches!(turn.outcome(), TurnResultOutcome::RecoveryRequired(_))
            })
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn paid_compaction_expired_dispatch_ack_never_consumes_start() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("nonzero lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:expired-compaction-ack",
        91_600,
    )
    .await?;
    let (builder, model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 3,
    ));
    store.provider_dispatched.store(false, Ordering::Release);
    store.lose_dispatched_ack.store(true, Ordering::Release);
    let endpoint = agent.session_endpoint();
    let task = tokio::spawn(async move {
        endpoint
            .compact_session(AgentCompactSessionRequest::new(
                "compact:expired-dispatch-ack".to_owned(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            ))
            .await
    });
    store.delayed_recovery_started.notified().await;
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    tokio::time::advance(Duration::from_secs(3)).await;
    store.delayed_recovery_release.notify_one();
    let result = task.await?;
    let _shutdown = agent.shutdown().await;
    assert_eq!(model.starts.load(Ordering::Acquire), 0);
    assert!(
        result.is_err()
            || result.is_ok_and(|outcome| matches!(
                outcome,
                AgentCompactSessionOutcome::Failed { .. }
                    | AgentCompactSessionOutcome::RecoveryRequired { .. }
            ))
    );
    Ok(())
}

#[derive(Default)]
pub(super) struct LeasePermitTool {
    pub(super) starts: AtomicUsize,
    pub(super) entered: Notify,
    pub(super) release: Notify,
}

impl plakous_tools::ToolExecutor for LeasePermitTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        self.starts.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        Box::pin(async move {
            self.release.notified().await;
            ToolOutcome::success_text("joined")
        })
    }
}

pub(super) fn lease_permit_builder(
    workspace: Arc<WorkspacePolicy>,
    tool: Arc<LeasePermitTool>,
    scheduler: ToolScheduler,
    durable: bool,
    effect: ToolEffect,
) -> TestResult<AgentBuilder> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Admission witness.",
        ToolInputContract::structured(
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{}, "required":[], "additionalProperties":false}),
        )?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        effect,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"lease-permit-probe"])?,
        definition,
        tool,
    ))?;
    let registry = registry.freeze();
    let descriptor = durable_test_descriptor("test.lease-permit")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            authority.clone(),
            0,
            if durable { 93_100 } else { 94_100 },
            false,
        )
        .with_tool_call_on(0),
    );
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        if durable {
            LocalDurabilityDisposition::DurableRequired
        } else {
            LocalDurabilityDisposition::Ephemeral
        },
        ContextAdmissionRequirement::AnyBounded,
    )?;
    Ok(AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .tool_scheduler(scheduler, ToolRuntimeConfig::new(1)?)
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        ))
}

struct LeasePermitPolicy(AtomicU64);

impl ToolPolicy for LeasePermitPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        ToolPolicyRevision::new("test.lease-permit-policy", self.0.load(Ordering::Acquire))
            .expect("fixture revision")
    }

    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        AlwaysAllowPolicy.evaluate(request)
    }
}

#[derive(Clone, Copy)]
enum LeasePermitInterruption {
    Expiry,
    Cancellation,
    PolicyDrift,
    RenewalExpiry,
}

async fn permit_wait_admission(
    effect: ToolEffect,
    interruption: LeasePermitInterruption,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let scheduler = ToolScheduler::new(1)?;
    let holder_tool = Arc::new(LeasePermitTool::default());
    let holder = lease_permit_builder(
        Arc::clone(&workspace),
        Arc::clone(&holder_tool),
        scheduler.clone(),
        false,
        ToolEffect::ReadOnly,
    )?
    .build()?;
    let held = holder.prompt("hold scheduler permit").await?;
    holder_tool.entered.notified().await;
    let tool = Arc::new(LeasePermitTool::default());
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(3).ok_or("lease")?,
    )?;
    let policy = Arc::new(LeasePermitPolicy(AtomicU64::new(1)));
    let agent = runtime
        .create(
            "create:permit-admission",
            lease_permit_builder(
                workspace,
                Arc::clone(&tool),
                scheduler.clone(),
                true,
                effect,
            )?
            .tool_policy(policy.clone()),
        )
        .await?;
    let initial = store.active_lease().ok_or("initial lease")?;
    *lock_mutex(&store.publication_clock) = Some((
        tokio::time::Instant::now(),
        initial.expires_at().seconds() - 3,
    ));
    let turn = agent.prompt("wait for permit then check lease").await?;
    while scheduler.status().waiting() != 1 {
        tokio::task::yield_now().await;
    }
    let renewal = if matches!(interruption, LeasePermitInterruption::RenewalExpiry) {
        store.delay_next_renewal_ack.store(true, Ordering::Release);
        let authority = agent.inner.authority.clone();
        let task = tokio::spawn(async move { authority.renew_lease().await });
        store.renew_started.notified().await;
        Some(task)
    } else {
        None
    };
    let gate_guard = if renewal.is_none() {
        Some(
            agent
                .inner
                .authority
                .pause_lease()
                .await
                .ok_or("durable authority")?,
        )
    } else {
        None
    };
    // Suppress heartbeat progress while the exact tool has already published
    // its Dispatched checkpoint and is waiting for the shared permit.
    if matches!(
        interruption,
        LeasePermitInterruption::Expiry | LeasePermitInterruption::RenewalExpiry
    ) {
        tokio::time::advance(Duration::from_secs(3)).await;
    }
    holder_tool.release.notify_one();
    while scheduler.status().waiting() != 0 {
        tokio::task::yield_now().await;
    }
    // Give the newly admitted permit owner a poll at the final lease gate.
    tokio::task::yield_now().await;
    if matches!(interruption, LeasePermitInterruption::PolicyDrift) {
        policy.0.store(2, Ordering::Release);
    }
    if matches!(interruption, LeasePermitInterruption::Cancellation) {
        let shared = lock_mutex(&agent.inner.known_turns)
            .get(&turn.turn_id)
            .and_then(Weak::upgrade)
            .ok_or("active turn")?;
        // An already-delivered root cancellation must win the final gate.
        // A queued public command is not yet an acknowledged cancellation.
        shared.cancellation.cancel();
    }
    assert_eq!(tool.starts.load(Ordering::Acquire), 0);
    drop(gate_guard);
    if let Some(renewal) = renewal {
        store.renew_gate.notify_one();
        renewal.await??;
    }
    let result = turn.result().await;
    let _shutdown = agent.shutdown().await;
    let _held = held.result().await;
    holder.shutdown().await?;
    assert_eq!(tool.starts.load(Ordering::Acquire), 0);
    if matches!(interruption, LeasePermitInterruption::PolicyDrift) {
        assert_eq!(result?.tool_calls(), 0);
    } else {
        assert!(
            result.is_err()
                || result.as_ref().is_ok_and(|turn| {
                    !matches!(turn.outcome(), TurnResultOutcome::Completed(_))
                })
        );
    }
    assert_eq!(scheduler.status().active(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn read_only_and_mutating_tool_permit_waits_recheck_exact_lease() -> TestResult {
    for effect in [ToolEffect::ReadOnly, ToolEffect::Mutating] {
        permit_wait_admission(effect, LeasePermitInterruption::Expiry).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_final_tool_lease_gate_never_enters_executor() -> TestResult {
    permit_wait_admission(ToolEffect::ReadOnly, LeasePermitInterruption::Cancellation).await
}

#[tokio::test(start_paused = true)]
async fn policy_revision_change_during_final_lease_gate_never_enters_executor() -> TestResult {
    for effect in [ToolEffect::ReadOnly, ToolEffect::Mutating] {
        permit_wait_admission(effect, LeasePermitInterruption::PolicyDrift).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn unavailable_store_lease_clock_refuses_start_and_preserves_receipt() -> TestResult {
    delayed_dispatch_ack_admission(0, true, true, false).await
}

#[tokio::test(start_paused = true)]
async fn renewal_ack_after_stamped_expiry_cannot_admit_waiting_executor() -> TestResult {
    permit_wait_admission(ToolEffect::ReadOnly, LeasePermitInterruption::RenewalExpiry).await
}

#[tokio::test(start_paused = true)]
async fn delivered_root_cancellation_during_scheduled_compaction_ack_refuses_paid_start()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = scheduled_tool_group_fence_builder(
        workspace,
        true,
        96_000,
        900_000,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create("create:cancel-scheduled-compaction", builder)
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    store.compaction_ack_only.store(true, Ordering::Release);
    store.lose_dispatched_ack.store(true, Ordering::Release);
    let turn = agent.prompt("tool then scheduled compaction").await?;
    store.delayed_recovery_started.notified().await;
    let prior_starts = model.calls.load(Ordering::Acquire);
    let shared = lock_mutex(&agent.inner.known_turns)
        .get(&turn.turn_id)
        .and_then(Weak::upgrade)
        .ok_or("active turn")?;
    shared.cancellation.cancel();
    store.delayed_recovery_release.notify_one();
    let result = turn.result().await;
    let _shutdown = agent.shutdown().await;
    assert_eq!(model.calls.load(Ordering::Acquire), prior_starts);
    assert!(
        result.is_err()
            || result
                .as_ref()
                .is_ok_and(|turn| !matches!(turn.outcome(), TurnResultOutcome::Completed(_)))
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn store_clock_expiry_without_heartbeat_tick_refuses_recovered_start() -> TestResult {
    delayed_dispatch_ack_admission(3, true, false, true).await
}

#[tokio::test(start_paused = true)]
async fn public_cancellation_during_scheduled_compaction_joins_pending_start_and_stream()
-> TestResult {
    for as_stream in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let (builder, model) = scheduled_tool_group_fence_builder(
            workspace,
            true,
            97_000,
            900_000,
            LocalDurabilityDisposition::DurableRequired,
        )?;
        let store = Arc::new(TestCreateStore::default());
        let runtime = DurableSessionRuntime::current(
            store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = runtime
            .create("create:public-cancel-compaction", builder)
            .await?;
        for prompt in ["older replaceable intent", "latest protected intent"] {
            let result = agent.prompt(prompt).await?.result().await?;
            assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        }
        model.held_call.store(3, Ordering::Release);
        model
            .held_call_as_stream
            .store(as_stream, Ordering::Release);
        let turn = agent.prompt("tool then scheduled compaction").await?;
        tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;
        let control = turn.control();
        let acknowledgement =
            tokio::time::timeout(Duration::from_secs(5), control.request_cancel()).await??;
        assert_eq!(acknowledgement, CancellationStatus::Requested);
        let result = tokio::time::timeout(Duration::from_secs(5), turn.result()).await??;
        assert!(model.held_call_joined.load(Ordering::Acquire));
        assert_eq!(model.calls.load(Ordering::Acquire), 4);
        // The driver that owns the fold closes it in place: the turn ends
        // cancelled, a definite outcome, and nothing is left for recovery.
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
            "an Esc inside a paid fold is a cancelled turn, not a fence: {:?}",
            result.outcome()
        );
        let head = agent.checkpoint();
        assert!(matches!(
            head.manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ));
        assert!(
            head.manifest()
                .ledger()
                .operations()
                .into_iter()
                .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
                .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
        );
        let next = agent.prompt("carry on after the cancelled fold").await?;
        let next = tokio::time::timeout(Duration::from_secs(30), next.result()).await??;
        assert!(
            !matches!(next.outcome(), TurnResultOutcome::RecoveryRequired(_)),
            "the session takes the next prompt: {:?}",
            next.outcome()
        );
        let _shutdown = agent.shutdown().await;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn public_controls_during_scheduled_compaction_survive_successful_fold_in_order() -> TestResult
{
    successful_compaction_controls(false).await
}

#[tokio::test(start_paused = true)]
async fn public_controls_during_returned_compaction_stream_survive_successful_fold() -> TestResult {
    successful_compaction_controls(true).await
}

async fn successful_compaction_controls(as_stream: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = scheduled_tool_group_fence_builder(
        workspace,
        true,
        98_000,
        900_000,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create("create:public-controls-compaction", builder)
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    model.held_call.store(3, Ordering::Release);
    model
        .held_call_as_stream
        .store(as_stream, Ordering::Release);
    let active = agent.prompt("tool then scheduled compaction").await?;
    tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;
    assert_eq!(
        model.held_call_stream_polled.load(Ordering::Acquire),
        as_stream
    );
    let normal_id = CommandId::new("compaction:normal")?;
    let follow_id = CommandId::new("compaction:follow")?;
    let steer_id = CommandId::new("compaction:steer")?;
    let normal = tokio::time::timeout(
        Duration::from_secs(5),
        agent.start(normal_id.clone(), "ordinary queued prompt"),
    )
    .await??;
    let follow = tokio::time::timeout(
        Duration::from_secs(5),
        agent.follow_up_with_id(follow_id.clone(), active.id(), "priority follow-up"),
    )
    .await??;
    let steer = tokio::time::timeout(
        Duration::from_secs(5),
        active
            .control()
            .steer_with_id(steer_id.clone(), "steering at pending compaction"),
    )
    .await??;
    let acknowledgements = [
        normal.acknowledgement().clone(),
        follow.acknowledgement().clone(),
        steer,
    ];
    for ack in &acknowledgements {
        assert_eq!(ack.disposition(), CommandAckDisposition::Accepted);
    }
    assert!(
        acknowledgements
            .windows(2)
            .all(|pair| pair[0].sequence() < pair[1].sequence())
    );
    let accepted = agent.checkpoint();
    let records: Vec<_> = accepted
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .filter(|record| {
            acknowledgements
                .iter()
                .any(|ack| ack.command_id().as_str() == record.command_id())
        })
        .cloned()
        .collect();
    assert_eq!(records.len(), 3);
    for (record, ack) in records.iter().zip(&acknowledgements) {
        assert_eq!(record.sequence(), ack.sequence());
        assert_eq!(record.turn_id(), ack.turn_id());
        assert_eq!(record.disposition(), DurableCommandDisposition::Accepted);
    }
    assert_eq!(model.calls.load(Ordering::Acquire), 4);
    release.notify_one();
    let active_result = active.result().await?;
    let operations = active_result.checkpoint().manifest().ledger().operations();
    let compactions = operations
        .iter()
        .filter(|operation| matches!(operation.kind(), DurableOperationKind::Compaction { .. }))
        .collect::<Vec<_>>();
    assert_eq!(compactions.len(), 1);
    let DurableOperationKind::Compaction { workflow_id } = compactions[0].kind() else {
        return Err("compaction workflow missing".into());
    };
    let DurableOperationState::Finalized(evidence) = compactions[0].state() else {
        return Err("compaction did not finalize".into());
    };
    let publication: DurableCompactionPublicationEvidence =
        serde_json::from_slice(evidence.bytes())?;
    assert_eq!(publication.version, 1);
    assert_eq!(publication.workflow_id, *workflow_id);
    let providers = operations.iter().filter(|operation| matches!(operation.kind(), DurableOperationKind::Provider { root: ModelOperationRoot::SessionWorkflow(candidate), .. } if candidate == workflow_id)).collect::<Vec<_>>();
    assert_eq!(providers.len(), 1);
    assert_eq!(
        providers[0]
            .transitions()
            .iter()
            .filter(|transition| matches!(transition.state(), DurableOperationState::Dispatched))
            .count(),
        1
    );
    let follow_result = follow.into_turn().ok_or("follow-up turn")?.result().await?;
    let normal_result = normal.into_turn().ok_or("normal turn")?.result().await?;
    for result in [&active_result, &follow_result, &normal_result] {
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        for record in &records {
            assert!(
                result
                    .checkpoint()
                    .manifest()
                    .ledger()
                    .commands()
                    .contains(&record)
            );
        }
    }
    let texts = |result: &TurnResult| {
        result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| {
                if let ContentBlock::Text(text) = block {
                    Some(text.text().to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let active_texts = texts(&active_result);
    assert!(
        active_texts
            .iter()
            .any(|text| text == "steering at pending compaction")
    );
    assert!(
        !active_texts
            .iter()
            .any(|text| text == "priority follow-up" || text == "ordinary queued prompt")
    );
    let final_texts = texts(&normal_result);
    let mut previous = None;
    for expected in [
        "artifact response 4",
        "priority follow-up",
        "artifact response 5",
        "ordinary queued prompt",
        "artifact response 6",
    ] {
        let position = final_texts
            .iter()
            .position(|text| text == expected)
            .ok_or("ordered canonical text")?;
        assert!(previous.is_none_or(|index| index < position));
        previous = Some(position);
    }
    assert_eq!(model.calls.load(Ordering::Acquire), 7);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn nested_delivery_cleanup_bounds_missing_receipts_without_certifying_acceptance() {
    let mut pending = PendingNestedDeliveries::default();
    pending.push(Box::pin(std::future::pending()));
    let errors = settle_local_composition_deliveries(&mut pending).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind(), AgentErrorKind::OutcomeUnknown);
    assert!(pending.is_empty());
}

#[tokio::test]
async fn pending_delivery_capacity_counts_each_unresolved_duplicate() {
    let mut pending = PendingNestedDeliveries::default();
    let mut releases = Vec::new();
    for _ in 0..16 {
        let (release, receipt) = tokio::sync::oneshot::channel();
        releases.push(release);
        pending.push(Box::pin(async move {
            receipt
                .await
                .map_err(|_| local_composition_started_error("lost test receipt"))
        }));
    }
    assert_eq!(pending.len(), 16);
    assert!(reserve_local_composition_nested_slot(pending.len(), 16, 1).is_err());
    releases
        .pop()
        .expect("first sender")
        .send(crate::runtime::composition::NestedDeliveryReceipt::Accepted)
        .expect("release one");
    pending.next().await.expect("receipt").expect("accepted");
    assert_eq!(pending.len(), 15);
    assert!(reserve_local_composition_nested_slot(pending.len(), 16, 1).is_ok());
    for release in releases {
        release
            .send(crate::runtime::composition::NestedDeliveryReceipt::Accepted)
            .expect("release remaining");
    }
    while let Some(result) = pending.next().await {
        result.expect("accepted");
    }
    assert!(pending.is_empty());
}

#[test]
fn nested_delivery_panics_preserve_remaining_shutdown_stages_and_prior_failures() -> TestResult {
    for scenario in [
        DriverSchedulerScenario::DeliveryPanics,
        DriverSchedulerScenario::DeliveryPanicsAfterIntegrationLoss,
    ] {
        run_driver_scheduler_scenario(scenario)?;
    }
    Ok(())
}

#[test]
fn failed_nested_edge_publication_retains_tool_evidence_without_delivery_or_replay() -> TestResult {
    run_driver_scheduler_scenario(DriverSchedulerScenario::EdgePublicationFailure)
}

pub(super) fn assert_publication_failure_witness(
    store: &TestCreateStore,
    evidence: &DriverSchedulerEvidence,
    result: &TurnResult,
) -> TestResult {
    let (before, rejected) = lock_mutex(&store.rejected_nested_edge)
        .clone()
        .ok_or("the exact nested child edge publication was not reached")?;
    let edge = rejected
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionChild {
                    request_sequence: 1,
                    ..
                }
            )
        })
        .ok_or("rejected proposal omitted child edge")?;
    let DurableOperationKind::CompositionChild { call_id, .. } = edge.kind() else {
        unreachable!()
    };
    assert!(matches!(edge.state(), DurableOperationState::Completed(_)));
    let prior_edge = before
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == edge.operation_id())
        .ok_or("durable edge is missing")?;
    assert!(matches!(
        prior_edge.state(),
        DurableOperationState::Dispatched
    ));
    let tool = before.manifest().ledger().operations().into_iter().find(|record|
        matches!(record.kind(), DurableOperationKind::Effect { call_id: candidate, .. } if candidate == call_id))
        .ok_or("definite tool terminal was not persisted before child edge proposal")?;
    let DurableOperationState::Completed(terminal) = tool.state() else {
        return Err("tool terminal was not completed".into());
    };
    let (tag, _, completed): (String, EntryId, ToolExecutionResult) =
        serde_json::from_slice(terminal.bytes())?;
    assert_eq!(tag, "completed-tool-outcome-v1");
    assert!(completed.dispatched);
    assert!(
        matches!(completed.result.parts(), [ToolResultPart::Text(text)] if text.text() == "read-a")
    );
    let last = lock_mutex(&store.state)
        .stored
        .as_ref()
        .ok_or("stored checkpoint")?
        .checkpoint()
        .clone();
    assert_eq!(last.id(), before.id());
    assert_eq!(result.checkpoint_id(), before.id());
    assert_ne!(last.id(), rejected.id());
    assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 1);
    assert_eq!(evidence.source_starts.load(Ordering::Acquire), 1);
    assert!(lock_mutex(&evidence.mutation_order).is_empty());
    assert!(
        lock_mutex(&evidence.deliveries)
            .iter()
            .all(|(identity, _, _)| identity != "A" && identity != "A_DUPLICATE")
    );
    let drops = lock_mutex(&evidence.responder_drops);
    assert_eq!(
        drops.iter().filter(|(identity, _)| identity == "A").count(),
        1
    );
    assert_eq!(
        drops
            .iter()
            .filter(|(identity, _)| identity == "A_DUPLICATE")
            .count(),
        1
    );
    assert!(
        drops
            .iter()
            .any(|(identity, shutdowns)| identity == "A" && *shutdowns == 0)
    );
    assert!(
        drops
            .iter()
            .any(|(identity, shutdowns)| identity == "A_DUPLICATE" && *shutdowns >= 1)
    );
    assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
    assert_eq!(
        evidence.counters.session_shutdowns.load(Ordering::Acquire),
        1
    );
    Ok(())
}

#[test]
fn lost_nested_delivery_receipt_preserves_committed_tool_result_without_replay() -> TestResult {
    run_driver_scheduler_scenario(DriverSchedulerScenario::DeliveryHostLost)
}

/// A refused acknowledgement is not a loss: the composition keeps running, the
/// turn keeps its own terminal, and no child result is replayed.
#[test]
fn refused_nested_delivery_keeps_the_composition_and_its_single_terminal() -> TestResult {
    run_driver_scheduler_scenario(DriverSchedulerScenario::DeliveryRefused)
}

// ---------------------------------------------------------------------------
// The placeholder rung (spec 01 §10.2): offline tool-result-body compaction.
// ---------------------------------------------------------------------------
