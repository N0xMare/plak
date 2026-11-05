//! A cancel sent right after a durable Start is admitted is always answered,
//! including when the actor stops behind it.
use super::*;
use crate::runtime::driver::exit_probe::LoopFault;
use crate::{ActorControl, CancellationStatus, ControlError, ControlErrorKind, TurnCommand};
use std::time::Duration;

/// Admits one Start whose only round ends at once with an unknown provider
/// outcome, which stops the actor, then cancels it after `delay_ms`.
async fn cancel_a_turn_that_stops_its_actor(
    delay_ms: u64,
    round: u128,
) -> TestResult<(Result<CancellationStatus, ControlError>, TurnResultOutcome)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 120_000 + round * 16)?;
    model.unknown_round_outcome.store(true, Ordering::Release);
    let runtime = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:cancel-ack",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let submission = agent
        .start(CommandId::new("cancel-ack:start")?, "start then cancel")
        .await?;
    assert_eq!(
        submission.acknowledgement().disposition(),
        CommandAckDisposition::Accepted
    );
    let turn = submission.into_turn().ok_or("turn")?;
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    let cancel = turn.control().cancel().await;
    let outcome = turn.result().await?.outcome().clone();
    let _ = agent.shutdown().await;
    Ok((cancel, outcome))
}

/// A cancel queued while the actor ends a recovery-requiring turn and stops
/// is refused typed (`Stale`, or `Shutdown` once ingress is closed), never
/// read as a lost reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_queued_behind_an_actor_stop_is_answered() -> TestResult {
    let mut round = 0_u128;
    for delay_ms in [0, 1, 5, 20] {
        for _ in 0..25 {
            round += 1;
            let (cancel, outcome) = cancel_a_turn_that_stops_its_actor(delay_ms, round).await?;
            assert!(
                matches!(outcome, TurnResultOutcome::RecoveryRequired(_)),
                "{delay_ms} ms: {outcome:?}"
            );
            match cancel {
                Ok(CancellationStatus::AlreadyTerminal) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ControlErrorKind::Stale | ControlErrorKind::Shutdown
                    ) => {}
                other => panic!("{delay_ms} ms: the cancel was not answered typed: {other:?}"),
            }
        }
    }
    Ok(())
}

/// What a cancel queued behind an actor's exit was answered, and whether its
/// turn's terminal was published when the answer arrived.
type ExitAnswer = (Result<CancellationStatus, ControlError>, bool);

/// Queues a cancel for `turn` without waiting for its answer.
fn queue_cancel(turn: &crate::Turn) -> tokio::task::JoinHandle<ExitAnswer> {
    let control = turn.control();
    tokio::spawn(async move {
        let answer = control.cancel().await;
        (answer, control.shared.has_published_result())
    })
}

/// A durable agent whose first turn's model call is held until cancelled,
/// with room for two queued roots behind it.
async fn agent_with_a_held_turn(
    seed: u128,
) -> TestResult<(tempfile::TempDir, DurableSessionRuntime, Agent, crate::Turn)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            seed,
            false,
        )
        .with_joined_cancellation(Arc::new(AtomicBool::new(false))),
    );
    let agent = sessions
        .create(
            "create:exit-ack",
            durable_artifact_test_builder(workspace, Arc::clone(&model))
                .config(AgentConfig::default().with_prompt_queue_capacity(3)),
        )
        .await?;
    let first = agent.prompt("held until cancelled").await?;
    // Liveness bounds only: generous, so load never trips them.
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok((directory, sessions, agent, first))
}

/// Ends the actor's loop with `fault` while one root is active and one is
/// queued, holds the actor before its ingress closes, and queues a cancel
/// for each of those roots, one for an already-cancelled turn and a send
/// that reserved its slot before the close. With `fail_queued_settlement`
/// the queued root's terminal cannot be published.
async fn cancels_behind_an_exit(
    fault: LoopFault,
    fail_queued_settlement: bool,
    seed: u128,
) -> TestResult {
    let (_directory, _sessions, agent, first) = agent_with_a_held_turn(seed).await?;
    let active = agent
        .start(CommandId::new("exit-ack:active")?, "active at the exit")
        .await?
        .into_turn()
        .ok_or("active root was not admitted")?;
    let queued = agent
        .start(CommandId::new("exit-ack:queued")?, "queued at the exit")
        .await?
        .into_turn()
        .ok_or("queued root was not admitted")?;
    if fail_queued_settlement {
        queued.control.shared.inject_terminal_fault(4);
    }
    let probe = Arc::clone(&agent.inner.exit_probe);
    probe.arm(fault);
    let exit_hold = probe.hold().await;
    first.control().cancel().await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !probe.reached() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(!active.control.shared.has_published_result());
    assert!(!queued.control.shared.has_published_result());
    let controls = agent.inner.controls.clone();
    let answers = [
        queue_cancel(&first),
        queue_cancel(&active),
        queue_cancel(&queued),
    ];
    tokio::time::timeout(Duration::from_secs(30), async {
        while controls.capacity() + answers.len() != controls.max_capacity() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let late = controls.reserve().await?;
    drop(exit_hold);
    let mut answered = Vec::new();
    for answer in answers {
        answered.push(tokio::time::timeout(Duration::from_secs(30), answer).await??);
    }
    let stale = |(answer, published): &ExitAnswer| {
        *published && matches!(answer, Err(error) if error.kind() == ControlErrorKind::Stale)
    };
    assert!(stale(&answered[0]), "{fault:?}: {:?}", answered[0]);
    assert!(stale(&answered[1]), "{fault:?}: {:?}", answered[1]);
    let queued_refusal = if fail_queued_settlement {
        assert!(
            matches!(&answered[2], (Err(error), false) if error.kind() == ControlErrorKind::Shutdown),
            "{fault:?}: {:?}",
            answered[2]
        );
        ControlErrorKind::Shutdown
    } else {
        assert!(stale(&answered[2]), "{fault:?}: {:?}", answered[2]);
        ControlErrorKind::Stale
    };
    // The actor is still draining: a send that reserved before the close
    // lands now and is answered, not dropped.
    let (reply, late_answer) = tokio::sync::oneshot::channel();
    late.send(ActorControl::Command {
        command: TurnCommand::Cancel {
            command_id: CommandId::new("exit-ack:late")?,
            turn_id: queued.control.turn_id(),
        },
        reply,
    });
    let late_answer = tokio::time::timeout(Duration::from_secs(30), late_answer).await??;
    assert_eq!(
        late_answer
            .acknowledgement
            .rejection()
            .map(ControlError::kind),
        Some(queued_refusal)
    );
    // After the close a send is refused at once.
    let after = active.control().cancel().await;
    assert!(
        matches!(&after, Err(error) if error.kind() == ControlErrorKind::Shutdown),
        "{after:?}"
    );
    let _ = agent.shutdown().await;
    assert!(active.result().await.is_ok());
    let queued_result = queued.result().await;
    if fail_queued_settlement {
        assert!(queued_result.is_err());
    } else {
        assert!(matches!(
            queued_result?.outcome(),
            TurnResultOutcome::Cancelled(_)
        ));
    }
    Ok(())
}

/// A fatal loop error and a loop panic: every cancel queued behind the exit
/// is answered after the roots settle, `Stale` with its terminal published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancels_behind_a_failed_or_panicked_loop_are_answered_after_settlement() -> TestResult {
    cancels_behind_an_exit(LoopFault::Error, false, 9_850).await?;
    cancels_behind_an_exit(LoopFault::Panic, false, 9_851).await
}

/// A queued root whose terminal cannot be published: its cancel is refused
/// `Shutdown`, never `Stale`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_for_a_root_whose_settlement_failed_is_refused_shutdown() -> TestResult {
    cancels_behind_an_exit(LoopFault::Error, true, 9_852).await?;
    cancels_behind_an_exit(LoopFault::Panic, true, 9_853).await
}
