//! Negative cases at the bound-turn custody settlement boundary.

use super::*;
use crate::agent::children::settle_bound_fence;
use crate::runtime::root_loss::{RecoveryFence, child_fence, classify_recovery_fence};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Case {
    Lease,
    Unproved,
    Abandoned,
    Rebound,
    NonQuiescent,
}

#[tokio::test]
async fn successor_does_not_complete_a_submitted_lease_fence() {
    reject_submission("submitted-custody-lease", Case::Lease).await;
}

#[tokio::test]
async fn successor_does_not_complete_an_unproved_submitted_fence() {
    reject_submission("submitted-custody-unproved", Case::Unproved).await;
}

#[tokio::test]
async fn successor_does_not_complete_a_submitted_fence_with_abandonment_evidence() {
    reject_submission("submitted-custody-abandoned", Case::Abandoned).await;
}

#[tokio::test]
async fn successor_does_not_complete_a_submission_rebound_to_another_turn() {
    reject_submission("submitted-custody-rebound", Case::Rebound).await;
}

#[tokio::test]
async fn successor_does_not_complete_a_submission_at_a_working_head() {
    reject_submission("submitted-custody-working", Case::NonQuiescent).await;
}

async fn reject_submission(name: &str, case: Case) {
    let fixture = Fixture::new(name);
    let id = fixture.spawn();
    let (locator, turn, fence) = fenced_head(&fixture, &id, case).await;
    let child = id.clone();
    fixture
        .runtime
        .mutate_then_emit(false, move |tree| {
            let turn_token = tree.attempt(&child).unwrap().turn_token;
            tree.submit_result(
                &child,
                &crate::agent_tree_protocol::SubmitResultRequest {
                    turn_token,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .map_err(|error| error.message)?;
            tree.apply_reopen_law();
            assert!(tree.recovery_owed(&child));
            if case == Case::Rebound {
                // The custody scan captured the old binding; the writer
                // must check its identity again, not just the attempt ID.
                tree.bind_attempt_turn(&child, "00000000-0000-4000-8000-000000000002")
                    .map_err(|error| error.message)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let result = settle_bound_fence(
        &fixture.runtime,
        &id,
        &turn,
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: fixture.sessions(),
            opened: None,
            recovered_turn: None,
            locator,
            topology_owner: None,
        },
        fence,
    )
    .await;
    if case == Case::NonQuiescent {
        assert!(
            result
                .unwrap_err()
                .contains("reopened head is not quiescent"),
            "the probe must refuse an exact working head"
        );
    } else {
        result.unwrap();
    }
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&id).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Submitted);
            assert_eq!(attempt.output_json.as_deref(), Some(r#"{"ok":true}"#));
            assert!(
                !tree
                    .messages()
                    .values()
                    .any(|mail| { mail.from == id && mail.purpose == MessagePurpose::Result })
            );
        })
        .unwrap();
}

async fn fenced_head(
    fixture: &Fixture,
    id: &str,
    case: Case,
) -> (SessionLocator, String, RecoveryFence) {
    let sessions = fixture.sessions();
    if case == Case::Lease {
        bind_recorded_lease_fence(fixture, id).await;
        let locator = fixture.locator(id).unwrap().parse().unwrap();
        let turn = fixture
            .runtime
            .read(|tree| tree.attempt(id).unwrap().turn_id.clone().unwrap())
            .unwrap();
        let fence = child_fence(&sessions, locator, &turn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fence, RecoveryFence::LeaseLoss);
        return (locator, turn, fence);
    }
    let (session, handle, _) = fixture.start_failing_turn(&sessions, id).await;
    let turn = handle.turn_id();
    let terminal = handle.result().await.unwrap();
    let TurnResultOutcome::RecoveryRequired(error) = terminal.outcome() else {
        panic!("fixture requires a committed recovery-required terminal");
    };
    let source = session.release().await.unwrap();
    let locator = if case == Case::NonQuiescent {
        plakous_offline_crash_harness::seed_finalization_debt(&fixture.database(), source, turn)
            .await
            .unwrap()
    } else {
        fixture.quiescent_after_reopen(source).await
    };
    fixture.bind_turn(id, &locator, turn);
    let fence = match case {
        Case::Unproved => RecoveryFence::Unproved,
        Case::Abandoned => {
            // A retained terminal read can carry an earlier abandonment
            // report even though the new open itself found no work left.
            let report = serde_json::from_value(json!({
                "claim": "ModelRequest", "cause": "Interrupted",
                "model_round_completed": false, "tool_round_completed": false,
                "operations": [], "effects": [], "uncommitted_tool_calls": [],
            }))
            .unwrap();
            classify_recovery_fence(error, Some(&report))
        }
        _ => classify_recovery_fence(error, None),
    };
    (locator, turn.to_string(), fence)
}
