//! Reopened command-ledger retention through the durable session workflow.

use super::*;
use crate::runtime::control::{command_binding, command_canonical_bytes, durable_rejection};

#[tokio::test]
async fn full_settled_ledger_reclaims_capacity_after_durable_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease duration")?,
    )?;
    let agent = sessions
        .create(
            "create:full-ledger",
            durable_artifact_test_builder(workspace.clone(), artifact_test_model(0, 96_000)?),
        )
        .await?;
    let root_id = CommandId::new("retained-root")?;
    agent
        .start(root_id.clone(), "retained prompt")
        .await?
        .into_turn()
        .ok_or("root turn")?
        .result()
        .await?;
    agent.shutdown().await?;
    let source = agent.checkpoint();
    let prior = source.manifest();
    assert_eq!(prior.lifecycle(), &RuntimeLifecycle::Quiescent);
    let mut commands = prior
        .ledger()
        .commands()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let next_sequence = commands
        .iter()
        .map(DurableCommandRecord::sequence)
        .max()
        .unwrap_or(0)
        + 1;
    for index in 0..4_096 - commands.len() {
        let command = TurnCommand::Start {
            command_id: CommandId::new(format!("seeded-rejection:{index}"))?,
            input: String::new().into(),
        };
        let envelope = DurableCommandEnvelope {
            version: 1,
            canonical_input: command_canonical_bytes(&command),
            effect: DurableActorCommandEffect::Rejected,
            rejection: Some(durable_rejection(&ControlError::new(
                ControlErrorKind::InvalidInput,
                "empty prompt",
            ))),
        };
        commands.push(DurableCommandRecord::new(
            command.command_id().as_str(),
            next_sequence + index as u64,
            None,
            command_binding(&command)?,
            runtime_component(
                RuntimeComponentKind::Command,
                serde_json::to_vec(&envelope)?,
            )?,
            DurableCommandDisposition::Rejected,
            Some("InvalidInput".into()),
        )?);
    }
    let ledger = RuntimeOperationLedger::new(
        commands,
        prior.ledger().operations().into_iter().cloned().collect(),
    )?;
    assert_eq!(ledger.command_count(), 4_096);
    let topology = prior.topology();
    let session = SessionRef::new(topology.session_id, topology.branch_id);
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(uuid::Uuid::from_u128(0x96000))?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let loaded = store.load_branch(session).await?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0x96001))?;
    let branch_revision = topology.branch_revision + 1;
    let lifecycle = RuntimeLifecycle::Quiescent;
    let history = prior.canonical().history().clone();
    let context = prior.canonical().context().retained_for_exact_resume();
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: history.identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: prior.family_artifact(),
    })?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(topology.checkpoint_id),
            branch_revision,
            fork: topology.fork,
            lease_fence: lease.fence().get(),
            commit_identity,
        },
        lifecycle,
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        RuntimeCanonicalState::new(
            prior.canonical().stable_prefix().clone(),
            history,
            WorkingProjection::default(),
            context,
        )?,
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    store
        .commit(CommitCheckpoint::new(
            lease.clone(),
            loaded.head(),
            RuntimeCheckpointCodec::bundle(&checkpoint, None)?,
        ))
        .await?;
    store.release(lease).await?;
    let persisted = store.load_checkpoint(checkpoint_id).await?;
    assert_eq!(
        persisted.checkpoint().manifest().ledger().command_count(),
        4_096
    );
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:full-ledger",
            DurableSessionAddress::new(topology.session_id, topology.branch_id, checkpoint_id),
            durable_artifact_test_builder(workspace, artifact_test_model(0, 97_000)?),
        )
        .await?
    else {
        return Err("full quiescent ledger did not reopen".into());
    };
    assert_eq!(
        reopened.checkpoint().manifest().ledger().command_count(),
        4_096
    );
    let fresh_id = CommandId::new("fresh-root")?;
    let fresh = reopened.start(fresh_id.clone(), "fresh prompt").await?;
    assert_eq!(
        fresh.acknowledgement().disposition(),
        CommandAckDisposition::Accepted
    );
    assert!(fresh.acknowledgement().is_accepted());
    let duplicate = reopened.start(root_id, "retained prompt").await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert!(duplicate.acknowledgement().is_accepted());
    duplicate
        .into_result()
        .ok_or("retained result")?
        .result()
        .await?;
    fresh.into_turn().ok_or("fresh turn")?.result().await?;
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn ledger_full_is_a_definite_session_refusal() {
    let error = crate::runtime::control::control_agent_error(ControlError::new(
        ControlErrorKind::LedgerFull,
        "bounded command ledger is full",
    ));
    assert_eq!(error.kind(), AgentErrorKind::Session);
}
