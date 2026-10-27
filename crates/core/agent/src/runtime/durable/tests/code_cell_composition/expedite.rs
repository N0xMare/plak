use super::*;

async fn fixture(
    directory: &std::path::Path,
    evidence: Arc<FakeCompositionEvidence>,
    source: &str,
) -> TestResult<Agent> {
    let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
    let registry = neutral_composition_test_registry_with_notices(true)?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let wait = serde_json::to_string(&json!({"cell_id":"C", "yield_time_ms":10000}))?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5500, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string(source)?),
                (1, "wait", wait.clone()),
                (2, "wait", wait),
            ]),
    );
    let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let builder = composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition,
        (evidence, LocalCompositionLifecycle::RootTurnJoined),
    )?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:expedited-resume", builder).await?;
    agent.set_notice_delivery_available(true);
    Ok(agent)
}

fn completed() -> LocalCompositionResponse {
    LocalCompositionResponse::Result {
        cell_id: "C".into(),
        items: vec![LocalCompositionContentItem::Text(
            "retained terminal".into(),
        )],
        error: None,
    }
}

fn third(evidence: &FakeCompositionEvidence, deliveries: Arc<CancelCellEvidence>) -> TestResult {
    lock_mutex(&evidence.events)
        .as_ref()
        .ok_or("events")?
        .send(LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "C".into(),
                runtime_call_id: "THIRD".into(),
                tool_name: "exec_command".into(),
                input: Some(json!({"cmd":"printf third", "yield_time_ms":1000})),
            },
            responder: Box::new(CancelCellResponder {
                identity: "THIRD".into(),
                evidence: deliveries,
            }),
        })?;
    Ok(())
}

fn assert_completed(result: &TurnResult) {
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .iter()
            .all(
                |operation| operation.transitions().iter().all(|transition| !matches!(
                    transition.state(),
                    DurableOperationState::OutcomeUnknown(_)
                ))
            )
    );
}

async fn nested_during_wait(notice: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    let (terminal, receiver) = tokio::sync::watch::channel(None);
    *lock_mutex(&evidence.retained_terminal) = Some(receiver);
    let agent = fixture(directory.path(), evidence.clone(), "yield once").await?;
    let turn = agent.prompt("finish sequential nested calls").await?;
    evidence.wait_entered.notified().await;
    if notice {
        turn.control().notice().await?;
        wait_for_scheduler_count(&evidence.expedites, 1).await;
    }
    let deliveries = Arc::new(CancelCellEvidence::default());
    third(&evidence, deliveries.clone())?;
    let worker = tokio::spawn(async move {
        loop {
            if lock_mutex(&deliveries.deliveries)
                .iter()
                .any(|(id, accepted, _)| id == "THIRD" && *accepted)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        terminal.send_replace(Some(completed()));
    });
    let started = tokio::time::Instant::now();
    let result = turn.result().await?;
    worker.abort();
    let _ = worker.await;
    agent.shutdown().await?;
    assert_completed(&result);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "held nested work must yield before the deadline"
    );
    assert_eq!(
        evidence.waits.load(Ordering::Acquire),
        1,
        "one physical resume"
    );
    assert_eq!(
        evidence.expedites.load(Ordering::Acquire),
        usize::from(notice)
    );
    Ok(())
}

#[test]
fn review_nested_call_during_expedited_wait_completes() -> TestResult {
    run_cancel_cell_scenario(|| nested_during_wait(true))
}

#[test]
fn nested_call_during_unexpedited_wait_completes() -> TestResult {
    run_cancel_cell_scenario(|| nested_during_wait(false))
}

#[test]
fn cpu_bound_expedited_wait_retains_its_six_second_answer() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        let (terminal, receiver) = tokio::sync::watch::channel(None);
        *lock_mutex(&evidence.retained_terminal) = Some(receiver);
        let agent = fixture(directory.path(), evidence.clone(), "yield once").await?;
        let turn = agent.prompt("finish computing").await?;
        evidence.wait_entered.notified().await;
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let worker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            terminal.send_replace(Some(completed()));
        });
        turn.control().notice_at(agent.checkpoint().id())?.await??;
        let result = turn.result().await?;
        let elapsed = started.elapsed();
        tokio::time::resume();
        worker.abort();
        let _ = worker.await;
        agent.shutdown().await?;
        assert_completed(&result);
        assert!(
            (Duration::from_secs(6)..Duration::from_secs(7)).contains(&elapsed),
            "{elapsed:?}"
        );
        assert_eq!(
            evidence.waits.load(Ordering::Acquire),
            1,
            "one retained resume"
        );
        assert_eq!(evidence.expedites.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.wait_observations.load(Ordering::Acquire),
            2,
            "yield after five seconds, then observe the same answer"
        );
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text().contains("New messages are waiting; end this turn to receive them."))))));
        Ok(())
    })
}

#[test]
fn notice_does_not_resolve_a_ready_terminal_past_a_queued_call() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(FakeCompositionEvidence::default());
        let agent = fixture(directory.path(), evidence.clone(), "ready after event").await?;
        let turn = agent.prompt("deliver the queued call first").await?;
        wait_for_scheduler_count(&evidence.physical_cell_starts, 1).await;
        *lock_mutex(&evidence.notice_on_event) = Some(turn.control());
        let deliveries = Arc::new(CancelCellEvidence::default());
        third(&evidence, deliveries.clone())?;
        let result = turn.result().await?;
        agent.shutdown().await?;
        assert_completed(&result);
        assert!(
            lock_mutex(&deliveries.deliveries)
                .iter()
                .any(|(id, accepted, _)| id == "THIRD" && *accepted),
            "terminal must not retire the queued call"
        );
        Ok(())
    })
}
