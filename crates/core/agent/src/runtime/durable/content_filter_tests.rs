use super::*;

#[tokio::test]
async fn crash_after_guidance_retains_one_instruction_without_redispatch() -> TestResult {
    use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(plakous_test_support::ScriptedModel::new(descriptor.clone(), [
        ModelScenario::new(ExpectedRequest::Any, ScenarioStart::Immediate,
            vec![ScenarioStep::Error(ModelError::new(ModelErrorKind::ContentFilteredIncomplete,
                "stream disconnected before completion: Incomplete response returned, reason: content_filter"))]),
    ]).with_runtime_authority(authority.clone())
        .with_context_observation(CountObservation::new("agent-scripted-counter", "agent-scripted-exact-v1", 64)?)
        .with_content_filter_instruction("<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>".to_owned()));
    let builder = || -> TestResult<AgentBuilder> {
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = configured_test_model(
            model.clone(),
            descriptor.clone(),
            authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        Ok(AgentBuilder::new(configured, registry, workspace.clone())
            .request_policy(AgentRequestPolicy::new(
                GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                ReasoningPolicy::Disabled,
            ))
            .identity_sources(
                Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                Arc::new(SystemIdGenerator),
            ))
    };
    let store = Arc::new(TestCreateStore::default());
    store
        .block_after_content_filter_guidance
        .store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create("create:filtered-guidance-crash", builder()?)
        .await?;
    let _turn = original.prompt("permitted task").await?;
    let address = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let checkpoint = original.checkpoint();
            if store.commits_blocked.load(Ordering::Acquire)
                && checkpoint
                    .manifest()
                    .canonical()
                    .history()
                    .segments()
                    .iter()
                    .flat_map(HistorySegment::items)
                    .any(|item| {
                        item.actor() == Actor::User && item.provenance() == Provenance::Instruction
                    })
            {
                break DurableSessionAddress::new(
                    original.session_id(),
                    original.branch_id(),
                    checkpoint.id(),
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        model.captured_plans().len(),
        1,
        "retry never reached dispatch"
    );
    let history = original
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .clone();
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store
        .block_after_content_filter_guidance
        .store(false, Ordering::Release);
    store.resume_commits();
    store.expire_active_lease();
    // The decided retry was never sent: the open abandons the turn as
    // `NothingStarted` and says so (spec 01 §5.5), without redispatch.
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:filtered-guidance-crash", address, builder()?)
        .await?
    else {
        return Err("the decided-retry cut did not close on open".into());
    };
    let note = serde_json::to_string(
        reopened
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .last()
            .ok_or("note")?,
    )?;
    assert!(note.contains("The decided retry was not sent"), "{note}");
    reopened.shutdown().await?;
    let stored = store.load_checkpoint(address.checkpoint_id).await?;
    assert_eq!(
        stored.checkpoint().manifest().canonical().history(),
        &history
    );
    assert_eq!(
        history
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .filter(
                |item| item.actor() == Actor::User && item.provenance() == Provenance::Instruction
            )
            .count(),
        1
    );
    assert_eq!(
        model.captured_plans().len(),
        1,
        "reopen must not replay an ordinary root"
    );
    Ok(())
}
