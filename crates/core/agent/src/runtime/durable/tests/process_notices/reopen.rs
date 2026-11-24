use super::*;
use plakous_tools::{JobStatus, JoinedSessionsPort};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_line_terminal_is_observed_once_after_reopen() -> TestResult {
    wait_after_reopen(false, true).await?;
    wait_after_reopen(true, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refreshed_terminal_is_observed_once_after_reopen() -> TestResult {
    wait_after_reopen(false, false).await?;
    wait_after_reopen(true, false).await
}

async fn wait_after_reopen(on_runner: bool, recorded: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            94_900,
            false,
        )
        .with_tool_first()
        .with_scripted_tool_calls([(2, "probe", "{}".to_owned())]),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    tool.await_first_job.store(true, Ordering::Release);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let runner = on_runner
        .then(|| placement_hold::placed_services(directory.path()))
        .transpose()?;
    let local = (!on_runner).then(|| Arc::new(jobs::RetainedJobHost::default()));
    let builder = |readopt_local| {
        let mut builder =
            session_update_builder(workspace.clone(), model.clone(), registry.clone())?;
        if let Some(runner) = &runner {
            builder = builder.with_runner_services(runner.clone());
        } else if readopt_local && let Some(host) = &local {
            builder = builder.job_host(host.clone());
        }
        Ok::<_, Box<dyn std::error::Error>>(builder)
    };
    let source = runtime.create("create:reopen-job", builder(true)?).await?;
    let started = source
        .prompt("start a promoted job")
        .await?
        .result()
        .await?;
    assert!(matches!(started.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = source.checkpoint();
    let row = &checkpoint.manifest().jobs().rows()[0];
    assert!(row.terminal.is_none());
    let source_jobs = source.inner.job_sessions.clone();
    let id = source_jobs
        .session_for_number(row.number)
        .ok_or("job handle")?;
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    model.held_call.store(2, Ordering::Release);
    source.abort_driver_for_test();
    store.expire_active_lease();
    if recorded {
        let view = finish_job(&source_jobs, &source_jobs, id, on_runner).await?;
        *lock_mutex(&store.line_terminals) =
            vec![crate::manifest::LineJobTerminal::from_view(&view)?.ok_or("terminal")?];
    }
    let address =
        DurableSessionAddress::new(source.session_id(), source.branch_id(), checkpoint.id());
    let DurableRuntimeOpen::Opened(agent) = runtime
        .open("open:reopen-job", address, builder(!recorded)?)
        .await?
    else {
        return Err("the job prevented reopen".into());
    };
    let fresh = &agent.inner.job_sessions;
    assert_eq!(
        agent.jobs()[0].status,
        if recorded {
            JobStatus::Exited(Some(7))
        } else {
            JobStatus::Running
        }
    );
    if recorded {
        assert!(lock_mutex(&store.line_terminals).is_empty());
        assert_eq!(
            agent.checkpoint().manifest().jobs().rows()[0]
                .terminal
                .as_ref()
                .and_then(|fact| fact.exit_code),
            Some(7)
        );
    }
    let turn = agent
        .start_instruction(
            CommandId::new("continue:wait")?,
            "continue by waiting for the job",
        )
        .await?
        .into_turn()
        .ok_or("continuation turn")?;
    tokio::time::timeout(Duration::from_secs(30), model.held_call_started.notified()).await?;
    assert_eq!(notice_count(&agent), usize::from(recorded));
    if !recorded {
        let terminal = finish_job(&source_jobs, fresh, id, on_runner).await?;
        assert_eq!(terminal.session_id, row.session_id.as_bytes());
    }
    release.notify_one();
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(tool.dispatches.load(Ordering::Acquire), 2);
    assert_terminal_once(&agent, &result, on_runner, recorded)?;
    assert!(fresh.terminal_jobs_since(0).is_empty());
    assert!(matches!(
        plakous_tools::internal::reported_process_session(fresh, row.number),
        Some((_, plakous_tools::JoinedProcessStatus::Exited(Some(7))))
    ));
    agent.shutdown().await?;
    Ok(())
}

fn assert_terminal_once(
    agent: &Agent,
    result: &TurnResult,
    on_runner: bool,
    recorded: bool,
) -> TestResult {
    let wait = result
        .conversation()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => result.parts().iter().find_map(|part| match part {
                ToolResultPart::Text(text) => {
                    serde_json::from_str::<serde_json::Value>(text.text())
                        .ok()
                        .filter(|value| value["tasks"].is_array())
                }
                _ => None,
            }),
            _ => None,
        })
        .last()
        .ok_or("missing Wait result")?;
    assert_eq!(
        notice_count(agent) + usize::from(wait["tasks"][0]["status"] == "exited"),
        1,
        "the terminal must appear in exactly one canonical result: runner={on_runner}, recorded={recorded}, wait={wait}",
    );
    if let Some(code) = wait["tasks"][0]["exit_code"].as_i64() {
        assert_eq!(code, 7);
    }
    assert!(agent.checkpoint().manifest().jobs().rows().is_empty());
    Ok(())
}

async fn finish_job(
    owner: &plakous_tools::JoinedProcessSessions,
    observer: &plakous_tools::JoinedProcessSessions,
    id: plakous_tools::JoinedProcessSessionId,
    on_runner: bool,
) -> TestResult<plakous_tools::JobView> {
    owner.write_stdin(
        id,
        b"finish\n",
        plakous_tools::JoinedProcessInputDisposition::KeepOpen,
    )?;
    let port = JoinedSessionsPort::runner(observer.clone());
    Ok(tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if on_runner {
                port.refresh_jobs()?;
            }
            if let Some(job) = observer
                .jobs()
                .into_iter()
                .find(|job| job.status == JobStatus::Exited(Some(7)))
            {
                return Ok::<_, plakous_tools::ExecutionBackendError>(job);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??)
}
