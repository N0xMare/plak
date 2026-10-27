use super::*;

async fn expedited_long_wait(unread_mail: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let evidence = Arc::new(CancelCellEvidence::default());
    evidence.long_wait_fixture.store(true, Ordering::Release);
    evidence.expedite_fixture.store(true, Ordering::Release);
    let agent = cancel_cell_agent(
        CancelCellMode::WaitWindow,
        evidence.clone(),
        directory.path(),
    )
    .await?;
    agent.set_notice_delivery_available(true);
    let turn = agent.prompt("observe a ten-minute window").await?;
    wait_for_scheduler_fact(&evidence.wait_started).await;
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let control = turn.control();
    if unread_mail {
        control.notice_at(agent.checkpoint().id())?.await??;
    } else {
        control.steer("new instruction").await?;
    }
    let result = tokio::time::timeout(Duration::from_secs(1), turn.result()).await;
    let elapsed = started.elapsed();
    tokio::time::resume();
    if result.is_err() {
        let _ = control.cancel().await;
    }
    agent.shutdown().await?;
    let result = result??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(
        evidence.wait_count.load(Ordering::Acquire),
        1,
        "retain the admitted Resume"
    );
    assert_eq!(evidence.expedites.load(Ordering::Acquire), 1, "rearm once");
    if unread_mail {
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text().contains("New messages are waiting; end this turn to receive them."))))));
    }
    Ok(())
}

#[test]
fn steer_expedites_a_six_hundred_second_js_wait_without_another_wait() -> TestResult {
    run_cancel_cell_scenario(|| expedited_long_wait(false))
}

#[test]
fn unread_mail_expedites_a_six_hundred_second_js_wait_without_another_wait() -> TestResult {
    run_cancel_cell_scenario(|| expedited_long_wait(true))
}

#[test]
fn buffered_wait_terminal_beats_synthetic_steer_yield() -> TestResult {
    run_cancel_cell_scenario(|| async {
        let directory = tempfile::tempdir()?;
        let evidence = Arc::new(CancelCellEvidence::default());
        evidence.buffered_wait.store(true, Ordering::Release);
        let (builder, model) = cancel_cell_builder(
            CancelCellMode::WaitWindow,
            evidence.clone(),
            directory.path(),
        )?;
        let release = Arc::new(Notify::new());
        *lock_mutex(&model.held_call_release) = Some(release.clone());
        model.held_call.store(1, Ordering::Release);
        let sessions = DurableSessionRuntime::current(
            Arc::new(TestCreateStore::default()),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions.create("create:buffered-wait", builder).await?;
        let turn = agent.prompt("finish the buffered wait").await?;
        model.held_call_started.notified().await;
        evidence.release_cell.store(true, Ordering::Release);
        *lock_mutex(&evidence.steer_on_wait) = Some(turn.control());
        release.notify_one();
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        assert!(
            !evidence.wait_started.load(Ordering::Acquire),
            "peek must not poll the resumable wait"
        );
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
        assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
            matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
                matches!(part, ToolResultPart::Text(text) if text.text().contains("buffered result"))))));
        agent.shutdown().await?;
        Ok(())
    })
}
