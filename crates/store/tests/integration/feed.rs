//! The change-feed conformance law (spec 05 "Feed and projector laws"), run
//! over any `SessionStore`, plus the memory backend's retention bounds.

use std::collections::BTreeSet;
use std::num::{NonZeroU64, NonZeroUsize};

use plakous_agent::internal::{
    AcquireBranch, ArchiveSession, CommitCheckpoint, DeleteSession, FeedChange, FeedFact,
    FeedHeadKind, FeedPage, FeedRecord, FeedSeq, FeedTail, FeedWorkflowKind, PruneBranch,
    RuntimeContextState, RuntimeDurability, SessionRef, SessionStore, SessionStoreError,
    StoreCapacityBound,
};
use plakous_model::{BranchId, CheckpointId, RunId, SessionId};
use plakous_store::{MemoryFeedLimits, MemorySessionStore, MemoryStoreLimits};

use crate::conformance::{
    ForkTarget, PruneChain, TestResult, bundle, chain_payload, checkpoint_with_payload, id,
    ordinary_fork, prune_chain, topology, workflow,
};

const DURABILITY: RuntimeDurability = RuntimeDurability::Disabled;

fn page(limit: usize) -> TestResult<NonZeroUsize> {
    Ok(NonZeroUsize::new(limit).ok_or("nonzero page")?)
}

async fn records(
    store: &dyn SessionStore,
    session_id: SessionId,
    after: u64,
) -> TestResult<(Vec<FeedRecord>, FeedTail)> {
    match store
        .read_feed(session_id, FeedSeq::new(after), page(256)?)
        .await?
    {
        FeedPage::Records { records, window } => Ok((records, window)),
        FeedPage::BehindRetention { .. } => Err("cursor fell behind retention".into()),
    }
}

async fn tail(store: &dyn SessionStore, session_id: SessionId) -> TestResult<u64> {
    Ok(store.feed_tail(session_id).await?.tail().get())
}

/// The chain's next commit, carrying `facts`.
fn next_commit(
    chain: &PruneChain,
    revision: u64,
    checkpoint_id: CheckpointId,
    facts: Vec<FeedFact>,
    durability: RuntimeDurability,
) -> TestResult<(CommitCheckpoint, plakous_agent::internal::RuntimeCheckpoint)> {
    let selector = chain.created.branch().session();
    let next = checkpoint_with_payload(
        topology(
            selector.session_id(),
            selector.branch_id(),
            checkpoint_id,
            Some(chain.head.checkpoint().checkpoint_id()),
            revision,
            None,
            chain.created.lease().fence().get(),
        )?,
        2,
        durability,
        RuntimeContextState::genesis(selector.session_id(), selector.branch_id(), [0x51; 32])?,
        false,
        &chain_payload(revision, 512),
    )?;
    let request = CommitCheckpoint::new(
        chain.created.lease().clone(),
        chain.head,
        bundle(&next, Some(&chain.prior))?,
    )
    .with_feed_facts(facts);
    Ok((request, next))
}

#[allow(
    clippy::too_many_lines,
    reason = "one law walks every mutation kind in order against one feed"
)]
pub(crate) async fn run_feed_conformance(
    store: &dyn SessionStore,
    durability: RuntimeDurability,
) -> TestResult {
    let base = 700;
    let session_id = id::<SessionId>(base)?;
    let source_branch = id::<BranchId>(base + 1)?;
    let source = SessionRef::new(session_id, source_branch);
    let mut chain = prune_chain(store, durability, base, 3, 512).await?;

    // Create and every commit appended one record each, gap-free from 1.
    let (created, window) = records(store, session_id, 0).await?;
    assert_eq!(window.tail(), FeedSeq::new(4));
    assert_eq!(window.retained_from(), FeedSeq::new(1));
    assert_eq!(chain.created.feed_seq(), FeedSeq::new(1));
    assert_eq!(
        created[0].header().change,
        FeedChange::Created {
            genesis: chain.ids[0],
            revision: plakous_agent::internal::BranchRevision::INITIAL,
        }
    );
    for (index, record) in created.iter().enumerate() {
        let header = record.header();
        assert_eq!(header.seq, FeedSeq::new(u64::try_from(index)? + 1));
        assert_eq!(
            (header.session_id, header.branch_id),
            (session_id, source_branch)
        );
        assert!(header.committed_at_ms > 0, "committed_at_ms is stamped");
        if index > 0 {
            assert_eq!(
                header.change,
                FeedChange::Advanced {
                    parent: chain.ids[index - 1],
                    checkpoint: chain.ids[index],
                    revision: plakous_agent::internal::BranchRevision::from_store(u64::try_from(
                        index
                    )?),
                    head: FeedHeadKind::Quiescent,
                }
            );
        }
    }

    // Exact retries and recovery return the original sequence and append nothing.
    let retried = store.commit(chain.commits[0].clone()).await?;
    assert_eq!(retried.feed_seq(), FeedSeq::new(2));
    assert_eq!(
        store
            .recover_commit(chain.ids[2])
            .await?
            .receipt()
            .feed_seq(),
        FeedSeq::new(3)
    );
    let lease = chain.created.lease().clone();
    let renewed = store.renew(lease).await?;
    assert_eq!(
        tail(store, session_id).await?,
        4,
        "retry, recovery and renewal append nothing"
    );

    // A refused mutation appends nothing; a record above the bound is refused
    // before anything is published.
    let stale_head = chain.created.branch();
    let (mut stale, _) = next_commit(&chain, 4, id(base + 150)?, Vec::new(), durability)?;
    stale = CommitCheckpoint::new(renewed.clone(), stale_head, stale.checkpoint().clone());
    assert!(
        store.commit(stale).await.is_err(),
        "a stale head is refused"
    );
    let oversized = vec![FeedFact::SessionCreated {
        operation_id: "x".repeat(plakous_agent::internal::MAX_FEED_RECORD_BYTES),
        seeded: false,
    }];
    let (mut too_large, _) = next_commit(&chain, 4, id(base + 151)?, oversized, durability)?;
    too_large = CommitCheckpoint::new(renewed.clone(), chain.head, too_large.checkpoint().clone())
        .with_feed_facts(too_large.feed_facts().to_vec());
    let refused = store.commit(too_large).await;
    assert!(
        matches!(refused, Err(SessionStoreError::Capacity(capacity)) if capacity.bound == StoreCapacityBound::FeedRecordBytes),
        "{refused:?}"
    );
    assert_eq!(store.load_branch(source).await?.head(), chain.head);
    assert_eq!(tail(store, session_id).await?, 4);

    // A commit's record carries the facts the runtime supplied, verbatim.
    let facts = vec![FeedFact::WorkflowAdmitted {
        workflow_id: workflow(40)?,
        operation_id: Some("fork:40".to_owned()),
        kind: FeedWorkflowKind::Fork,
    }];
    let (commit, next) = next_commit(&chain, 4, id(base + 152)?, facts.clone(), durability)?;
    let commit = CommitCheckpoint::new(renewed.clone(), chain.head, commit.checkpoint().clone())
        .with_feed_facts(facts.clone());
    let receipt = store.commit(commit.clone()).await?;
    assert_eq!(receipt.feed_seq(), FeedSeq::new(5));
    chain.head = receipt.head();
    chain.prior = commit.checkpoint().clone();
    chain.last = next;
    let (fifth, _) = records(store, session_id, 4).await?;
    assert_eq!(fifth.len(), 1);
    assert_eq!(fifth[0].facts(), facts.as_slice());

    // A fork's record is on the target branch and names its source.
    let target_branch = id::<BranchId>(base + 60)?;
    let fork_facts = vec![FeedFact::WorkflowAdmitted {
        workflow_id: workflow(41)?,
        operation_id: Some("fork:41".to_owned()),
        kind: FeedWorkflowKind::Fork,
    }];
    let fork_chain = chain;
    let fork = {
        let session = store.load_branch(source).await?.session();
        let mut request = ordinary_fork(
            &fork_chain,
            ForkTarget {
                branch: target_branch,
                checkpoint: id(base + 61)?,
                owner: id::<RunId>(base + 62)?,
                workflow: workflow(41)?,
                session_revision: session.revision(),
                source_revision: fork_chain.head.revision().get(),
            },
            durability,
        )?;
        request = plakous_agent::internal::ForkBranch::new(
            request.workflow_id(),
            request.semantic_request(),
            renewed.clone(),
            request.expected_session_revision(),
            request.source(),
            request.target_branch(),
            request.owner(),
            request.lease_seconds(),
            request.target().clone(),
        )
        .with_feed_facts(fork_facts.clone());
        request
    };
    let forked = store.fork(fork.clone()).await?;
    assert_eq!(forked.feed_seq(), FeedSeq::new(6));
    assert_eq!(store.fork(fork).await?.feed_seq(), FeedSeq::new(6));
    let (sixth, _) = records(store, session_id, 5).await?;
    let header = sixth[0].header();
    assert_eq!(header.branch_id, target_branch);
    assert!(matches!(
        header.change,
        FeedChange::Branched { source, target, .. }
            if source.branch_id == source_branch
                && source.checkpoint_id == fork_chain.head.checkpoint().checkpoint_id()
                && target == id::<CheckpointId>(base + 61)?
    ));
    assert_eq!(sixth[0].facts(), fork_facts.as_slice());

    // Releases append one record each; an exact repetition appends nothing.
    store.release(forked.lease().clone()).await?;
    store.release(renewed.clone()).await?;
    store.release(renewed.clone()).await?;
    assert_eq!(tail(store, session_id).await?, 8);
    let (released, _) = records(store, session_id, 6).await?;
    assert_eq!(
        released[1].header().change,
        FeedChange::LeaseReleased {
            head: fork_chain.head.checkpoint().checkpoint_id(),
            fence: renewed.fence().get(),
        }
    );

    // Acquisition appends nothing; a prune that reclaims appends one record.
    let owner = store
        .acquire(AcquireBranch::new(
            source,
            id::<RunId>(base + 70)?,
            NonZeroU64::new(1_000).ok_or("nonzero lease")?,
        ))
        .await?;
    assert_eq!(tail(store, session_id).await?, 8);
    let pruned = store
        .prune(PruneBranch::new(
            owner.clone(),
            fork_chain.head,
            BTreeSet::new(),
            1,
        ))
        .await?;
    assert!(pruned.pruned_checkpoints() > 0);
    let (prune_record, _) = records(store, session_id, 8).await?;
    assert!(matches!(
        prune_record[0].header().change,
        FeedChange::Pruned { head, .. } if head == fork_chain.head.checkpoint().checkpoint_id()
    ));

    // Archive appends one record naming the initial branch.
    store.release(owner).await?;
    let revision = store.load_branch(source).await?.session().revision();
    store
        .archive(ArchiveSession::new(session_id, revision))
        .await?;
    let (all, window) = records(store, session_id, 0).await?;
    assert_eq!(window.tail(), FeedSeq::new(11));
    assert_eq!(
        all.iter()
            .map(|record| record.header().seq.get())
            .collect::<Vec<_>>(),
        (1..=11).collect::<Vec<_>>(),
        "gap-free across branches"
    );
    let archived = all[10].header();
    assert_eq!(archived.branch_id, source_branch);
    assert_eq!(
        archived.change,
        FeedChange::Archived {
            revision: revision.checked_next().ok_or("revision")?,
        }
    );

    // Pages are bounded and the tail reads empty.
    match store.read_feed(session_id, FeedSeq::ZERO, page(3)?).await? {
        FeedPage::Records { records, .. } => assert_eq!(records.len(), 3),
        FeedPage::BehindRetention { .. } => return Err("unexpected retention".into()),
    }
    assert!(records(store, session_id, 11).await?.0.is_empty());

    // Deletion removes the feed with the rest of the session's authority.
    let archived_revision = revision.checked_next().ok_or("revision")?;
    store
        .delete(DeleteSession::new(session_id, archived_revision))
        .await?;
    assert_eq!(
        store.feed_tail(session_id).await,
        Err(SessionStoreError::NotFound)
    );
    Ok(())
}

#[tokio::test]
async fn memory_feed_conformance() -> TestResult {
    let store = MemorySessionStore::new(MemoryStoreLimits::default())?;
    run_feed_conformance(&store, DURABILITY).await
}

/// Retention trims a prefix by the record bound, in bounded batches after the
/// publishing commit, and reads below it as behind retention.
#[tokio::test]
async fn memory_feed_trims_the_oldest_records_in_batches() -> TestResult {
    let limits = MemoryStoreLimits::default().with_feed_limits(MemoryFeedLimits::new(
        16,
        u64::MAX,
        page(2)?,
    ));
    let store = MemorySessionStore::new(limits)?;
    // Create plus 15 commits fills the bound without trimming.
    let chain = prune_chain(&store, DURABILITY, 800, 15, 8).await?;
    let session_id = chain.created.branch().session().session_id();
    let window = store.feed_tail(session_id).await?;
    assert_eq!((window.retained_from().get(), window.tail().get()), (1, 16));
    assert_eq!(store.stats()?.feed_trims(), 0);

    // The 17th record exceeds it: trim to 7/8 (14) in batches of two.
    let mut chain = chain;
    chain.ids.push(id(800 + 100 + 16)?);
    let chain = crate::conformance::extend_chain(&store, DURABILITY, chain, 16, 8).await?;
    let window = store.feed_tail(session_id).await?;
    assert_eq!((window.retained_from().get(), window.tail().get()), (4, 17));
    assert_eq!(store.stats()?.feed_trims(), 2);

    // A cursor below the window is behind retention and gets the heads.
    match store
        .read_feed(session_id, FeedSeq::new(2), page(8)?)
        .await?
    {
        FeedPage::BehindRetention { window, heads } => {
            assert_eq!(window.retained_from(), FeedSeq::new(4));
            assert_eq!(heads, [chain.head]);
        }
        FeedPage::Records { .. } => return Err("a trimmed cursor read records".into()),
    }
    let (retained, _) = records(&store, session_id, 3).await?;
    assert_eq!(
        retained
            .iter()
            .map(|record| record.header().seq.get())
            .collect::<Vec<_>>(),
        (4..=17).collect::<Vec<_>>()
    );
    Ok(())
}

/// The byte bound trims independently of the record bound.
#[tokio::test]
async fn memory_feed_trims_by_encoded_bytes() -> TestResult {
    let limits = MemoryStoreLimits::default().with_feed_limits(MemoryFeedLimits::new(
        u64::MAX,
        2_048,
        page(4_096)?,
    ));
    let store = MemorySessionStore::new(limits)?;
    let chain = prune_chain(&store, DURABILITY, 900, 12, 8).await?;
    let session_id = chain.created.branch().session().session_id();
    let window = store.feed_tail(session_id).await?;
    assert_eq!(window.tail().get(), 13);
    assert!(window.retained_from().get() > 1);
    let (retained, _) = records(&store, session_id, window.retained_from().get() - 1).await?;
    let bytes: usize = retained
        .iter()
        .map(|record| record.encode().map(|bytes| bytes.len()))
        .sum::<Result<_, _>>()?;
    assert!(bytes <= 2_048, "{bytes} retained bytes");
    Ok(())
}
