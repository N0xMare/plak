#![allow(
    clippy::too_many_lines,
    reason = "Live child fixtures construct real sessions to exercise restart controls and asynchronous admission ownership."
)]

use super::*;
use plakous::agent::{CommandAckDisposition, TurnResultOutcome};
use plakous::model::ModelErrorKind;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::ledger::ChildLedger;
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};
#[tokio::test]
async fn fresh_restart_retains_control_after_terminal_before_ack() {
    restart_control_fixture(false, false).await;
}

/// A throwaway release owner runs its actor and nothing else: it has no
/// observation task.
#[test]
fn throwaway_release_owner_has_no_observation_task() {
    let executor = tokio::runtime::Runtime::new().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (attached, turn, control) = executor.block_on(async {
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("release.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("release-evidence.jsonl"),
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("throwaway-release").unwrap(),
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("session required");
        };
        let turn = session.agent().prompt("work").await.unwrap();
        let (id, control) = (turn.id(), turn.control());
        turn.result().await.unwrap();
        (
            AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                locator: session.locator(),
                sessions,
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            },
            id,
            control,
        )
    });
    executor
        .block_on(async {
            let child = ChildActorHandle::without_observation(
                &crate::TaskSet::new(),
                &crate::TaskSet::new(),
                attached,
                turn,
                control,
            );
            assert!(child.host_observation().is_none());
            child.into_attached().await.release_opened().await
        })
        .unwrap();
}

#[tokio::test]
async fn duplicate_wake_restart_below_retention_is_consumed_without_rebinding() {
    restart_control_fixture(true, false).await;
}

#[tokio::test]
async fn reclaimed_wake_restart_is_consumed_without_rebinding() {
    restart_control_fixture(false, true).await;
}

/// Drives a restart until the child's actor reaches its acknowledgement,
/// then drops it there.
async fn abandon_at_acknowledgement<T>(
    pending: impl Future<Output = T>,
    acknowledging: tokio::sync::oneshot::Receiver<()>,
    unacknowledged: &str,
) {
    tokio::select! {
        biased;
        _ = pending => panic!("{unacknowledged}"),
        entered = acknowledging => entered.unwrap(),
    }
}

async fn restart_control_fixture(below_retention: bool, reclaimed: bool) {
    let root = tempfile::tempdir().unwrap();
    let tree_id = uuid::Uuid::new_v4().to_string();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        root.path().to_owned(),
        "selected",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let runtime = crate::topology_runtime::insert_root_with(
        tree_id,
        tree,
        Arc::new(crate::agent_tree::MemoryTreeStore::new()),
        None,
    )
    .await
    .unwrap();
    ChildLedger::new(runtime.driver(), "root")
        .prepare(CommandIntent {
            command_id: "restart-next",
            kind: CommandKind::Followup,
            base_text: "submit result",
            batch: BatchRule::Results,
        })
        .await
        .unwrap();
    let evidence = root.path().join("restart-evidence.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("restart.sqlite3")).unwrap())
            .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("restart-create").unwrap(),
            terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                .unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    let agent = session.agent().clone();
    let first = agent
        .start(CommandId::new("restart-first").unwrap(), "work")
        .await
        .unwrap();
    let first_turn = first.acknowledgement().turn_id().unwrap();
    let control = first.turn().unwrap().control();
    first.result().unwrap().clone().result().await.unwrap();
    let mut child = ChildActorHandle::new(
        &crate::TaskSet::new(),
        &crate::TaskSet::new(),
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        },
        first_turn,
        control,
    );
    let command = CommandId::new("restart-next").unwrap();
    let previous_projection = crate::runtime::child_projection::of(&child).unwrap();
    if !below_retention && !reclaimed {
        let (entered, acknowledging) = tokio::sync::oneshot::channel();
        let pending = child.restart_with_ack(
            runtime.driver(),
            CommandId::new("delayed-ack-probe").unwrap(),
            "submit result".into(),
            async move {
                let _ = entered.send(());
                std::future::pending::<()>().await;
            },
        );
        // Wait for acknowledgement delivery, then cancel just this observer.
        abandon_at_acknowledgement(pending, acknowledging, "the acknowledgement never settles")
            .await;
        assert_eq!(
            child.turn_id(),
            first_turn,
            "an unsettled acknowledgement cannot rebind the handle"
        );
        assert!(Arc::ptr_eq(
            &crate::runtime::child_projection::of(&child).unwrap(),
            &previous_projection
        ));
        let replay = agent
            .start_instruction(
                CommandId::new("delayed-ack-probe").unwrap(),
                "submit result",
            )
            .await
            .unwrap();
        replay.result().unwrap().clone().result().await.unwrap();
    }
    let result = child
        .restart_with_ack(runtime.driver(), command.clone(), "submit result".into(), {
            let (agent, command) = (agent.clone(), command.clone());
            async move {
                tokio::time::timeout(Duration::from_secs(20), async {
                    let replay = agent
                        .start_instruction(command.clone(), "submit result")
                        .await
                        .unwrap();
                    assert_eq!(
                        replay.acknowledgement().disposition(),
                        CommandAckDisposition::Duplicate
                    );
                    let result = replay.result().unwrap().clone();
                    let turn = result.turn_id();
                    assert!(matches!(
                        result.result().await.unwrap().outcome(),
                        TurnResultOutcome::Failed(_)
                    ));
                    while agent.turn_control(turn).is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
        })
        .await
        .unwrap();
    assert_ne!(result.turn_id(), first_turn);
    assert_eq!(child.turn_id(), result.turn_id());
    assert_eq!(child.turn_control().turn_id(), result.turn_id());
    let projection = crate::runtime::child_projection::of(&child).unwrap();
    assert!(!Arc::ptr_eq(&projection, &previous_projection));
    assert!(matches!(
        result.result().await.unwrap().outcome(),
        TurnResultOutcome::Failed(_)
    ));
    if reclaimed {
        for round in 0..17 {
            child
                .agent()
                .unwrap()
                .start_instruction(
                    CommandId::new(format!("prune:{round}")).unwrap(),
                    "submit result",
                )
                .await
                .unwrap()
                .result()
                .unwrap()
                .clone()
                .result()
                .await
                .unwrap();
        }
        let sessions = child.sessions();
        let (bound, control) = (child.turn_id(), child.turn_control());
        let locator = child.into_attached().await.release_opened().await.unwrap();
        assert!(matches!(
            sessions.prune_history(locator).await.unwrap(),
            plakous::sessions::PruneHistoryOutcome::Pruned(_)
        ));
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = sessions
            .open(plakous::sessions::OpenSessionRequest::new(
                SessionOperationId::new("restart:pruned:reopen").unwrap(),
                locator,
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("pruned session must reopen");
        };
        child = ChildActorHandle::new(
            &crate::TaskSet::new(),
            &crate::TaskSet::new(),
            AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            },
            bound,
            control,
        );
        let submission = child
            .agent()
            .unwrap()
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id: command.clone(),
                text: "submit result".to_owned(),
            })
            .await
            .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
        assert_eq!(
            submission
                .acknowledgement()
                .rejection()
                .map(plakous::agent::ControlError::kind),
            Some(plakous::agent::ControlErrorKind::Reclaimed)
        );
    }
    if below_retention {
        let connection = rusqlite::Connection::open(root.path().join("restart.sqlite3")).unwrap();
        connection
            .execute_batch(
                "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
                 UPDATE sessions SET feed_retained_from=feed_tail,\
                 feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
            )
            .unwrap();
    }
    let bound = child.turn_id();
    let (entered, acknowledging) = tokio::sync::oneshot::channel();
    let pending = child.restart_with_ack(
        runtime.driver(),
        command.clone(),
        "submit result".into(),
        async move {
            let _ = entered.send(());
            std::future::pending::<()>().await;
        },
    );
    abandon_at_acknowledgement(
        pending,
        acknowledging,
        "consumed/rejected checks must not bypass pending acknowledgement delivery",
    )
    .await;
    assert_eq!(child.turn_id(), bound);
    let Err(error) = child
        .restart_with_ack(
            runtime.driver(),
            command,
            "submit result".into(),
            std::future::ready(()),
        )
        .await
    else {
        panic!("retired duplicate cannot start another turn");
    };
    if below_retention || reclaimed {
        assert!(matches!(error, ChildRestartError::Consumed));
        assert_eq!(child.turn_id(), bound);
    } else {
        assert!(matches!(error, ChildRestartError::ReconciliationRequired));
    }
    let locator = child.locator();
    let cursor = child
        .sessions()
        .subscribe_feed(locator.session_id())
        .await
        .unwrap()
        .seen()
        .get();
    let observed = released_with_feed(&child.sessions(), locator, None, cursor, false).await;
    assert!(observed.failure.is_none());
    assert_eq!(
        observed.feed.unwrap().records,
        [] as [plakous::sessions::feed::FeedRecord; 0]
    );
    assert!(
        Box::new(child)
            .release(runtime.driver().clone())
            .await
            .failure
            .is_none()
    );
    assert_eq!(
        std::fs::read_to_string(evidence)
            .unwrap()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    == "provider_dispatched"
            })
            .count(),
        if reclaimed {
            19
        } else if below_retention {
            2
        } else {
            3
        },
        "one dispatch per admitted turn, none for the duplicate"
    );
}
