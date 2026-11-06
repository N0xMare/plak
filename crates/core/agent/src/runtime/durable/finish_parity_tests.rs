//! The finish row is a literal replay (spec 01 §5.5): for the same recorded
//! decision, the consequence commit a newly opened owner publishes equals the
//! one the live driver published, modulo the new checkpoint's identity.

#![allow(
    clippy::too_many_lines,
    reason = "Live-versus-recovered finish parity compares canonical history, usage, and terminal projection for the same turn"
)]

use super::super::turn_recovery::{RootClosure, root_turn_closure_for_test};
use super::*;
use crate::runtime::terminal::decode_terminal_result_record;

/// Runs one turn to completion live, then rewinds the same store to the
/// turn's decision commit and reopens it, and compares the two consequence
/// commits.
async fn assert_finish_parity(
    label: &str,
    refused: bool,
    make: impl Fn() -> TestResult<AgentBuilder>,
) -> TestResult<Arc<TurnResult>> {
    Ok(finish_parity(label, refused, make).await?.0)
}

/// [`assert_finish_parity`], also returning the occupancy anchor the live
/// consequence commit recorded.
async fn finish_parity(
    label: &str,
    refused: bool,
    make: impl Fn() -> TestResult<AgentBuilder>,
) -> TestResult<(Arc<TurnResult>, Option<plakous_model::ContextAnchor>)> {
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create(&format!("create:{label}"), make()?).await?;
    let command = CommandId::new(format!("root:{label}"))?;
    if refused {
        store.fail_quiescent_terminal();
    }
    let result = agent
        .start(command.clone(), "finish parity")
        .await?
        .into_turn()
        .ok_or("live turn was not admitted")?
        .result()
        .await?;
    let (live, live_facts) = if refused {
        assert!(matches!(
            result.outcome(),
            TurnResultOutcome::RecoveryRequired(_)
        ));
        lock_mutex(&store.refused_terminal)
            .take()
            .ok_or("refused consequence")?
    } else {
        assert!(matches!(
            result.outcome(),
            TurnResultOutcome::Completed(_) | TurnResultOutcome::Failed(_)
        ));
        lock_mutex(&store.accepted_commits)
            .last()
            .ok_or("live consequence")?
            .clone()
    };
    let decisions = lock_mutex(&store.state)
        .stored_commits
        .values()
        .map(|stored| Arc::clone(stored.checkpoint()))
        .filter(|checkpoint| {
            matches!(
                root_turn_closure_for_test(checkpoint),
                Ok(Some(RootClosure::Complete(_)))
            )
        })
        .collect::<Vec<_>>();
    // A multi-round turn leaves an earlier closable head per round; the
    // consequence commit follows the newest one.
    let cut = decisions
        .iter()
        .find(|decision| live.manifest().topology().parent_checkpoint_id == Some(decision.id()))
        .map(Arc::clone)
        .ok_or_else(|| {
            format!("{label}: the live consequence commit follows no decision commit")
        })?;
    if refused {
        assert_eq!(
            agent.checkpoint().id(),
            cut.id(),
            "{label}: refusal keeps decision"
        );
        agent.abort_driver_for_test();
        tokio::task::yield_now().await;
    } else {
        agent.shutdown().await?;
    }
    let source = rewind_to(&store, &cut)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(&format!("open:{label}"), source, make()?)
        .await?
    else {
        return Err(format!("{label}: the decision commit did not reopen").into());
    };
    assert!(
        reopened.abandoned_turn().is_none(),
        "{label}: no abandonment"
    );
    let recovered = reopened.checkpoint();
    assert_eq!(
        recovered.manifest().topology().parent_checkpoint_id,
        Some(cut.id()),
        "{label}: the recovered commit follows the decision commit"
    );
    assert_consequence_parity(label, &live, &recovered, &result)?;
    let live_anchor = live.manifest().canonical().context().occupancy_anchor();
    // The reopened owner's measure is the one its next turn starts from.
    let next_turn_status = reopened
        .session_endpoint()
        .context_status(
            reopened.session_id(),
            reopened.branch_id(),
            recovered.id(),
            None,
        )
        .await
        .map_err(|_| format!("{label}: reopened context status"))?;
    assert_eq!(
        next_turn_status.context_anchor(),
        live_anchor,
        "{label}: the next turn starts from the live occupancy anchor"
    );
    let recovered_facts = lock_mutex(&store.accepted_commits)
        .last()
        .ok_or("recovered consequence")?
        .1
        .clone();
    let terminals = |facts: Vec<crate::feed::FeedFact>| {
        facts
            .into_iter()
            .filter(|fact| matches!(fact, crate::feed::FeedFact::TurnTerminal { .. }))
            .collect::<Vec<_>>()
    };
    let live_terminals = terminals(live_facts);
    assert_eq!(live_terminals.len(), 1, "{label}: one live terminal fact");
    assert_eq!(
        live_terminals,
        terminals(recovered_facts),
        "{label}: terminal fact"
    );
    let replayed = reopened
        .start(command, "finish parity")
        .await?
        .into_result()
        .ok_or("duplicate turn lacks its result")?
        .result()
        .await?;
    assert_eq!(
        replayed.checkpoint_id(),
        recovered.id(),
        "{label}: duplicate replays"
    );
    assert_eq!(replayed.usage(), result.usage(), "{label}: replayed usage");
    if !refused {
        assert_eq!(
            replayed.context_overflow_remedy(),
            result.context_overflow_remedy(),
            "{label}: remedy"
        );
    }
    reopened.shutdown().await?;
    Ok((replayed, live_anchor))
}

/// Rewinds the store's branch to `cut`, with no lease held, as a lost owner
/// leaves it.
fn rewind_to(
    store: &TestCreateStore,
    cut: &RuntimeCheckpoint,
) -> TestResult<DurableSessionAddress> {
    let topology = cut.manifest().topology();
    let source = DurableSessionAddress {
        session_id: topology.session_id,
        branch_id: topology.branch_id,
        checkpoint_id: topology.checkpoint_id,
    };
    let mut state = lock_mutex(&store.state);
    let stored = state
        .stored_commits
        .get(&cut.id())
        .ok_or("cut bundle")?
        .clone();
    let head = BranchHead::from_store(
        source.session(),
        BranchRevision::from_store(topology.branch_revision),
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
    Ok(source)
}

fn assert_consequence_parity(
    label: &str,
    live: &RuntimeCheckpoint,
    recovered: &RuntimeCheckpoint,
    result: &TurnResult,
) -> TestResult {
    let (live, recovered) = (live.manifest(), recovered.manifest());
    assert!(matches!(live.lifecycle(), RuntimeLifecycle::Quiescent));
    assert!(
        matches!(recovered.lifecycle(), RuntimeLifecycle::Quiescent),
        "{label}"
    );
    assert_eq!(
        live.canonical().history().identity(),
        recovered.canonical().history().identity(),
        "{label}: history"
    );
    assert_eq!(
        live.family_artifact(),
        recovered.family_artifact(),
        "{label}: family artifact"
    );
    assert_eq!(
        live.canonical().context().request_anchor(),
        recovered.canonical().context().request_anchor(),
        "{label}: request anchor"
    );
    assert_eq!(
        live.canonical().context().occupancy_anchor(),
        recovered.canonical().context().occupancy_anchor(),
        "{label}: occupancy anchor"
    );
    let (live_records, recovered_records) =
        (live.ledger().operations(), recovered.ledger().operations());
    assert_eq!(
        live_records.len(),
        recovered_records.len(),
        "{label}: ledger"
    );
    let mut terminals = 0;
    for (live, recovered) in live_records.iter().zip(&recovered_records) {
        assert_eq!(live.operation_id(), recovered.operation_id(), "{label}");
        assert_eq!(live.kind(), recovered.kind(), "{label}");
        assert_eq!(
            live.request().bytes(),
            recovered.request().bytes(),
            "{label}"
        );
        let (
            DurableOperationState::Finalized(live_evidence),
            DurableOperationState::Finalized(recovered_evidence),
        ) = (live.state(), recovered.state())
        else {
            return Err(format!(
                "{label}: operation {} is not finalized in both",
                live.operation_id()
            )
            .into());
        };
        match (
            decode_terminal_result_record(live_evidence.bytes()),
            decode_terminal_result_record(recovered_evidence.bytes()),
        ) {
            (Some(live_terminal), Some(mut recovered_terminal)) => {
                terminals += 1;
                assert_eq!(recovered_terminal.usage, result.usage(), "{label}: usage");
                recovered_terminal.checkpoint_id = live_terminal.checkpoint_id;
                assert_eq!(live_terminal, recovered_terminal, "{label}: terminal");
            }
            (None, None) => assert_eq!(
                live_evidence.bytes(),
                recovered_evidence.bytes(),
                "{label}: operation {} evidence",
                live.operation_id()
            ),
            _ => return Err(format!("{label}: one side lacks the terminal record").into()),
        }
    }
    assert_eq!(terminals, 1, "{label}: one root terminal");
    Ok(())
}

fn text_final_builder(report_usage: bool, candidate_calls: usize) -> TestResult<AgentBuilder> {
    let directory = std::env::temp_dir();
    let descriptor = durable_test_descriptor("test.route")?;
    let model = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        candidate_calls,
        0xF1_5000,
        false,
    );
    let model = Arc::new(if report_usage {
        model.with_report_usage()
    } else {
        model
    });
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    session_update_builder(
        Arc::new(WorkspacePolicy::host(&directory, ".")?),
        model,
        registry,
    )
}

#[tokio::test]
async fn the_recovered_finish_commit_equals_the_live_consequence_commit() -> TestResult {
    assert_finish_parity("finish-parity:plain", false, || {
        text_final_builder(false, 0)
    })
    .await?;
    assert_finish_parity("finish-parity:usage", false, || text_final_builder(true, 0)).await?;
    assert_finish_parity("finish-parity:artifact", false, || {
        text_final_builder(true, 1)
    })
    .await?;
    Ok(())
}

/// A usage-reporting final anchors the live terminal, so the turn after its
/// recovered finish starts anchored rather than from a whole-request estimate.
#[tokio::test]
async fn the_turn_after_a_recovered_finish_starts_from_the_live_occupancy_anchor() -> TestResult {
    let (_, anchor) = finish_parity("finish-parity:next-turn-anchor", false, || {
        text_final_builder(true, 0)
    })
    .await?;
    assert!(anchor.is_some(), "the live terminal records an anchor");
    Ok(())
}

/// Code Cell usage parity: a turn that ran a cell and then answered finishes
/// on reopen with the usage the live counters reported.
#[tokio::test]
async fn a_recovered_code_cell_turn_reports_the_live_usage() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().to_path_buf();
    assert_finish_parity("finish-parity:code-cell", false, || {
        code_cell_builder(&path)
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_recovered_context_limit_finish_preserves_the_withheld_prompt_remedy_and_note()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0xF1_6000,
            false,
        )
        .with_provider_overflows([0])
        .with_report_usage(),
    );
    // A foreign-bound refusal ends the attempt instead of spending the overflow fold.
    model.foreign_overflow.store(true, Ordering::Release);
    let result = assert_finish_parity("finish-parity:context-limit", false, || {
        Ok(durable_artifact_test_builder(
            workspace.clone(),
            model.clone(),
        ))
    })
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(error)
        if error.model_kind() == Some(ModelErrorKind::ContextLimit)));
    assert_eq!(
        result.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::ResendShorter)
    );
    let items = result.conversation().items().to_vec();
    assert!(items.iter().all(|item| text_of(item) != "finish parity"));
    let note = items.last().ok_or("withheld prompt note")?;
    assert_eq!(note.actor(), Actor::User);
    assert_eq!(note.provenance(), Provenance::Instruction);
    assert!(text_of(note).contains("user message of 13 bytes was withheld"));
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "recovery never redispatches"
    );
    Ok(())
}

/// A turn whose first round published a family artifact and reported usage,
/// then failed for context capacity on its next round, finishes on reopen
/// with the artifact and request anchor the live terminal committed.
#[tokio::test]
async fn a_recovered_capacity_failure_after_a_tool_round_equals_the_live_terminal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            0xF1_6100,
            false,
        )
        .with_tool_first()
        .with_provider_overflows([1])
        .with_report_usage(),
    );
    model.foreign_overflow.store(true, Ordering::Release);
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let result = assert_finish_parity("finish-parity:capacity-artifact", false, || {
        session_update_builder(workspace.clone(), model.clone(), registry.clone())
    })
    .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Failed(error)
            if error.model_kind() == Some(ModelErrorKind::ContextLimit)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "recovery never redispatches"
    );
    Ok(())
}

#[tokio::test]
async fn a_recovered_dispatch_rejected_finish_equals_the_live_consequence_commit() -> TestResult {
    dispatch_rejected_finish_parity(false).await
}

#[tokio::test]
async fn a_recovered_refused_consequence_equals_the_live_proposal() -> TestResult {
    assert_finish_parity("finish-parity:refused-answer", true, || {
        text_final_builder(true, 1)
    })
    .await?;
    dispatch_rejected_finish_parity(true).await
}

async fn dispatch_rejected_finish_parity(refused: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 0xF1_7000)?;
    model.reject_round_dispatch.store(true, Ordering::Release);
    let result = assert_finish_parity("finish-parity:dispatch-rejected", refused, || {
        Ok(durable_artifact_test_builder(
            workspace.clone(),
            model.clone(),
        ))
    })
    .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(error)
        if error.model_kind() == Some(ModelErrorKind::ResourceLimit)));
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "recovery never redispatches"
    );
    assert_eq!(result.context_overflow_remedy(), None);
    let items = result.conversation().items().to_vec();
    let note = items.last().ok_or("failure note")?;
    assert!(
        text_of(note).starts_with("the turn handling the request above failed (ResourceLimit)")
    );
    Ok(())
}

fn code_cell_builder(directory: &std::path::Path) -> TestResult<AgentBuilder> {
    let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
    let registry = neutral_composition_test_registry()?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_f150, false)
            .with_report_usage()
            .with_scripted_tool_calls([(0, "exec", serde_json::to_string("load bounded state")?)]),
    );
    let composition_descriptor =
        neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition_descriptor,
        (
            Arc::new(FakeCompositionEvidence::default()),
            LocalCompositionLifecycle::AgentIncarnationPersistent,
        ),
    )
}

/// What else is outstanding when a turn's outcome-unknown `FailTurn` is
/// recorded.
#[derive(Clone, Copy, Debug)]
enum RecoveryRequiredCut {
    /// Only the unobserved request.
    Request,
    /// A steer queued while the request was in flight, never applied.
    QueuedSteer,
}

/// A recorded `FailTurn` whose failure is outcome-unknown finishes as
/// `RecoveryRequired`, and live that consequence leaves the turn's unresolved
/// work to the `Finalized RecoveryRequired` row. Reopening the decision commit
/// runs both rows in one closure, so it closes the turn as reopening the live
/// consequence commit does: the same history, note and abandonment report.
/// Returns that report, the closed history's last text, and the two cuts.
async fn recovery_required_finish_parity(
    cut: RecoveryRequiredCut,
) -> TestResult<RecoveryRequiredCuts> {
    let label = format!("finish-parity:recovery-required:{cut:?}");
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 0xF1_8000)?;
    let release = Arc::new(Notify::new());
    if matches!(cut, RecoveryRequiredCut::QueuedSteer) {
        *lock_mutex(&model.held_call_release) = Some(release.clone());
        model.held_call.store(0, Ordering::Release);
    }
    model.unknown_outcome_from_call.store(0, Ordering::Release);
    let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let make = || session_update_builder(workspace.clone(), model.clone(), registry.clone());
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create(&format!("create:{label}"), make()?).await?;
    let turn = agent
        .start(CommandId::new(format!("root:{label}"))?, "finish parity")
        .await?
        .into_turn()
        .ok_or("live turn was not admitted")?;
    if matches!(cut, RecoveryRequiredCut::QueuedSteer) {
        // A liveness bound only: generous, so load never trips it.
        tokio::time::timeout(Duration::from_secs(30), model.held_call_started.notified()).await?;
        turn.control().steer("queued instruction").await?;
        release.notify_one();
    }
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
        "{cut:?}: {:?}",
        result.outcome()
    );
    let _ = agent.shutdown().await;
    let commits = lock_mutex(&store.state)
        .stored_commits
        .values()
        .map(|stored| Arc::clone(stored.checkpoint()))
        .collect::<Vec<_>>();
    let decision = commits
        .iter()
        .find(|checkpoint| {
            matches!(
                root_turn_closure_for_test(checkpoint),
                Ok(Some(RootClosure::Complete(_)))
            )
        })
        .map(Arc::clone)
        .ok_or("the turn's decision commit")?;
    let terminal = commits
        .iter()
        .find(|checkpoint| {
            checkpoint.manifest().topology().parent_checkpoint_id == Some(decision.id())
        })
        .map(Arc::clone)
        .ok_or("the live consequence commit")?;
    assert!(
        matches!(
            root_turn_closure_for_test(&terminal),
            Ok(Some(RootClosure::Abandon { .. }))
        ),
        "{cut:?}: the live consequence leaves the turn to the RecoveryRequired row"
    );
    let mut closures = Vec::new();
    for (side, at) in [("decision", &decision), ("terminal", &terminal)] {
        let source = rewind_to(&store, at)?;
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open(&format!("open:{label}:{side}"), source, make()?)
            .await?
        else {
            return Err(format!("{cut:?} {side}: the cut did not reopen").into());
        };
        let report = reopened
            .abandoned_turn()
            .cloned()
            .ok_or_else(|| format!("{cut:?} {side}: the closure reported no abandonment"))?;
        let recovered = reopened.checkpoint();
        assert!(
            matches!(
                recovered.manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ),
            "{cut:?} {side}"
        );
        let note = recovered
            .manifest()
            .canonical()
            .projection()
            .items()
            .into_iter()
            .last()
            .map(text_of)
            .unwrap_or_default();
        closures.push((
            report,
            recovered.manifest().canonical().history().identity(),
            note,
        ));
        reopened.shutdown().await?;
    }
    assert_eq!(
        closures[0].0.cause(),
        crate::runtime::AbandonmentCause::Interrupted,
        "{cut:?}"
    );
    assert_eq!(
        closures[0], closures[1],
        "{cut:?}: one closure from either cut"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "{cut:?}: recovery never redispatches"
    );
    let (report, _, note) = closures.swap_remove(0);
    Ok(RecoveryRequiredCuts {
        report,
        note,
        decision,
        terminal,
    })
}

/// What [`recovery_required_finish_parity`] closed, and the two cuts it
/// reopened: the decision commit and the live consequence commit.
struct RecoveryRequiredCuts {
    report: crate::runtime::AbandonedTurnReport,
    note: String,
    decision: Arc<RuntimeCheckpoint>,
    terminal: Arc<RuntimeCheckpoint>,
}

#[tokio::test]
async fn a_recovered_recovery_required_finish_closes_like_the_reopened_live_terminal() -> TestResult
{
    let closed = recovery_required_finish_parity(RecoveryRequiredCut::Request).await?;
    assert_eq!(
        closed.report.claim(),
        crate::runtime::AbandonedTurnClaim::ModelRequest
    );
    Ok(())
}

/// A process session the turn left running (still `Dispatched`) at the
/// recorded failure makes the strongest claim a running tool, and both cuts
/// close it alike. The live driver closes the sessions it can before its
/// decision commit, so the open session is set into both recorded cuts.
#[tokio::test]
async fn a_recovered_recovery_required_finish_with_an_open_process_claims_a_running_tool()
-> TestResult {
    use super::super::turn_recovery::closure_for_test;
    use super::super::turn_recovery::review_tests::checkpoint_with_review_state;
    let RecoveryRequiredCuts {
        decision, terminal, ..
    } = recovery_required_finish_parity(RecoveryRequiredCut::Request).await?;
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = decision.manifest().lifecycle()
    else {
        return Err("the decision cut is a working turn".into());
    };
    let open_process = |cut: &RuntimeCheckpoint| -> TestResult<RuntimeCheckpoint> {
        let mut records = cut
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let next = records
            .iter()
            .map(DurableOperationRecord::operation_id)
            .max()
            .unwrap_or_default()
            + 1;
        records.push(DurableOperationRecord::new(
            next,
            DurableOperationKind::ProcessSession {
                turn_id: *turn_id,
                session_id: crate::manifest::ProcessSessionId::new([0x94; 16])?,
                owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(1)?,
            },
            runtime_component(
                RuntimeComponentKind::Operation,
                b"open process request".to_vec(),
            )?,
            vec![
                DurableOperationTransition::new(
                    CheckpointId::try_from(uuid::Uuid::from_u128(0xF1_8A00))?,
                    DurableOperationState::Prepared,
                ),
                DurableOperationTransition::new(
                    CheckpointId::try_from(uuid::Uuid::from_u128(0xF1_8A01))?,
                    DurableOperationState::Dispatched,
                ),
            ],
        )?);
        checkpoint_with_review_state(
            cut,
            records,
            cut.manifest().canonical().history().clone(),
            false,
        )
    };
    let (finished, finished_report) = closure_for_test(&open_process(&decision)?)?;
    let (reopened, reopened_report) = closure_for_test(&open_process(&terminal)?)?;
    let report = finished_report.ok_or("the finish reported no abandonment")?;
    assert_eq!(
        report.claim(),
        crate::runtime::AbandonedTurnClaim::RunningTool
    );
    assert_eq!(report.effects().len(), 1, "{report:?}");
    assert_eq!(Some(report), reopened_report, "one report from either cut");
    assert_eq!(
        finished.identity(),
        reopened.identity(),
        "one history from either cut"
    );
    Ok(())
}

/// A steer queued during the unobserved request is named unapplied in the
/// one note, from either cut.
#[tokio::test]
async fn a_recovered_recovery_required_finish_names_its_unapplied_steer() -> TestResult {
    let RecoveryRequiredCuts { report, note, .. } =
        recovery_required_finish_parity(RecoveryRequiredCut::QueuedSteer).await?;
    assert_eq!(
        report.claim(),
        crate::runtime::AbandonedTurnClaim::ModelRequest
    );
    assert!(
        note.ends_with("Queued steering was not applied and is not retried."),
        "{note}"
    );
    Ok(())
}
