//! The finish row is a literal replay (spec 01 §5.5): for the same recorded
//! decision, the consequence commit a newly opened owner publishes equals the
//! one the live driver published, modulo the new checkpoint's identity.

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
    }
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
