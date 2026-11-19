use super::*;

#[derive(Default)]
struct ObservedJobHost {
    service: Mutex<Option<plakous_tools::JoinedSessionsPort>>,
    observations: Arc<Mutex<Vec<plakous_tools::JobView>>>,
}

impl plakous_tools::JobHost for ObservedJobHost {
    fn service(&self, _: plakous_model::LineId) -> Option<plakous_tools::JoinedSessionsPort> {
        lock_mutex(&self.service).clone()
    }

    fn attach(
        &self,
        _: plakous_tools::JobOwner,
        fresh: plakous_tools::JoinedSessionsPort,
    ) -> Result<plakous_tools::JoinedSessionsPort, plakous_tools::ExecutionBackendError> {
        let observations = self.observations.clone();
        fresh.observe_jobs(
            move |job| {
                lock_mutex(&observations).push(job);
                true
            },
            || true,
        );
        *lock_mutex(&self.service) = Some(fresh.clone());
        Ok(fresh)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_release_without_a_job_host_stops_jobs() -> TestResult {
    release(Release::Stop).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_release_with_a_job_host_transfers_jobs() -> TestResult {
    release(Release::Transfer).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_shutdown_reuses_a_started_job_stop() -> TestResult {
    release(Release::AlreadyStopping).await
}

enum Release {
    Stop,
    Transfer,
    AlreadyStopping,
}

async fn release(release: Release) -> TestResult {
    let directory = tempfile::tempdir()?;
    let (services, runner) = placed(directory.path())?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            147_000,
            false,
        )
        .with_tool_first(),
    );
    let (registry, tool) = session_update_registry(SessionUpdateFixtureMode::Promoted)?;
    let mut builder = session_update_builder(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        model,
        registry,
    )?
    .with_runner_services(services);
    let host = Arc::new(ObservedJobHost::default());
    let retained = matches!(release, Release::Transfer);
    if retained {
        builder = builder.job_host(host.clone());
    }
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:runner-release", builder).await?;
    agent.prompt("start a job").await?.result().await?;
    let service = lock_mutex(&tool.sessions).clone().ok_or("service")?;
    let port = plakous_tools::JoinedSessionsPort::runner(service.clone());
    port.refresh_jobs()?;
    let before = port.jobs();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].status, plakous_tools::JobStatus::Running);
    if retained {
        // This link has no watch stream: the promotion answer must report
        // custody before the caller can finish its turn and release.
        let observations = lock_mutex(&host.observations);
        assert_eq!(observations.len(), 1, "promotion waited for the job watch");
        assert_eq!(observations[0].session_id, before[0].session_id);
    }
    let already_stopping = matches!(release, Release::AlreadyStopping);
    if already_stopping {
        agent.begin_job_stop();
    }
    agent.shutdown().await?;
    if already_stopping {
        assert_eq!(
            runner.job_stops.load(Ordering::SeqCst),
            1,
            "joined shutdown must use the stop already sent by begin_job_stop"
        );
    }
    port.refresh_jobs()?;
    let after = port.jobs();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].number, before[0].number);
    assert_eq!(after[0].owner_generation, before[0].owner_generation);
    let checkpoint = agent.checkpoint();
    let terminal = &checkpoint.manifest().jobs().rows()[0].terminal;
    if retained {
        assert_eq!(after[0].status, plakous_tools::JobStatus::Running);
        assert!(terminal.is_none());
        service.stop_jobs(
            plakous_tools::JobStopReason::User,
            Duration::from_millis(200),
        )?;
    } else {
        assert_eq!(
            after[0].status,
            plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::SessionExit)
        );
        assert_eq!(
            terminal.as_ref().ok_or("terminal")?.stopper,
            Some(plakous_tools::JobStopReason::SessionExit)
        );
    }
    assert!(lock_mutex(&store.state).active_lease.is_none());
    Ok(())
}
