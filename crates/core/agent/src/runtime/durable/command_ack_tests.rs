use super::*;
use crate::{CommandAckDisposition, ControlErrorKind};

#[derive(Clone, Copy)]
enum AdmissionEvidence {
    FeedOnly,
    CheckpointOnly,
    Reclaimed,
}

async fn killed_owner_ack(evidence: AdmissionEvidence) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 48_000)?;
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let original = runtime
        .create(
            "create:ack-reopen",
            durable_artifact_test_builder(Arc::clone(&workspace), Arc::clone(&model)),
        )
        .await?;
    let root = CommandId::new("root:ack-reopen")?;
    let submission = original.start(root.clone(), "accepted").await?;
    let root_seq = submission
        .acknowledgement()
        .feed_seq()
        .ok_or("root sequence")?;
    let root_terminal = submission
        .into_turn()
        .ok_or("root turn")?
        .result()
        .await?
        .checkpoint_id();
    let rejected = CommandId::new("rejected:ack-reopen")?;
    let rejection = original
        .command(TurnCommand::Start {
            command_id: rejected.clone(),
            input: String::new().into(),
        })
        .await?;
    let rejected_seq = rejection
        .acknowledgement()
        .feed_seq()
        .ok_or("rejection sequence")?;
    let rejected_kind = rejection
        .acknowledgement()
        .rejection()
        .ok_or("recorded rejection")?
        .kind();
    // Move both admissions behind the head and its immediate parent.
    original.prompt("move head").await?.result().await?;
    let checkpoint = original.checkpoint();
    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), checkpoint.id());
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.expire_active_lease();
    if matches!(
        evidence,
        AdmissionEvidence::CheckpointOnly | AdmissionEvidence::Reclaimed
    ) {
        let tail = lock_mutex(&store.feed_records)
            .last()
            .ok_or("feed tail")?
            .header()
            .seq
            .get();
        store.feed_retained_from.store(tail + 1, Ordering::Release);
    }
    if matches!(
        evidence,
        AdmissionEvidence::FeedOnly | AdmissionEvidence::Reclaimed
    ) {
        let parent = checkpoint.manifest().topology().parent_checkpoint_id;
        let keep = |id: &CheckpointId| {
            *id == checkpoint.id() || Some(*id) == parent || *id == root_terminal
        };
        let mut state = lock_mutex(&store.state);
        if matches!(evidence, AdmissionEvidence::Reclaimed) {
            state.commits.retain(|id, _| keep(id));
        }
        state.stored_commits.retain(|id, _| keep(id));
    }
    let DurableRuntimeOpen::Opened(reopened) = runtime
        .open(
            "open:ack-reopen",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?
    else {
        return Err("quiescent reopen".into());
    };
    for (command, text, seq, rejection_kind) in [
        (root.clone(), "accepted", root_seq, None),
        (rejected, "", rejected_seq, Some(rejected_kind)),
    ] {
        let duplicate = reopened
            .command(TurnCommand::Start {
                command_id: command,
                input: text.into(),
            })
            .await?;
        let ack = duplicate.acknowledgement();
        assert!(duplicate.turn().is_none());
        if matches!(
            evidence,
            AdmissionEvidence::Reclaimed | AdmissionEvidence::CheckpointOnly
        ) {
            assert_eq!(ack.disposition(), CommandAckDisposition::Rejected);
            assert_eq!(
                ack.rejection().map(ControlError::kind),
                Some(ControlErrorKind::Reclaimed)
            );
            assert_eq!(ack.feed_seq(), None);
        } else {
            assert_eq!(ack.disposition(), CommandAckDisposition::Duplicate);
            assert_eq!(
                ack.feed_seq(),
                Some(seq),
                "a rebuilt ack keeps its original admission sequence"
            );
            assert_eq!(ack.rejection().map(ControlError::kind), rejection_kind);
        }
    }
    let conflict = reopened
        .command(TurnCommand::Start {
            command_id: root,
            input: "different binding".into(),
        })
        .await?;
    assert_eq!(
        conflict
            .acknowledgement()
            .rejection()
            .map(ControlError::kind),
        Some(ControlErrorKind::IdentityConflict)
    );
    assert_eq!(
        reopened.checkpoint().id(),
        checkpoint.id(),
        "resubmissions do not publish another admission"
    );
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn killed_owner_duplicate_ack_recovers_from_retained_feed() -> TestResult {
    killed_owner_ack(AdmissionEvidence::FeedOnly).await
}

#[tokio::test]
async fn killed_owner_duplicate_ack_is_reclaimed_below_feed_retention_with_checkpoint_retained()
-> TestResult {
    killed_owner_ack(AdmissionEvidence::CheckpointOnly).await
}

#[tokio::test]
async fn killed_owner_ack_is_reclaimed_when_feed_and_admission_checkpoint_are_gone() -> TestResult {
    killed_owner_ack(AdmissionEvidence::Reclaimed).await
}

#[tokio::test]
async fn live_and_reopened_duplicates_below_feed_retention_are_reclaimed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let model = artifact_test_model(0, 48_500)?;
    let agent = runtime
        .create(
            "create:live-trim",
            durable_artifact_test_builder(workspace.clone(), model.clone()),
        )
        .await?;
    let command = CommandId::new("root:live-trim")?;
    agent
        .start(command.clone(), "accepted")
        .await?
        .into_turn()
        .ok_or("turn")?
        .result()
        .await?;
    let tail = lock_mutex(&store.feed_records)
        .last()
        .ok_or("tail")?
        .header()
        .seq
        .get();
    store.feed_retained_from.store(tail + 1, Ordering::Release);
    let live = agent
        .command(TurnCommand::Start {
            command_id: command.clone(),
            input: "accepted".into(),
        })
        .await?;
    assert_eq!(
        live.acknowledgement().rejection().map(ControlError::kind),
        Some(ControlErrorKind::Reclaimed)
    );
    assert_eq!(live.acknowledgement().feed_seq(), None);
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let DurableRuntimeOpen::Opened(reopened) = runtime
        .open(
            "open:live-trim",
            address,
            durable_artifact_test_builder(workspace, model),
        )
        .await?
    else {
        return Err("trimmed head did not reopen".into());
    };
    let duplicate = reopened
        .command(TurnCommand::Start {
            command_id: command,
            input: "accepted".into(),
        })
        .await?;
    assert_eq!(
        duplicate
            .acknowledgement()
            .rejection()
            .map(ControlError::kind),
        Some(ControlErrorKind::Reclaimed)
    );
    assert_eq!(duplicate.acknowledgement().feed_seq(), None);
    reopened.shutdown().await?;
    Ok(())
}
#[tokio::test]
async fn durable_open_reads_bounded_ancestry_pages_without_unrelated_tail() -> TestResult {
    for (trimmed, pruned) in [(false, false), (false, true), (true, true)] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let runtime = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let model = artifact_test_model(0, 48_600)?;
        let agent = runtime
            .create(
                "create:open-budget",
                durable_artifact_test_builder(workspace.clone(), model.clone()),
            )
            .await?;
        for _ in 0..12 {
            agent.prompt("advance").await?.result().await?;
        }
        let checkpoint = agent.checkpoint();
        let address =
            DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
        agent.shutdown().await?;
        let head_seq = lock_mutex(&store.state)
            .commits
            .get(&checkpoint.id())
            .ok_or("head receipt")?
            .receipt()
            .feed_seq()
            .get();
        {
            let mut records = lock_mutex(&store.feed_records);
            let original = records.last().ok_or("tail")?.clone();
            for offset in 1..=4_096 {
                let mut header = *original.header();
                header.branch_id = BranchId::try_from(uuid::Uuid::from_u128(0xA11_515))?;
                header.seq = crate::feed::FeedSeq::new(header.seq.get() + offset);
                header.change = crate::feed::FeedChange::LeaseReleased {
                    head: checkpoint.id(),
                    fence: offset,
                };
                records.push(crate::feed::FeedRecord::from_store(header, Vec::new()));
            }
            if trimmed {
                store.feed_retained_from.store(
                    records.last().ok_or("tail")?.header().seq.get(),
                    Ordering::Release,
                );
            }
            if pruned {
                let parent = checkpoint.manifest().topology().parent_checkpoint_id;
                let mut state = lock_mutex(&store.state);
                lock_mutex(&store.pruned_commits).extend(
                    state
                        .commits
                        .keys()
                        .filter(|id| **id != checkpoint.id() && Some(**id) != parent)
                        .copied(),
                );
                state
                    .commits
                    .retain(|id, _| *id == checkpoint.id() || Some(*id) == parent);
                state
                    .stored_commits
                    .retain(|id, _| *id == checkpoint.id() || Some(*id) == parent);
            }
        }
        lock_mutex(&store.feed_read_budgets).clear();
        lock_mutex(&store.feed_read_ranges).clear();
        store.checkpoint_loads.store(0, Ordering::Release);
        let DurableRuntimeOpen::Opened(reopened) = runtime
            .open(
                "open:budget",
                address,
                durable_artifact_test_builder(workspace, model),
            )
            .await?
        else {
            return Err("budget head did not reopen".into());
        };
        let budgets = lock_mutex(&store.feed_read_budgets).clone();
        assert_ne!(budgets, [] as [usize; 0]);
        assert!(budgets.iter().all(|budget| *budget <= 256), "{budgets:?}");
        if !pruned {
            // The head's receipt bounds one page that holds all twelve turns.
            assert_eq!(budgets.len(), 1, "{budgets:?}");
        }
        if pruned && !trimmed {
            assert!(budgets.iter().any(|budget| *budget > 1));
        }
        assert!(
            lock_mutex(&store.feed_read_ranges)
                .iter()
                .all(
                    |(after, limit)| *after + u64::try_from(*limit).unwrap_or(u64::MAX) <= head_seq
                ),
            "read beyond head into sibling branch tail"
        );
        assert!(
            budgets.len() < 100,
            "unrelated retained feed was scanned: {budgets:?}"
        );
        let loads = store.checkpoint_loads.load(Ordering::Acquire);
        assert!(
            loads <= if trimmed { 5 } else { 1 },
            "checkpoint read budget: {loads}"
        );
        reopened.shutdown().await?;
    }
    Ok(())
}
