//! An admitted session-workflow compaction whose owner was lost closes at the
//! next open, in one commit carrying its `WorkflowSettled` fact, without any
//! explicit workflow and without redispatching its summary.

use super::*;
use crate::feed::{FeedFact, FeedWorkflowOutcome};

struct Row {
    label: &'static str,
    cut: fn(&DurableCompactionIdentity) -> CheckpointId,
    outcome: CompactionFixtureOutcome,
    target: fn(&DurableCompactionIdentity) -> CheckpointId,
    settled: fn(&FeedWorkflowOutcome) -> bool,
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            label: "prepared",
            cut: |identity| identity.prepared_checkpoint,
            outcome: CompactionFixtureOutcome::Summary,
            target: |identity| identity.abandonment_evidence_checkpoint,
            settled: |outcome| {
                *outcome
                    == FeedWorkflowOutcome::Abandoned {
                        summary_unsent: true,
                    }
            },
        },
        Row {
            label: "dispatched",
            cut: |identity| identity.dispatched_checkpoint,
            outcome: CompactionFixtureOutcome::Summary,
            target: |identity| identity.abandonment_evidence_checkpoint,
            settled: |outcome| {
                *outcome
                    == FeedWorkflowOutcome::Abandoned {
                        summary_unsent: false,
                    }
            },
        },
        Row {
            label: "completed",
            cut: |identity| identity.completed_checkpoint,
            outcome: CompactionFixtureOutcome::Summary,
            target: |identity| identity.success_checkpoint,
            settled: |outcome| matches!(outcome, FeedWorkflowOutcome::Compacted { .. }),
        },
        Row {
            label: "failure-evidence",
            cut: |identity| identity.failure_evidence_checkpoint,
            outcome: CompactionFixtureOutcome::DefiniteFailure(ModelError::new(
                ModelErrorKind::Protocol,
                "summary failed definitively",
            )),
            target: |identity| identity.failure_checkpoint,
            settled: |outcome| matches!(outcome, FeedWorkflowOutcome::Failed { .. }),
        },
    ]
}

fn settlements(store: &TestCreateStore, operation_id: &str) -> Vec<(usize, FeedWorkflowOutcome)> {
    lock_mutex(&store.feed_facts)
        .iter()
        .enumerate()
        .flat_map(|(index, record)| {
            record.iter().filter_map(move |fact| match fact {
                FeedFact::WorkflowSettled {
                    operation_id: Some(settled),
                    outcome,
                    ..
                } if settled == operation_id => Some((index, *outcome)),
                _ => None,
            })
        })
        .collect()
}

#[tokio::test]
async fn open_closes_each_workflow_row_in_one_commit_without_redispatch() -> TestResult {
    for (index, row) in rows().into_iter().enumerate() {
        let label = row.label;
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let entry_base = 93_100 + u128::try_from(index)? * 100;
        let (agent, source) = seeded_compaction_source(
            &sessions,
            Arc::clone(&workspace),
            &format!("create:workflow-closure-{label}"),
            entry_base,
        )
        .await?;
        let operation = format!("compact:workflow-closure-{label}");
        let identity = DurableCompactionIdentity::new(&operation, source)?;
        let cut = (row.cut)(&identity);
        store.halt_after_checkpoint(cut);
        let (builder, model) = recovery_compaction_fixture(
            Arc::clone(&workspace),
            plakous_test_support::ScriptedRecoveryState::new(),
            row.outcome,
            None,
        )?;
        let outcome = agent
            .session_endpoint()
            .compact_session(AgentCompactSessionRequest::new(
                operation.clone(),
                source.session_id,
                source.branch_id,
                source.checkpoint_id,
                AgentCompactionReason::Manual,
                builder,
            ))
            .await
            .map_err(|_| "exact source rejected")?;
        assert!(
            matches!(outcome, AgentCompactSessionOutcome::RecoveryRequired { .. }),
            "{label}: {outcome:?}"
        );
        let _ = agent.shutdown().await;
        store.resume_commits();
        store.expire_active_lease();
        let head = store.load_branch(source.session()).await?;
        assert_eq!(head.checkpoint().checkpoint().id(), cut, "{label}");
        // The test store records a halted commit's facts too, so only
        // records appended by the open count.
        let records_before = lock_mutex(&store.feed_facts).len();
        let starts = model.starts.load(Ordering::Acquire);

        let open_descriptor = durable_test_descriptor("test.route")?;
        let open_authority = artifact_authority(&open_descriptor, 1)?;
        let open_model = RecoveryCompactionModel::new(
            open_descriptor,
            open_authority.clone(),
            plakous_test_support::ScriptedRecoveryState::new(),
            CompactionFixtureOutcome::Summary,
            None,
        )?;
        let open_builder = recovery_session_builder(
            Arc::clone(&workspace),
            Arc::clone(&open_model),
            open_authority,
        );
        let address = DurableSessionAddress::new(source.session_id, source.branch_id, cut);
        let commits = store.commit_calls.load(Ordering::Acquire);
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open(
                &format!("open:workflow-closure-{label}"),
                address,
                open_builder,
            )
            .await?
        else {
            return Err(format!("{label}: the open did not close the workflow").into());
        };
        let target = (row.target)(&identity);
        assert_eq!(reopened.checkpoint().id(), target, "{label}");
        assert!(matches!(
            reopened.checkpoint().manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ));
        assert_eq!(
            store.commit_calls.load(Ordering::Acquire),
            commits + 1,
            "{label}: the closure is one commit"
        );
        let settled = settlements(&store, &operation)
            .into_iter()
            .filter(|(record, _)| *record >= records_before)
            .collect::<Vec<_>>();
        let records = lock_mutex(&store.feed_facts).len();
        match settled.as_slice() {
            [(record, outcome)] => {
                assert!((row.settled)(outcome), "{label}: {outcome:?}");
                assert_eq!(
                    *record + 1,
                    records,
                    "{label}: settled by the closure commit"
                );
            }
            other => return Err(format!("{label}: one settlement, got {other:?}").into()),
        }
        assert_eq!(model.starts.load(Ordering::Acquire), starts, "{label}");
        assert_eq!(model.recover_calls.load(Ordering::Acquire), 0, "{label}");
        assert_eq!(open_model.starts.load(Ordering::Acquire), 0, "{label}");
        assert_eq!(
            open_model.recover_calls.load(Ordering::Acquire),
            0,
            "{label}"
        );
        let receipt = sessions.compaction_receipt(&operation, source).await?;
        let receipt_target = match receipt {
            DurableCompactionReceipt::Abandoned { target, .. }
            | DurableCompactionReceipt::Compacted { target, .. }
            | DurableCompactionReceipt::Failed { target, .. } => target.checkpoint_id,
            other => return Err(format!("{label}: receipt {other:?}").into()),
        };
        assert_eq!(receipt_target, target, "{label}");
        reopened.shutdown().await?;
    }
    Ok(())
}

/// Workflow debt beside a foreign unfinished record is no row: the verdict
/// refuses it as typed integrity rather than restoring it.
#[tokio::test]
async fn a_workflow_head_no_row_admits_is_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (agent, source) = seeded_compaction_source(
        &sessions,
        Arc::clone(&workspace),
        "create:workflow-closure-refused",
        93_900,
    )
    .await?;
    let operation = "compact:workflow-closure-refused";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    store.halt_after_checkpoint(identity.prepared_checkpoint);
    let (builder, _model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let _ = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "exact source rejected")?;
    let _ = agent.shutdown().await;
    let cut = store.load_checkpoint(identity.prepared_checkpoint).await?;
    let operations = decode_paid_compaction_operations(cut.checkpoint(), identity)?;
    assert_eq!(
        crate::runtime::checkpoint_recovery_verdict(cut.checkpoint()),
        crate::runtime::CheckpointRecoveryVerdict::SettleWorkflow
    );
    // A summary proven unsent beside an open compaction record is a shape no
    // live workflow writes.
    let evidence = runtime_component(RuntimeComponentKind::Operation, b"{}".to_vec())?;
    let mut records = Vec::new();
    for record in cut.checkpoint().manifest().ledger().operations() {
        records.push(if record.operation_id() == operations.provider.0 {
            record.append_transitions([DurableOperationTransition::new(
                identity.abandonment_evidence_checkpoint,
                DurableOperationState::ProvenNotDispatched(evidence.clone()),
            )])?
        } else {
            record.clone()
        });
    }
    let poisoned = checkpoint_with_compaction_records(cut.checkpoint(), identity, records)?;
    assert_eq!(
        crate::runtime::checkpoint_recovery_verdict(&poisoned),
        crate::runtime::CheckpointRecoveryVerdict::Refuse
    );
    Ok(())
}
