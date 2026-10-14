//! Host loss while a Code Cell is running, cut from the real driver.
//!
//! Each case runs the actual turn loop up to one point inside a cell, keeps the
//! exact durable prefix the store holds at that moment (as a `SIGKILL` would),
//! and opens it in a fresh runtime. The reopen must close the turn itself —
//! abandoned, never replayed — so a product holding the pre-turn locator may
//! adopt the head and the next prompt runs.

use super::*;

#[derive(Clone, Copy, Debug)]
enum CrashPoint {
    /// The model asked for a cell and the host was lost before the cell's
    /// evaluation was durably prepared: `exec` was already shown as started.
    BeforeCellPrepared,
    /// Execution startup admitted the source; no nested call exists. The
    /// fixture drives this shape over the product JavaScript and Python
    /// presentations; the ledger is the same for native `exec`.
    BeforeNested,
    /// A nested mutating tool is running inside the cell.
    NestedToolRunning,
    /// A nested `exec_command` process is running inside the cell.
    NestedProcessRunning,
    /// A nested mutating tool returned; the cell is still running.
    NestedToolReturned,
    /// The effect is completed, but its child and evaluation remain dispatched.
    NestedEffectCompleted,
    /// The child is prepared, before its effect is prepared or dispatched.
    NestedChildPrepared,
    /// A nested `exec_command` yielded with its process still running; the
    /// cell is still running.
    NestedProcessYielded,
}

#[derive(Clone, Copy, Debug)]
enum CellLanguage {
    Native,
    JavaScript,
    Python,
}

impl CrashPoint {
    const fn mode(self) -> CancelCellMode {
        match self {
            Self::BeforeNested => CancelCellMode::ExecutePending,
            Self::NestedToolRunning
            | Self::NestedToolReturned
            | Self::NestedEffectCompleted
            | Self::NestedChildPrepared => CancelCellMode::ExecInFlight,
            Self::NestedProcessRunning | Self::BeforeCellPrepared => {
                CancelCellMode::ProcessInFlight
            }
            Self::NestedProcessYielded => CancelCellMode::ProcessYield,
        }
    }
}

fn crash_evidence(point: CrashPoint, language: CellLanguage) -> Arc<CancelCellEvidence> {
    let evidence = Arc::new(CancelCellEvidence::default());
    if matches!(language, CellLanguage::Python) {
        evidence.python_fixture.store(true, Ordering::Release);
    }
    if matches!(point, CrashPoint::NestedToolReturned) {
        // The nested mutation waits for `release_block`, then settles once.
        evidence.steer_fixture.store(true, Ordering::Release);
    }
    evidence
}

fn describe_obligations(checkpoint: &RuntimeCheckpoint) -> String {
    let manifest = checkpoint.manifest();
    let mut lines = vec![format!("lifecycle {:?}", manifest.lifecycle())];
    lines.extend(manifest.ledger().operations().into_iter().map(|record| {
        format!(
            "op {} {:?} -> {}",
            record.operation_id(),
            record.kind(),
            durable_state_name(record.state())
        )
    }));
    lines.join("\n")
}

async fn reach(
    point: CrashPoint,
    evidence: &CancelCellEvidence,
    events: &mut TurnEvents,
) -> TestResult {
    match point {
        CrashPoint::BeforeNested => wait_for_scheduler_fact(&evidence.execute_started).await,
        CrashPoint::BeforeCellPrepared => loop {
            let event = events.recv().await?;
            if matches!(event.kind(), AgentEventKind::ToolStarted { .. }) {
                break;
            }
        },
        CrashPoint::NestedToolRunning => wait_for_scheduler_fact(&evidence.block_started).await,
        CrashPoint::NestedProcessRunning => loop {
            let event = events.recv().await?;
            if let AgentEventKind::ToolStarted { identity, .. } = event.kind()
                && identity.canonical_name().as_str() == PROCESS_SESSION_CAPABILITY_NAME
            {
                break;
            }
        },
        CrashPoint::NestedEffectCompleted | CrashPoint::NestedChildPrepared => {}
        CrashPoint::NestedToolReturned => {
            wait_for_scheduler_fact(&evidence.block_started).await;
            evidence.release_block.store(true, Ordering::Release);
            while !lock_mutex(&evidence.deliveries)
                .iter()
                .any(|(id, ok, _)| id == "BLOCK" && *ok)
            {
                tokio::task::yield_now().await;
            }
        }
        CrashPoint::NestedProcessYielded => {
            while !lock_mutex(&evidence.deliveries)
                .iter()
                .any(|(id, ok, _)| id == "SLEEP" && *ok)
            {
                tokio::task::yield_now().await;
            }
        }
    }
    Ok(())
}

/// Opens a session, optionally leaves a live job from a cancelled first cell,
/// runs a cell to `point`, cuts the store there, and reopens the cut head.
async fn crash_and_reopen(
    point: CrashPoint,
    language: CellLanguage,
    prior_job: bool,
) -> TestResult {
    let label = format!("{point:?}/{language:?}/prior_job={prior_job}");
    let directory = tempfile::tempdir()?;
    let evidence = crash_evidence(point, language);
    evidence
        .second_exec_fixture
        .store(prior_job, Ordering::Release);
    let mode = point.mode();
    let store = Arc::new(TestCreateStore::default());
    store.block_after_completed_nested_effect.store(
        matches!(point, CrashPoint::NestedEffectCompleted),
        Ordering::Release,
    );
    store.block_after_prepared_nested_child.store(
        matches!(point, CrashPoint::NestedChildPrepared),
        Ordering::Release,
    );
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (builder, _) = cancel_cell_builder(mode, Arc::clone(&evidence), directory.path())?;
    let agent = sessions.create("create:cell-crash", builder).await?;
    store.block_after_completed_round.store(
        matches!(point, CrashPoint::BeforeCellPrepared),
        Ordering::Release,
    );
    if prior_job {
        // A cancelled cell whose nested process is still running leaves it
        // as a live job of the session.
        let mut first = agent.prompt("start a process").await?;
        let mut first_events = first.take_events()?;
        tokio::time::timeout(
            Duration::from_secs(15),
            reach(point, &evidence, &mut first_events),
        )
        .await
        .map_err(|_| format!("{label}: the first cell never started its process"))??;
        first.control().cancel().await?;
        first.result().await?;
        assert_eq!(agent.jobs().len(), 1, "{label}: no live job to carry");
    }
    let pre_turn = agent.checkpoint().id();
    let mut turn = agent.prompt("open a cell").await?;
    let turn_id = turn.id();
    let mut events = turn.take_events()?;
    let reached = if prior_job {
        // The second cell has started; its nested work is not the subject.
        tokio::time::timeout(
            Duration::from_secs(15),
            wait_for_scheduler_count(&evidence.executes, 2),
        )
        .await
        .map_err(|_| "never started")
    } else {
        tokio::time::timeout(
            Duration::from_secs(15),
            reach(point, &evidence, &mut events),
        )
        .await
        .map_err(|_| "never reached")
        .and_then(|reached| reached.map_err(|_| "event stream ended"))
    };
    reached.map_err(|reason| {
        format!(
            "{label}: the cell {reason} the crash point\n{}",
            describe_obligations(&agent.checkpoint())
        )
    })?;
    if matches!(
        point,
        CrashPoint::NestedEffectCompleted | CrashPoint::NestedChildPrepared
    ) {
        tokio::time::timeout(
            Duration::from_secs(15),
            wait_for_scheduler_fact(&store.commits_blocked),
        )
        .await?;
    }
    // Let the driver publish whatever it publishes at this point.
    if !matches!(
        point,
        CrashPoint::NestedEffectCompleted | CrashPoint::NestedChildPrepared
    ) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    assert_ne!(
        source.checkpoint_id, pre_turn,
        "{label}: the turn left no head"
    );
    // The exact durable prefix survives; the process-local driver, cell and
    // interpreter do not, and the lease expires as it would for a new process.
    let crashed = Arc::new(TestCreateStore::default());
    crashed
        .validate_candidate_children
        .store(true, Ordering::Release);
    *lock_mutex(&crashed.state) = lock_mutex(&store.state).clone();
    *lock_mutex(&crashed.genesis) = lock_mutex(&store.genesis).clone();
    *lock_mutex(&crashed.feed_records) = lock_mutex(&store.feed_records).clone();
    crashed.feed_retained_from.store(
        store.feed_retained_from.load(Ordering::Acquire),
        Ordering::Release,
    );
    lock_mutex(&crashed.state).active_lease = None;
    agent.begin_abandon();
    agent.abort_driver_for_test();
    drop(turn);
    drop(agent);

    let head = crashed.load_checkpoint(source.checkpoint_id).await?;
    let head = head.checkpoint();
    let shape = describe_obligations(head);
    if matches!(
        point,
        CrashPoint::NestedEffectCompleted | CrashPoint::NestedChildPrepared
    ) {
        let records = head.manifest().ledger().operations();
        let expected = if matches!(point, CrashPoint::NestedChildPrepared) {
            vec![
                (1, "Dispatched"),
                (2, "Completed"),
                (3, "Dispatched"),
                (4, "Prepared"),
            ]
        } else {
            vec![
                (1, "Dispatched"),
                (2, "Completed"),
                (3, "Dispatched"),
                (4, "Dispatched"),
                (5, "Completed"),
            ]
        };
        assert_eq!(records.len(), expected.len(), "{label}\n{shape}");
        for (record, (id, state)) in records.iter().zip(expected) {
            assert_eq!(record.operation_id(), id, "{label}\n{shape}");
            assert_eq!(
                durable_state_name(record.state()),
                state,
                "{label}\n{shape}"
            );
        }
    }
    // A root's product record lags the head, so it adopts the head only when
    // the reopen will close it; a child opens the adopted head and needs the
    // same answer from the open's own gate.
    assert!(
        crate::runtime::checkpoint_recovers_an_abandoned_turn(head),
        "{label}: a product holding the pre-turn locator may not adopt this head\n{shape}"
    );
    assert!(
        crate::runtime::checkpoint_closes_on_reopen(head),
        "{label}: an exact open of this head would report recovery-required\n{shape}"
    );
    // Operations the cell may have started: the evaluation, its nested calls,
    // their effects and processes.
    let uncertain = head
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::CompositionEvaluation { .. }
                    | DurableOperationKind::CompositionChild { .. }
                    | DurableOperationKind::Effect { .. }
                    | DurableOperationKind::ProcessSession { .. }
            ) && matches!(
                record.state(),
                DurableOperationState::Dispatched
                    | DurableOperationState::Claimed(_)
                    | DurableOperationState::LocatorBound(_)
            )
        })
        .map(DurableOperationRecord::operation_id)
        .collect::<Vec<_>>();
    let before_cell = matches!(point, CrashPoint::BeforeCellPrepared);
    // Before the cell is prepared the head holds only the landed round.
    assert_eq!(uncertain.is_empty(), before_cell, "{label}\n{shape}");
    if before_cell {
        assert!(
            head.manifest()
                .ledger()
                .operations()
                .into_iter()
                .all(|record| matches!(
                    record.kind(),
                    DurableOperationKind::Command { .. } | DurableOperationKind::Provider { .. }
                )),
            "{label}: the cut is not before the cell\n{shape}"
        );
    }

    let reopening = DurableSessionRuntime::current(
        crashed.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let fresh = crash_evidence(point, language);
    let (builder, model) = cancel_cell_builder(mode, Arc::clone(&fresh), directory.path())?;
    let reopened = match reopening.open("open:cell-crash", source, builder).await? {
        DurableRuntimeOpen::Opened(agent) => agent,
        DurableRuntimeOpen::RestoredWorking { .. } => {
            return Err(format!("{label}: a vanished cell was restored\n{shape}").into());
        }
        DurableRuntimeOpen::RecoveryRequired { .. } => {
            return Err(format!("{label}: the reopen did not close the turn\n{shape}").into());
        }
    };
    let report = reopened
        .abandoned_turn()
        .ok_or_else(|| format!("{label}: the reopen reported no abandoned turn"))?;
    assert_eq!(
        report.cause(),
        crate::AbandonmentCause::ProcessLost,
        "{label}"
    );
    // Even before a nested call the interpreter may have evaluated source, so
    // the cell is as uncertain as any running tool. A cell never prepared never
    // started: nothing is prepared after it would have been dispatched.
    assert_eq!(
        report.claim(),
        if before_cell {
            crate::AbandonedTurnClaim::NothingStarted
        } else {
            crate::AbandonedTurnClaim::RunningTool
        },
        "{label}"
    );
    let reopened_head = reopened.checkpoint();
    if matches!(
        point,
        CrashPoint::NestedEffectCompleted | CrashPoint::NestedChildPrepared
    ) {
        for prior in head.manifest().ledger().operations() {
            if let DurableOperationState::Completed(evidence) = prior.state() {
                let records = reopened_head.manifest().ledger().operations();
                let current = records
                    .iter()
                    .find(|record| record.operation_id() == prior.operation_id())
                    .ok_or("completed operation vanished")?;
                assert_eq!(
                    current.state(),
                    &DurableOperationState::Finalized(evidence.clone())
                );
            }
        }
    }
    let after = describe_obligations(&reopened_head);
    if matches!(point, CrashPoint::NestedChildPrepared) {
        let records = reopened_head.manifest().ledger().operations();
        let child = records
            .iter()
            .find(|record| record.operation_id() == 4)
            .ok_or("prepared child vanished")?;
        assert!(matches!(child.state(), DurableOperationState::Finalized(_)));
        assert!(child.transitions().iter().any(|transition| matches!(
            transition.state(),
            DurableOperationState::ProvenNotDispatched(_)
        )));
        assert!(!child.transitions().iter().any(|transition| matches!(
            transition.state(),
            DurableOperationState::OutcomeUnknown(_)
        )));
    }
    assert!(
        matches!(
            reopened_head.manifest().lifecycle(),
            RuntimeLifecycle::Quiescent
        ),
        "{label}\n{after}"
    );
    // Every operation the cell may have started is settled outcome-unknown
    // from the durable evidence alone, and named as a possible effect.
    for operation in &uncertain {
        let record = reopened_head
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .find(|record| record.operation_id() == *operation)
            .ok_or("operation vanished")?;
        assert!(
            record.transitions().iter().any(|transition| matches!(
                transition.state(),
                DurableOperationState::OutcomeUnknown(_)
            )) && matches!(record.state(), DurableOperationState::Finalized(_)),
            "{label}: op {operation} was not settled outcome-unknown\n{after}"
        );
        assert!(
            report.effects().contains(operation),
            "{label}: op {operation}"
        );
    }
    assert!(
        reopened_head
            .manifest()
            .ledger()
            .operations()
            .iter()
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_))),
        "{label}: unsettled debt survived\n{after}"
    );
    // Nothing the cell started is replayed: no source, no nested call, no
    // provider request.
    assert!(
        fresh.executes.load(Ordering::Acquire) == 0,
        "{label}: source re-ran"
    );
    assert!(lock_mutex(&fresh.deliveries).is_empty(), "{label}");
    assert_eq!(model.calls.load(Ordering::Acquire), 0, "{label}");
    // The single terminal for the admitted root is the abandonment.
    let root = reopened_head
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .find(|command| command.turn_id() == Some(turn_id))
        .ok_or("root command missing")?;
    let terminal =
        terminal_record_at_checkpoint(&reopened_head, &CommandId::new(root.command_id())?)?;
    assert!(
        matches!(&terminal.outcome, DurableTerminalOutcome::Cancelled(error)
            if format!("{error:?}").contains("abandoned after the process was lost")),
        "{label}: {:?}",
        terminal.outcome
    );
    // A live job the lost process owned keeps the ordinary crash rule: its
    // row survives and reports that its outcome is unknown.
    if prior_job {
        let jobs = reopened.jobs();
        assert_eq!(jobs.len(), 1, "{label}");
        assert_eq!(
            jobs[0].status,
            plakous_tools::JobStatus::OutcomeUnknown,
            "{label}"
        );
    }

    // The next prompt runs on the recovered session. The first scripted
    // rounds belong to the abandoned turns, so skip past them.
    model.calls.store(3, Ordering::Release);
    let next = reopened.prompt("after the crash").await?.result().await?;
    assert!(
        matches!(next.outcome(), TurnResultOutcome::Completed(_)),
        "{label}: {:?}",
        next.outcome()
    );
    assert_eq!(fresh.executes.load(Ordering::Acquire), 0, "{label}");
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn completed_nested_effect_with_dispatched_cell_reopens_a_closed_candidate() -> TestResult {
    crash_matrix(
        CellLanguage::JavaScript,
        &[CrashPoint::NestedEffectCompleted],
    )
}

#[test]
fn prepared_nested_child_with_dispatched_cell_reopens_a_closed_candidate() -> TestResult {
    crash_matrix(CellLanguage::JavaScript, &[CrashPoint::NestedChildPrepared])
}

fn crash_matrix(language: CellLanguage, points: &'static [CrashPoint]) -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(120), move || async move {
        for &point in points {
            Box::pin(crash_and_reopen(point, language, false)).await?;
        }
        Ok(())
    })
}

#[test]
fn host_loss_during_a_native_code_mode_cell_reopens_abandoned_without_replay() -> TestResult {
    crash_matrix(
        CellLanguage::Native,
        &[
            CrashPoint::BeforeCellPrepared,
            CrashPoint::NestedProcessRunning,
            CrashPoint::NestedProcessYielded,
        ],
    )
}

#[test]
fn host_loss_during_a_javascript_cell_reopens_abandoned_without_replay() -> TestResult {
    crash_matrix(
        CellLanguage::JavaScript,
        &[
            CrashPoint::BeforeNested,
            CrashPoint::NestedToolRunning,
            CrashPoint::NestedToolReturned,
        ],
    )
}

#[test]
fn host_loss_during_a_python_cell_reopens_abandoned_without_replay() -> TestResult {
    crash_matrix(
        CellLanguage::Python,
        &[CrashPoint::BeforeNested, CrashPoint::NestedToolRunning],
    )
}

#[test]
fn host_loss_during_a_later_cell_keeps_the_earlier_cells_job_outcome_unknown() -> TestResult {
    run_cancel_cell_scenario_with_timeout(8 << 20, Duration::from_secs(60), || async {
        Box::pin(crash_and_reopen(
            CrashPoint::NestedProcessYielded,
            CellLanguage::Native,
            true,
        ))
        .await
    })
}
