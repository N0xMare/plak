//! Agent-line conformance (spec 01 §5.1): the head fence, the rebind's head
//! advance, forks and carries, line job terminals and session deletion, for
//! every backend.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use plakous_agent::internal::{
    ArchiveSession, BranchLease, BranchTransitionMode, CheckpointObjectBundle, CommitCheckpoint,
    CreateSession, DeleteSession, FeedChange, FeedFact, FeedPage, FeedSeq, ForkBranch, ForkLineage,
    JobTerminalStatus, LeaseClock, LineId, LineJobTerminal, LineOrigin, PruneBranch,
    RecordLineJobTerminals, RuntimeCheckpoint, RuntimeDurability, RuntimeTopology, SessionRef,
    SessionStore, SessionStoreError,
};
use plakous_agent::{JobStatus, JobStopReason, JobView};
use plakous_model::{BranchId, CheckpointId, RunId, SessionId, Timestamp};

use super::conformance::{
    ManualLeaseClock, TestResult, bundle, checkpoint, checkpoint_with_context, id,
    semantic_request, topology, workflow,
};

fn on_line(mut topology: RuntimeTopology, line: LineId, generation: u64) -> RuntimeTopology {
    topology.line = line;
    topology.line_generation = generation;
    topology
}

fn terminal(number: u32) -> TestResult<LineJobTerminal> {
    let terminal = LineJobTerminal::from_view(&JobView {
        number,
        command: "sleep 60".to_owned(),
        started_at: Timestamp::new(10, 0)?,
        owner: "driver".to_owned(),
        elapsed: Duration::from_secs(3),
        status: JobStatus::Stopped(JobStopReason::User),
        output: "stopped while the head could not open".to_owned(),
        session_id: [7; 16],
        owner_generation: 1,
    })?
    .ok_or("a stopped job carries terminal evidence")?;
    assert_eq!(terminal.terminal.status, JobTerminalStatus::Stopped);
    assert_eq!(LineJobTerminal::decode(&terminal.encode()?)?, terminal);
    Ok(terminal)
}

struct Head {
    lease: BranchLease,
    branch: plakous_agent::internal::BranchHead,
    checkpoint: RuntimeCheckpoint,
    bundle: CheckpointObjectBundle,
}

/// The identities of one head advance.
struct Step {
    branch: BranchId,
    checkpoint: CheckpointId,
    workflow: u8,
    line: LineId,
    generation: u64,
}

/// One rebind of `source` onto `step`'s branch: the next head of its line.
async fn rebind(
    store: &dyn SessionStore,
    source: &Head,
    step: Step,
    durability: RuntimeDurability,
) -> TestResult<Result<Head, SessionStoreError>> {
    let Step {
        branch: target_branch,
        checkpoint: checkpoint_id,
        workflow: workflow_byte,
        line,
        generation,
    } = step;
    let source_topology = source.checkpoint.manifest().topology();
    let lineage = ForkLineage {
        session_id: source_topology.session_id,
        branch_id: source_topology.branch_id,
        checkpoint_id: source_topology.checkpoint_id,
        branch_revision: source_topology.branch_revision,
    };
    // The store derives the line step from the topology, not the transition:
    // a source already on the rebound binding advances by an exact fork.
    let rebound_source = source_topology.fork.is_some();
    let source_context = source.checkpoint.manifest().canonical().context();
    let (mode, context) = if rebound_source {
        (
            BranchTransitionMode::OrdinaryFork,
            source_context.ordinary_fork(
                source_topology.checkpoint_id,
                source_topology.session_id,
                target_branch,
                [workflow_byte; 32],
            )?,
        )
    } else {
        (
            BranchTransitionMode::Rebind,
            source_context.rebind(
                source_topology.checkpoint_id,
                source_topology.session_id,
                target_branch,
                [workflow_byte; 32],
            )?,
        )
    };
    let target = checkpoint_with_context(
        on_line(
            topology(
                source_topology.session_id,
                target_branch,
                checkpoint_id,
                Some(source_topology.checkpoint_id),
                0,
                Some(lineage),
                1,
            )?,
            line,
            generation,
        ),
        2,
        durability,
        context,
        true,
    )?;
    let session = store.load_branch(source.branch.session()).await?.session();
    let request = ForkBranch::new(
        workflow(workflow_byte)?,
        semantic_request(&[workflow_byte])?,
        source.lease.clone(),
        session.revision(),
        source.branch,
        target_branch,
        id::<RunId>(9_000 + u128::from(workflow_byte))?,
        NonZeroU64::new(10).ok_or("lease")?,
        bundle(&target, Some(&source.bundle))?,
    )
    .with_transition(mode);
    Ok(match store.fork(request).await {
        Ok(receipt) => Ok(Head {
            lease: receipt.lease().clone(),
            branch: receipt.target(),
            bundle: bundle(&target, Some(&source.bundle))?,
            checkpoint: target,
        }),
        Err(error) => Err(error),
    })
}

fn successor(head: &Head, checkpoint_id: CheckpointId) -> TestResult<RuntimeCheckpoint> {
    let prior = head.checkpoint.manifest().topology();
    checkpoint_with_context(
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(prior.checkpoint_id),
            branch_revision: prior.branch_revision + 1,
            lease_fence: head.lease.fence().get(),
            ..prior
        },
        2,
        head.checkpoint.manifest().durability(),
        head.checkpoint.manifest().canonical().context().clone(),
        prior.fork.is_some(),
    )
}

async fn commit(
    store: &dyn SessionStore,
    head: &Head,
    checkpoint_id: CheckpointId,
    consumed: Vec<u32>,
) -> TestResult<Result<Head, SessionStoreError>> {
    let next = successor(head, checkpoint_id)?;
    let next_bundle = bundle(&next, Some(&head.bundle))?;
    Ok(
        match store
            .commit(
                CommitCheckpoint::new(head.lease.clone(), head.branch, next_bundle.clone())
                    .with_consumed_line_terminals(consumed),
            )
            .await
        {
            Ok(receipt) => Ok(Head {
                lease: head.lease.clone(),
                branch: receipt.head(),
                checkpoint: next,
                bundle: next_bundle,
            }),
            Err(error) => Err(error),
        },
    )
}

async fn feed_changes(store: &dyn SessionStore, session: SessionId) -> TestResult<Vec<FeedChange>> {
    let FeedPage::Records { records, .. } = store
        .read_feed(
            session,
            FeedSeq::ZERO,
            std::num::NonZeroUsize::new(256).ok_or("page")?,
        )
        .await?
    else {
        return Err("feed behind retention".into());
    };
    Ok(records
        .iter()
        .map(|record| record.header().change)
        .collect())
}

pub(crate) async fn run_line_conformance<F>(durability: RuntimeDurability, open: F) -> TestResult
where
    F: FnOnce(Arc<dyn LeaseClock>) -> TestResult<Arc<dyn SessionStore>>,
{
    let clock = Arc::new(ManualLeaseClock::new(Timestamp::new(100, 0)?));
    let store = open(clock)?;
    let store = store.as_ref();
    let (source, target) = a_rebind_moves_the_head(store, durability).await?;
    let fork_line = the_source_is_read_only(store, &source, durability).await?;
    let third_head = only_the_head_records_terminals(store, &source, target, durability).await?;
    a_carry_continues_and_deletion_ends_the_line(store, &third_head, fork_line, durability).await
}

async fn a_rebind_moves_the_head(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult<(Head, Head)> {
    let session_id = id::<SessionId>(70_001)?;
    let first = id::<BranchId>(70_002)?;
    let line = LineId::started_by(first);
    let genesis = checkpoint(
        topology(session_id, first, id(70_010)?, None, 0, None, 1)?,
        1,
        durability,
    )?;
    let genesis_bundle = bundle(&genesis, None)?;
    let created = store
        .create(CreateSession::new(
            SessionRef::new(session_id, first),
            Timestamp::new(50, 0)?,
            id(70_020)?,
            NonZeroU64::new(10).ok_or("lease")?,
            genesis_bundle.clone(),
        ))
        .await?;
    let record = store.load_line(line).await?;
    assert_eq!(record.head(), SessionRef::new(session_id, first));
    assert_eq!(record.generation(), 1);
    assert_eq!(record.origin(), &LineOrigin::Created);
    let source = Head {
        lease: created.lease().clone(),
        branch: created.branch(),
        checkpoint: genesis,
        bundle: genesis_bundle,
    };
    let source = commit(store, &source, id(70_011)?, Vec::new()).await??;

    // A rebind moves the head and the generation in its own publication.
    let second = id::<BranchId>(70_003)?;
    let target = rebind(
        store,
        &source,
        Step {
            branch: second,
            checkpoint: id(70_012)?,
            workflow: 71,
            line,
            generation: 2,
        },
        durability,
    )
    .await??;
    let record = store.load_line(line).await?;
    assert_eq!(record.head(), SessionRef::new(session_id, second));
    assert_eq!(record.generation(), 2);
    assert!(matches!(
        feed_changes(store, session_id).await?.last(),
        Some(FeedChange::Branched { line: published, new_head: true, generation: 2, .. })
            if *published == line
    ));

    Ok((source, target))
}

async fn the_source_is_read_only(
    store: &dyn SessionStore,
    source: &Head,
    durability: RuntimeDurability,
) -> TestResult<LineId> {
    let line = LineId::started_by(id::<BranchId>(70_002)?);
    // The source's lease is still live, but the branch is read-only now:
    // a commit, a prune and a second rebind are all fenced.
    let source_head_before = store.load_branch(source.branch.session()).await?.head();
    assert_eq!(
        commit(store, source, id(70_013)?, Vec::new()).await?.err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        store
            .prune(PruneBranch::new(
                source.lease.clone(),
                source.branch,
                std::collections::BTreeSet::new(),
                1,
            ))
            .await
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        rebind(
            store,
            source,
            Step {
                branch: id(70_004)?,
                checkpoint: id(70_014)?,
                workflow: 72,
                line,
                generation: 2
            },
            durability
        )
        .await?
        .err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert_eq!(
        store.load_branch(source.branch.session()).await?.head(),
        source_head_before
    );

    a_fork_is_a_new_line(store, source, durability).await
}

async fn a_fork_is_a_new_line(
    store: &dyn SessionStore,
    source: &Head,
    durability: RuntimeDurability,
) -> TestResult<LineId> {
    let session_id = id::<SessionId>(70_001)?;
    let first = id::<BranchId>(70_002)?;
    let line = LineId::started_by(first);
    // A fork of the predecessor is a new line that names where it came from.
    let forked = id::<BranchId>(70_005)?;
    let fork_line = LineId::started_by(forked);
    let fork_topology = source.checkpoint.manifest().topology();
    let fork_target = checkpoint_with_context(
        topology(
            session_id,
            forked,
            id(70_015)?,
            Some(fork_topology.checkpoint_id),
            0,
            Some(ForkLineage {
                session_id,
                branch_id: first,
                checkpoint_id: fork_topology.checkpoint_id,
                branch_revision: fork_topology.branch_revision,
            }),
            1,
        )?,
        2,
        durability,
        source
            .checkpoint
            .manifest()
            .canonical()
            .context()
            .ordinary_fork(fork_topology.checkpoint_id, session_id, forked, [73; 32])?,
        false,
    )?;
    let fork_receipt = store
        .fork(ForkBranch::new(
            workflow(73)?,
            semantic_request(&[73])?,
            source.lease.clone(),
            store
                .load_branch(source.branch.session())
                .await?
                .session()
                .revision(),
            source.branch,
            forked,
            id(70_021)?,
            NonZeroU64::new(10).ok_or("lease")?,
            bundle(&fork_target, Some(&source.bundle))?,
        ))
        .await?;
    let fork_record = store.load_line(fork_line).await?;
    assert_eq!(fork_record.generation(), 1);
    assert_eq!(
        fork_record.origin(),
        &LineOrigin::Forked {
            line,
            source: SessionRef::new(session_id, first),
            checkpoint: fork_topology.checkpoint_id,
        }
    );
    store.release(fork_receipt.lease().clone()).await?;

    Ok(fork_line)
}

async fn only_the_head_records_terminals(
    store: &dyn SessionStore,
    source: &Head,
    target: Head,
    durability: RuntimeDurability,
) -> TestResult<Head> {
    let session_id = id::<SessionId>(70_001)?;
    let line = LineId::started_by(id::<BranchId>(70_002)?);
    // Only the head records line job terminals, and only its next commit
    // consumes them; the head cannot advance while they wait.
    assert_eq!(
        store
            .record_line_job_terminals(RecordLineJobTerminals::new(
                source.lease.clone(),
                line,
                1,
                source.branch.checkpoint().checkpoint_id(),
                vec![terminal(4)?],
            ))
            .await
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    let recorded = store
        .record_line_job_terminals(RecordLineJobTerminals::new(
            target.lease.clone(),
            line,
            2,
            target.branch.checkpoint().checkpoint_id(),
            vec![terminal(4)?, terminal(6)?],
        ))
        .await?;
    assert_eq!(recorded.recorded(), [4, 6]);
    assert!(recorded.feed_seq().is_some());
    let repeated = store
        .record_line_job_terminals(RecordLineJobTerminals::new(
            target.lease.clone(),
            line,
            2,
            target.branch.checkpoint().checkpoint_id(),
            vec![terminal(4)?],
        ))
        .await?;
    assert_eq!(repeated.recorded().len(), 0);
    assert_eq!(repeated.feed_seq(), None);
    assert_eq!(
        store.line_job_terminals(line).await?,
        vec![terminal(4)?, terminal(6)?]
    );
    let third = id::<BranchId>(70_006)?;
    assert_eq!(
        rebind(
            store,
            &target,
            Step {
                branch: third,
                checkpoint: id(70_016)?,
                workflow: 74,
                line,
                generation: 3
            },
            durability
        )
        .await?
        .err(),
        Some(SessionStoreError::Conflict)
    );
    let target = commit(store, &target, id(70_017)?, vec![4, 6]).await??;
    assert_eq!(store.line_job_terminals(line).await?.len(), 0);
    assert!(
        feed_changes(store, session_id)
            .await?
            .iter()
            .any(|change| matches!(change, FeedChange::LineJobTerminals { .. }))
    );
    let third_head = rebind(
        store,
        &target,
        Step {
            branch: third,
            checkpoint: id(70_016)?,
            workflow: 74,
            line,
            generation: 3,
        },
        durability,
    )
    .await??;

    Ok(third_head)
}

async fn a_carry_continues_and_deletion_ends_the_line(
    store: &dyn SessionStore,
    third_head: &Head,
    fork_line: LineId,
    durability: RuntimeDurability,
) -> TestResult {
    let line = LineId::started_by(id::<BranchId>(70_002)?);
    // A same-store carry continues the line across sessions.
    let carried_session = id::<SessionId>(70_101)?;
    let carried_branch = id::<BranchId>(70_102)?;
    let carried_genesis = checkpoint(
        on_line(
            topology(
                carried_session,
                carried_branch,
                id(70_110)?,
                None,
                0,
                None,
                1,
            )?,
            line,
            4,
        ),
        1,
        durability,
    )?;
    let carried = store
        .create(CreateSession::new(
            SessionRef::new(carried_session, carried_branch),
            Timestamp::new(60, 0)?,
            id(70_120)?,
            NonZeroU64::new(10).ok_or("lease")?,
            bundle(&carried_genesis, None)?,
        ))
        .await?;
    let record = store.load_line(line).await?;
    assert_eq!(
        record.head(),
        SessionRef::new(carried_session, carried_branch)
    );
    assert_eq!(record.generation(), 4);
    assert!(matches!(
        feed_changes(store, carried_session).await?.first(),
        Some(FeedChange::Created { generation: 4, .. })
    ));
    assert_eq!(
        commit(store, third_head, id(70_018)?, Vec::new())
            .await?
            .err(),
        Some(SessionStoreError::NotLineHead)
    );

    // Deleting the head's session ends the line; the predecessors' session
    // stays, and its branches stay read-only.
    store.release(carried.lease().clone()).await?;
    let archived = store
        .archive(ArchiveSession::new(
            carried_session,
            carried.session().revision(),
        ))
        .await?;
    store
        .delete(DeleteSession::new(
            carried_session,
            archived.session().revision(),
        ))
        .await?;
    assert_eq!(
        store.load_line(line).await.err(),
        Some(SessionStoreError::NotFound)
    );
    assert_eq!(
        commit(store, third_head, id(70_019)?, Vec::new())
            .await?
            .err(),
        Some(SessionStoreError::NotLineHead)
    );
    assert!(store.load_line(fork_line).await.is_ok());
    let _ = FeedFact::LineJobTerminalRecorded {
        line,
        generation: 2,
        job: 4,
    };
    Ok(())
}

#[tokio::test]
async fn memory_backend_satisfies_line_conformance() -> TestResult {
    run_line_conformance(RuntimeDurability::Disabled, |clock| {
        Ok(Arc::new(plakous_store::MemorySessionStore::with_clock(
            plakous_store::MemoryStoreLimits::default(),
            clock,
        )?))
    })
    .await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_backend_satisfies_line_conformance() -> TestResult {
    let db = super::sqlite::Database::new();
    run_line_conformance(RuntimeDurability::Required, |clock| {
        Ok(Arc::new(
            plakous_store::SqliteSessionStore::open_with_clock(&db.path, clock)?,
        ))
    })
    .await?;
    // The full verification also admits every line row the corpus left.
    drop(plakous_store::SqliteSessionStore::open_verified(&db.path)?);
    Ok(())
}
