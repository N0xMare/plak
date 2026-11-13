//! Durable terminal evidence, not an idle process view, settles a source job.
use super::*;
use plakous_agent::JobOwner;
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
            operation,
            leases: vec![lease],
        })
        .await?;
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
