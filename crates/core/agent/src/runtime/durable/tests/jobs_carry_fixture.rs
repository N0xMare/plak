use super::*;

pub(in crate::runtime::durable) async fn checkpoint() -> TestResult<RuntimeCheckpoint> {
    let directory = tempfile::tempdir()?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        95_800,
        false,
    ));
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:jobs-carry-fixture",
            durable_artifact_test_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                model,
            ),
        )
        .await?;
    let checkpoint = agent.checkpoint();
    let prior = checkpoint.manifest();
    let jobs = crate::manifest::JobLedger::new(
        8,
        vec![crate::manifest::JobRow {
            number: 7,
            command: "completed fixture".to_owned(),
            started_at: Timestamp::new(10, 0)?,
            owner: "fixture-agent".to_owned(),
            session_id: crate::manifest::ProcessSessionId::new([7; 16])?,
            owner_generation: crate::manifest::ProcessSessionOwnerGeneration::new(1)?,
            terminal: Some(crate::manifest::JobTerminal {
                status: crate::manifest::JobTerminalStatus::Exited,
                exit_code: Some(7),
                stopper: None,
                elapsed: Duration::from_secs(2),
                tail: "retained terminal evidence".to_owned(),
            }),
        }],
    )?;
    let seeded = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        prior.topology(),
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        prior.ledger().clone(),
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        jobs,
        prior.durability(),
    )?)?;
    agent.shutdown().await?;
    Ok(seeded)
}
