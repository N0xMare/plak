//! Host handle consumption and retry after a definite persistence refusal.

use super::*;
use crate::runtime::test_kit::TestExtension;
use crate::surface_protocol::HostEvent;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_host_protocol::ApprovalChoice;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn approval_intent_consumes_the_handle_before_any_resolution_event_is_applied() {
    approval_flow(ApprovalChoice::Allow, false).await;
}

#[tokio::test]
async fn approval_intent_retries_the_same_handle_after_transient_persistence_refusal() {
    for choice in [ApprovalChoice::Allow, ApprovalChoice::Deny] {
        approval_flow(choice, true).await;
    }
}

async fn approval_flow(choice: ApprovalChoice, refuse_once: bool) {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("runtime.sqlite3");
    let evidence = root.path().join("evidence.jsonl");
    let release = root.path().join("continue");
    let script = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version":1, "tool_policy":"decision_required", "steps":[
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"gate":{"file":release},"output":{"text":"decision complete"}}
            ]
        }))
        .unwrap(),
    );
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("host-approval:create").unwrap(),
            scripted_runtime_binding(root.path(), controller, script).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("fresh session")
    };
    let session_id = session.locator().session_id();
    let mut turn = session.agent().prompt("decide this effect").await.unwrap();
    let mut observations = turn.take_events().unwrap();
    let decision = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let AgentEventKind::EffectDecisionRequired { decision, .. } =
                observations.recv().await.unwrap().kind()
            {
                break decision.clone();
            }
        }
    })
    .await
    .unwrap();
    let mut projection = TurnProjection::<TestExtension>::default();
    let handle = "approval-host-single-use";
    projection
        .approvals
        .insert(handle.into(), PendingApproval { decision });
    let (sent, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let sent = Sender::over(sent);

    if refuse_once {
        // Inject decision-receipt object-capacity exhaustion at the real SQLite
        // publication seam, then restore the counter after the rolled-back
        // command classification. No effect or rejection receipt may commit.
        let connection = rusqlite::Connection::open(&database).unwrap();
        let old: i64 = connection
            .query_row("SELECT objects FROM store_meta WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        connection
            .execute(
                "UPDATE store_meta SET objects=?1 WHERE id=1",
                [i64::MAX / 4],
            )
            .unwrap();
        let before = crate::agent::read_tree_feed(&sessions, session_id, 0)
            .await
            .unwrap()
            .records
            .len();
        let applied =
            resolve_approval_intent(&turn.control(), &sent, &mut projection, handle, choice).await;
        connection
            .execute("UPDATE store_meta SET objects=?1 WHERE id=1", [old])
            .unwrap();
        assert!(!applied);
        let refusals = received.try_iter().collect::<Vec<_>>();
        assert_eq!(
            projection.approvals.len(),
            1,
            "definite refusal stranded its offered handle: {refusals:?}"
        );
        assert_eq!(
            crate::agent::read_tree_feed(&sessions, session_id, 0)
                .await
                .unwrap()
                .records
                .len(),
            before
        );
        assert!(
            read_evidence(&evidence)
                .unwrap()
                .iter()
                .all(|row| row.kind != EvidenceKind::EffectDispatched)
        );
        assert!(refusals.iter().any(|event| matches!(event,
            HostEvent::Row(TranscriptRow::Notice(notice)) if notice.text.starts_with("approval was not applied:")
        )));
    }

    assert!(resolve_approval_intent(&turn.control(), &sent, &mut projection, handle, choice).await);
    // Leave every runtime resolution event unapplied. The host, not a later
    // event, must consume the handle. Wait only for the gated continuation so
    // concurrent consequence publication cannot muddy the no-runtime-call law.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if read_evidence(&evidence)
                .unwrap()
                .iter()
                .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
                .count()
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = crate::agent::read_tree_feed(&sessions, session_id, 0)
        .await
        .unwrap()
        .records
        .len();
    assert!(
        !resolve_approval_intent(&turn.control(), &sent, &mut projection, handle, choice).await
    );
    assert!(received.try_iter().any(|event| matches!(event,
        HostEvent::Row(TranscriptRow::Notice(notice)) if notice.text == APPROVAL_REFUSED_NOTE
    )));
    assert_eq!(
        crate::agent::read_tree_feed(&sessions, session_id, 0)
            .await
            .unwrap()
            .records
            .len(),
        before,
        "duplicate approval reached the runtime control ledger"
    );
    std::fs::write(release, "continue").unwrap();
    assert!(matches!(
        turn.result().await.unwrap().outcome(),
        plakous::agent::TurnResultOutcome::Completed(_)
    ));
    assert_eq!(
        read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| row.kind == EvidenceKind::EffectDispatched)
            .count(),
        usize::from(choice == ApprovalChoice::Allow)
    );
    session.release().await.unwrap();
}
