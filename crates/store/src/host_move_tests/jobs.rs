//! Durable terminal evidence, not an idle process view, settles a source job.
use super::*;
use plakous_agent::JobOwner;
use plakous_agent::internal::host_move::{MoveJobCut, MoveSession};
use plakous_agent::internal::host_move::{ProcessSessionId, ProcessSessionOwnerGeneration};
use plakous_agent::internal::{
    JobLedger, JobRow, JobTerminal, JobTerminalStatus, LineJobTerminal, RuntimeCheckpoint,
    RuntimeManifest,
};

#[tokio::test]
async fn host_move_accepts_only_exact_durable_line_terminal_evidence_for_an_unfolded_job()
-> TestResult {
    let source = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
    let lease = seed(&source, RuntimeDurability::Disabled).await?;
    let operation = SessionWorkflowId::new([145; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            job_cut: MoveJobCut::Settled,
            operation,
            leases: vec![lease],
        })
        .await?;
    let (members, terminal) = unsettled_job(&install)?;
    let mut lines = install.lines().to_vec();
    assert!(
        MoveInstall::new(members.clone(), lines.clone()).is_err(),
        "an idle source is not terminal evidence"
    );
    lines[0].terminals.push(terminal.clone());
    MoveInstall::new(members.clone(), lines.clone())?;
    lines[0].terminals[0].owner_generation = ProcessSessionOwnerGeneration::new(4)?;
    assert!(
        MoveInstall::new(members.clone(), lines.clone()).is_err(),
        "a different allocation generation cannot settle the job"
    );
    lines[0].terminals[0] = terminal;
    lines[0].terminals[0].session_id = ProcessSessionId::new([6; 16])?;
    assert!(
        MoveInstall::new(members, lines).is_err(),
        "a reused job number does not authorize another process"
    );
    Ok(())
}

fn unsettled_job(install: &MoveInstall) -> TestResult<(Vec<MoveSession>, LineJobTerminal)> {
    let mut members = install.sessions().to_vec();
    let manifest = members[0].checkpoints[0].manifest();
    let topology = manifest.topology();
    let job = JobRow {
        number: 1,
        command: "completed process".into(),
        started_at: Timestamp::new(1, 0)?,
        owner: JobOwner {
            line: topology.line,
            generation: topology.line_generation,
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            fence: topology.lease_fence,
        },
        session_id: ProcessSessionId::new([5; 16])?,
        owner_generation: ProcessSessionOwnerGeneration::new(3)?,
        terminal: None,
    };
    let terminal = LineJobTerminal {
        number: job.number,
        session_id: job.session_id,
        owner_generation: job.owner_generation,
        terminal: JobTerminal {
            status: JobTerminalStatus::Exited,
            exit_code: Some(0),
            stopper: None,
            elapsed: std::time::Duration::from_secs(1),
            tail: "done".into(),
        },
    };
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        manifest.provenance().clone(),
        topology,
        manifest.lifecycle().clone(),
        manifest.harness_plan_component().clone(),
        manifest.provider_configuration().clone(),
        manifest.canonical().clone(),
        manifest.binding().clone(),
        manifest.tools().clone(),
        manifest.ledger().clone(),
        None,
        None,
        JobLedger::new(2, vec![job])?,
        manifest.named_state().clone(),
        manifest.durability(),
    )?)?;
    members[0].checkpoints[0] = checkpoint;
    Ok((members, terminal))
}

async fn retained_contract(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult<(SessionWorkflowId, plakous_agent::internal::Digest)> {
    let lease = seed(source, durability).await?;
    let original = source
        .prepare_move(PrepareMove {
            job_cut: MoveJobCut::Settled,
            operation: SessionWorkflowId::new([146; 32])?,
            leases: vec![lease.clone()],
        })
        .await?;
    let (members, _) = unsettled_job(&original)?;
    let same_heads = MoveInstall::with_job_cut(
        original.sessions().to_vec(),
        original.lines().to_vec(),
        MoveJobCut::Retained,
    )?;
    assert_ne!(
        same_heads.digest(),
        original.digest(),
        "job policy is part of the batch identity"
    );
    let install =
        MoveInstall::with_job_cut(members, original.lines().to_vec(), MoveJobCut::Retained)?;
    let operation = SessionWorkflowId::new([147; 32])?;
    target.stage_move(operation, install.clone()).await?;
    let recovered = target.recover_move(operation).await?;
    assert_eq!(recovered.job_cut(), MoveJobCut::Retained);
    assert_eq!(recovered.digest(), install.digest());
    assert!(
        recovered.sessions()[0].checkpoints[0]
            .manifest()
            .jobs()
            .rows()[0]
            .terminal
            .is_none()
    );
    assert_eq!(
        target.load_line(install.lines()[0].line.line()).await.err(),
        Some(SessionStoreError::NotLineHead)
    );
    target
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    let held = target
        .acquire(AcquireBranch::new(
            lease.session(),
            fixture::id(150)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    let operation = SessionWorkflowId::new([148; 32])?;
    assert!(
        target
            .prepare_move(PrepareMove {
                job_cut: MoveJobCut::Settled,
                operation,
                leases: vec![held.clone()]
            })
            .await
            .is_err()
    );
    let prepared = target
        .prepare_move(PrepareMove {
            job_cut: MoveJobCut::Retained,
            operation,
            leases: vec![held],
        })
        .await?;
    assert_eq!(prepared.job_cut(), MoveJobCut::Retained);
    assert_eq!(
        target.recover_move(operation).await?.digest(),
        prepared.digest()
    );
    Ok((operation, prepared.digest()))
}

#[tokio::test]
async fn host_move_retained_job_cut_memory_recovers_without_inventing_terminals() -> TestResult {
    let source = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
    let target = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
    retained_contract(&source, &target, RuntimeDurability::Disabled).await?;
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_retained_job_cut_sqlite_preserves_policy_across_restart() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("retained.sqlite3");
    let source = crate::SqliteSessionStore::open(root.path().join("source.sqlite3"))?;
    let target = crate::SqliteSessionStore::open(&path)?;
    let (operation, digest) =
        retained_contract(&source, &target, RuntimeDurability::Required).await?;
    source.shutdown().await?;
    target.shutdown().await?;
    drop(target);
    let reopened = crate::SqliteSessionStore::open_verified(path)?;
    let recovered = reopened.recover_move(operation).await?;
    assert_eq!(recovered.digest(), digest);
    assert_eq!(recovered.job_cut(), MoveJobCut::Retained);
    assert!(
        recovered.sessions()[0].checkpoints[0]
            .manifest()
            .jobs()
            .rows()[0]
            .terminal
            .is_none()
    );
    reopened
        .finish_move(operation, digest, MoveDisposition::Abort)
        .await?;
    reopened.shutdown().await?;
    Ok(())
}
