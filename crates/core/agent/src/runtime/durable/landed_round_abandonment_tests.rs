//! A landed round cut before its terminal commit.
//!
//! The root is still Dispatched and the provider attempt is Completed. The
//! recorded decision alone selects the row (spec 01 §5.5): a `ToolBatch` with
//! no batch record is `NothingStarted`, and a `Terminal` (a provider-hosted
//! call the provider already ran included) finishes the turn.

use super::super::turn_recovery::{abandoned_turn_claim, abandoned_turn_recovery_fold};
use super::*;
use crate::runtime::operation_record::{RoundDecision, is_accepted_root};

#[derive(Clone, Copy)]
pub(in crate::runtime::durable) enum ExtraCall {
    None,
    /// Harness-dispatched call with the finish the driver requires.
    LocalRequested,
    LocalDuplicateRequested,
    /// A call the provider already ran: a terminal answer.
    HostedCompleted,
}

#[tokio::test]
async fn root_turn_closure_for_test_keeps_the_landed_round_accept_set() -> TestResult {
    use super::super::turn_recovery::{RootClosure, root_turn_closure_for_test};
    for extra in [
        ExtraCall::None,
        ExtraCall::LocalRequested,
        ExtraCall::HostedCompleted,
    ] {
        let cut = cut_text_turn(extra).await?;
        let closure = root_turn_closure_for_test(&cut);
        match extra {
            ExtraCall::None | ExtraCall::HostedCompleted => {
                assert!(matches!(closure?, Some(RootClosure::Complete(_))));
            }
            ExtraCall::LocalRequested => assert!(matches!(
                closure?,
                Some(RootClosure::Abandon {
                    claim: crate::AbandonedTurnClaim::NothingStarted,
                    ..
                })
            )),
            ExtraCall::LocalDuplicateRequested => assert!(closure.is_err()),
        }
    }
    for (mode, claim) in [
        (
            SessionUpdateFixtureMode::JoinedForeground,
            crate::AbandonedTurnClaim::ToolRoundCompleted,
        ),
        (
            SessionUpdateFixtureMode::Active,
            crate::AbandonedTurnClaim::RunningTool,
        ),
    ] {
        let cut = completed_tool_round_cut(mode, false, false).await?;
        assert!(matches!(
            root_turn_closure_for_test(&cut)?,
            Some(RootClosure::Abandon { claim: actual, .. }) if actual == claim
        ));
    }
    Ok(())
}

/// A completed one-round text turn, cut back to: root Dispatched, its one
/// provider attempt Completed, nothing else outstanding. Optionally the
/// recorded response also carries one tool call.
pub(in crate::runtime::durable) async fn completed_text_round_cut() -> TestResult<RuntimeCheckpoint>
{
    cut_text_turn(ExtraCall::None).await
}

pub(in crate::runtime::durable) async fn cut_text_turn(
    extra: ExtraCall,
) -> TestResult<RuntimeCheckpoint> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        0xE6C_000,
        false,
    ));
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(
            "create:landed-round",
            session_update_builder(Arc::clone(&workspace), model, registry)?,
        )
        .await?;
    let turn = agent.prompt("answer in text").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let completed = lock_mutex(&store.state)
        .stored_commits
        .values()
        .find(|stored| {
            stored
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .any(|record| {
                    matches!(record.kind(), DurableOperationKind::Provider { .. })
                        && matches!(record.state(), DurableOperationState::Completed(_))
                })
        })
        .ok_or("landed provider cut")?
        .checkpoint()
        .as_ref()
        .clone();
    agent.shutdown().await?;

    let records = completed.manifest().ledger().operations();
    let provider_id = records
        .iter()
        .rfind(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider missing")?
        .operation_id();
    let cut = records
        .into_iter()
        .map(|record| {
            let is_root = matches!(
                record.kind(),
                DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id
            );
            if !is_root && record.operation_id() != provider_id {
                return Ok(record.clone());
            }
            let end = record
                .transitions()
                .iter()
                .position(|transition| {
                    if is_root {
                        matches!(transition.state(), DurableOperationState::Dispatched)
                    } else {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    }
                })
                .ok_or("cut transition missing")?;
            let mut transitions = record.transitions()[..=end].to_vec();
            if !is_root && !matches!(extra, ExtraCall::None) {
                let DurableOperationState::Completed(evidence) = transitions[end].state() else {
                    return Err("provider evidence missing".into());
                };
                let (tag, response) =
                    crate::runtime::operation_record::test_support::recorded_response_parts(
                        evidence,
                    )?;
                let hosted = matches!(extra, ExtraCall::HostedCompleted);
                let (owner, state, name) = if hosted {
                    (
                        ToolExecutionOwner::ProviderHosted {
                            provider_kind: NamespaceId::new("hosted.web_search")?,
                        },
                        ToolCallState::Completed,
                        "web_search",
                    )
                } else {
                    (ToolExecutionOwner::Local, ToolCallState::Requested, "probe")
                };
                let call = ToolCallBlock::new(
                    CallId::try_from(uuid::Uuid::from_u128(0xE6C_1001))?,
                    ToolName::new(name)?,
                    owner,
                    state,
                    ToolCorrelation::default(),
                    "{}",
                )?;
                let mut blocks = vec![ContentBlock::ToolCall(call.clone())];
                if matches!(extra, ExtraCall::LocalDuplicateRequested) {
                    blocks.push(ContentBlock::ToolCall(call));
                }
                let mut output = vec![ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_u128(0xE6C_1002))?,
                    Actor::Assistant,
                    blocks,
                    Provenance::ModelOutput,
                )?];
                output.extend(response.output().iter().cloned());
                let local = matches!(
                    extra,
                    ExtraCall::LocalRequested | ExtraCall::LocalDuplicateRequested
                );
                let finish = if local {
                    FinishDetails::new(FinishReason::ToolCalls, None)
                } else {
                    response.finish().clone()
                };
                let response = ModelResponse::new(
                    output,
                    finish,
                    response.usage(),
                    response.response_id().map(str::to_owned),
                    response.plan_digest(),
                );
                let mut evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    crate::runtime::operation_record::test_support::recorded_response_bytes(
                        tag, response,
                    )?,
                )?;
                // The live driver records a tool batch for a single local
                // call. A duplicate call never reaches a decision: its
                // response does not validate, and the fixture keeps the
                // original record to prove recovery refuses it.
                if matches!(extra, ExtraCall::LocalRequested) {
                    evidence = runtime_component(
                        RuntimeComponentKind::Operation,
                        crate::runtime::operation_record::test_support::recorded_decision_bytes(
                            &evidence,
                            RoundDecision::ToolBatch,
                        )?,
                    )?;
                }
                transitions[end] = DurableOperationTransition::new(
                    transitions[end].checkpoint_id(),
                    DurableOperationState::Completed(evidence),
                );
            }
            Ok(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions,
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    checkpoint_with_code_cell_records(&completed, turn_id, cut)
}

fn refuses_abandonment(cut: &RuntimeCheckpoint) {
    let fold = abandoned_turn_recovery_fold(cut);
    assert!(
        fold.is_err(),
        "a round the harness would not dispatch was abandoned as {:?}",
        fold.as_ref().map(abandoned_turn_claim).ok()
    );
    assert!(!crate::runtime::checkpoint_recovers_an_abandoned_turn(cut));
}

/// The landed round asked for a local tool and nothing was prepared after it.
#[tokio::test]
async fn local_tool_request_on_a_landed_round_is_nothing_started() -> TestResult {
    let cut = cut_text_turn(ExtraCall::LocalRequested).await?;
    assert_eq!(
        abandoned_turn_claim(&abandoned_turn_recovery_fold(&cut)?),
        crate::AbandonedTurnClaim::NothingStarted
    );
    Ok(())
}

#[tokio::test]
async fn recovery_classifier_rejects_duplicate_calls_on_a_landed_round() -> TestResult {
    let cut = cut_text_turn(ExtraCall::LocalDuplicateRequested).await?;
    refuses_abandonment(&cut);
    Ok(())
}

pub(in crate::runtime::durable) async fn root_without_provider_cut(
    prepared: bool,
) -> TestResult<RuntimeCheckpoint> {
    let cut = cut_text_turn(ExtraCall::None).await?;
    let turn_id = match cut.manifest().lifecycle() {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => *turn_id,
        _ => return Err("working turn".into()),
    };
    let root = cut.manifest().ledger().operations().into_iter().find(|record| {
        matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
    }).ok_or("root")?;
    let transitions = if prepared {
        root.transitions()[..1].to_vec()
    } else {
        root.transitions().to_vec()
    };
    checkpoint_with_code_cell_records(
        &cut,
        turn_id,
        vec![DurableOperationRecord::new(
            root.operation_id(),
            root.kind().clone(),
            root.request().clone(),
            transitions,
        )?],
    )
}

#[tokio::test]
async fn recovery_dispatched_root_without_provider_is_nothing_started() -> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert_eq!(
        abandoned_turn_claim(&fold),
        crate::AbandonedTurnClaim::NothingStarted
    );
    assert_eq!(fold.root, super::super::AbandonedRoot::Dispatched);
    assert_eq!(fold.provider_operations, [] as [OperationId; 0]);
    assert_eq!(fold.effect_operations, [] as [OperationId; 0]);
    Ok(())
}

#[tokio::test]
async fn recovery_prepared_admitted_root_is_abandoned_not_requeued() -> TestResult {
    let cut = root_without_provider_cut(true).await?;
    let command = cut
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .find(|command| is_accepted_root(command))
        .ok_or("admitted root")?;
    assert_eq!(command.disposition(), DurableCommandDisposition::Accepted);
    assert!(command.turn_id().is_some());
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert_eq!(fold.root, super::super::AbandonedRoot::Prepared);
    assert_eq!(fold.command_id.as_str(), command.command_id());
    assert_eq!(
        abandoned_turn_claim(&fold),
        crate::AbandonedTurnClaim::NothingStarted
    );
    Ok(())
}

#[tokio::test]
async fn text_only_landed_round_keeps_its_recovery_rule() -> TestResult {
    refuses_abandonment(&cut_text_turn(ExtraCall::None).await?);
    Ok(())
}

/// A provider-hosted call the provider already ran did not ask this harness
/// for a tool. It must not be closed as `NothingStarted`.
#[tokio::test]
async fn hosted_tool_call_on_a_landed_round_is_not_nothing_started() -> TestResult {
    refuses_abandonment(&cut_text_turn(ExtraCall::HostedCompleted).await?);
    Ok(())
}

pub(in crate::runtime::durable) async fn completed_tool_round_cut(
    mode: SessionUpdateFixtureMode,
    reopen: bool,
    completed_debt: bool,
) -> TestResult<RuntimeCheckpoint> {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0xE8C_000,
            false,
        )
        .with_tool_first(),
    );
    let (registry, _) = session_update_registry(mode)?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:completed-tool-round",
            session_update_builder(workspace.clone(), model.clone(), registry.clone())?,
        )
        .await?;
    let turn = agent.prompt("run the tool then answer").await?;
    let turn_id = turn.id();
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let cut = lock_mutex(&store.state).stored_commits.values().find_map(|stored| {
        let checkpoint = stored.checkpoint();
        let records = checkpoint.manifest().ledger().operations();
        let providers = records.iter().filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. })).collect::<Vec<_>>();
        let effects = records.iter().filter(|record| matches!(record.kind(), DurableOperationKind::Effect { .. })).collect::<Vec<_>>();
        (providers.len() == 1 && !effects.is_empty()
            && providers.iter().chain(effects.iter()).all(|record| if completed_debt { matches!(record.state(), DurableOperationState::Completed(_)) } else { matches!(record.state(), DurableOperationState::Finalized(_)) })
            && records.iter().any(|record| matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(bound) } if *bound == turn_id)
                && matches!(record.state(), DurableOperationState::Dispatched)))
            .then(|| checkpoint.as_ref().clone())
    }).ok_or("no live completed-tool / before-next-provider cut")?;
    agent.shutdown().await?;
    if reopen {
        let topology = cut.manifest().topology();
        let source = DurableSessionAddress {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id: topology.checkpoint_id,
        };
        {
            let mut state = lock_mutex(&store.state);
            let stored = state
                .stored_commits
                .get(&cut.id())
                .ok_or("cut bundle")?
                .clone();
            let head = BranchHead::from_store(
                source.session(),
                BranchRevision::from_store(cut.manifest().topology().branch_revision),
                stored.reference(),
            );
            let session = state.loaded.as_ref().ok_or("branch")?.session();
            state.loaded = Some(crate::LoadedBranch::from_store(
                session,
                head,
                stored.clone(),
            ));
            state.stored = Some(stored);
            state.active_lease = None;
            state.released_lease = None;
        }
        let calls = model.calls.load(Ordering::Acquire);
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open(
                "open:completed-tool-round",
                source,
                session_update_builder(workspace, model.clone(), registry)?,
            )
            .await?
        else {
            return Err("completed tool round did not reopen".into());
        };
        let report = reopened.abandoned_turn().ok_or("abandonment report")?;
        assert_eq!(report.tool_round_completed(), !completed_debt);
        assert!(!report.nothing_started());
        let head = reopened.checkpoint();
        let fold = abandoned_turn_recovery_fold(&cut)?;
        assert_eq!(report.claim(), abandoned_turn_claim(&fold));
        if completed_debt {
            assert_eq!(report.claim(), crate::AbandonedTurnClaim::RunningTool);
            for record in cut.manifest().ledger().operations() {
                if let DurableOperationState::Completed(evidence) = record.state() {
                    let retained = head
                        .manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .find(|retained| retained.operation_id() == record.operation_id())
                        .ok_or("exact debt")?;
                    assert_eq!(
                        retained.state(),
                        &DurableOperationState::Finalized(evidence.clone())
                    );
                    assert!(!retained.transitions().iter().any(|transition| matches!(
                        transition.state(),
                        DurableOperationState::OutcomeUnknown(_)
                    )));
                }
            }
        }
        for operation in &fold.process_operations {
            let records = head.manifest().ledger().operations();
            let record = records
                .iter()
                .find(|record| record.operation_id() == operation.0)
                .ok_or("lost process record")?;
            assert!(matches!(
                record.state(),
                DurableOperationState::Finalized(_)
            ));
            assert!(record.transitions().iter().any(|transition| matches!(
                transition.state(),
                DurableOperationState::OutcomeUnknown(_)
            )));
            assert!(report.effects().contains(&operation.0));
        }
        let before = cut.manifest().canonical().history().segments().to_vec();
        let after = head.manifest().canonical().history().segments().to_vec();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(after.len(), before.len() + 1);
        if completed_debt {
            assert!(
                after.last().ok_or("note")?.items().iter().all(|item| {
                    item.blocks()
                        .iter()
                        .all(|block| matches!(block, ContentBlock::Text(_)))
                }),
                "recovery must not synthesize a tool result or provider output"
            );
            for record in cut.manifest().ledger().operations() {
                if matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                    assert!(report.effects().contains(&record.operation_id()));
                }
            }
        }
        let note = after
            .last()
            .ok_or("note")?
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text()),
                _ => None,
            })
            .ok_or("text")?;
        if completed_debt {
            assert_eq!(
                note,
                "this turn was abandoned after a tool finished, but its result was not added \
                 to the conversation. It may have changed the workspace and is not retried. \
                 Check the workspace before relying on what came before this point."
            );
        } else {
            assert!(
                note.contains("tool round completed") && note.contains("before the next model"),
                "{note}"
            );
        }
        for record in cut.manifest().ledger().operations() {
            if matches!(record.state(), DurableOperationState::Finalized(_)) {
                assert_eq!(
                    head.manifest()
                        .ledger()
                        .operations()
                        .into_iter()
                        .find(|new| new.operation_id() == record.operation_id()),
                    Some(record)
                );
            }
        }
        assert_eq!(
            model.calls.load(Ordering::Acquire),
            calls,
            "reopen dispatched inference"
        );
        assert!(matches!(
            reopened
                .prompt("follow-up after the completed tool round")
                .await?
                .result()
                .await?
                .outcome(),
            TurnResultOutcome::Completed(_)
        ));
        reopened.shutdown().await?;
    }
    Ok(cut)
}

#[tokio::test]
async fn completed_tool_round_boundary_is_abandoned_not_nothing_started() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, true, false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert_eq!(
        abandoned_turn_claim(&fold),
        crate::AbandonedTurnClaim::ToolRoundCompleted
    );
    assert!(crate::runtime::checkpoint_recovers_an_abandoned_turn(&cut));
    Ok(())
}

#[tokio::test]
async fn completed_tool_round_with_live_process_preserves_unknown_outcome() -> TestResult {
    let cut = completed_tool_round_cut(SessionUpdateFixtureMode::Active, true, false).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert!(fold.tool_round_completed);
    assert_ne!(fold.process_operations, [] as [OperationId; 0]);
    assert_eq!(
        abandoned_turn_claim(&fold),
        crate::AbandonedTurnClaim::RunningTool
    );
    Ok(())
}

/// A `Terminal` decision on a finalized P is typed integrity: P finalizes
/// only in R's terminal commit (spec 01 §5.5).
#[tokio::test]
async fn completed_tool_round_boundary_refuses_final_answers_and_corrupt_evidence() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
    for corrupt in [false, true] {
        let mut changed = Vec::new();
        for record in cut.manifest().ledger().operations() {
            if !matches!(record.kind(), DurableOperationKind::Provider { .. }) {
                changed.push(record.clone());
                continue;
            }
            let DurableOperationState::Finalized(evidence) = record.state() else {
                return Err("provider not finalized".into());
            };
            let replacement = runtime_component(
                RuntimeComponentKind::Operation,
                if corrupt {
                    b"corrupt provider evidence".to_vec()
                } else {
                    crate::runtime::operation_record::test_support::recorded_decision_bytes(
                        evidence,
                        RoundDecision::Terminal,
                    )?
                },
            )?;
            let transitions = record
                .transitions()
                .iter()
                .map(|transition| {
                    DurableOperationTransition::new(
                        transition.checkpoint_id(),
                        match transition.state() {
                            DurableOperationState::Completed(_) => {
                                DurableOperationState::Completed(replacement.clone())
                            }
                            DurableOperationState::Finalized(_) => {
                                DurableOperationState::Finalized(replacement.clone())
                            }
                            state => state.clone(),
                        },
                    )
                })
                .collect();
            changed.push(DurableOperationRecord::new(
                record.operation_id(),
                record.kind().clone(),
                record.request().clone(),
                transitions,
            )?);
        }
        let turn_id = match cut.manifest().lifecycle() {
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn { turn_id, .. },
                ..
            } => *turn_id,
            _ => return Err("working turn".into()),
        };
        refuses_abandonment(&checkpoint_with_code_cell_records(&cut, turn_id, changed)?);
    }
    Ok(())
}

#[tokio::test]
async fn completed_tool_round_boundary_refuses_unclosed_neighbours() -> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
    let records = cut
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let provider = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let turn_id = operation_turn_id(provider).ok_or("turn")?;
    // A completed effect still owing finalization is not proof that the tool
    // round finished. The finalized `ToolBatch` alone is: P finalizes only
    // with its exchange, so recovery never reads the effect records for it.
    for remove in [false, true] {
        let mut changed = Vec::new();
        for record in &records {
            if matches!(record.kind(), DurableOperationKind::Effect { .. }) {
                if remove {
                    continue;
                }
                let end = record
                    .transitions()
                    .iter()
                    .position(|transition| {
                        matches!(transition.state(), DurableOperationState::Completed(_))
                    })
                    .ok_or("completed effect")?;
                changed.push(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    record.transitions()[..=end].to_vec(),
                )?);
            } else {
                changed.push(record.clone());
            }
        }
        let changed = checkpoint_with_code_cell_records(&cut, turn_id, changed)?;
        if remove {
            assert_eq!(
                abandoned_turn_claim(&abandoned_turn_recovery_fold(&changed)?),
                crate::AbandonedTurnClaim::ToolRoundCompleted
            );
        } else {
            refuses_abandonment(&changed);
        }
    }
    for kind in [
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        provider.kind().clone(),
        DurableOperationKind::ServerPauseContinuation {
            turn_id,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE8C_999))?,
        },
    ] {
        let mut changed = records.clone();
        changed.push(DurableOperationRecord::new(
            records
                .iter()
                .map(DurableOperationRecord::operation_id)
                .max()
                .ok_or("id")?
                + 1,
            kind.clone(),
            provider.request().clone(),
            vec![DurableOperationTransition::new(
                cut.id(),
                if matches!(kind, DurableOperationKind::ServerPauseContinuation { .. }) {
                    DurableOperationState::ContinuationRequired(runtime_component(
                        RuntimeComponentKind::Operation,
                        b"pending continuation".to_vec(),
                    )?)
                } else {
                    DurableOperationState::Prepared
                },
            )],
        )?);
        let neighbour = checkpoint_with_code_cell_records(&cut, turn_id, changed)?;
        // A prepared next provider is still an ordinary mid-model kill, not
        // the completed-tool boundary.
        if matches!(
            neighbour
                .manifest()
                .ledger()
                .operations()
                .last()
                .ok_or("last")?
                .kind(),
            DurableOperationKind::Provider { .. }
        ) {
            let fold = abandoned_turn_recovery_fold(&neighbour)?;
            assert!(!fold.tool_round_completed);
            assert_eq!(
                abandoned_turn_claim(&fold),
                crate::AbandonedTurnClaim::ModelRequest
            );
        } else {
            refuses_abandonment(&neighbour);
        }
    }
    Ok(())
}

async fn finalization_usage_parity(report_usage: bool, candidate_calls: usize) -> TestResult {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        candidate_calls,
        0xE7C_000,
        false,
    );
    let model = Arc::new(if report_usage {
        model.with_report_usage()
    } else {
        model
    });
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:finalization-predicate",
            session_update_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
                registry,
            )?,
        )
        .await?;
    let genesis = agent.checkpoint();
    let command = CommandId::new("start:finalization-predicate")?;
    let result = agent
        .start(command.clone(), "complete root")
        .await?
        .into_result()
        .ok_or("result")?
        .result()
        .await?;
    let checkpoints = lock_mutex(&store.state)
        .stored_commits
        .values()
        .map(|stored| Arc::clone(stored.checkpoint()))
        .collect::<Vec<_>>();
    let folds = checkpoints
        .iter()
        .filter_map(|checkpoint| {
            super::super::turn_recovery::local_terminal_recovery_fold(checkpoint).ok()
        })
        .collect::<Vec<_>>();
    for fold in &folds {
        assert_eq!(
            fold.family_artifact.as_ref(),
            result.checkpoint().manifest().family_artifact()
        );
    }
    let synthesized = folds
        .iter()
        .filter_map(|fold| fold.synthesized.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        synthesized.len(),
        1,
        "no root-Dispatched final-response cut"
    );
    let (operation, evidence) = synthesized[0];
    let terminal =
        decode_pending_terminal_record(evidence.bytes()).ok_or("synthesized terminal missing")?;
    assert_eq!(terminal.usage, result.usage());
    let DurableTerminalOutcome::Completed(response) = terminal.outcome else {
        return Err("not completed".into());
    };
    assert_eq!(response, *result.response().ok_or("live response")?);
    assert_eq!(response.usage().is_some(), report_usage);
    let live_evidence = checkpoints
        .iter()
        .flat_map(|checkpoint| checkpoint.manifest().ledger().operations())
        .filter(|record| record.operation_id() == operation.0)
        .find_map(live_pending_terminal)
        .ok_or("live pending terminal missing")?;
    assert_eq!(
        evidence.bytes(),
        live_evidence.as_slice(),
        "recovery differs from the live driver's terminal"
    );
    let pending = checkpoints
        .iter()
        .find(|checkpoint| {
            super::super::turn_recovery::local_terminal_recovery_fold(checkpoint).is_ok()
        })
        .ok_or("no pending terminal checkpoint")?;
    assert!(crate::runtime::checkpoint_finalizes_root_turn(
        pending, &command
    )?);
    assert!(!crate::runtime::checkpoint_finalizes_root_turn(
        pending,
        &CommandId::new("start:other")?
    )?);
    assert!(!crate::runtime::checkpoint_finalizes_root_turn(
        &genesis, &command
    )?);
    assert!(!crate::runtime::checkpoint_finalizes_root_turn(
        result.checkpoint(),
        &command
    )?);
    let unfinished = checkpoints
        .iter()
        .find(|checkpoint| {
            matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
                    ..
                }
            )
        })
        .ok_or("no provider-working checkpoint")?;
    assert!(!crate::runtime::checkpoint_finalizes_root_turn(
        unfinished, &command
    )?);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn finalization_adoption_names_only_the_exact_locally_closable_root() -> TestResult {
    finalization_usage_parity(false, 0).await
}

#[tokio::test]
async fn finalization_synthesized_usage_matches_live_driver_with_usage() -> TestResult {
    finalization_usage_parity(true, 0).await
}

#[tokio::test]
async fn finalization_family_artifact_uses_the_live_terminal_evidence() -> TestResult {
    finalization_usage_parity(true, 1).await
}

#[tokio::test]
async fn finalization_refuses_tools_continuations_other_commands_and_corrupt_evidence() -> TestResult
{
    let command_of = |cut: &RuntimeCheckpoint| -> TestResult<CommandId> {
        let command = cut
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .find(|command| is_accepted_root(command))
            .ok_or("root")?;
        Ok(CommandId::new(command.command_id())?)
    };
    let cut = cut_text_turn(ExtraCall::LocalRequested).await?;
    assert!(!crate::runtime::checkpoint_finalizes_root_turn(
        &cut,
        &command_of(&cut)?
    )?);
    let cut = cut_text_turn(ExtraCall::None).await?;
    let command = command_of(&cut)?;
    let records = cut
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let provider = records
        .iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let turn_id = operation_turn_id(provider).ok_or("turn")?;
    // Only the recorded decision selects the finish row; the response's
    // finish reason is never read.
    for decision in [
        RoundDecision::ToolBatch,
        RoundDecision::Continue,
        RoundDecision::Terminal,
    ] {
        let mut changed = records.clone();
        let record = changed
            .iter_mut()
            .find(|record| record.operation_id() == provider.operation_id())
            .ok_or("provider")?;
        let mut transitions = record.transitions().to_vec();
        let last = transitions.last_mut().ok_or("transition")?;
        let DurableOperationState::Completed(evidence) = last.state() else {
            return Err("evidence".into());
        };
        let (tag, response) =
            crate::runtime::operation_record::test_support::recorded_response_parts(evidence)?;
        let response = ModelResponse::new(
            response.output().to_vec(),
            FinishDetails::new(FinishReason::Length, None),
            response.usage(),
            response.response_id().map(str::to_owned),
            response.plan_digest(),
        );
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            crate::runtime::operation_record::test_support::recorded_response_bytes(tag, response)?,
        )?;
        let terminal = decision == RoundDecision::Terminal;
        *last = DurableOperationTransition::new(
            last.checkpoint_id(),
            DurableOperationState::Completed(runtime_component(
                RuntimeComponentKind::Operation,
                crate::runtime::operation_record::test_support::recorded_decision_bytes(
                    &evidence, decision,
                )?,
            )?),
        );
        *record = DurableOperationRecord::new(
            record.operation_id(),
            record.kind().clone(),
            record.request().clone(),
            transitions,
        )?;
        let changed = checkpoint_with_code_cell_records(&cut, turn_id, changed)?;
        assert_eq!(
            crate::runtime::checkpoint_finalizes_root_turn(&changed, &command)?,
            terminal
        );
        if terminal {
            assert!(matches!(
                super::super::turn_recovery::root_turn_closure_for_test(&changed)?,
                Some(super::super::turn_recovery::RootClosure::Complete(_))
            ));
        }
    }
    for kind in [
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        DurableOperationKind::ServerPauseContinuation {
            turn_id,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(0xE7C_999))?,
        },
    ] {
        let root = records
            .iter()
            .find(|record| matches!(record.kind(), DurableOperationKind::Command { .. }))
            .ok_or("root")?;
        let evidence =
            runtime_component(RuntimeComponentKind::Operation, b"closed sibling".to_vec())?;
        let mut changed = records.clone();
        let transitions = if matches!(kind, DurableOperationKind::Command { .. }) {
            vec![DurableOperationTransition::new(
                cut.id(),
                DurableOperationState::Finalized(evidence),
            )]
        } else {
            vec![
                DurableOperationTransition::new(
                    root.transitions()[0].checkpoint_id(),
                    DurableOperationState::ContinuationRequired(evidence.clone()),
                ),
                DurableOperationTransition::new(
                    cut.id(),
                    DurableOperationState::Finalized(evidence),
                ),
            ]
        };
        changed.push(DurableOperationRecord::new(
            provider.operation_id() + 1,
            kind,
            root.request().clone(),
            transitions,
        )?);
        let changed = checkpoint_with_code_cell_records(&cut, turn_id, changed)?;
        assert!(!crate::runtime::checkpoint_finalizes_root_turn(
            &changed, &command
        )?);
    }
    let mut changed = records.clone();
    let record = changed
        .iter_mut()
        .find(|record| record.operation_id() == provider.operation_id())
        .ok_or("provider")?;
    let mut transitions = record.transitions().to_vec();
    let last = transitions.last_mut().ok_or("transition")?;
    *last = DurableOperationTransition::new(
        last.checkpoint_id(),
        DurableOperationState::Completed(runtime_component(
            RuntimeComponentKind::Operation,
            b"corrupt".to_vec(),
        )?),
    );
    *record = DurableOperationRecord::new(
        record.operation_id(),
        record.kind().clone(),
        record.request().clone(),
        transitions,
    )?;
    let changed = checkpoint_with_code_cell_records(&cut, turn_id, changed)?;
    let error = crate::runtime::checkpoint_finalizes_root_turn(&changed, &command).unwrap_err();
    assert_ne!(error.kind(), AgentErrorKind::OutcomeUnknown);
    Ok(())
}

#[tokio::test]
async fn recovery_completed_effect_debt_keeps_exact_evidence_without_a_result_or_dispatch()
-> TestResult {
    let cut =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, true, true).await?;
    let fold = abandoned_turn_recovery_fold(&cut)?;
    assert_eq!(
        abandoned_turn_claim(&fold),
        crate::AbandonedTurnClaim::RunningTool
    );
    assert!(!fold.tool_round_completed);
    Ok(())
}
