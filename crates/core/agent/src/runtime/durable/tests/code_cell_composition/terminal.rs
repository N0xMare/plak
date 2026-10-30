#![allow(
    clippy::too_many_lines,
    reason = "Cell-terminal scenarios connect interpreter completion to exactly one outer turn result and canonical tool exchange"
)]

use super::*;

pub(in super::super) struct WaitObservation<'a> {
    pub(in super::super) evidence: &'a FakeCompositionEvidence,
    pub(in super::super) finished: bool,
}

impl Drop for WaitObservation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.evidence.abandoned_waits.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn nested_process(
    evidence: &FakeCompositionEvidence,
    deliveries: &Arc<CancelCellEvidence>,
    identity: &str,
    hint: u64,
) -> TestResult {
    lock_mutex(&evidence.events)
        .as_ref()
        .ok_or("cell events")?
        .send(LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "C".into(),
                runtime_call_id: identity.into(),
                tool_name: "exec_command".into(),
                input: Some(json!({"cmd":"exec sleep 3600", "yield_time_ms":hint})),
            },
            responder: Box::new(CancelCellResponder {
                identity: identity.into(),
                evidence: deliveries.clone(),
            }),
        })?;
    Ok(())
}

async fn notice_agent(
    directory: &std::path::Path,
) -> TestResult<(Agent, Arc<FakeCompositionEvidence>)> {
    let workspace = Arc::new(WorkspacePolicy::host(directory, ".")?);
    let registry = neutral_composition_test_registry_with_notices(true)?;
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_5400, false)
            .with_scripted_tool_calls([
                (0, "exec", serde_json::to_string("seed job")?),
                (1, "exec", serde_json::to_string("await notice")?),
                (
                    2,
                    "wait",
                    serde_json::to_string(&json!({"cell_id":"C", "yield_time_ms":1000}))?,
                ),
            ]),
    );
    let composition = neutral_composition_for(&model, &descriptor, &authority, &registry)?;
    let evidence = Arc::new(FakeCompositionEvidence::default());
    evidence.hold_wait.store(true, Ordering::Release);
    let builder = composition_test_builder(
        workspace,
        registry,
        &model,
        descriptor,
        authority,
        composition,
        (evidence.clone(), LocalCompositionLifecycle::RootTurnJoined),
    )?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:notice-resume", builder).await?;
    agent.set_notice_delivery_available(true);
    Ok((agent, evidence))
}

#[test]
fn job_notices_preserve_the_resumed_cell_observation() -> TestResult {
    let phase = Arc::new(AtomicUsize::new(0));
    let progress = Arc::clone(&phase);
    let result = run_cancel_cell_scenario(move || async move {
        let directory = tempfile::tempdir()?;
        let (agent, evidence) = notice_agent(directory.path()).await?;
        let mut turn = agent.prompt("observe the cell terminal").await?;
        let mut events = turn.take_events()?;
        progress.store(1, Ordering::Release);
        // Wait for source acceptance before injecting the cell's nested call.
        while evidence.physical_cell_starts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let deliveries = Arc::new(CancelCellEvidence::default());
        progress.store(2, Ordering::Release);
        nested_process(&evidence, &deliveries, "FIRST", 0)?;
        while lock_mutex(&deliveries.deliveries).is_empty() {
            tokio::task::yield_now().await;
        }
        // Commit the first job with the completed setup cell before opening
        // the cell whose observation its exit will preempt.
        evidence.source_release.notify_one();
        while evidence.physical_cell_starts.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
        nested_process(&evidence, &deliveries, "SECOND", 10000)?;
        let mut started = 0;
        while started < 2 {
            let event = events.recv().await?;
            if matches!(event.kind(), AgentEventKind::ToolStarted { identity, .. }
                if identity.canonical_name().as_str() == PROCESS_SESSION_CAPABILITY_NAME)
            {
                started += 1;
            }
        }
        progress.store(3, Ordering::Release);
        let first = agent
            .inner
            .job_sessions
            .session_for_number(1)
            .ok_or("first job")?;
        // The first terminal raises Notice during the nested process start.
        agent
            .inner
            .job_sessions
            .stop(first, plakous_tools::JobStopReason::User)?;
        evidence.wait_entered.notified().await;
        progress.store(4, Ordering::Release);
        assert_eq!(
            lock_mutex(&deliveries.deliveries).len(),
            2,
            "each nested call settled once"
        );
        // The second terminal arrives after wait has issued its resume.
        let second = agent
            .inner
            .job_sessions
            .session_for_number(2)
            .ok_or("second job")?;
        agent
            .inner
            .job_sessions
            .stop(second, plakous_tools::JobStopReason::User)?;
        // Ordered control receipts force fresh driver boundaries after the notice.
        turn.control().notice().await?;
        turn.control().notice().await?;
        progress.store(5, Ordering::Release);
        assert_eq!(
            evidence.expedites.load(Ordering::Acquire),
            1,
            "repeated notices rearm the existing observation only once"
        );
        assert_eq!(
            evidence.abandoned_waits.load(Ordering::Acquire),
            0,
            "notice abandoned an in-flight resume and returned stale running state"
        );
        evidence.wait_release.notify_one();
        let result = turn.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(evidence.waits.load(Ordering::Acquire), 1);
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
        let observations = result
            .conversation()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 3);
        assert!(observations[2].parts().iter().any(|part|
            matches!(part, ToolResultPart::Text(text) if text.text().contains("Script completed"))));
        agent.shutdown().await?;
        Ok(())
    });
    result.map_err(|error| {
        format!(
            "notice/resume phase {}: {error}",
            phase.load(Ordering::Acquire)
        )
        .into()
    })
}
