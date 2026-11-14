//! A move receipt describes its captured terminal set, not later observations.
use super::*;
use plakous_agent::internal::host_move::{
    MoveJobCut, ProcessSessionId, ProcessSessionOwnerGeneration,
};
use plakous_agent::internal::{
    JobTerminal, JobTerminalStatus, LineId, LineJobTerminal, RecordLineJobTerminals,
};

fn terminal(number: u32) -> TestResult<LineJobTerminal> {
    Ok(LineJobTerminal {
        number,
        session_id: ProcessSessionId::new([u8::try_from(number)?; 16])?,
        owner_generation: ProcessSessionOwnerGeneration::new(1)?,
        terminal: JobTerminal {
            status: JobTerminalStatus::Exited,
            exit_code: Some(0),
            stopper: None,
            elapsed: std::time::Duration::from_secs(1),
            tail: format!("output-{number}"),
        },
    })
}

async fn publish_then_observe(
    source: &dyn SessionStore,
    target: &dyn SessionStore,
    durability: RuntimeDurability,
    captured: bool,
) -> TestResult<(SessionWorkflowId, MoveInstall)> {
    let lease = seed(source, durability).await?;
    let selector = lease.session();
    let line = LineId::started_by(selector.branch_id());
    let head = source
        .load_branch(selector)
        .await?
        .checkpoint()
        .reference()
        .checkpoint_id();
    if captured {
        source
            .record_line_job_terminals(RecordLineJobTerminals::new(
                lease.clone(),
                line,
                1,
                head,
                vec![terminal(1)?],
            ))
            .await?;
    }
    let operation = SessionWorkflowId::new([149; 32])?;
    let install = source
        .prepare_move(PrepareMove {
            operation,
            leases: vec![lease],
            job_cut: MoveJobCut::Settled,
        })
        .await?;
    target.stage_move(operation, install.clone()).await?;
    target
        .finish_move(operation, install.digest(), MoveDisposition::Publish)
        .await?;
    let lease = target
        .acquire(AcquireBranch::new(
            selector,
            fixture::id(19)?,
            NonZeroU64::new(300).ok_or("nonzero")?,
        ))
        .await?;
    target
        .record_line_job_terminals(RecordLineJobTerminals::new(
            lease.clone(),
            line,
            1,
            head,
            vec![terminal(2)?],
        ))
        .await?;
    target.release(lease).await?;
    let expected = if captured {
        vec![terminal(1)?, terminal(2)?]
    } else {
        vec![terminal(2)?]
    };
    assert_eq!(target.line_job_terminals(line).await?, expected);
    assert_original(target, operation, &install).await?;
    Ok((operation, install))
}

async fn assert_original(
    target: &dyn SessionStore,
    operation: SessionWorkflowId,
    original: &MoveInstall,
) -> TestResult {
    let recovered = target.recover_move(operation).await?;
    assert_eq!(recovered.digest(), original.digest());
    for (recovered, original) in recovered.lines().iter().zip(original.lines()) {
        assert_eq!(recovered.line.line(), original.line.line());
        assert_eq!(recovered.terminals, original.terminals);
    }
    Ok(())
}

#[tokio::test]
async fn host_move_memory_recovery_keeps_the_captured_terminal_set() -> TestResult {
    for captured in [false, true] {
        let source = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
        let target = crate::MemorySessionStore::new(crate::MemoryStoreLimits::default())?;
        publish_then_observe(&source, &target, RuntimeDurability::Disabled, captured).await?;
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn host_move_sqlite_recovery_keeps_the_captured_terminal_set_after_restart() -> TestResult {
    for captured in [false, true] {
        let root = tempfile::tempdir()?;
        let source = crate::SqliteSessionStore::open(root.path().join("source.sqlite3"))?;
        let path = root.path().join("target.sqlite3");
        let target = crate::SqliteSessionStore::open(&path)?;
        let (operation, install) =
            publish_then_observe(&source, &target, RuntimeDurability::Required, captured).await?;
        source.shutdown().await?;
        target.shutdown().await?;
        let reopened = crate::SqliteSessionStore::open(&path)?;
        assert_original(&reopened, operation, &install).await?;
        assert!(
            reopened
                .line_job_terminals(install.lines()[0].line.line())
                .await?
                .iter()
                .any(|row| row.number == 2)
        );
        reopened.shutdown().await?;
    }
    Ok(())
}
