use super::*;
use crate::CheckpointObjectBundle;
use crate::local_composition::{StateOp, StateResult};

struct StateResponder(
    tokio::sync::oneshot::Sender<Result<StateResult, crate::local_composition::StateError>>,
);

impl crate::local_composition::LocalCompositionStateResponder for StateResponder {
    fn respond(
        self: Box<Self>,
        result: Result<StateResult, crate::local_composition::StateError>,
    ) -> bool {
        self.0.send(result).is_ok()
    }
}

pub(super) fn start_cell(
    fixture: &FakeCompositionSession,
    source: String,
) -> LocalCompositionStartedCell {
    let events = fixture.state_events.clone();
    let evidence = Arc::clone(&fixture.evidence);
    let generation_lost = Arc::clone(&fixture.generation_lost);
    LocalCompositionStartedCell {
        cell_id: "named".to_owned(),
        initial_response: Box::pin(async move {
            let (responder, response) = tokio::sync::oneshot::channel();
            events
                .send(LocalCompositionEvent::State {
                    cell_id: "foreign-cell".to_owned(),
                    op: StateOp::Put {
                        key: "foreign".to_owned(),
                        value: json!(true),
                    },
                    responder: Box::new(StateResponder(responder)),
                })
                .unwrap();
            assert!(
                matches!(
                    response.await.unwrap(),
                    Err(crate::local_composition::StateError::ForeignCell)
                ),
                "foreign cells cannot access state"
            );
            let ops = if source == "named state load" {
                vec![
                    StateOp::Get {
                        key: "saved".to_owned(),
                    },
                    StateOp::List,
                ]
            } else {
                vec![
                    StateOp::Put {
                        key: "saved".to_owned(),
                        value: json!({"z": [1, true, null], "a": "exact"}),
                    },
                    StateOp::Put {
                        key: "second".to_owned(),
                        value: json!(42),
                    },
                    StateOp::Get {
                        key: "saved".to_owned(),
                    },
                ]
            };
            for op in ops {
                let (responder, response) = tokio::sync::oneshot::channel();
                events
                    .send(LocalCompositionEvent::State {
                        cell_id: "named".to_owned(),
                        op,
                        responder: Box::new(StateResponder(responder)),
                    })
                    .map_err(|_| {
                        LocalCompositionSessionLoss::HostLost("state broker closed".to_owned())
                    })?;
                let result = response
                    .await
                    .map_err(|_| {
                        LocalCompositionSessionLoss::HostLost("state response lost".to_owned())
                    })?
                    .map_err(|error| {
                        LocalCompositionSessionLoss::ProtocolViolation(error.to_string())
                    })?;
                if let StateResult::Value(value) = result {
                    lock_mutex(&evidence.loaded_values)
                        .push(serde_json::to_string(&value).unwrap());
                }
            }
            if source == "named state save" {
                let (sender, response) = tokio::sync::oneshot::channel();
                events.send(LocalCompositionEvent::NestedCall {
                    call: crate::local_composition::LocalCompositionNestedCall {
                        cell_id: "named".to_owned(), runtime_call_id: "checkpoint-between-writes-and-result".to_owned(),
                        tool_name: "update_plan".to_owned(),
                        input: Some(json!({"plan": [{"step": "state checkpoint", "status": "in_progress"}]})),
                    }, responder: Box::new(StateNestedResponder(sender)),
                }).unwrap();
                response
                    .await
                    .unwrap()
                    .expect("the nested checkpoint must complete");
            }
            if source == "named state abandon" {
                return Err(LocalCompositionSessionLoss::HostLost(
                    "abandoned after writes".to_owned(),
                ));
            }
            if source == "named state terminate" {
                return Ok(LocalCompositionResponse::Terminated {
                    cell_id: "named".to_owned(),
                    items: Vec::new(),
                });
            }
            if source == "named state yield" {
                return Ok(LocalCompositionResponse::Yielded {
                    cell_id: "named".to_owned(),
                    items: Vec::new(),
                });
            }
            if evidence
                .lose_generation_on_named_state_result
                .load(Ordering::Acquire)
            {
                generation_lost.store(true, Ordering::Release);
            }
            Ok(LocalCompositionResponse::Result {
                cell_id: if source == "named state foreign-result" {
                    "foreign"
                } else {
                    "named"
                }
                .to_owned(),
                items: if source == "named state refusal" {
                    // Unlike media omission or outcome clipping, a text block
                    // larger than the canonical block bound refuses projection.
                    vec![LocalCompositionContentItem::Text(
                        "x".repeat(16 * 1024 * 1024 + 1),
                    )]
                } else {
                    vec![LocalCompositionContentItem::Text(
                        "state cell settled".to_owned(),
                    )]
                },
                error: (source == "named state fail").then(|| "ordinary cell failure".to_owned()),
            })
        }),
    }
}

struct StateNestedResponder(tokio::sync::oneshot::Sender<Result<(), String>>);

impl crate::local_composition::LocalCompositionNestedResponder for StateNestedResponder {
    fn resolve(
        self: Box<Self>,
        _value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            self.0.send(Ok(())).unwrap();
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            self.0.send(Err(reason)).unwrap();
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
}

#[tokio::test]
async fn named_state_survives_reopen_and_is_read_by_a_new_cell() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0x5a7e_1000, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("named state save")?),
                (2, "exec", serde_json::to_string("named state load")?),
                (4, "exec", serde_json::to_string("named state load")?),
            ]),
    );
    let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let builder = || {
        composition_test_builder(
            Arc::clone(&workspace),
            registry.clone(),
            &model,
            descriptor.clone(),
            authority.clone(),
            composition.clone(),
            (
                Arc::clone(&evidence),
                LocalCompositionLifecycle::RootTurnJoined,
            ),
        )
    };
    let store: Arc<dyn SessionStore> = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        Arc::clone(&store),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:named-state", builder()?).await?;
    let saved = agent.prompt("save state").await?.result().await?;
    assert!(matches!(saved.outcome(), TurnResultOutcome::Completed(_)));
    let state = saved.checkpoint().manifest().named_state().clone();
    assert_eq!(state.values().len(), 2);
    assert_eq!(
        state.get("saved")?,
        Some(json!({"z": [1, true, null], "a": "exact"}))
    );
    let joined = agent
        .prompt("load after root-turn joining")
        .await?
        .result()
        .await?;
    assert_eq!(joined.checkpoint().manifest().named_state(), &state);
    assert_eq!(
        evidence.session_creates.load(Ordering::Acquire),
        2,
        "joined roots start fresh interpreters"
    );
    compact_saved_state(&agent, &model, Arc::clone(&workspace), joined.checkpoint()).await?;
    assert_eq!(agent.checkpoint().manifest().named_state(), &state);
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:named-state", address, builder()?)
        .await?
    else {
        return Err("saved state unexpectedly needs recovery".into());
    };
    assert_eq!(reopened.checkpoint().manifest().named_state(), &state);
    let loaded = reopened
        .prompt("load saved state in a new evaluator")
        .await?
        .result()
        .await?;
    assert!(matches!(loaded.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(loaded.checkpoint().manifest().named_state(), &state);
    let notes = loaded
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) if text.text().contains("<session_state>") => {
                Some(text.text())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        notes
            .iter()
            .any(|note| note.contains(&format!("saved: {}", state.values()["saved"].len())))
    );
    assert!(
        notes.iter().all(|note| !note.contains("exact")),
        "harness notes reveal keys and sizes, not values"
    );
    let values = lock_mutex(&evidence.loaded_values).clone();
    assert_eq!(values.len(), 3);
    assert_eq!(
        values[0], values[1],
        "the new cell must read exactly the saved value, not a default"
    );
    assert_eq!(
        values[0], values[2],
        "process-reopen interpreter reset preserves values"
    );
    reopened.shutdown().await?;
    Ok(())
}

async fn compact_saved_state(
    agent: &Agent,
    model: &Arc<ArtifactModel>,
    workspace: Arc<WorkspacePolicy>,
    source: &RuntimeCheckpoint,
) -> TestResult {
    let compaction_model = RecoveryCompactionModel::new(
        model.descriptor.clone(),
        model.authority.clone(),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        compaction_model.clone(),
        source.manifest().provider_configuration().clone(),
        model.authority.clone(),
    )?;
    let builder = AgentBuilder::new(configured, neutral_composition_test_registry()?, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::default(),
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let compacted = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:named-state".to_owned(),
            agent.session_id(),
            agent.branch_id(),
            source.id(),
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("compaction refused: {error:?}"))?;
    assert!(
        matches!(compacted, AgentCompactSessionOutcome::Compacted { .. }),
        "{compacted:?}"
    );
    assert_eq!(compaction_model.starts.load(Ordering::Acquire), 1);
    Ok(())
}

#[tokio::test]
async fn named_state_failed_stopped_abandoned_or_refused_cells_publish_no_writes() -> TestResult {
    for source in [
        "named state fail",
        "named state terminate",
        "named state abandon",
        "named state refusal",
        "named state foreign-result",
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let registry = neutral_composition_test_registry()?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0x5a7e_2000, false)
                .with_scripted_tool_calls([(0, "exec", serde_json::to_string(source)?)]),
        );
        let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        let builder = composition_test_builder(
            workspace,
            registry,
            &model,
            descriptor,
            authority,
            composition,
            (
                Arc::clone(&evidence),
                LocalCompositionLifecycle::RootTurnJoined,
            ),
        )?;
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create(&format!("create:{}", source.replace(' ', "-")), builder)
            .await?;
        let result = agent
            .prompt("do not retain partial writes")
            .await?
            .result()
            .await?;
        if source == "named state refusal" {
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "a refusal fallback remains model-visible, not a turn failure"
            );
            assert_refusal_fallback(result.checkpoint());
        }
        assert_eq!(
            lock_mutex(&evidence.loaded_values).len(),
            1,
            "writes must have run before {source}"
        );
        assert!(
            agent
                .checkpoint()
                .manifest()
                .named_state()
                .values()
                .is_empty(),
            "{source}"
        );
        let _shutdown = agent.shutdown().await;
    }
    Ok(())
}

fn assert_refusal_fallback(checkpoint: &RuntimeCheckpoint) {
    let fallback = checkpoint.manifest().canonical().history().segments().iter()
        .flat_map(HistorySegment::items).flat_map(ConversationItem::blocks)
        .filter_map(|block| match block { ContentBlock::ToolResult(result) => Some(result), _ => None })
        .flat_map(ToolResultBlock::parts)
        .any(|part| matches!(part, ToolResultPart::Text(text) if text.text().contains("Warning: cell output omitted:")));
    assert!(fallback, "the model must receive the refusal fallback");
}

#[tokio::test]
async fn named_state_definite_terminal_survives_generation_loss_only_on_presented_success()
-> TestResult {
    for source in [
        "named state save",
        "named state fail",
        "named state refusal",
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let registry = neutral_composition_test_registry()?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0x5a7e_6000, false)
                .with_scripted_tool_calls([(0, "exec", serde_json::to_string(source)?)]),
        );
        let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        evidence
            .lose_generation_on_named_state_result
            .store(true, Ordering::Release);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let builder = composition_test_builder(
            workspace,
            registry,
            &model,
            descriptor,
            authority,
            composition,
            (
                Arc::clone(&evidence),
                LocalCompositionLifecycle::AgentIncarnationPersistent,
            ),
        )?;
        let agent = sessions
            .create("create:named-state-lost-generation", builder)
            .await?;
        let result = agent
            .prompt("settle a definite result despite evaluator loss")
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{source}"
        );
        let observation = result
            .checkpoint()
            .manifest()
            .interpreter_observation()
            .ok_or("missing reset")?;
        assert_eq!(observation.availability(), InterpreterAvailability::Reset);
        assert_eq!(
            observation.reset_reason(),
            Some(InterpreterResetReason::ProtocolViolation)
        );
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            1
        );
        assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 1);
        assert_eq!(
            lock_mutex(&evidence.loaded_values).len(),
            1,
            "both writes ran before the terminal"
        );
        let state = result.checkpoint().manifest().named_state();
        if source == "named state save" {
            assert_eq!(state.values().len(), 2);
            assert_eq!(
                state.get("saved")?,
                Some(json!({"z": [1, true, null], "a": "exact"}))
            );
            assert_eq!(state.get("second")?, Some(json!(42)));
            assert_state_publication(&lock_mutex(&store.accepted_commits), false)?;
        } else {
            assert!(state.values().is_empty());
            assert!(
                lock_mutex(&store.accepted_commits)
                    .iter()
                    .all(|(checkpoint, _)| checkpoint.manifest().named_state().values().is_empty())
            );
        }
        if source == "named state refusal" {
            assert_refusal_fallback(result.checkpoint());
        }
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn named_state_crash_cuts_keep_old_or_complete_new_state() -> TestResult {
    let mut publish_index = None;
    for cut in 0..3 {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let registry = neutral_composition_test_registry()?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0x5a7e_4000, false)
                .with_scripted_tool_calls([(
                    0,
                    "exec",
                    serde_json::to_string("named state save")?,
                )]),
        );
        let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        let store = Arc::new(TestCreateStore::default());
        if cut != 0 {
            let publication = publish_index.ok_or("baseline omitted state publication")?;
            store.halt_after_accepted_commit.store(
                if cut == 1 {
                    publication - 1
                } else {
                    publication
                },
                Ordering::Release,
            );
        }
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let builder = || {
            composition_test_builder(
                Arc::clone(&workspace),
                registry.clone(),
                &model,
                descriptor.clone(),
                authority.clone(),
                composition.clone(),
                (
                    Arc::clone(&evidence),
                    LocalCompositionLifecycle::RootTurnJoined,
                ),
            )
        };
        let agent = sessions
            .create("create:named-state-cut", builder()?)
            .await?;
        let _result = agent
            .prompt("publish both values or neither")
            .await?
            .result()
            .await?;
        assert_eq!(evidence.physical_cell_starts.load(Ordering::Acquire), 1);
        assert_eq!(
            lock_mutex(&evidence.loaded_values).len(),
            1,
            "the overlay ran before the cut"
        );
        let source = DurableSessionAddress::new(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
        );
        let _shutdown = agent.shutdown().await;
        let accepted = lock_mutex(&store.accepted_commits).clone();
        if cut == 0 {
            let first = accepted
                .iter()
                .position(|(checkpoint, _)| {
                    !checkpoint.manifest().named_state().values().is_empty()
                })
                .ok_or("state was never published")?;
            assert_state_publication(&accepted, false)?;
            publish_index = Some(first + 1);
            assert!(
                accepted[..first].iter().all(|(checkpoint, _)| checkpoint
                    .manifest()
                    .named_state()
                    .values()
                    .is_empty())
            );
            assert!(
                accepted[first..].iter().all(|(checkpoint, _)| checkpoint
                    .manifest()
                    .named_state()
                    .values()
                    .len()
                    == 2)
            );
        }
        let stored = lock_mutex(&store.state)
            .stored
            .clone()
            .ok_or("missing stored head")?;
        let bundle = stored.bundle();
        // Reconstruct only from persisted objects; never from the driver's overlay.
        let reopened = CheckpointObjectBundle::from_store(
            bundle.checkpoint_id(),
            bundle.checkpoint_integrity(),
            bundle.root(),
            bundle.current_root_closure()?.objects().to_vec(),
        )?
        .validated_checkpoint()?;
        assert_eq!(
            reopened.manifest().named_state().values().len(),
            if cut == 1 { 0 } else { 2 }
        );
        if cut != 1 {
            assert_eq!(
                reopened.manifest().named_state().get("saved")?,
                Some(json!({"z": [1, true, null], "a": "exact"}))
            );
            assert_eq!(
                reopened.manifest().named_state().get("second")?,
                Some(json!(42))
            );
        }
        if cut != 0 {
            store.resume_commits();
            store.halt_after_accepted_commit.store(0, Ordering::Release);
            let DurableRuntimeOpen::Opened(opened) = sessions
                .open("open:named-state-cut", source, builder()?)
                .await?
            else {
                return Err("state cut did not recover through real open".into());
            };
            assert_eq!(
                opened.checkpoint().manifest().named_state(),
                reopened.manifest().named_state()
            );
            let terminals = lock_mutex(&store.accepted_commits)
                .iter()
                .flat_map(|(_, facts)| facts)
                .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
                .count();
            assert_eq!(terminals, 1, "the recovered root has exactly one terminal");
            assert_eq!(
                evidence.physical_cell_starts.load(Ordering::Acquire),
                1,
                "open must not replay source"
            );
            opened.shutdown().await?;
        }
    }
    Ok(())
}

fn assert_state_publication(
    accepted: &[(Arc<RuntimeCheckpoint>, Vec<crate::feed::FeedFact>)],
    continuation: bool,
) -> TestResult {
    let first = accepted
        .iter()
        .position(|(checkpoint, _)| !checkpoint.manifest().named_state().values().is_empty())
        .ok_or("state never published")?;
    let operations = accepted[first].0.manifest().ledger().operations();
    let outer = operations
        .iter()
        .find(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionContinuation { .. }
            ) && continuation
                || matches!(
                    record.kind(),
                    DurableOperationKind::CompositionEvaluation { .. }
                ) && !continuation
        })
        .ok_or("outer operation missing")?;
    assert!(
        matches!(outer.state(), DurableOperationState::Completed(_)),
        "state must land with outer Completed"
    );
    let previous = accepted[first - 1]
        .0
        .manifest()
        .ledger()
        .operation(outer.operation_id())
        .ok_or("prior outer missing")?;
    assert!(!matches!(
        previous.state(),
        DurableOperationState::Completed(_) | DurableOperationState::Finalized(_)
    ));
    assert!(
        accepted[..first].iter().all(|(checkpoint, _)| checkpoint
            .manifest()
            .named_state()
            .values()
            .is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn named_state_yield_publishes_only_with_wait_completed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0x5a7e_5000, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("named state yield")?),
                (
                    1,
                    "wait",
                    serde_json::to_string(&json!({"cell_id": "named", "yield_time_ms": 1000}))?,
                ),
            ]),
    );
    let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:named-state-yield",
            composition_test_builder(
                workspace,
                registry,
                &model,
                descriptor,
                authority,
                composition,
                (
                    Arc::clone(&evidence),
                    LocalCompositionLifecycle::RootTurnJoined,
                ),
            )?,
        )
        .await?;
    let result = agent
        .prompt("retain writes across yield")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        result.checkpoint().manifest().named_state().values().len(),
        2
    );
    assert_eq!(evidence.waits.load(Ordering::Acquire), 1);
    assert_state_publication(&lock_mutex(&store.accepted_commits), true)?;
    agent.shutdown().await?;
    Ok(())
}
