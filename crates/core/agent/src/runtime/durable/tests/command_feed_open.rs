//! Opening a session recovers command admission sequences in feed pages.
use super::*;

#[tokio::test]
async fn delayed_model_start_observation_retains_its_committed_prefix() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0x0fee_e000,
            false,
        )
        .with_tool_first(),
    );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:delayed-round-observer",
            session_update_builder(workspace, model, one_tool_registry()?)?,
        )
        .await?;
    let mut turn = agent.prompt("finish before observing").await?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let terminal = result.feed_seq().ok_or("terminal sequence")?;
    let mut starts = Vec::new();
    loop {
        let event = events.recv().await?;
        if matches!(event.kind(), AgentEventKind::ModelStarted) {
            starts.push(event.feed_seq().ok_or("round prefix")?);
        }
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(starts.len(), 2);
    assert!(starts[0] < starts[1]);
    assert!(starts[1] < terminal);
    agent.shutdown().await?;
    Ok(())
}

/// Every retained command's admission sequence comes from a bounded number of
/// feed pages, not from one receipt and one feed read per ancestor commit, so
/// opening a long session costs the same few store calls as a short one.
#[tokio::test]
async fn opening_a_long_session_reads_command_sequences_in_feed_pages() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 0x0fee_d000)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:long-command-feed",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let mut admitted = Vec::new();
    for turn in 0..40 {
        let command_id = CommandId::new(format!("root:long-command-feed-{turn}"))?;
        let submission = agent.start(command_id.clone(), "next").await?;
        let seq = submission
            .acknowledgement()
            .feed_seq()
            .ok_or("admission sequence")?;
        admitted.push((command_id, seq));
        submission
            .into_turn()
            .ok_or("turn was not admitted")?
            .result()
            .await?;
    }
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let reads_before = lock_mutex(&store.feed_read_ranges).len();
    let receipts_before = store.recover_commit_calls.load(Ordering::Acquire);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:long-command-feed",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("long session did not reopen".into());
    };
    let reads = lock_mutex(&store.feed_read_ranges).len() - reads_before;
    let receipts = store.recover_commit_calls.load(Ordering::Acquire) - receipts_before;
    assert!(
        reads <= 4,
        "open read the feed {reads} times for 40 commits"
    );
    assert!(
        receipts <= 1,
        "open recovered {receipts} commit receipts for 40 commits"
    );
    for (command_id, seq) in admitted {
        let duplicate = reopened.start(command_id, "next").await?;
        assert_eq!(
            duplicate.acknowledgement().disposition(),
            CommandAckDisposition::Duplicate
        );
        assert_eq!(duplicate.acknowledgement().feed_seq(), Some(seq));
    }
    reopened.shutdown().await?;
    Ok(())
}
