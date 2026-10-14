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
    assert_eq!(decisions.len(), 1, "{label}: one decision commit");
    let cut = Arc::clone(&decisions[0]);
    assert_eq!(
        live.manifest().topology().parent_checkpoint_id,
        Some(cut.id()),
        "{label}: the live consequence commit follows the decision commit"
    );
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
    Ok(replayed)
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
