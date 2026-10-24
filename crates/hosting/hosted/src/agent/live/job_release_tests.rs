#![allow(clippy::unwrap_used, reason = "offline release fixture")]

use super::*;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn retiring_interrupted_child_transfers_job_and_crash_settles_unknown() {
    terminal_release_fixture(true).await;
}

#[tokio::test]
async fn direct_attachment_release_stops_job_and_reopens_with_terminal() {
    terminal_release_fixture(false).await;
}

#[tokio::test]
async fn slow_follow_up_allows_list_steer_and_close_without_losing_or_duplicating_delivery() {
    tokio::time::timeout(Duration::from_secs(10), async {
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
        .unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let plan = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "steps": [{"stream": [{"text": "waiting"}, {"stall": 30000}]}]
            }))
            .unwrap(),
        );
        let binding = scripted_runtime_binding(root.path(), controller, plan).unwrap();
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("slow-mail.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("slow-mail:create").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let turn = session.agent().prompt("start").await.unwrap();
        let child = ProductLiveChild::new(
            AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            },
            turn.id(),
            turn.control(),
        );
        let queued = Arc::clone(&child.follow_ups);
        let admitted = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let command = CommandId::new("slow-mail:follow-up").unwrap();
        let delivery = child.follow_up_with_ack(command.clone(), "follow up".into(), {
            let admitted = admitted.clone();
            let finish = finish.clone();
            async move {
                admitted.notify_one();
                finish.notified().await;
            }
        });
        assert!(
            runtime
                .try_occupy_live("root".into(), Box::new(child))
                .await
                .is_ok()
        );
        let delivery = tokio::spawn(delivery);
        admitted.notified().await;
        let (control, duplicate) = {
            let live = runtime.live.lock().await;
            assert_eq!(live.keys().count(), 1);
            let child = live.get("root").unwrap();
            (
                child.turn_control(),
                child.follow_up(command, "follow up".into()),
            )
        };
        assert!(
            control
                .steer_with_id(
                    CommandId::new("slow-mail:steer").unwrap(),
                    "steer while mail acknowledgement is slow"
                )
                .await
                .unwrap()
                .rejection()
                .is_none()
        );
        let duplicate = tokio::spawn(duplicate);
        let mut close = Box::pin(runtime.cancel_live("root"));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx).is_pending()))
                .await
        );
        {
            let live = runtime.live.lock().await;
            assert!(
                live.contains_key("root"),
                "retirement reserves the child slot"
            );
            assert!(
                live.get("root").is_none(),
                "close admitted despite slow delivery"
            );
        }
        assert!(queued.lock().unwrap().is_empty());
        finish.notify_one();
        delivery.await.unwrap().unwrap();
        duplicate.await.unwrap().unwrap();
        assert_eq!(
            queued.lock().unwrap().len(),
            1,
            "exact replay registers no duplicate"
        );
        close.await.unwrap();
        assert!(runtime.live.lock().await.is_empty());
    })
    .await
    .unwrap();
}

async fn terminal_release_fixture(child: bool) {
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
    .unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("evidence.jsonl"),
    )
    .unwrap();
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "process_tools": true,
            "steps": [
                {"output": {"tool": {"name": "process_session_start", "arguments": {
                    "command": "read line", "workdir": ".", "login": false,
                    "tty": false, "keep_stdin_open": true, "yield_time_ms": 0,
                    "max_output_bytes": 8192
                }}}, "finish": "tool_calls"},
                {"stream": [{"text": "waiting"}, {"stall": 30000}]}
            ]
        }))
        .unwrap(),
    );
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let binding = || {
        scripted_runtime_binding(root.path(), controller.clone(), plan.clone())
            .unwrap()
            .with_job_host(registry.clone())
    };
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap())
            .unwrap(),
    );
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("child-job:create").unwrap(),
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created session required")
    };
    let agent = session.agent().clone();
    let turn = agent.prompt("start").await.unwrap();
    let control = turn.control();
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent.jobs().is_empty()
            || plakous_offline_crash_harness::evidence::read_evidence(
                root.path().join("evidence.jsonl"),
            )
            .unwrap()
            .iter()
            .filter(|record| {
                record.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
                < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.cancel().await.unwrap();
    let result = turn.result().await.unwrap();
    assert!(matches!(
        result.outcome(),
        plakous::agent::TurnResultOutcome::Cancelled(_)
    ));
    assert_eq!(agent.jobs()[0].status, plakous::tools::JobStatus::Running);
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: sessions.clone(),
        locator: session.locator(),
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    };
    let locator = if child {
        let released = Box::new(ProductLiveChild::new(attached, result.turn_id(), control))
            .release(runtime.driver().clone())
            .await;
        assert!(released.failure.is_none(), "{:?}", released.failure);
        let feed = released.feed.as_ref().unwrap();
        assert!(feed.records.iter().any(|record| matches!(
            record.header().change,
            plakous::sessions::feed::FeedChange::LeaseReleased { .. }
        )));
        released.locator.unwrap().parse().unwrap()
    } else {
        attached.release().await.unwrap()
    };
    if child {
        let number = agent.jobs()[0].number;
        assert_eq!(agent.jobs()[0].status, plakous::tools::JobStatus::Running);
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("child-job:transfer").unwrap(),
                locator,
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("transferred session required")
        };
        assert_eq!(session.agent().jobs()[0].number, number);
        assert_eq!(
            session.agent().jobs()[0].status,
            plakous::tools::JobStatus::Running
        );
        let locator = session.release_for_transfer().await.unwrap();
        // A new process has no exact registry authority: never reconstruct a PID.
        let fresh = scripted_runtime_binding(root.path(), controller.clone(), plan.clone())
            .unwrap()
            .with_job_host(Arc::new(crate::jobs::JobRegistry::default()));
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("child-job:after-crash").unwrap(),
                locator,
                fresh,
            ))
            .await
            .unwrap()
        else {
            panic!("crash-settled session required")
        };
        assert_eq!(session.agent().jobs()[0].number, number);
        assert_eq!(
            session.agent().jobs()[0].status,
            plakous::tools::JobStatus::OutcomeUnknown
        );
        session.release().await.unwrap();
        registry.stop_all(Duration::from_millis(250)).await.unwrap();
        return;
    }
    assert_eq!(
        agent.jobs()[0].status,
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::SessionExit)
    );
    // A fresh registry cannot infer a stop: only a pre-release durable terminal proves it.
    let fresh = scripted_runtime_binding(root.path(), controller, plan)
        .unwrap()
        .with_job_host(Arc::new(crate::jobs::JobRegistry::default()));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("child-job:reopen").unwrap(),
            locator,
            fresh,
        ))
        .await
        .unwrap()
    else {
        panic!("released locator must open exactly")
    };
    assert_eq!(
        session.agent().jobs()[0].status,
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::SessionExit)
    );
    session.release().await.unwrap();
}
