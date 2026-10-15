use super::*;

fn recovery_compaction_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<RecoveryCompactionModel>,
    authority: RuntimeModelAuthority,
) -> AgentBuilder {
    let descriptor = model.scripted.descriptor().clone();
    let model: Arc<dyn LanguageModel> = model;
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )
    .expect("valid configured recovery compaction model");
    AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())
                .expect("valid fixture generation limits"),
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(
                Timestamp::new(10, 0).expect("valid fixture timestamp"),
            )),
            Arc::new(SystemIdGenerator),
        )
}

pub(super) fn recovery_session_builder(
    workspace: Arc<WorkspacePolicy>,
    model: Arc<RecoveryCompactionModel>,
    authority: RuntimeModelAuthority,
) -> AgentBuilder {
    let descriptor = model.scripted.descriptor().clone();
    let model: Arc<dyn LanguageModel> = model;
    let registry = ToolRegistryBuilder::default().freeze();
    let configured = configured_test_model(
        model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )
    .expect("valid configured recovery session model");
    AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())
                .expect("valid fixture generation limits"),
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(
                Timestamp::new(10, 0).expect("valid fixture timestamp"),
            )),
            Arc::new(SystemIdGenerator),
        )
}

pub(super) fn recovery_compaction_fixture(
    workspace: Arc<WorkspacePolicy>,
    recovery: plakous_test_support::ScriptedRecoveryState,
    outcome: CompactionFixtureOutcome,
    fail_prepare_on: Option<usize>,
) -> TestResult<(AgentBuilder, Arc<RecoveryCompactionModel>)> {
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = RecoveryCompactionModel::new(
        descriptor,
        authority.clone(),
        recovery,
        outcome,
        fail_prepare_on,
    )?;
    Ok((
        recovery_compaction_builder(workspace, Arc::clone(&model), authority),
        model,
    ))
}

pub(super) async fn seeded_compaction_source(
    sessions: &DurableSessionRuntime,
    workspace: Arc<WorkspacePolicy>,
    operation_id: &str,
    entry_base: u128,
) -> TestResult<(Agent, DurableSessionAddress)> {
    seeded_compaction_source_with_prompts(
        sessions,
        workspace,
        operation_id,
        entry_base,
        &[
            "older replaceable intent".to_owned(),
            "latest protected intent".to_owned(),
        ],
    )
    .await
}

pub(super) async fn seeded_compaction_source_with_prompts(
    sessions: &DurableSessionRuntime,
    workspace: Arc<WorkspacePolicy>,
    operation_id: &str,
    entry_base: u128,
    prompts: &[String],
) -> TestResult<(Agent, DurableSessionAddress)> {
    let source_model = artifact_test_model(0, entry_base)?;
    let agent = sessions
        .create(
            operation_id,
            durable_artifact_test_builder(workspace, source_model),
        )
        .await?;
    for prompt in prompts {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "source seeding failed: {:?}",
            result.outcome()
        );
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    Ok((agent, source))
}

pub(super) fn checkpoint_with_compaction_records(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
    operations: Vec<DurableOperationRecord>,
) -> TestResult<RuntimeCheckpoint> {
    checkpoint_with_compaction_state(
        checkpoint,
        identity,
        operations,
        checkpoint.manifest().canonical().clone(),
    )
}

pub(super) fn checkpoint_with_compaction_state(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
    operations: Vec<DurableOperationRecord>,
    canonical: RuntimeCanonicalState,
) -> TestResult<RuntimeCheckpoint> {
    let prior = checkpoint.manifest();
    let ledger = RuntimeOperationLedger::new(
        prior.ledger().commands().into_iter().cloned().collect(),
        operations,
    )?;
    let lifecycle = if matches!(prior.lifecycle(), RuntimeLifecycle::Quiescent) {
        RuntimeLifecycle::Quiescent
    } else {
        working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?
    };
    let old_topology = prior.topology();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id: old_topology.checkpoint_id,
        parent_checkpoint_id: old_topology
            .parent_checkpoint_id
            .ok_or("working checkpoint omitted its parent")?,
        branch_revision: old_topology.branch_revision,
        lifecycle: &lifecycle,
        history_identity: canonical.history().identity(),
        ledger: &ledger,
        context_identity: canonical.context().identity(),
        family_artifact: prior.family_artifact(),
    })?;
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: old_topology.session_id,
            branch_id: old_topology.branch_id,
            checkpoint_id: old_topology.checkpoint_id,
            parent_checkpoint_id: old_topology.parent_checkpoint_id,
            branch_revision: old_topology.branch_revision,
            fork: old_topology.fork,
            lease_fence: old_topology.lease_fence,
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        canonical,
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?)
}

// Re-prepare the forged input so decode cannot reject mere request/prepared
// identity drift. Changing user intent is never a permitted result-body elision.
pub(super) fn coherently_malformed_compaction_input(
    checkpoint: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
    model: &Arc<RecoveryCompactionModel>,
) -> TestResult<RuntimeCheckpoint> {
    let operations = decode_paid_compaction_operations(checkpoint, identity)?;
    let mut request = operations.request;
    assert_eq!(request.version, 2);
    let mut changed = false;
    let segments = request
        .request
        .input()
        .history()
        .segments()
        .iter()
        .map(|segment| {
            let items = segment
                .items()
                .iter()
                .map(|item| {
                    if !changed && item.actor() == Actor::User {
                        changed = true;
                        Ok(ConversationItem::new(
                            item.id(),
                            item.actor(),
                            vec![ContentBlock::Text(TextBlock::new(
                                "forged older intent",
                                None,
                            )?)],
                            item.provenance(),
                        )?)
                    } else {
                        Ok(item.clone())
                    }
                })
                .collect::<TestResult<Vec<_>>>()?;
            Ok(HistorySegment::new(segment.kind(), items)?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert!(changed);
    request.request = ModelRequest::from_parts(
        request.request.stable_prefix().clone(),
        ConversationView::from_shared(
            CanonicalHistory::new(segments)?,
            WorkingProjection::default(),
        ),
        request.request.metadata(),
    )?;
    let descriptor = model.scripted.descriptor();
    let plan = descriptor.validate_request(request.request.clone(), ValidationMode::Strict)?;
    let authority = operations.prepared.authority_evidence().clone();
    let prepared = Arc::clone(model).prepare_call(
        plan,
        ModelPreparationContext::new(authority, Timestamp::new(10, 0)?),
    )?;
    let mut records = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for (operation, bytes) in [
        (operations.compaction, serde_json::to_vec(&request)?),
        (operations.provider, serde_json::to_vec(prepared.record())?),
    ] {
        let record = records
            .iter_mut()
            .find(|record| record.operation_id() == operation.0)
            .ok_or("coherent mutation operation missing")?;
        *record = DurableOperationRecord::new(
            record.operation_id(),
            record.kind().clone(),
            runtime_component(RuntimeComponentKind::Operation, bytes)?,
            record.transitions().to_vec(),
        )?;
    }
    let poisoned = checkpoint_with_compaction_records(checkpoint, identity, records)?;
    let decoded = decode_paid_compaction_operations(&poisoned, identity)?;
    assert_eq!(
        decoded.prepared.projection().request(),
        decoded.request.request.identity()
    );
    assert_ne!(
        decoded.request.request.identity(),
        decode_paid_compaction_operations(checkpoint, identity)?
            .request
            .request
            .identity()
    );
    Ok(poisoned)
}

pub(super) fn checkpoint_with_code_cell_records(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
    operations: Vec<DurableOperationRecord>,
) -> TestResult<RuntimeCheckpoint> {
    let prior = checkpoint.manifest();
    let ledger = RuntimeOperationLedger::new(
        prior.ledger().commands().into_iter().cloned().collect(),
        operations,
    )?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    let old_topology = prior.topology();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id: old_topology.checkpoint_id,
        parent_checkpoint_id: old_topology
            .parent_checkpoint_id
            .ok_or("working checkpoint omitted its parent")?,
        branch_revision: old_topology.branch_revision,
        lifecycle: &lifecycle,
        history_identity: prior.canonical().history().identity(),
        ledger: &ledger,
        context_identity: prior.canonical().context().identity(),
        family_artifact: prior.family_artifact(),
    })?;
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: old_topology.session_id,
            branch_id: old_topology.branch_id,
            checkpoint_id: old_topology.checkpoint_id,
            parent_checkpoint_id: old_topology.parent_checkpoint_id,
            branch_revision: old_topology.branch_revision,
            fork: old_topology.fork,
            lease_fence: old_topology.lease_fence,
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?)
}

/// The pending terminal record the live driver committed for a root, as the
/// bytes recovery synthesizes: the consequence commit finalizes the root in
/// the commit that records it, so it is read back from the finalized record.
pub(super) fn live_pending_terminal(record: &DurableOperationRecord) -> Option<Vec<u8>> {
    if !matches!(record.kind(), DurableOperationKind::Command { .. }) {
        return None;
    }
    match record.state() {
        DurableOperationState::Completed(evidence) => {
            decode_pending_terminal_record(evidence.bytes()).map(|_| evidence.bytes().to_vec())
        }
        DurableOperationState::Finalized(evidence) => {
            let terminal = read_terminal_result_record(evidence.bytes()).ok()?;
            encode_pending_terminal_record(PendingTerminalResultRecord {
                remedy: terminal.remedy,
                version: terminal.version,
                command_id: terminal.command_id,
                turn_id: terminal.turn_id,
                operation_id: terminal.operation_id,
                outcome: terminal.outcome,
                usage: terminal.usage,
            })
            .ok()
        }
        _ => None,
    }
}

pub(super) async fn seed_code_cell_recovery_checkpoint(
    store: &Arc<TestCreateStore>,
    source: Arc<RuntimeCheckpoint>,
    turn_id: TurnId,
) -> TestResult<DurableSessionAddress> {
    let prior = source.manifest();
    let evaluation_operation_id = prior
        .ledger()
        .operations()
        .into_iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("operation identity exhausted")?;
    let evaluation_attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(0xc0de_0001))?;
    let continuation_attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(0xc0de_0006))?;
    let call_id = CallId::try_from(uuid::Uuid::from_u128(0xc0de_0002))?;
    let correlation = ToolCorrelation::new(
        Some("remote-code-cell-call".to_owned()),
        None,
        plakous_model::ToolCallerLineage::Direct,
    )?;
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("exec".to_owned())?,
        ToolExecutionOwner::PlakLocal,
        ToolCallState::Requested,
        correlation,
        "text('started')",
    )?;
    let descriptor_identity = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[b"test-code-mode-descriptor"],
    )?;
    let host_identity = b"test-neutral-host-v1".to_vec();
    let evaluation_request = runtime_component(
        RuntimeComponentKind::Operation,
        encode_local_composition_evaluation_request(&DurableLocalCompositionEvaluationRequestV2 {
            version: 2,
            descriptor_identity,
            host_identity: host_identity.clone(),
            call,
            arguments_digest: Digest::sha256(
                DigestDomain::ProviderOperation,
                3,
                &[b"test-code-mode-arguments"],
            )?,
            source_digest: Some(Digest::sha256(
                DigestDomain::Blob,
                1,
                &[b"test-code-mode-source"],
            )?),
            result_entry_id: EntryId::try_from(uuid::Uuid::from_u128(0xc0de_0005))?,
        })?,
    )?;
    let root = prior
        .ledger()
        .operations()
        .into_iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id
            )
        })
        .ok_or("root operation missing")?;
    // The live driver leaves the root `Dispatched` behind a recorded
    // decision; the seed stands in the pre-consequence pending terminal.
    if !matches!(root.state(), DurableOperationState::Dispatched) {
        return Err("root operation was not awaiting its consequence".into());
    }
    let command_id = prior
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command missing")?
        .command_id()
        .to_owned();
    let mut pending = PendingTerminalResultRecord {
        remedy: None,
        version: TERMINAL_RESULT_RECORD_VERSION,
        command_id,
        turn_id,
        operation_id: OperationId(root.operation_id()),
        outcome: DurableTerminalOutcome::Cancelled(DurableAgentError::from_error(
            &AgentError::new(AgentErrorKind::Cancelled, "seed"),
        )?),
        usage: TurnUsage::default(),
    };
    pending.outcome =
        DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(&AgentError::new(
            AgentErrorKind::OutcomeUnknown,
            "Code Cell outcome requires explicit abandonment",
        ))?);
    let replacement_evidence = runtime_component(
        RuntimeComponentKind::Operation,
        encode_pending_terminal_record(pending)?,
    )?;
    let mut root_transitions = root.transitions().to_vec();
    root_transitions.push(DurableOperationTransition::new(
        source.id(),
        DurableOperationState::Completed(replacement_evidence),
    ));
    let replacement_root = DurableOperationRecord::new(
        root.operation_id(),
        root.kind().clone(),
        root.request().clone(),
        root_transitions,
    )?;
    let evaluation = DurableOperationRecord::new(
        evaluation_operation_id,
        DurableOperationKind::CompositionEvaluation {
            turn_id,
            attempt_id: evaluation_attempt_id,
            call_id,
            source_index: 0,
        },
        evaluation_request,
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_0003))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_0004))?,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let continuation_operation_id = evaluation_operation_id
        .checked_add(1)
        .ok_or("operation identity exhausted")?;
    let continuation_call_id = CallId::try_from(uuid::Uuid::from_u128(0xc0de_0007))?;
    let continuation_call = ToolCallBlock::new(
        continuation_call_id,
        ToolName::new("wait".to_owned())?,
        ToolExecutionOwner::PlakLocal,
        ToolCallState::Requested,
        ToolCorrelation::new(
            Some("remote-code-cell-wait".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?,
        r#"{"cell_id":"cell-continued"}"#,
    )?;
    let continuation_request = runtime_component(
        RuntimeComponentKind::Operation,
        encode_local_composition_continuation_request(
            &DurableLocalCompositionContinuationRequestV1 {
                version: 1,
                descriptor_identity,
                host_identity,
                evaluation_operation_id,
                call: continuation_call,
                arguments_digest: Digest::sha256(
                    DigestDomain::ProviderOperation,
                    3,
                    &[b"test-code-mode-wait-arguments"],
                )?,
                result_entry_id: EntryId::try_from(uuid::Uuid::from_u128(0xc0de_0008))?,
            },
        )?,
    )?;
    let continuation = DurableOperationRecord::new(
        continuation_operation_id,
        DurableOperationKind::CompositionContinuation {
            turn_id,
            attempt_id: continuation_attempt_id,
            call_id: continuation_call_id,
            source_index: 1,
            evaluation_operation_id,
        },
        continuation_request,
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_0009))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_000a))?,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let child_operation_id = continuation_operation_id
        .checked_add(1)
        .ok_or("operation identity exhausted")?;
    let child_request = serde_json::to_vec(&(
        "plakous-local-composition-child-request-v1",
        "cell-continued",
        "nested-during-wait",
        "probe",
        Option::<serde_json::Value>::None,
    ))?;
    let child_request_digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/local-composition-child-request-v1",
            &child_request,
        ],
    )?;
    let child_edge = serde_json::to_vec(&(
        "plakous-local-composition-child-edge-v2",
        evaluation_operation_id,
        1_u64,
        "cell-continued",
        1_u32,
        child_request_digest,
        &child_request,
    ))?;
    let child_binding = Digest::sha256(
        DigestDomain::ProviderOperation,
        4,
        &[
            b"plakous-agent/local-composition-child-edge-v2",
            &child_edge,
        ],
    )?;
    let child = DurableOperationRecord::new(
        child_operation_id,
        DurableOperationKind::CompositionChild {
            turn_id,
            attempt_id: continuation_attempt_id,
            call_id: local_composition_child_call_id(child_binding)?,
            evaluation_operation_id,
            interpreter_generation: 1,
            cell_id: "cell-continued".to_owned(),
            request_sequence: 1,
        },
        runtime_component(RuntimeComponentKind::Operation, child_edge)?,
        vec![
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_000b))?,
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_000c))?,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0xc0de_1001))?;
    let mut records = prior
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            if record.operation_id() == replacement_root.operation_id() {
                return Ok(replacement_root.clone());
            }
            match (record.kind(), record.state()) {
                (
                    DurableOperationKind::Provider { .. },
                    DurableOperationState::Completed(evidence),
                ) => record.append_transition(DurableOperationTransition::new(
                    checkpoint_id,
                    DurableOperationState::Finalized(evidence.clone()),
                )),
                _ => Ok(record.clone()),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    records.extend([evaluation, continuation, child]);
    let ledger = RuntimeOperationLedger::new(
        prior.ledger().commands().into_iter().cloned().collect(),
        records,
    )?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    assert!(matches!(
        lifecycle,
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
            ..
        }
    ));
    let topology = prior.topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0xc0de_1000))?,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        ))
        .await?;
    let loaded = store.load_branch(session).await?;
    let branch_revision = topology.branch_revision.saturating_add(1);
    let history = prior.canonical().history().clone();
    let context = prior.canonical().context().retained_for_exact_resume();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: history.identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        context,
    )?;
    let manifest = RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            fork: topology.fork,
            lease_fence: lease.fence().get(),
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        canonical,
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )
    .map_err(|error| format!("seed manifest failed: {error:?}"))?;
    let checkpoint = RuntimeCheckpoint::new(manifest)
        .map_err(|error| format!("seed checkpoint failed: {error:?}"))?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(DurableSessionAddress::new(
        topology.session_id,
        topology.branch_id,
        checkpoint_id,
    ))
}
